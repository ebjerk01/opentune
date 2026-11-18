//! Optional **GPS channels** for a live log ([`opentune_gps`]).
//!
//! With a GPS source attached, every logged sample (one tick of the ECU
//! channels) gets eight more channels from the receiver's latest state:
//! [`GPS_CHANNELS`] (`gps_lat`, `gps_lon`, `gps_alt_m`, `gps_speed_kmh`,
//! `gps_course_deg`, `gps_sats`, `gps_fix`, `gps_hdop`). When the log also
//! has a vehicle-speed channel, a derived [`SPEED_ERROR_ID`] channel carries
//! vehicle speed minus GPS speed in km/h — only while the fix is good and the
//! GPS speed is at least [`SPEED_ERROR_MIN_KMH`].
//!
//! A stale source or a lost fix never stops the log: the affected values are
//! left empty and the sample's `note` says why (`stale: no NMEA for 3.0 s`,
//! `no fix (GGA quality 0)`, `replay ended …`). `gps_sats`, `gps_fix` and
//! `gps_hdop` stay filled while the receiver is talking without a fix, which
//! is exactly when they are interesting.

use crate::live::Sample;
use crate::model::{Addressing, LogParam};
use opentune_gps::{GpsFeed, GpsSnapshot, NmeaStats};

/// The GPS channels, in output order: `(id, name, units)`.
pub const GPS_CHANNELS: [(&str, &str, &str); 8] = [
    ("gps_lat", "GPS latitude", "deg"),
    ("gps_lon", "GPS longitude", "deg"),
    ("gps_alt_m", "GPS altitude (MSL)", "m"),
    ("gps_speed_kmh", "GPS speed over ground", "km/h"),
    ("gps_course_deg", "GPS course over ground", "deg"),
    ("gps_sats", "GPS satellites used", "count"),
    (
        "gps_fix",
        "GPS fix quality (0 none, 1 GPS, 2 DGPS, 4/5 RTK, 6 estimated)",
        "",
    ),
    ("gps_hdop", "GPS HDOP", ""),
];

/// Id of the derived vehicle-minus-GPS speed channel.
pub const SPEED_ERROR_ID: &str = "speed_error_kmh";

/// GPS speed below which [`SPEED_ERROR_ID`] is left empty: at walking pace
/// the GPS speed noise swamps any speedometer error.
pub const SPEED_ERROR_MIN_KMH: f64 = 5.0;

/// Parameter ids recognised as vehicle speed (case-insensitive), besides OBD
/// PID `0x0D` and names containing "vehicle speed".
const SPEED_IDS: [&str; 8] = [
    "vss",
    "speed",
    "vehicle_speed",
    "vehicle_speed_kmh",
    "veh_speed",
    "obd_speed",
    "obd_vss",
    "spd",
];

fn blank(id: &str, name: &str, units: &str) -> Sample {
    Sample {
        param_id: id.into(),
        name: name.into(),
        raw: None,
        value: None,
        units: units.into(),
        note: String::new(),
        text: None,
    }
}

/// The eight GPS samples for one snapshot, in [`GPS_CHANNELS`] order.
pub fn gps_samples(snap: &GpsSnapshot) -> Vec<Sample> {
    let f = &snap.fix;
    let pos = |v: Option<f64>| if snap.good { v } else { None };
    // Receiver status: meaningful whenever it is talking, fix or not.
    let status = |v: Option<f64>| if snap.stale { None } else { v };
    let values = [
        pos(f.lat),
        pos(f.lon),
        pos(f.alt_m),
        pos(f.speed_kmh),
        pos(f.course_deg),
        status(f.sats.map(f64::from)),
        status(Some(f64::from(f.fix_code()))),
        status(f.hdop),
    ];
    GPS_CHANNELS
        .iter()
        .zip(values)
        .map(|(&(id, name, units), v)| {
            let mut s = blank(id, name, units);
            s.value = v;
            if v.is_none() || !snap.good {
                s.note.clone_from(&snap.note);
            }
            s
        })
        .collect()
}

