//! # Haldex panel (backend)
//!
//! A thin shell over [`opentune_haldex`] (docs/HALDEX.md, "Backend API for a
//! desktop panel"); nothing protocol-specific lives here.
//!
//! - **OpenHaldex** — live lock / slip over its supplier UDS DIDs on
//!   `0x70F`/`0x779` ([`OpenHaldexUds`], read-only, through the adapter hub),
//!   the mode list, and a mode change that refuses without an explicit
//!   `confirm` and returns the unit's **read-back** state. `sim: true` talks to
//!   an in-app [`SimOpenHaldex`] instead (kept in [`HaldexState`], so a mode
//!   change persists between polls).
//! - **Gen4 read** — [`read_gen4`] needs a VW TP2.0 channel, which no shipped
//!   driver opens yet; a real read is refused with that explanation (the same
//!   rule as `opentune haldex gen4`). `sim: true` runs the real `VwTp20` +
//!   KWP2000 stack against the simulated Gen4 unit.
//! - **CAN-log decode** — a candump / OpenTune CSV / SavvyCAN log becomes the
//!   crate's summary plus chart-ready series (lock request, lock actual, clutch
//!   value, Gen4 temperature, front/rear wheel-speed delta), and can be
//!   exported as a Live-log CSV for the Log viewer.
//!
//! Commands: `haldex_modes`, `haldex_status`, `haldex_set_mode`,
//! `haldex_gen4_read`, `haldex_decode_log`, `haldex_export_csv`.

use crate::adapter::{AdapterHub, Addr, ConnKey};
use opentune_core::protocol::vwtp20::VwTp20;
use opentune_core::RawDiag;
use opentune_haldex::canlog::{parse_log, summarize, LogFrame};
use opentune_haldex::gen4::{
    read_gen4, Gen4ReadOptions, DEFAULT_GROUPS, TP20_LOGICAL_ADDRESS, TP20_TESTER_RX_ID,
};
use opentune_haldex::openhaldex::{self, Mode, OpenHaldexUds};
use opentune_haldex::sim::{SimGen4, SimGen4Link, SimOpenHaldex};
use opentune_haldex::{Bus, HaldexFrame};
use serde_json::{json, Value};
use std::sync::Mutex;

/// The OpenHaldex's diagnostic address (the Haldex physical pair).
const OH_ADDR: Addr = Addr {
    tx_id: openhaldex::UDS_REQ_ID,
    rx_id: openhaldex::UDS_RESP_ID,
};

/// Most points per chart series (longer logs are decimated evenly).
pub const MAX_POINTS: usize = 1500;
/// Most decoded frames listed in the panel's table.
pub const MAX_ROWS: usize = 200;

/// Tauri-managed Haldex state: the simulated OpenHaldex used with `sim: true`.
#[derive(Default)]
pub struct HaldexState {
    sim: Mutex<SimOpenHaldex>,
}

