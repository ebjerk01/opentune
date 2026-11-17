//! # Live OBD-II dashboard (backend)
//!
//! Three Tauri commands over [`opentune_core::obd`], the data-driven SAE J1979
//! mode-01 table + polling engine:
//!
//! - [`obd_pid_table`]: the channel table (offline, no adapter).
//! - [`obd_supported`]: walk the `0100`/`0120`/… support bitmaps + read the VIN.
//! - [`obd_read`]: one poll of the requested PIDs (multi-PID requests, up to six
//!   per request, with automatic fall-back to single PIDs).
//!
//! Like the live-log panel this is stateless per call (open adapter, read, drop),
//! but the learned [`ObdSession`] (supported PIDs, whether the ECU accepts
//! multi-PID requests, PIDs it never answers) is cached per
//! `driver|port|baud|bus` so each poll tick does not re-learn ECU quirks.
//! [`obd_supported`] resets that cache entry.
//!
//! The pseudo driver `obdsim` serves a simulated turbo-diesel
//! ([`opentune_core::obd::sim::SimEcu::demo`]) for UI work without hardware.
//!
//! All adapter commands are `async` so serial I/O never runs on the UI thread.

use opentune_core::obd::{self, sim::SimEcu, ObdSession};
use opentune_core::RawDiag;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Per-request timeout for mode-01 exchanges, milliseconds.
const TIMEOUT_MS: u64 = 1000;
/// Pseudo driver id for the built-in simulated ECU.
const SIM_DRIVER: &str = "obdsim";

fn sessions() -> &'static Mutex<HashMap<String, ObdSession>> {
    static S: OnceLock<Mutex<HashMap<String, ObdSession>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

fn session_key(driver: &str, port: Option<&str>, baud: Option<&str>, bus: Option<&str>) -> String {
    format!(
        "{driver}|{}|{}|{}",
        port.unwrap_or("").trim(),
        baud.unwrap_or("").trim(),
        bus.unwrap_or("").trim()
    )
}

fn load_session(key: &str) -> ObdSession {
    sessions()
        .lock()
        .ok()
        .and_then(|m| m.get(key).cloned())
        .unwrap_or_default()
}

