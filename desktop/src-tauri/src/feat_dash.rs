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
//! All adapter I/O goes through the app-wide [`AdapterHub`] (op name
//! `"Dashboard"`): the opened adapter is reused across poll ticks, and a tick is
//! rejected with "adapter busy" (passed through unchanged) while another panel
//! uses the adapter. Reads are addressed to the UI's `tx_id`/`rx_id`, else the
//! engine ECU (7E0/7E8).
//!
//! The learned [`ObdSession`] (supported PIDs, whether the ECU accepts
//! multi-PID requests, PIDs it never answers) is cached per connection key so a
//! tick does not re-learn ECU quirks. [`obd_supported`] replaces that entry; a
//! tick where nothing answered drops both it and the opened adapter.
//!
//! The pseudo driver `obdsim` serves a simulated turbo-diesel
//! ([`SimEcu::demo`]) for UI work without hardware; it never touches the hub.

use crate::adapter::{self, AdapterHub, ConnKey, PollLink, Session};
use opentune_core::obd::{self, sim::SimEcu, ObdSession, Readout};
use opentune_core::resilient::Liveness;
use opentune_core::Interface;
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};
use std::sync::{Mutex, OnceLock};

/// Per-request timeout for mode-01 exchanges, milliseconds.
const TIMEOUT_MS: u64 = 1000;
/// Pseudo driver id for the built-in simulated ECU.
const SIM_DRIVER: &str = "obdsim";
/// Hub operation name (shown in "adapter busy" errors).
const OP: &str = "Dashboard";

fn sessions() -> &'static Mutex<HashMap<String, ObdSession>> {
    static S: OnceLock<Mutex<HashMap<String, ObdSession>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
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

/// Connection args as the UI sends them (`window.OT.connArgs()`).
#[derive(Debug, Clone, Default)]
struct Conn {
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    tx_id: Option<String>,
    rx_id: Option<String>,
}

impl Conn {
    fn is_sim(&self) -> bool {
        self.driver.trim() == SIM_DRIVER
    }

    /// Hub key: UI-entered module ids, else the engine ECU (read-only).
    fn key(&self) -> Result<ConnKey, String> {
        let (addr, _assumed) = adapter::read_addr(adapter::resolve(
            self.tx_id.as_deref(),
            self.rx_id.as_deref(),
            None,
        )?);
        Ok(ConnKey::new(
            &self.driver,
            self.port.as_deref(),
            self.baud.as_deref(),
            self.bus.as_deref(),
            addr,
        ))
    }

    /// [`ObdSession`] cache key.
    fn cache_key(&self) -> Result<String, String> {
        if self.is_sim() {
            return Ok(SIM_DRIVER.into());
        }
        Ok(format!("{:?}", self.key()?))
    }
}

/// Battery voltage of the opened adapter, if it can measure it.
fn battery(s: &mut Session, key: &ConnKey) -> Option<u32> {
    s.interface(key)
        .ok()
        .and_then(|i: &mut dyn Interface| i.battery_millivolts().ok().flatten())
}

/// Discovery on the hub worker: `(session, pids, vin)`.
fn supported_on(
    s: &mut Session,
    key: &ConnKey,
) -> Result<(ObdSession, BTreeSet<u8>, Option<String>), String> {
    let mut sess = ObdSession::new();
    let (pids, vin) = s.with_diag(key, TIMEOUT_MS, |d| {
        let pids = sess.discover(d)?;
        Ok((pids, obd::read_vin(d)))
    })?;
    Ok((sess, pids, vin))
}

/// One poll on the hub worker: `(session, readout, battery_mv, link)`. When
/// nothing answered at the transport (and the adapter reports no battery
/// voltage either), or the adapter reset, drops the opened adapter (next tick
/// re-opens it) and returns a fresh session so an unplugged adapter cannot
/// poison the cache. A poll the ECU only refused (negative responses) is
/// alive: the adapter and the learned session are kept.
fn read_on(
    s: &mut Session,
    key: &ConnKey,
    mut sess: ObdSession,
    pids: &[u8],
) -> Result<(ObdSession, Readout, Option<u32>, PollLink), String> {
    let (ro, link) = s.with_diag(key, TIMEOUT_MS, |d| {
        let mut lv = Liveness::new(d);
        let ro = sess.read(&mut lv, pids);
        Ok((ro, PollLink::of(&lv)))
    })?;
    let battery_mv = battery(s, key);
    let silent = link.state == "silent" && battery_mv.is_none();
    if !pids.is_empty() && (link.state == "lost" || silent) {
        s.invalidate();
        sess = ObdSession::new();
    }
    Ok((sess, ro, battery_mv, link))
}