/// The km/h factor for a speed unit string (`km/h`, `kph`, `mph`, `m/s`), or
/// `None` when it is not a speed unit.
pub fn speed_unit_factor(units: &str) -> Option<f64> {
    let u: String = units
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    match u.as_str() {
        "km/h" | "kmh" | "kph" | "km/hr" => Some(1.0),
        "mph" | "mi/h" => Some(1.609_344),
        "m/s" => Some(3.6),
        _ => None,
    }
}

/// Index of the vehicle-speed channel in `params`: the explicit `id` when
/// given, else OBD-II PID `0x0D`, a known speed id (`vss`, `vehicle_speed`,
/// …) or a name containing "vehicle speed".
pub fn vehicle_speed_index(params: &[LogParam], id: Option<&str>) -> Option<usize> {
    if let Some(id) = id {
        return params.iter().position(|p| p.id == id);
    }
    let pid = params
        .iter()
        .position(|p| matches!(p.addressing, Addressing::ObdPid { pid: 0x0D }));
    pid.or_else(|| {
        params.iter().position(|p| {
            let id = p.id.to_ascii_lowercase();
            !id.starts_with("gps_")
                && (SPEED_IDS.contains(&id.as_str())
                    || p.name.to_ascii_lowercase().contains("vehicle speed"))
        })
    })
}

/// The derived speed-error sample: vehicle minus GPS speed, km/h. Empty (with
/// the reason in `note`) unless the fix is good, the GPS speed is at least
/// [`SPEED_ERROR_MIN_KMH`] and the vehicle speed has a value in a speed unit.
/// `default_kmh` treats a unit-less vehicle value as km/h (OBD PID `0x0D` is
/// km/h by definition).
pub fn speed_error_sample(
    vehicle: Option<&Sample>,
    default_kmh: bool,
    snap: &GpsSnapshot,
) -> Sample {
    let mut s = blank(SPEED_ERROR_ID, "Speed error (vehicle − GPS)", "km/h");
    let gps = snap.fix.speed_kmh.filter(|_| snap.good);
    let veh = vehicle.and_then(|v| {
        let factor =
            speed_unit_factor(&v.units).or(if default_kmh && v.units.trim().is_empty() {
                Some(1.0)
            } else {
                None
            })?;
        v.value.map(|x| x * factor)
    });
    match (veh, gps) {
        (_, None) => {
            s.note = if snap.good {
                "no GPS speed".into()
            } else {
                snap.note.clone()
            }
        }
        (None, _) => {
            s.note = match vehicle {
                Some(v) if v.value.is_some() => {
                    format!("vehicle speed unit '{}' not recognised", v.units)
                }
                Some(v) if !v.note.is_empty() => format!("no vehicle speed: {}", v.note),
                _ => "no vehicle speed".into(),
            }
        }
        (Some(_), Some(g)) if g < SPEED_ERROR_MIN_KMH => {
            s.note = format!("GPS speed below {SPEED_ERROR_MIN_KMH} km/h");
        }
        (Some(v), Some(g)) => s.value = Some(v - g),
    }
    s
}

/// A GPS source attached to a live log: produces the GPS (and speed-error)
/// samples to append to each tick's ECU samples.
pub struct GpsLogger {
    feed: Box<dyn GpsFeed>,
    speed: Option<(usize, bool)>,
    last: Option<GpsSnapshot>,
    good: u64,
    ticks: u64,
}