impl HaldexState {
    fn sim(&self) -> std::sync::MutexGuard<'_, SimOpenHaldex> {
        self.sim.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn mode_json(m: Mode) -> Value {
    json!({
        "mode": m.as_str(),
        "index": m.index(),
        "lock_pct": m.nominal_lock_pct(),
        "can_command": m.can_command(),
        "label": match m {
            Mode::Stock => "Stock (OEM behaviour)",
            Mode::Fwd => "FWD (no lock)",
            Mode::Split5050 => "50:50 (100 % lock)",
            Mode::Split6040 => "60:40 (40 % lock)",
            Mode::Split7525 => "75:25 (25 % lock)",
            Mode::Expert => "Expert (unit's own map)",
        },
    })
}

/// The OpenHaldex modes in the unit's index order. Offline.
#[tauri::command]
pub fn haldex_modes() -> Vec<Value> {
    Mode::ALL.iter().copied().map(mode_json).collect()
}

/// Read lock status (`0xFDA0`) and wheel slip (`0xFDA1`). Read-only. A unit
/// that does not answer the slip DID still returns its lock status.
fn read_status(d: &mut dyn RawDiag) -> Result<Value, String> {
    let mut oh = OpenHaldexUds::new(d);
    let lock = oh
        .read_lock()
        .map_err(|e| format!("OpenHaldex lock status (22 FD A0): {e}"))?;
    let (slip, slip_error) = match oh.read_slip() {
        Ok(s) => (Some(s), None),
        Err(e) => (None, Some(e.to_string())),
    };
    Ok(json!({ "lock": lock, "slip": slip, "slip_error": slip_error }))
}

/// Change the mode (`2E FD A2 <mode>`) and return the read-back lock status.
fn set_mode_on(d: &mut dyn RawDiag, mode: Mode) -> Result<Value, String> {
    let st = OpenHaldexUds::new(d)
        .set_mode(mode)
        .map_err(|e| format!("set OpenHaldex mode {mode}: {e}"))?;
    Ok(json!({
        "requested": mode.as_str(),
        "lock": st,
        "mode": st.mode.map(Mode::as_str),
    }))
}

/// The exact request a mode change sends, for the confirm dialog.
pub fn mode_request_hex(mode: Mode) -> String {
    let [h, l] = openhaldex::DID_MODE.to_be_bytes();
    format!("2E {h:02X} {l:02X} {:02X}", mode.index())
}

fn oh_key(driver: &str, port: Option<&str>, baud: Option<&str>, bus: Option<&str>) -> ConnKey {
    ConnKey::new(driver, port, baud, bus, OH_ADDR)
}

/// Live OpenHaldex status: `{ lock, slip, slip_error, sim, target }`.
/// Read-only (RDBI only). Poll at most a few times a second.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn haldex_status(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    sim: bool,
    timeout_ms: u64,
    hub: tauri::State<'_, AdapterHub>,
    state: tauri::State<'_, HaldexState>,
) -> Result<Value, String> {
    let mut v = if sim {
        read_status(&mut *state.sim())?
    } else {
        let key = oh_key(&driver, port.as_deref(), baud.as_deref(), bus.as_deref());
        hub.run("Haldex status", move |s| {
            s.with_diag(&key, timeout_ms, |d| read_status(d))
        })?
    };
    v["sim"] = json!(sim);
    v["target"] = OH_ADDR.json(false);
    Ok(v)
}

/// Change the OpenHaldex mode. A **write**: refuses without `confirm`
/// (naming the request it would send), then returns the unit's read-back
/// state, and fails if the unit does not report the requested mode.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn haldex_set_mode(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    mode: String,
    confirm: bool,
    sim: bool,
    timeout_ms: u64,
    hub: tauri::State<'_, AdapterHub>,
    state: tauri::State<'_, HaldexState>,
) -> Result<Value, String> {
    let mode: Mode = mode.parse()?;
    if !confirm {
        return Err(format!(
            "refusing to change the AWD mode without confirmation. This would send {} to 0x{:03X} (OpenHaldex mode {mode}).",
            mode_request_hex(mode),
            OH_ADDR.tx_id
        ));
    }
    let mut v = if sim {
        set_mode_on(&mut *state.sim(), mode)?
    } else {
        let key = oh_key(&driver, port.as_deref(), baud.as_deref(), bus.as_deref());
        hub.run("Haldex mode change", move |s| {
            s.with_diag(&key, timeout_ms, |d| set_mode_on(d, mode))
        })?
    };
    v["sim"] = json!(sim);
    v["request"] = json!(mode_request_hex(mode));
    v["target"] = OH_ADDR.json(false);
    Ok(v)
}

/// Drivers that can open a VW TP2.0 channel (none yet; see docs/HALDEX.md).
const TP20_DRIVERS: &[&str] = &[];

/// Why a real Gen4 read is refused with `driver`.
pub fn gen4_refusal(driver: &str) -> String {
    format!(
        "driver '{driver}' cannot open a VW TP2.0 channel, which the Haldex Gen4 needs (KWP2000 \
         over TP2.0 at logical address 0x0A). No OpenTune driver does yet: the ELM327 drivers \
         refuse TP2.0 and socketcan / slcan / gs_usb only build ISO-TP. Use the simulated unit \
         to see the read, or capture the drivetrain CAN (CAN capture) and decode it below."
    )
}

/// The simulated Gen4 read: the real `VwTp20` channel and KWP2000 client
/// against [`SimGen4`] (synthetic data).
pub fn gen4_sim(opts: &Gen4ReadOptions) -> Value {
    let link = SimGen4Link::new(SimGen4::default());
    let mut tp = VwTp20::new(Box::new(link), TP20_LOGICAL_ADDRESS, TP20_TESTER_RX_ID);
    let r = read_gen4(&mut tp, opts);
    tp.disconnect();
    json!({ "report": r, "responded": r.responded(), "sim": true })
}