fn store_session(key: String, s: ObdSession) {
    if let Ok(mut m) = sessions().lock() {
        m.insert(key, s);
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Run `f` against the chosen adapter's diagnostic seam, also returning the
/// adapter battery voltage (read after `f`; `None` if unsupported).
fn with_diag<T>(
    driver: &str,
    port: Option<&str>,
    baud: Option<&str>,
    bus: Option<&str>,
    f: impl FnOnce(&mut dyn RawDiag) -> T,
) -> Result<(T, Option<u32>), String> {
    if driver == SIM_DRIVER {
        let mut ecu = SimEcu::demo(now_ms() / 100);
        let out = f(&mut ecu);
        return Ok((out, Some(14_100)));
    }
    let mut drv = crate::debug_open_diag(driver, port, baud, bus)?;
    let out = {
        let t = drv
            .diag()
            .ok_or_else(|| "adapter has no diagnostic transport".to_string())?;
        let mut diag = opentune_core::CoreDiag::new(t, TIMEOUT_MS);
        f(&mut diag)
    };
    let battery = drv.battery_millivolts().ok().flatten();
    Ok((out, battery))
}

/// The OBD channel table (offline). One row per dashboard channel; a PID with
/// several fields (e.g. `0x70`, `0x7C`) has several rows sharing `pid`. Derived
/// rows (`boost_kpa`, `boost_bar`) carry `derived: true` and `pid` 0x0B: they
/// are computed by [`obd_read`] whenever `0x0B` or `0x87` is requested and the
/// ECU reports baro (`0x33`, fetched automatically).
#[tauri::command]
pub fn obd_pid_table() -> Vec<Value> {
    pid_table_rows()
}

fn pid_table_rows() -> Vec<Value> {
    obd::channels()
        .iter()
        .map(|c| {
            json!({
                "pid": c.pid,
                "key": c.key,
                "name": c.name,
                "unit": c.unit,
                "min": c.min,
                "max": c.max,
                "warn": c.warn,
                "crit": c.crit,
                "group": c.group.as_str(),
                "derived": c.is_derived(),
            })
        })
        .collect()
}

/// Discover the ECU's supported mode-01 PIDs (range markers excluded) and read
/// the VIN (mode 09 PID 02, UDS `22 F190` fallback). Resets the cached session
/// for this adapter. Returns `{pids: [u8], vin: string|null}`.
#[tauri::command]
pub async fn obd_supported(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
) -> Result<Value, String> {
    let (port, baud, bus) = (port.as_deref(), baud.as_deref(), bus.as_deref());
    let key = session_key(&driver, port, baud, bus);
    let mut session = ObdSession::new();
    let ((pids, vin), _) = with_diag(&driver, port, baud, bus, |d| {
        let pids = session.discover(d);
        let vin = obd::read_vin(d);
        (pids, vin)
    })?;
    let pids = pids?;
    store_session(key, session);
    Ok(json!({
        "pids": pids.into_iter().collect::<Vec<u8>>(),
        "vin": vin,
    }))
}

/// One poll of `pids`. Returns `{t_ms, battery_mv, values, errors}`: `values`
/// holds every channel key of the requested PIDs (number, positional array for
/// multi-sensor PIDs like EGT, or `null`) plus derived `boost_kpa`/`boost_bar`
/// when computable; `errors` explains each `null` (unknown PIDs as `pid_XX`).
#[tauri::command]
pub async fn obd_read(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    pids: Vec<u8>,
) -> Result<Value, String> {
    let (port, baud, bus) = (port.as_deref(), baud.as_deref(), bus.as_deref());
    let key = session_key(&driver, port, baud, bus);
    let mut session = load_session(&key);
    let (readout, battery_mv) = with_diag(&driver, port, baud, bus, |d| session.read(d, &pids))?;
    store_session(key, session);
    Ok(json!({
        "t_ms": now_ms(),
        "battery_mv": battery_mv,
        "values": readout.values,
        "errors": readout.errors,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tauri::async_runtime::block_on(f)
    }

    #[test]
    fn table_rows_follow_contract() {
        let rows = pid_table_rows();
        assert!(rows.len() >= 40);
        let groups = ["engine", "air", "fuel", "exhaust", "electrical", "vehicle"];
        for r in &rows {
            assert!(r["pid"].is_u64());
            assert!(r["key"].is_string() && r["name"].is_string() && r["unit"].is_string());
            assert!(r["min"].is_number() && r["max"].is_number());
            assert!(r["warn"].is_number() || r["warn"].is_null());
            assert!(r["crit"].is_number() || r["crit"].is_null());
            assert!(groups.contains(&r["group"].as_str().unwrap()));
        }
        let keys: Vec<&str> = rows.iter().map(|r| r["key"].as_str().unwrap()).collect();
        for k in [
            "engine_load",
            "coolant_temp",
            "intake_air_temp",
            "throttle_pos",
            "commanded_egr",
            "baro_pressure",
            "commanded_lambda",
            "commanded_throttle",
            "boost_control",
            "exhaust_pressure",
            "egt_b1",
            "egt_b2",
            "dpf_dp_b1",
            "dpf_temp",
            "intake_map_abs",
            "boost_kpa",
        ] {
            assert!(keys.contains(&k), "missing key {k}");
        }
    }

    #[test]
    fn sim_supported_and_read() {
        let s = block_on(obd_supported("obdsim".into(), None, None, None)).unwrap();
        assert_eq!(s["vin"], "WVWZZZ3CZ9E000000");
        let pids: Vec<u64> = s["pids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap())
            .collect();
        assert!(pids.contains(&0x0C) && !pids.contains(&0x79));

        let r = block_on(obd_read(
            "obdsim".into(),
            None,
            None,
            None,
            vec![0x0C, 0x05, 0x0B, 0x78, 0x79, 0x02],
        ))
        .unwrap();
        assert!(r["t_ms"].as_u64().unwrap() > 0);
        assert_eq!(r["battery_mv"], 14_100);
        assert!(r["values"]["rpm"].is_number());
        assert!(r["values"]["boost_kpa"].is_number());
        assert!(r["values"]["egt_b1"].is_array());
        assert!(r["values"]["egt_b2"].is_null());
        assert!(r["errors"]["egt_b2"].is_string());
        assert!(r["errors"]["pid_02"].is_string());
    }

    #[test]
    fn unknown_driver_is_an_error() {
        assert!(block_on(obd_read("nope".into(), None, None, None, vec![0x0C])).is_err());
    }
}