impl GpsLogger {
    /// Attach `feed` to a log over `params` (the session's channels, in tick
    /// order). `speed_param` names the vehicle-speed channel explicitly;
    /// without it one is detected ([`vehicle_speed_index`]). An explicit id
    /// that is not among `params` is an error.
    pub fn new(
        feed: Box<dyn GpsFeed>,
        params: &[LogParam],
        speed_param: Option<&str>,
    ) -> Result<Self, String> {
        let idx = vehicle_speed_index(params, speed_param);
        if let (Some(id), None) = (speed_param, idx) {
            return Err(format!(
                "vehicle-speed channel '{id}' is not among the logged channels"
            ));
        }
        let speed = idx.map(|i| {
            let obd = matches!(params[i].addressing, Addressing::ObdPid { pid: 0x0D });
            (i, obd)
        });
        Ok(Self {
            feed,
            speed,
            last: None,
            good: 0,
            ticks: 0,
        })
    }

    /// The source's description.
    pub fn describe(&self) -> String {
        self.feed.describe()
    }

    /// Index (into the session's channels) of the vehicle-speed channel used
    /// for [`SPEED_ERROR_ID`], if any.
    pub fn speed_channel(&self) -> Option<usize> {
        self.speed.map(|(i, _)| i)
    }

    /// The ids this logger appends, in order.
    pub fn channel_ids(&self) -> Vec<&'static str> {
        let mut ids: Vec<&'static str> = GPS_CHANNELS.iter().map(|c| c.0).collect();
        if self.speed.is_some() {
            ids.push(SPEED_ERROR_ID);
        }
        ids
    }

    /// Take a snapshot and build the samples to append to `ecu` (one tick's
    /// ECU samples, in session channel order).
    pub fn samples(&mut self, ecu: &[Sample]) -> Vec<Sample> {
        let snap = self.feed.snapshot();
        self.ticks += 1;
        if snap.good {
            self.good += 1;
        }
        let mut out = gps_samples(&snap);
        if let Some((i, obd)) = self.speed {
            out.push(speed_error_sample(ecu.get(i), obd, &snap));
        }
        self.last = Some(snap);
        out
    }

    /// The most recent snapshot, if any tick ran.
    pub fn last(&self) -> Option<&GpsSnapshot> {
        self.last.as_ref()
    }

    /// `(ticks with a good fix, ticks)` so far.
    pub fn good_ticks(&self) -> (u64, u64) {
        (self.good, self.ticks)
    }

    /// NMEA line counters of the latest snapshot.
    pub fn stats(&self) -> NmeaStats {
        self.last
            .as_ref()
            .map(|s| s.stats.clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentune_gps::{GpsFix, ReplayFeed};

    fn good_snap(kmh: f64) -> GpsSnapshot {
        GpsSnapshot {
            fix: GpsFix {
                lat: Some(56.5),
                lon: Some(3.0),
                alt_m: Some(4.0),
                speed_kmh: Some(kmh),
                course_deg: Some(90.0),
                sats: Some(9),
                quality: Some(1),
                rmc_valid: Some(true),
                hdop: Some(0.9),
                ..GpsFix::default()
            },
            good: true,
            silent_ms: Some(0),
            age_ms: Some(0),
            ..GpsSnapshot::default()
        }
    }

    fn veh(v: Option<f64>, units: &str) -> Sample {
        Sample {
            param_id: "vss".into(),
            name: "Vehicle speed".into(),
            raw: None,
            value: v,
            units: units.into(),
            note: String::new(),
            text: None,
        }
    }

    #[test]
    fn samples_follow_the_snapshot_state() {
        let s = gps_samples(&good_snap(50.0));
        assert_eq!(s.len(), 8);
        assert_eq!(s[0].param_id, "gps_lat");
        assert_eq!(s[3].value, Some(50.0));
        assert!(s.iter().all(|x| x.value.is_some() && x.note.is_empty()));

        // No fix but talking: position empty, status present, all flagged.
        let mut nofix = good_snap(0.0);
        nofix.good = false;
        nofix.fix.quality = Some(0);
        nofix.note = "no fix (GGA quality 0)".into();
        let s = gps_samples(&nofix);
        assert!(s[..5].iter().all(|x| x.value.is_none()));
        assert_eq!(s[5].value, Some(9.0));
        assert_eq!(s[6].value, Some(0.0));
        assert!(s.iter().all(|x| x.note == "no fix (GGA quality 0)"));

        // Stale: everything empty.
        let mut stale = nofix.clone();
        stale.stale = true;
        stale.note = "stale: no NMEA for 3.0 s".into();
        assert!(gps_samples(&stale).iter().all(|x| x.value.is_none()));
    }

    #[test]
    fn speed_error_rules() {
        let snap = good_snap(100.0);
        let e = speed_error_sample(Some(&veh(Some(103.0), "km/h")), false, &snap);
        assert_eq!(e.value, Some(3.0));
        let e = speed_error_sample(Some(&veh(Some(62.5), "mph")), false, &snap);
        assert!((e.value.unwrap() - 0.584).abs() < 1e-9);
        // Unit-less OBD 0x0D is km/h; unit-less otherwise is not trusted.
        assert_eq!(
            speed_error_sample(Some(&veh(Some(98.0), "")), true, &snap).value,
            Some(-2.0)
        );
        let e = speed_error_sample(Some(&veh(Some(98.0), "")), false, &snap);
        assert!(e.value.is_none() && e.note.contains("not recognised"));
        // Below the threshold.
        let e = speed_error_sample(Some(&veh(Some(3.0), "km/h")), false, &good_snap(2.0));
        assert!(e.value.is_none() && e.note.contains("below"));
        // Bad fix.
        let mut bad = good_snap(100.0);
        bad.good = false;
        bad.note = "no fix (RMC status V)".into();
        let e = speed_error_sample(Some(&veh(Some(100.0), "km/h")), false, &bad);
        assert_eq!((e.value, e.note.as_str()), (None, "no fix (RMC status V)"));
        // Vehicle channel failed.
        let mut v = veh(None, "km/h");
        v.note = "negative response".into();
        let e = speed_error_sample(Some(&v), false, &snap);
        assert_eq!(e.note, "no vehicle speed: negative response");
    }

    fn obd(id: &str, pid: u8) -> LogParam {
        LogParam {
            id: id.into(),
            name: id.into(),
            description: String::new(),
            units: String::new(),
            protocol: crate::ProtocolKind::Obd2Mode01,
            addressing: Addressing::ObdPid { pid },
            storagetype: None,
            endian: opentune_core::ecudef::Endian::Big,
            value_kind: crate::ValueKind::Number,
            conversion: None,
            conversions: Vec::new(),
            min: None,
            max: None,
            alternates: Vec::new(),
            depends_on: Vec::new(),
            convert_group: None,
            enabled: None,
        }
    }

    #[test]
    fn logger_detects_the_speed_channel() {
        let params = vec![obd("P1", 0x0C)];
        let feed = Box::new(ReplayFeed::from_bytes("empty", b""));
        let mut g = GpsLogger::new(feed, &params, None).unwrap();
        assert_eq!(g.speed_channel(), None);
        assert_eq!(g.channel_ids().len(), 8);
        let s = g.samples(&[]);
        assert!(s.iter().all(|x| x.value.is_none()));
        assert!(s[0].note.starts_with("replay ended"), "{}", s[0].note);
        assert!(GpsLogger::new(
            Box::new(ReplayFeed::from_bytes("empty", b"")),
            &params,
            Some("nope")
        )
        .is_err());
        let mut renamed = params.clone();
        renamed[0].id = "vehicle_speed".into();
        let g = GpsLogger::new(Box::new(ReplayFeed::from_bytes("e", b"")), &renamed, None).unwrap();
        assert_eq!(g.speed_channel(), Some(0));
        assert_eq!(g.channel_ids().last(), Some(&SPEED_ERROR_ID));
        // OBD PID 0x0D wins over a name match, whatever its id.
        let both = vec![renamed[0].clone(), obd("P13", 0x0D)];
        assert_eq!(vehicle_speed_index(&both, None), Some(1));
    }
}
