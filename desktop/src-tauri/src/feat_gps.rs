//! # Optional GPS source (backend for the Live log / Dashboard GPS card)
//!
//! One app-wide GPS source ([`opentune_gps`]): a u-blox style receiver on a
//! serial / USB CDC-ACM port read by a background thread, or a recorded NMEA
//! file replayed (looping, one epoch per [`gps_fix`] call) for demos and
//! offline checks. It never touches the adapter hub: the receiver is its own
//! port.
//!
//! - [`gps_ports`]: serial ports, likely GNSS receivers first.
//! - [`gps_open`]: open a port or a file (replaces any open source).
//! - [`gps_fix`]: the latest snapshot plus the `gps_*` samples (and
//!   `speed_error_kmh` when the panel passes a vehicle speed), the same
//!   channels `opentune log --gps` writes.
//! - [`gps_close`]: stop the source.
//!
//! Bench tier: the panels that host the card are Bench until vehicle-tested.

use opentune_gps::{GpsFeed, GpsReader, GpsSnapshot, ReplayFeed};
use opentune_logdef::gps::{gps_samples, speed_error_sample, SPEED_ERROR_MIN_KMH};
use opentune_logdef::Sample;
use serde_json::{json, Value};
use std::sync::{Mutex, MutexGuard, OnceLock};

struct Active {
    feed: Box<dyn GpsFeed>,
    source: String,
    kind: &'static str,
}

fn slot() -> MutexGuard<'static, Option<Active>> {
    static S: OnceLock<Mutex<Option<Active>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Serial ports that could be a GNSS receiver (u-blox VID / GPS in the
/// product string first).
#[tauri::command]
pub fn gps_ports() -> Value {
    json!(opentune_gps::list_ports())
}

/// Open `source`: an existing file is replayed (looping), anything else is
/// opened as a serial port at `baud` (default 9600; CDC-ACM ignores it).
#[tauri::command]
pub async fn gps_open(source: String, baud: Option<u32>) -> Result<Value, String> {
    let spec = source.trim().to_string();
    if spec.is_empty() {
        return Err("choose a GPS port or an NMEA file".into());
    }
    // Close the old source first: it may hold the same port.
    *slot() = None;
    let path = std::path::Path::new(&spec);
    let (feed, kind): (Box<dyn GpsFeed>, &'static str) = if path.is_file() {
        let r = ReplayFeed::open(path)
            .map_err(|e| format!("read NMEA file '{spec}': {e}"))?
            .looping(true);
        (Box::new(r), "replay")
    } else {
        let r = GpsReader::open_serial(&spec, baud.unwrap_or(opentune_gps::DEFAULT_BAUD))?;
        (Box::new(r), "serial")
    };
    let describe = feed.describe();
    *slot() = Some(Active {
        feed,
        source: spec.clone(),
        kind,
    });
    Ok(json!({ "open": true, "source": spec, "kind": kind, "describe": describe }))
}

/// The latest GPS state. `vehicle_kmh` (the panel's own vehicle-speed
/// reading, km/h) adds the derived `speed_error_kmh` sample.
#[tauri::command]
pub fn gps_fix(vehicle_kmh: Option<f64>) -> Value {
    let mut g = slot();
    let Some(a) = g.as_mut() else {
        return json!({ "open": false });
    };
    let snap = a.feed.snapshot();
    fix_json(&a.source, a.kind, &snap, vehicle_kmh)
}

/// Stop the GPS source (no-op when none is open).
#[tauri::command]
pub fn gps_close() -> Value {
    let was = slot().take().is_some();
    json!({ "open": false, "closed": was })
}

fn sample_json(s: &Sample) -> Value {
    json!({
        "paramId": s.param_id,
        "name": s.name,
        "raw": s.raw,
        "value": s.value,
        "units": s.units,
        "note": s.note,
        "text": s.text,
    })
}

fn fix_json(source: &str, kind: &str, snap: &GpsSnapshot, vehicle_kmh: Option<f64>) -> Value {
    let mut samples = gps_samples(snap);
    if let Some(v) = vehicle_kmh {
        let veh = Sample {
            param_id: "vehicle_speed".into(),
            name: "Vehicle speed".into(),
            raw: None,
            value: Some(v),
            units: "km/h".into(),
            note: String::new(),
            text: None,
        };
        samples.push(speed_error_sample(Some(&veh), true, snap));
    }
    let f = &snap.fix;
    let utc = match (f.date, f.time) {
        (Some(d), Some(t)) => Some(format!("{d} {t}")),
        (None, Some(t)) => Some(t.to_string()),
        _ => None,
    };
    json!({
        "open": true,
        "source": source,
        "kind": kind,
        "good": snap.good,
        "stale": snap.stale,
        "note": snap.note,
        "ageMs": snap.age_ms,
        "silentMs": snap.silent_ms,
        "ended": snap.ended,
        "fix": {
            "lat": f.lat,
            "lon": f.lon,
            "altM": f.alt_m,
            "speedKmh": f.speed_kmh,
            "courseDeg": f.course_deg,
            "sats": f.sats,
            "fix": f.fix_code(),
            "hdop": f.hdop,
            "utc": utc,
            "talker": f.talker,
        },
        "stats": snap.stats,
        "speedErrorMinKmh": SPEED_ERROR_MIN_KMH,
        "samples": samples.iter().map(sample_json).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates/opentune-gps/tests/fixtures")
            .join(name)
            .to_string_lossy()
            .into_owned()
    }

    fn block<F: std::future::Future>(f: F) -> F::Output {
        tauri::async_runtime::block_on(f)
    }

    #[test]
    fn replay_source_end_to_end() {
        assert_eq!(gps_fix(None)["open"], false);
        assert!(block(gps_open("  ".into(), None)).is_err());
        let r = block(gps_open(fixture("route_gn.nmea"), None)).unwrap();
        assert_eq!(r["kind"], "replay");
        let mut last = Value::Null;
        for _ in 0..4 {
            last = gps_fix(Some(50.0));
        }
        assert_eq!(last["good"], true, "{last}");
        assert_eq!(last["fix"]["fix"], 1);
        assert_eq!(last["fix"]["utc"], "2026-06-15 10:00:03.000");
        let samples = last["samples"].as_array().unwrap();
        assert_eq!(samples.len(), 9);
        assert_eq!(samples[0]["paramId"], "gps_lat");
        // Epoch 3: GPS 18 km/h, vehicle 50 km/h.
        let err = samples[8]["value"].as_f64().unwrap();
        assert!((err - 32.0).abs() < 1e-6, "{err}");
        assert_eq!(gps_fix(None)["samples"].as_array().unwrap().len(), 8);
        // A file that is not NMEA still opens and just never gets a fix.
        let r = block(gps_open(fixture("../sources.rs"), None)).unwrap();
        assert_eq!(r["kind"], "replay");
        let f = gps_fix(None);
        assert_eq!(f["good"], false);
        assert!(f["stats"]["noise"].as_u64().unwrap() > 0, "{f}");
        assert_eq!(gps_close()["closed"], true);
        assert_eq!(gps_close()["closed"], false);
        assert_eq!(gps_fix(None)["open"], false);
    }
}