/// Read-only Gen4 controller read (identification, stored codes, measuring
/// groups). Refused on real hardware until a driver opens TP2.0.
#[tauri::command]
pub async fn haldex_gen4_read(
    driver: Option<String>,
    sim: bool,
    groups: Option<Vec<u8>>,
    read_dtcs: Option<bool>,
    timeout_ms: Option<u64>,
) -> Result<Value, String> {
    let mut opts = Gen4ReadOptions {
        groups: groups
            .filter(|g| !g.is_empty())
            .unwrap_or_else(|| DEFAULT_GROUPS.to_vec()),
        read_dtcs: read_dtcs.unwrap_or(true),
        ..Gen4ReadOptions::default()
    };
    if let Some(t) = timeout_ms {
        opts.timeout_ms = t;
    }
    if !sim {
        let d = driver.unwrap_or_default();
        if !TP20_DRIVERS.contains(&d.as_str()) {
            return Err(gen4_refusal(&d));
        }
    }
    tauri::async_runtime::spawn_blocking(move || gen4_sim(&opts))
        .await
        .map_err(|e| format!("Gen4 read task failed: {e}"))
}

/// One chart series.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Series {
    pub key: &'static str,
    pub name: &'static str,
    pub units: &'static str,
    /// Seconds from the first timestamped frame.
    pub t: Vec<f64>,
    pub v: Vec<f64>,
}

impl Series {
    fn new(key: &'static str, name: &'static str, units: &'static str) -> Self {
        Series {
            key,
            name,
            units,
            t: Vec::new(),
            v: Vec::new(),
        }
    }

    fn push(&mut self, t: f64, v: f64) {
        if v.is_finite() {
            self.t.push(t);
            self.v.push(v);
        }
    }

    /// Keep at most `max` evenly spaced points (always the first and last).
    fn decimate(&mut self, max: usize) {
        let n = self.v.len();
        if max < 2 || n <= max {
            return;
        }
        let idx: Vec<usize> = (0..max).map(|i| i * (n - 1) / (max - 1)).collect();
        self.t = idx.iter().map(|&i| self.t[i]).collect();
        self.v = idx.iter().map(|&i| self.v[i]).collect();
    }
}

/// Chart-ready series from decoded frames: lock request / actual (%), clutch
/// value in the DBC scale, Gen4 temperature and the front-rear wheel-speed
/// delta. Empty series are dropped.
pub fn build_series(decoded: &[(LogFrame, HaldexFrame)], max_points: usize) -> Vec<Series> {
    let mut req = Series::new("lock_request", "Lock request", "%");
    let mut act = Series::new("lock_actual", "Lock actual", "%");
    let mut clutch = Series::new("clutch_actual", "Clutch actual (DBC scale)", "Nm/min");
    let mut temp = Series::new("gen4_temp", "Gen4 temperature", "°C");
    let mut delta = Series::new("wheel_delta", "Front - rear wheel speed", "km/h");
    let t0 = decoded.iter().find_map(|(f, _)| f.ts);
    let mut last_t = 0.0;
    for (f, d) in decoded {
        let t = match (f.ts, t0) {
            (Some(ts), Some(t0)) => ts - t0,
            _ => last_t,
        };
        last_t = t;
        match d {
            HaldexFrame::LockRequestPq { lock_pct, .. } => req.push(t, f64::from(*lock_pct)),
            HaldexFrame::StatusPq {
                engagement_pct,
                actual_nm_per_min,
                gen4_temp_c,
                ..
            } => {
                act.push(t, f64::from(*engagement_pct));
                clutch.push(t, f64::from(*actual_nm_per_min));
                if let Some(c) = gen4_temp_c {
                    temp.push(t, f64::from(*c));
                }
            }
            HaldexFrame::StatusMqb {
                engagement_pct: Some(p),
                ..
            } => act.push(t, f64::from(*p)),
            HaldexFrame::WheelSpeeds(w) => delta.push(t, f64::from(w.front_rear_delta())),
            _ => {}
        }
    }
    let mut out: Vec<Series> = [req, act, clutch, temp, delta]
        .into_iter()
        .filter(|s| !s.v.is_empty())
        .collect();
    for s in &mut out {
        s.decimate(max_points);
    }
    out
}