fn supported_json(pids: BTreeSet<u8>, vin: Option<String>) -> Value {
    json!({
        "pids": pids.into_iter().collect::<Vec<u8>>(),
        "vin": vin,
    })
}

/// `{ t_ms, battery_mv, values, errors, link }`. A lost link (adapter reset)
/// is also noted in `errors.link`.
fn read_json(ro: Readout, battery_mv: Option<u32>, link: PollLink) -> Value {
    let mut errors = ro.errors;
    if link.state == "lost" {
        let why = link.error.clone().unwrap_or_else(|| "link lost".into());
        errors.insert("link".into(), format!("{why} — re-opening the adapter"));
    }
    json!({
        "t_ms": now_ms(),
        "battery_mv": battery_mv,
        "values": ro.values,
        "errors": errors,
        "link": link,
    })
}

fn supported_impl(hub: &AdapterHub, conn: Conn) -> Result<Value, String> {
    let cache = conn.cache_key()?;
    if conn.is_sim() {
        let mut ecu = SimEcu::demo(now_ms() / 100);
        let mut sess = ObdSession::new();
        let pids = sess.discover(&mut ecu)?;
        let vin = obd::read_vin(&mut ecu);
        store_session(cache, sess);
        return Ok(supported_json(pids, vin));
    }
    let key = conn.key()?;
    let (sess, pids, vin) = hub.run(OP, move |s| supported_on(s, &key))?;
    store_session(cache, sess);
    Ok(supported_json(pids, vin))
}

fn read_impl(hub: &AdapterHub, conn: Conn, pids: Vec<u8>) -> Result<Value, String> {
    let cache = conn.cache_key()?;
    let sess = load_session(&cache);
    if conn.is_sim() {
        let mut ecu = SimEcu::demo(now_ms() / 100);
        let mut sess = sess;
        let ro = sess.read(&mut ecu, &pids);
        store_session(cache, sess);
        let up = PollLink {
            state: "up",
            error: None,
        };
        return Ok(read_json(ro, Some(14_100), up));
    }
    let key = conn.key()?;
    let (sess, ro, battery_mv, link) = hub.run(OP, move |s| read_on(s, &key, sess, &pids))?;
    store_session(cache, sess);
    Ok(read_json(ro, battery_mv, link))
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
/// for this connection. Returns `{pids: [u8], vin: string|null}`.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn obd_supported(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    tx_id: Option<String>,
    rx_id: Option<String>,
    hub: tauri::State<'_, AdapterHub>,
) -> Result<Value, String> {
    let conn = Conn {
        driver,
        port,
        baud,
        bus,
        tx_id,
        rx_id,
    };
    supported_impl(&hub, conn)
}

/// One poll of `pids`. Returns `{t_ms, battery_mv, values, errors}`: `values`
/// holds every channel key of the requested PIDs (number, positional array for
/// multi-sensor PIDs like EGT, or `null`) plus derived `boost_kpa`/`boost_bar`
/// when computable; `errors` explains each `null` (unknown PIDs as `pid_XX`).
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn obd_read(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    tx_id: Option<String>,
    rx_id: Option<String>,
    pids: Vec<u8>,
    hub: tauri::State<'_, AdapterHub>,
) -> Result<Value, String> {
    let conn = Conn {
        driver,
        port,
        baud,
        bus,
        tx_id,
        rx_id,
    };
    read_impl(&hub, conn, pids)
}

/// Header every Dashboard recording starts with: the Live-log CSV layout the
/// Log viewer reads (`opentune_logview`, "OpenTune Live log").
pub const RECORD_HEADER: &str = "ts,param_id,name,raw,value,units,note,text";

