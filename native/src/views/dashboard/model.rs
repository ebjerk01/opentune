//! Dashboard decisions (pure, unit-tested): the value presets, which PIDs a
//! poll requests, warn / crit levels, display units, the live-feed frame and
//! the Live-log CSV rows of a poll. The channel table itself is the core's
//! [`opentune_core::obd::CHANNELS`].

use crate::host::{FeedEvent, FeedValue};
use opentune_core::obd::{self, Channel, ObdValue, Readout};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// `source` of the frames the Dashboard publishes on the live feed.
pub const SOURCE: &str = "dashboard";

/// Seconds of history a tile's sparkline keeps.
pub const HIST_S: f64 = 90.0;

/// The values a preset shows (the gauges show RPM and boost on their own).
pub const DIESEL: &[&str] = &[
    "engine_load",
    "coolant_temp",
    "intake_air_temp",
    "intake_map",
    "maf",
    "fuel_rail_pressure",
    "egt_b1",
    "accel_pedal_d",
    "speed",
    "module_voltage",
    "oil_temp",
    "commanded_egr",
];
pub const PETROL: &[&str] = &[
    "engine_load",
    "coolant_temp",
    "intake_air_temp",
    "intake_map",
    "throttle_pos",
    "timing_advance",
    "stft_b1",
    "ltft_b1",
    "maf",
    "speed",
    "module_voltage",
    "oil_temp",
];

/// Values the Drive view's default tiles use: always polled when supported,
/// so its live mode has them whatever preset is shown here.
pub const DRIVE_KEYS: &[&str] = &[
    "rpm",
    "speed",
    "coolant_temp",
    "intake_air_temp",
    "boost_kpa",
    "engine_load",
];

/// The gauge keys (not tiles).
pub const RPM: &str = "rpm";
pub const BOOST: &str = "boost_kpa";

/// Which values the tiles show.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Preset {
    #[default]
    Diesel,
    Petrol,
    /// Everything the ECU reports.
    All,
    /// The user's own pick.
    Custom,
}

impl Preset {
    pub const ALL: [Preset; 4] = [Preset::Diesel, Preset::Petrol, Preset::All, Preset::Custom];

    pub fn label(self) -> &'static str {
        match self {
            Preset::Diesel => "Diesel set",
            Preset::Petrol => "Petrol set",
            Preset::All => "Everything supported",
            Preset::Custom => "My selection",
        }
    }

    /// The RPM gauge's top and redline start.
    pub fn redline(self) -> (f64, f64) {
        match self {
            Preset::Petrol => (7000.0, 6400.0),
            _ => (5000.0, 4500.0),
        }
    }
}

/// Is `ch` reported by an ECU whose supported PIDs are `sup`? Derived boost
/// needs a MAP PID and barometric pressure.
pub fn is_supported(ch: &Channel, sup: &BTreeSet<u8>) -> bool {
    if ch.is_derived() {
        (sup.contains(&0x0B) || sup.contains(&0x87)) && sup.contains(&0x33)
    } else {
        sup.contains(&ch.pid)
    }
}

/// Is the channel `key` known and (when discovery ran) supported?
pub fn available(key: &str, sup: Option<&BTreeSet<u8>>) -> bool {
    match obd::channel(key) {
        Some(c) => sup.is_none_or(|s| is_supported(c, s)),
        None => false,
    }
}

/// The tile keys of `preset` (`custom` for [`Preset::Custom`]), in order:
/// known, not a gauge, and supported once discovery ran.
pub fn selection(preset: Preset, custom: &[String], sup: Option<&BTreeSet<u8>>) -> Vec<String> {
    let keys: Vec<String> = match preset {
        Preset::Diesel => DIESEL.iter().map(|s| s.to_string()).collect(),
        Preset::Petrol => PETROL.iter().map(|s| s.to_string()).collect(),
        Preset::All => obd::channels().iter().map(|c| c.key.to_string()).collect(),
        Preset::Custom => custom.to_vec(),
    };
    let mut seen = BTreeSet::new();
    keys.into_iter()
        .filter(|k| k != RPM && k != BOOST && available(k, sup) && seen.insert(k.clone()))
        .collect()
}

/// Mode-01 PIDs one poll requests for `keys`: the gauges and the Drive
/// view's defaults too, only supported ones once discovery ran (baro for
/// boost is fetched by the core itself). Sorted, unique.
pub fn pids_for(keys: &[String], sup: Option<&BTreeSet<u8>>) -> Vec<u8> {
    let mut out = BTreeSet::new();
    let all = keys
        .iter()
        .map(String::as_str)
        .chain(DRIVE_KEYS.iter().copied());
    for k in all {
        let Some(c) = obd::channel(k) else { continue };
        if !sup.is_none_or(|s| is_supported(c, s)) {
            continue;
        }
        if c.is_derived() {
            // Boost: MAP (0x0B, else 0x87); the core adds baro.
            let map = match sup {
                Some(s) if !s.contains(&0x0B) => 0x87,
                _ => 0x0B,
            };
            out.insert(map);
        } else {
            out.insert(c.pid);
        }
    }
    out.into_iter().collect()
}

/// Warn / crit state of a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Normal,
    Warn,
    Crit,
}

/// The channel's high-side level of `v` (at or above the threshold).
pub fn level(ch: Option<&Channel>, v: Option<f64>) -> Level {
    match (ch, v) {
        (Some(c), Some(v)) if c.crit.is_some_and(|x| v >= x) => Level::Crit,
        (Some(c), Some(v)) if c.warn.is_some_and(|x| v >= x) => Level::Warn,
        _ => Level::Normal,
    }
}

/// Pressure display unit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PressureUnit {
    #[default]
    Bar,
    KPa,
}