/// Decode a log text: `{ summary, series, rows, bad_lines, bus }`.
pub fn decode_text(text: &str, bus: Bus, max_points: usize) -> Value {
    let log = parse_log(text);
    let (summary, decoded) = summarize(&log, bus);
    let t0 = decoded.iter().find_map(|(f, _)| f.ts);
    let rows: Vec<Value> = decoded
        .iter()
        .take(MAX_ROWS)
        .map(|(f, d)| {
            json!({
                "t": f.ts.zip(t0).map(|(a, b)| a - b),
                "id": format!("{:03X}", f.id),
                "decoded": d,
            })
        })
        .collect();
    json!({
        "summary": summary,
        "series": build_series(&decoded, max_points),
        "rows": rows,
        "rows_total": decoded.len(),
        "bad_lines": log.bad_lines,
        "bus": bus,
    })
}

fn parse_bus(bus: Option<&str>) -> Result<Bus, String> {
    match bus.map(str::trim).filter(|b| !b.is_empty()) {
        Some(b) => b.parse(),
        None => Ok(Bus::Auto),
    }
}

fn read_log(path: &str) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read '{path}': {e}"))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Decode a candump / OpenTune CSV / SavvyCAN CSV capture offline.
#[tauri::command]
pub async fn haldex_decode_log(path: String, bus: Option<String>) -> Result<Value, String> {
    let bus = parse_bus(bus.as_deref())?;
    let text = read_log(&path)?;
    let mut v = tauri::async_runtime::spawn_blocking(move || decode_text(&text, bus, MAX_POINTS))
        .await
        .map_err(|e| format!("decode task failed: {e}"))?;
    v["path"] = json!(path);
    Ok(v)
}

/// The series as a Live-log CSV (`ts,param_id,name,raw,value,units,note`,
/// `ts` in ms from the start of the capture) that the Log viewer opens.
pub fn series_csv(series: &[Series]) -> String {
    let mut rows: Vec<(u64, usize, f64)> = Vec::new();
    for (si, s) in series.iter().enumerate() {
        for (t, v) in s.t.iter().zip(&s.v) {
            rows.push(((t * 1000.0).round().max(0.0) as u64, si, *v));
        }
    }
    rows.sort_by_key(|r| (r.0, r.1));
    let mut out = String::from("ts,param_id,name,raw,value,units,note\n");
    for (ts, si, v) in rows {
        let s = &series[si];
        out.push_str(&format!("{ts},{},{},,{v},{},\n", s.key, s.name, s.units));
    }
    out
}