fn record_write(path: &str, csv: &str, append: bool) -> Result<Value, String> {
    use std::io::Write;
    if !path.to_ascii_lowercase().ends_with(".csv") {
        return Err(format!("a recording is a .csv file, not {path}"));
    }
    if !append && !csv.starts_with(RECORD_HEADER) {
        return Err("a new recording must start with the Live-log CSV header".into());
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(path)
        .map_err(|e| crate::common::write_error(path, &e))?;
    f.write_all(csv.as_bytes())
        .map_err(|e| crate::common::write_error(path, &e))?;
    let bytes = f.metadata().map(|m| m.len()).unwrap_or(0);
    Ok(json!({ "path": path, "bytes": bytes }))
}

/// Dashboard **Record**: create (`append: false`, `csv` starting with
/// [`RECORD_HEADER`]) or extend (`append: true`) a Live-log CSV at `path`.
/// The panel batches the polled rows and flushes them about once a second.
/// Returns `{ path, bytes }` (the file size after the write).
#[tauri::command]
pub async fn dash_record_write(path: String, csv: String, append: bool) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || record_write(&path, &csv, append))
        .await
        .map_err(|e| format!("record task failed: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_writes_a_live_log_the_log_viewer_opens() {
        let dir = std::env::temp_dir().join(format!("ot-dash-rec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("rec.csv").to_string_lossy().into_owned();
        // The panel's flushes: header first, then batches of rows.
        record_write(&p, &format!("{RECORD_HEADER}\n"), false).unwrap();
        record_write(
            &p,
            "1700000000000,rpm,Engine speed,,800,rpm,,\n\
             1700000000000,vin,VIN,,,text,,WVWZZZ00000000000\n",
            true,
        )
        .unwrap();
        let r = record_write(&p, "1700000000500,rpm,Engine speed,,900,rpm,,\n", true).unwrap();
        assert!(r["bytes"].as_u64().unwrap() > 0);
        let log = opentune_logview::parse_file(
            std::path::Path::new(&p),
            &opentune_logview::ParseOptions::default(),
        )
        .unwrap();
        assert_eq!(log.format, opentune_logview::LogFormat::OpentuneLong);
        assert_eq!(log.t, vec![0.0, 0.5]);
        assert_eq!(log.channels.len(), 1, "the VIN is listed, not plotted");
        assert_eq!(log.channels[0].values, vec![800.0, 900.0]);
        // A new recording truncates; refusals for other files / no header.
        record_write(&p, &format!("{RECORD_HEADER}\n"), false).unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            format!("{RECORD_HEADER}\n")
        );
        assert!(record_write(&dir.join("x.bin").to_string_lossy(), "", true).is_err());
        assert!(record_write(&p, "1,rpm\n", false).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
    use crate::adapter::Opener;
    use opentune_core::protocol::DiagTransport;
    use opentune_core::RawDiag;
    use opentune_core::{
        Capabilities, ConnectConfig, EcuId, Error, InterfaceInfo, MemoryRegion, Progress,
    };
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc};

    /// A fake adapter whose transport is a [`SimEcu`] (or answers nothing).
    struct ObdIface {
        ecu: Option<SimEcu>,
        requests: Arc<AtomicUsize>,
        /// Refuse every request (`7F <sid> 12`), no battery reading.
        nrc: bool,
        /// Fail the next request with an adapter reset.
        reset: Arc<AtomicBool>,
    }

    impl DiagTransport for ObdIface {
        fn request(&mut self, req: &[u8], timeout_ms: u64) -> opentune_core::Result<Vec<u8>> {
            self.requests.fetch_add(1, Ordering::SeqCst);
            if self.reset.swap(false, Ordering::SeqCst) {
                return Err(Error::AdapterReset("banner".into()));
            }
            if self.nrc {
                return Ok(vec![0x7F, req.first().copied().unwrap_or(0), 0x12]);
            }
            match self.ecu.as_mut() {
                Some(e) => e.request(req).map_err(Error::Protocol),
                None => Err(Error::Timeout(timeout_ms)),
            }
        }
    }

    impl Interface for ObdIface {
        fn info(&self) -> InterfaceInfo {
            InterfaceInfo {
                id: "sim".into(),
                display_name: "Simulated OBD".into(),
                firmware: None,
                serial: None,
            }
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }
        fn open(&mut self, _cfg: &ConnectConfig) -> opentune_core::Result<()> {
            Ok(())
        }
        fn close(&mut self) -> opentune_core::Result<()> {
            Ok(())
        }
        fn battery_millivolts(&mut self) -> opentune_core::Result<Option<u32>> {
            Ok(self.ecu.as_ref().filter(|_| !self.nrc).map(|_| 12_600))
        }
        fn read_ecu_id(&mut self) -> opentune_core::Result<EcuId> {
            Err(Error::Unsupported("sim".into()))
        }
        fn read_memory(
            &mut self,
            _region: MemoryRegion,
            _progress: &mut dyn FnMut(Progress),
        ) -> opentune_core::Result<Vec<u8>> {
            Err(Error::Unsupported("sim".into()))
        }
        fn write_memory(
            &mut self,
            _region: MemoryRegion,
            _data: &[u8],
            _progress: &mut dyn FnMut(Progress),
        ) -> opentune_core::Result<()> {
            Err(Error::Unsupported("sim".into()))
        }
        fn diag(&mut self) -> Option<&mut dyn DiagTransport> {
            Some(self)
        }
    }

    /// A hub whose adapter is a simulated ECU (`alive`) or a dead bus. Counts
    /// opens, requests, and records each open's tx id.
    struct Rig {
        hub: AdapterHub,
        opens: Arc<AtomicUsize>,
        requests: Arc<AtomicUsize>,
        last_tx: Arc<AtomicUsize>,
        reset: Arc<AtomicBool>,
    }

    fn rig(alive: bool) -> Rig {
        rig_with(alive, false)
    }

    fn rig_with(alive: bool, nrc: bool) -> Rig {
        let opens = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));
        let last_tx = Arc::new(AtomicUsize::new(0));
        let reset = Arc::new(AtomicBool::new(false));
        let (o, r, t, x) = (
            opens.clone(),
            requests.clone(),
            last_tx.clone(),
            reset.clone(),
        );
        let hub = AdapterHub::spawn(move || -> Opener {
            Box::new(move |k: &ConnKey| {
                o.fetch_add(1, Ordering::SeqCst);
                t.store(k.addr.tx_id as usize, Ordering::SeqCst);
                Ok(Box::new(ObdIface {
                    ecu: alive.then(|| SimEcu::demo(0)),
                    requests: r.clone(),
                    nrc,
                    reset: x.clone(),
                }) as Box<dyn Interface>)
            })
        });
        Rig {
            hub,
            opens,
            requests,
            last_tx,
            reset,
        }
    }

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    fn conn(driver: &str) -> Conn {
        Conn {
            driver: driver.into(),
            // Unique per test so the global session cache does not leak.
            port: Some(format!(
                "/dev/test-{driver}-{}",
                NEXT.fetch_add(1, Ordering::SeqCst)
            )),
            ..Conn::default()
        }
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
    fn obdsim_works_without_hardware() {
        // A dead hub proves the sim never touches the adapter.
        let r = rig(false);
        let s = supported_impl(&r.hub, conn(SIM_DRIVER)).unwrap();
        assert_eq!(s["vin"], "WVWZZZ3CZ9E000000");
        let pids: Vec<u64> = s["pids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap())
            .collect();
        assert!(pids.contains(&0x0C) && !pids.contains(&0x79));
        let v = read_impl(
            &r.hub,
            conn(SIM_DRIVER),
            vec![0x0C, 0x05, 0x0B, 0x78, 0x79, 0x02],
        )
        .unwrap();
        assert!(v["t_ms"].as_u64().unwrap() > 0);
        assert_eq!(v["battery_mv"], 14_100);
        assert!(v["values"]["rpm"].is_number());
        assert!(v["values"]["boost_kpa"].is_number());
        assert!(v["values"]["egt_b1"].is_array());
        assert!(v["values"]["egt_b2"].is_null());
        assert!(v["errors"]["egt_b2"].is_string());
        assert!(v["errors"]["pid_02"].is_string());
        assert_eq!(r.opens.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn hub_reads_reuse_adapter_and_cached_session() {
        let r = rig(true);
        let c = conn("elm327obd");
        let s = supported_impl(&r.hub, c.clone()).unwrap();
        assert_eq!(s["vin"], "WVWZZZ3CZ9E000000");
        assert_eq!(r.last_tx.load(Ordering::SeqCst), 0x7E0, "engine default");

        r.requests.store(0, Ordering::SeqCst);
        let pids = vec![0x0C, 0x05, 0x0D, 0x79];
        let v = read_impl(&r.hub, c.clone(), pids.clone()).unwrap();
        assert_eq!(v["battery_mv"], 12_600);
        assert!(v["values"]["rpm"].is_number());
        // 0x79 filtered by the cached supported set: one 3-PID request.
        assert_eq!(v["errors"]["egt_b2"], "not supported by ECU");
        assert_eq!(r.requests.load(Ordering::SeqCst), 1);
        read_impl(&r.hub, c, pids).unwrap();
        assert_eq!(r.opens.load(Ordering::SeqCst), 1, "ticks reuse one open");
    }

    #[test]
    fn tx_rx_ids_select_the_module() {
        let r = rig(true);
        let mut c = conn("elm327obd");
        c.tx_id = Some("7E1".into());
        c.rx_id = Some("7E9".into());
        read_impl(&r.hub, c.clone(), vec![0x0C]).unwrap();
        assert_eq!(r.last_tx.load(Ordering::SeqCst), 0x7E1);
        c.rx_id = None;
        assert!(read_impl(&r.hub, c, vec![0x0C]).is_err());
    }

    #[test]
    fn silent_tick_reopens_adapter() {
        let r = rig(false);
        let c = conn("elm327obd");
        let v = read_impl(&r.hub, c.clone(), vec![0x0C, 0x05]).unwrap();
        assert!(v["values"]["rpm"].is_null());
        assert!(v["errors"]["rpm"].is_string());
        read_impl(&r.hub, c, vec![0x0C, 0x05]).unwrap();
        assert_eq!(
            r.opens.load(Ordering::SeqCst),
            2,
            "invalidated after silence"
        );
    }

    #[test]
    fn a_poll_the_ecu_only_refused_is_alive() {
        // Every request gets a negative response and the adapter reports no
        // battery voltage: no value, but the link answered — keep the adapter.
        let r = rig_with(true, true);
        let c = conn("elm327obd");
        let v = read_impl(&r.hub, c.clone(), vec![0x0C, 0x05]).unwrap();
        assert!(v["values"]["rpm"].is_null());
        assert_eq!(v["link"]["state"], "up", "{v}");
        read_impl(&r.hub, c, vec![0x0C, 0x05]).unwrap();
        assert_eq!(r.opens.load(Ordering::SeqCst), 1, "no re-open for NRCs");
    }

    #[test]
    fn adapter_reset_is_noted_and_reopens() {
        let r = rig(true);
        let c = conn("elm327obd");
        r.reset.store(true, Ordering::SeqCst);
        let v = read_impl(&r.hub, c.clone(), vec![0x0C]).unwrap();
        assert_eq!(v["link"]["state"], "lost", "{v}");
        let note = v["errors"]["link"].as_str().unwrap();
        assert!(note.contains("adapter reset"), "{note}");
        let v = read_impl(&r.hub, c, vec![0x0C]).unwrap();
        assert_eq!(v["link"]["state"], "up");
        assert!(v["values"]["rpm"].is_number());
        assert_eq!(
            r.opens.load(Ordering::SeqCst),
            2,
            "re-opened after the reset"
        );
    }

    #[test]
    fn adapter_busy_passes_through() {
        let r = Arc::new(rig(true));
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let r2 = r.clone();
        let t = std::thread::spawn(move || {
            r2.hub.run("long scan", move |_s| {
                started_tx.send(()).unwrap();
                go_rx.recv().unwrap();
                Ok(())
            })
        });
        started_rx.recv().unwrap();
        let e = read_impl(&r.hub, conn("elm327obd"), vec![0x0C]).unwrap_err();
        assert!(
            e.starts_with("adapter busy") && e.contains("long scan"),
            "{e}"
        );
        go_tx.send(()).unwrap();
        t.join().unwrap().unwrap();
    }
}