/// A value as shown: number, unit, decimals (kPa follows the pressure unit).
pub fn shown(unit: &str, v: Option<f64>, p: PressureUnit) -> (Option<f64>, String, usize) {
    if unit == "kPa" && p == PressureUnit::Bar {
        return (v.map(|v| v / 100.0), "bar".into(), 2);
    }
    let dp = match unit {
        "V" | "λ" | "bar" => 2,
        "g/s" | "%" => 1,
        _ => 0,
    };
    (v, unit.to_string(), dp)
}

/// `v` with `dp` decimals ("—" for none; never "-0").
pub fn fmt_num(v: Option<f64>, dp: usize) -> String {
    match v.filter(|v| v.is_finite()) {
        None => "\u{2014}".into(),
        Some(v) => {
            let s = format!("{v:.dp$}");
            if s.starts_with('-') && s[1..].chars().all(|c| c == '0' || c == '.') {
                s[1..].to_string()
            } else {
                s
            }
        }
    }
}

/// One poll as live-feed values: a multi-sensor channel becomes `key#i`
/// ("Name S<i+1>", answering sensors only); a channel without a value is
/// sent as `None`.
pub fn feed_values(ro: &Readout) -> Vec<FeedValue> {
    let mut out = Vec::new();
    for (key, v) in &ro.values {
        let (name, unit) = obd::channel(key).map_or((key.as_str(), ""), |c| (c.name, c.unit));
        match v {
            Some(ObdValue::Array(a)) => {
                for (i, x) in a.iter().enumerate() {
                    if let Some(x) = x {
                        out.push(FeedValue {
                            key: format!("{key}#{i}"),
                            name: format!("{name} S{}", i + 1),
                            unit: unit.into(),
                            value: Some(*x).filter(|x| x.is_finite()),
                        });
                    }
                }
            }
            Some(ObdValue::Scalar(x)) => out.push(FeedValue {
                key: key.clone(),
                name: name.into(),
                unit: unit.into(),
                value: Some(*x).filter(|x| x.is_finite()),
            }),
            None => out.push(FeedValue {
                key: key.clone(),
                name: name.into(),
                unit: unit.into(),
                value: None,
            }),
        }
    }
    out
}

/// The feed frame of one poll.
pub fn feed_frame(t_ms: u64, ro: &Readout) -> FeedEvent {
    FeedEvent::Frame {
        source: SOURCE.into(),
        t_ms,
        values: feed_values(ro),
    }
}

/// A failed poll for the feed (`busy`: another operation holds the adapter).
pub fn feed_error(message: &str) -> FeedEvent {
    FeedEvent::Error {
        source: SOURCE.into(),
        message: message.into(),
        busy: is_busy(message),
    }
}

pub fn is_busy(message: &str) -> bool {
    message.to_ascii_lowercase().contains("busy")
}

/// The Live-log CSV header columns ([`obd::LIVE_LOG_HEADER`]).
pub fn csv_header() -> Vec<&'static str> {
    obd::LIVE_LOG_HEADER.split(',').collect()
}

/// The CSV rows of one poll, as cells: the same rows as the core's
/// [`obd::live_log_lines`] (the desktop Dashboard recording), so the Log
/// viewer opens the file as an OpenTune Live log.
pub fn csv_rows(ts_ms: u64, ro: &Readout) -> Vec<[String; 8]> {
    let mut out = Vec::new();
    for (key, v) in &ro.values {
        let (name, unit) = obd::channel(key).map_or((key.as_str(), ""), |c| (c.name, c.unit));
        let err = ro.errors.get(key).map_or("no value", String::as_str);
        let row = |id: String, nm: String, x: Option<f64>| -> [String; 8] {
            let (value, note) = match x.filter(|x| x.is_finite()) {
                Some(x) => (x.to_string(), String::new()),
                None => (String::new(), err.to_string()),
            };
            [
                ts_ms.to_string(),
                id,
                nm,
                String::new(),
                value,
                unit.to_string(),
                note,
                String::new(),
            ]
        };
        match v {
            Some(ObdValue::Array(a)) => {
                for (i, x) in a.iter().enumerate() {
                    if x.is_some() {
                        out.push(row(format!("{key}#{i}"), format!("{name} S{}", i + 1), *x));
                    }
                }
            }
            Some(ObdValue::Scalar(x)) => out.push(row(key.clone(), name.into(), Some(*x))),
            None => out.push(row(key.clone(), name.into(), None)),
        }
    }
    out
}

/// What a tile shows of a channel: the latest value, min / max since Start
/// and the recent history.
#[derive(Clone, Debug, Default)]
pub struct Track {
    pub last: Option<f64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// `[seconds since Start, value]`, at most [`HIST_S`] back.
    pub hist: VecDeque<[f64; 2]>,
}

impl Track {
    pub fn push(&mut self, t: f64, v: Option<f64>) {
        self.last = v;
        if let Some(v) = v {
            self.min = Some(self.min.map_or(v, |m| m.min(v)));
            self.max = Some(self.max.map_or(v, |m| m.max(v)));
            self.hist.push_back([t, v]);
        }
        while self.hist.front().is_some_and(|p| p[0] < t - HIST_S) {
            self.hist.pop_front();
        }
    }
}

/// The scalar a tile / gauge shows for a value (first sensor of an array).
pub fn scalar(v: &Option<ObdValue>) -> Option<f64> {
    v.as_ref()
        .and_then(ObdValue::first)
        .filter(|x| x.is_finite())
}

/// Update `tracks` from one poll at `t` seconds since Start.
pub fn track_poll(tracks: &mut BTreeMap<String, Track>, t: f64, ro: &Readout) {
    for (k, v) in &ro.values {
        tracks.entry(k.clone()).or_default().push(t, scalar(v));
    }
}