/// Write the decoded series of `path` as a Live-log CSV to `out` (full
/// resolution). Returns `{ out, rows }`.
#[tauri::command]
pub async fn haldex_export_csv(
    path: String,
    bus: Option<String>,
    out: String,
) -> Result<Value, String> {
    let bus = parse_bus(bus.as_deref())?;
    let text = read_log(&path)?;
    tauri::async_runtime::spawn_blocking(move || {
        let log = parse_log(&text);
        let (_, decoded) = summarize(&log, bus);
        let csv = series_csv(&build_series(&decoded, usize::MAX));
        let rows = csv.lines().count().saturating_sub(1);
        std::fs::write(&out, csv).map_err(|e| crate::common::write_error(&out, &e))?;
        Ok(json!({ "out": out, "rows": rows }))
    })
    .await
    .map_err(|e| format!("export task failed: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::testkit::SimBus;
    use opentune_udssim::server::UdsServer;

    const LOG: &str = "\
(1.0) can0 2A0#7F0000
(1.1) can0 2A0#FE0000
(1.2) can0 2C0#1880000054C0FE73
(1.3) can0 2C0#01BF0080560000F3
(1.4) can0 4A0#88130000A00F0000
(1.5) can0 6B0#0000C0280000020A
(1.6) can0 7E8#0102
";

    #[test]
    fn modes_are_listed_in_index_order() {
        let m = haldex_modes();
        assert_eq!(m.len(), 6);
        assert_eq!(m[0]["mode"], "stock");
        assert_eq!(m[2]["mode"], "5050");
        assert_eq!(m[2]["lock_pct"], 100);
        assert!(m[0]["lock_pct"].is_null());
        assert_eq!(mode_request_hex(Mode::Split6040), "2E FD A2 03");
    }

    #[test]
    fn simulated_openhaldex_status_and_confirmed_mode_change() {
        let state = HaldexState::default();
        let st = read_status(&mut *state.sim()).unwrap();
        assert_eq!(st["lock"]["mode"], "stock");
        assert_eq!(st["slip"]["fl"], 0);
        assert!(st["slip"]["rl"].is_null(), "-128 = no data");
        let r = set_mode_on(&mut *state.sim(), Mode::Split6040).unwrap();
        assert_eq!(r["mode"], "6040", "read-back, not the request");
        assert_eq!(r["lock"]["commanded_pct"], 40);
        // The mode persists for the next poll.
        let st = read_status(&mut *state.sim()).unwrap();
        assert_eq!(st["lock"]["mode"], "6040");
        // A disabled controller ignores the change: the read-back check fails.
        state.sim().controller_enabled = false;
        let e = set_mode_on(&mut *state.sim(), Mode::Fwd).unwrap_err();
        assert!(e.contains("fwd"), "{e}");
        assert_eq!(state.sim().mode, Mode::Split6040);
    }

    #[test]
    fn status_goes_through_the_hub_on_the_openhaldex_address() {
        // An OpenHaldex answering on 0x70F/0x779 (udssim with the two DIDs).
        let hub = AdapterHub::spawn(|| {
            let bus = SimBus::default();
            let mut s = UdsServer::new();
            s.set_did(openhaldex::DID_LOCK, vec![25, 0xA0, 4]);
            s.set_did(openhaldex::DID_SLIP, vec![2, 3, 0x80, 0x80]);
            bus.add(0x70F, 0x779, s);
            bus.opener()
        });
        let key = oh_key("sim", None, None, Some("hs"));
        let v = hub
            .run("Haldex status", move |s| {
                s.with_diag(&key, 50, |d| read_status(d))
            })
            .unwrap();
        assert_eq!(v["lock"]["commanded_pct"], 25);
        assert_eq!(v["lock"]["mode"], "7525");
        assert_eq!(v["slip"]["fr"], 3);
        // Nothing answers on the engine address: the status read is tied to 0x70F.
        let eng = ConnKey::new("sim", None, None, Some("hs"), Addr::ENGINE);
        assert!(hub
            .run("x", move |s| s.with_diag(&eng, 50, |d| read_status(d)))
            .is_err());
    }

    #[test]
    fn gen4_real_read_is_refused_and_sim_read_works() {
        let e = gen4_refusal("elm327obd");
        assert!(e.contains("TP2.0") && e.contains("elm327obd"), "{e}");
        let v = gen4_sim(&Gen4ReadOptions::default());
        assert_eq!(v["responded"], true);
        assert!(v["report"]["identity"]["part_number"]
            .as_str()
            .unwrap()
            .starts_with("0AY"));
        assert!(!v["report"]["groups"].as_array().unwrap().is_empty());
    }

    #[test]
    fn log_decode_gives_summary_series_and_rows() {
        let v = decode_text(LOG, Bus::Auto, MAX_POINTS);
        assert_eq!(v["summary"]["frames"], 7);
        assert_eq!(v["rows_total"], 6);
        assert_eq!(v["rows"][0]["id"], "2A0");
        let series = v["series"].as_array().unwrap();
        let get = |k: &str| series.iter().find(|s| s["key"] == k).cloned();
        let req = get("lock_request").unwrap();
        assert_eq!(req["v"], json!([0.0, 100.0]));
        assert!((req["t"][1].as_f64().unwrap() - 0.1).abs() < 1e-9);
        let temp = get("gen4_temp").unwrap();
        assert_eq!(temp["v"], json!([24.0, 26.0]));
        assert!(get("lock_actual").is_some() && get("clutch_actual").is_some());
        assert!(get("wheel_delta").is_some());
    }

    #[test]
    fn series_decimate_and_export_as_live_log_csv() {
        let mut s = Series::new("x", "X", "u");
        for i in 0..1000 {
            s.push(i as f64 * 0.01, i as f64);
        }
        s.push(10.0, f64::NAN);
        assert_eq!(s.v.len(), 1000, "non-finite values are skipped");
        s.decimate(11);
        assert_eq!(s.v.len(), 11);
        assert_eq!((s.v[0], s.v[10]), (0.0, 999.0));
        let log = parse_log(LOG);
        let (_, decoded) = summarize(&log, Bus::Auto);
        let csv = series_csv(&build_series(&decoded, usize::MAX));
        let mut lines = csv.lines();
        assert_eq!(lines.next(), Some("ts,param_id,name,raw,value,units,note"));
        assert_eq!(lines.next(), Some("0,lock_request,Lock request,,0,%,"));
        assert!(csv.contains("200,gen4_temp,Gen4 temperature,,24,°C,"));
    }
}
