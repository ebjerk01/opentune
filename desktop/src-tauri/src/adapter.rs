//! Shared live-adapter access for every diagnostic panel.
//!
//! Three safety properties live here, so no feature panel can get them wrong:
//!
//! 1. **Module addressing.** Every opened adapter is bound to an explicit CAN
//!    request/response id pair ([`Addr`]), carried in the [`ConnKey`] and applied
//!    through [`ConnectConfig::tx_id`]/[`ConnectConfig::rx_id`] (an ELM327 turns
//!    them into `ATSH`/`ATCRA`). A panel resolves the target with [`resolve`]:
//!    an address typed in the UI wins, then the definition's own top-level
//!    `req_id`/`resp_id`, else *unknown*. Read-only operations may fall back to
//!    the engine ECU (0x7E0/0x7E8) and say so ([`read_addr`]); **mutating
//!    operations refuse an unknown target** ([`write_addr`]) instead of silently
//!    writing to the engine.
//! 2. **One adapter operation at a time.** A serial adapter opens exclusively, so
//!    two panels (OBD live + logger, a Detect click during a scan) used to
//!    collide. [`AdapterHub::run`] claims a single app-wide busy slot and fails
//!    fast with a clear "adapter busy" error while another operation runs.
//! 3. **A reused, owned connection.** The hub runs every operation on one worker
//!    thread that owns the opened driver ([`Session`]), so a poll tick reuses
//!    the already-initialised adapter instead of re-running the full `ATZ`
//!    handshake. The driver is re-opened whenever the driver/port/baud/bus or
//!    the module address changes, after any failed operation, and it is closed
//!    (port released) after [`IDLE_RELEASE`] without use.
//!
//! Driver objects are not `Send`, which is why they never leave the worker
//! thread: commands send it a closure and wait for the result.

use opentune_core::{ConnectConfig, CoreDiag, Interface, RawDiag};
use serde_json::{json, Value};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

/// Close the cached adapter (releasing the serial port) after this long idle.
pub const IDLE_RELEASE: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------------------
// Addressing
// ---------------------------------------------------------------------------

/// A module's diagnostic CAN request (`tx_id`) / response (`rx_id`) ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Addr {
    pub tx_id: u32,
    pub rx_id: u32,
}

impl Addr {
    /// The engine ECU's physical diagnostic address (ISO 15765-4).
    pub const ENGINE: Addr = Addr {
        tx_id: 0x7E0,
        rx_id: 0x7E8,
    };

    /// `{ tx, rx, assumed }` for the UI, so it can show which module was talked to.
    pub fn json(&self, assumed: bool) -> Value {
        json!({
            "tx": format!("{:03X}", self.tx_id),
            "rx": format!("{:03X}", self.rx_id),
            "assumed": assumed,
        })
    }
}

/// Parse a CAN id: hex with or without `0x` (`"7E0"`, `"0x7E0"`). CAN ids are
/// conventionally written in hex, so a bare number is hex too.
pub fn parse_can_id(s: &str) -> Result<u32, String> {
    let t = s.trim();
    let h = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    let v = u32::from_str_radix(h, 16).map_err(|_| format!("invalid CAN id '{s}' (hex)"))?;
    if v > 0x1FFF_FFFF {
        return Err(format!("CAN id '{s}' is larger than 29 bits"));
    }
    Ok(v)
}

fn nz(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

/// Read an optional top-level `req_id`/`resp_id` pair from a definition's JSON
/// text (the same field names the auto-scan module list uses). A JSON number is
/// taken as-is, a string as hex. Both or neither must be present.
pub fn def_addr(def_text: &str) -> Result<Option<Addr>, String> {
    let v: Value =
        serde_json::from_str(def_text).map_err(|e| format!("parse definition JSON: {e}"))?;
    let get = |k: &str| -> Result<Option<u32>, String> {
        match v.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Number(n)) => n
                .as_u64()
                .filter(|n| *n <= 0x1FFF_FFFF)
                .map(|n| Some(n as u32))
                .ok_or_else(|| format!("definition '{k}' is not a valid CAN id")),
            Some(Value::String(s)) => parse_can_id(s).map(Some),
            Some(_) => Err(format!("definition '{k}' must be a number or hex string")),
        }
    };
    match (get("req_id")?, get("resp_id")?) {
        (Some(tx_id), Some(rx_id)) => Ok(Some(Addr { tx_id, rx_id })),
        (None, None) => Ok(None),
        _ => Err("definition has only one of req_id/resp_id — set both".into()),
    }
}

/// Resolve the target module address: UI-entered ids win, then the
/// definition's `req_id`/`resp_id`; `Ok(None)` when neither is given.
pub fn resolve(
    tx: Option<&str>,
    rx: Option<&str>,
    def_text: Option<&str>,
) -> Result<Option<Addr>, String> {
    match (nz(tx), nz(rx)) {
        (Some(t), Some(r)) => Ok(Some(Addr {
            tx_id: parse_can_id(t)?,
            rx_id: parse_can_id(r)?,
        })),
        (None, None) => match def_text {
            Some(d) => def_addr(d),
            None => Ok(None),
        },
        _ => Err("set both the target request and response CAN ids (or neither)".into()),
    }
}

/// Address for a **read-only** operation: the resolved one, else the engine
/// ECU. The flag is `true` when the engine was assumed.
pub fn read_addr(resolved: Option<Addr>) -> (Addr, bool) {
    match resolved {
        Some(a) => (a, false),
        None => (Addr::ENGINE, true),
    }
}

/// Address for a **mutating** operation (write / routine / actuator / service /
/// clear): refuses rather than defaulting to the engine ECU.
pub fn write_addr(resolved: Option<Addr>, what: &str) -> Result<Addr, String> {
    resolved.ok_or_else(|| {
        format!(
            "{what} refused: the target module's CAN address is unknown. Enter the module's \
             request/response ids in the Target fields (engine = 7E0 / 7E8) or add \
             \"req_id\"/\"resp_id\" to the definition — OpenTune will not default a write to the engine ECU."
        )
    })
}

// ---------------------------------------------------------------------------
// Connection key
// ---------------------------------------------------------------------------

/// Everything that identifies one opened adapter binding. Two operations with
/// the same key reuse the same opened driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnKey {
    pub driver: String,
    pub port: Option<String>,
    pub baud: Option<String>,
    pub bus: Option<String>,
    pub addr: Addr,
}

impl ConnKey {
    /// Build a key from the UI's connection options (blank strings ignored).
    pub fn new(
        driver: &str,
        port: Option<&str>,
        baud: Option<&str>,
        bus: Option<&str>,
        addr: Addr,
    ) -> Self {
        ConnKey {
            driver: driver.to_string(),
            port: nz(port).map(str::to_string),
            baud: nz(baud).map(str::to_string),
            bus: nz(bus).map(str::to_string),
            addr,
        }
    }

    /// The same connection re-bound to another module address.
    pub fn with_addr(&self, addr: Addr) -> Self {
        ConnKey {
            addr,
            ..self.clone()
        }
    }

    /// The driver `ConnectConfig` for this key, with the module's tx/rx ids.
    /// Refuses 29-bit ids on the ELM327 drivers, which only program 11-bit
    /// headers and would otherwise silently truncate to a different module.
    pub fn config(&self) -> Result<ConnectConfig, String> {
        let a = self.addr;
        if self.driver.starts_with("elm327") && (a.tx_id > 0x7FF || a.rx_id > 0x7FF) {
            return Err(format!(
                "the {} driver supports 11-bit CAN ids only (got {:X}/{:X})",
                self.driver, a.tx_id, a.rx_id
            ));
        }
        let mut cfg = ConnectConfig {
            tx_id: a.tx_id,
            rx_id: a.rx_id,
            ..ConnectConfig::default()
        };
        for (k, v) in [
            ("port", &self.port),
            ("baud", &self.baud),
            ("bus", &self.bus),
        ] {
            if let Some(v) = v {
                cfg.options.insert(k.into(), v.clone());
            }
        }
        Ok(cfg)
    }
}

// ---------------------------------------------------------------------------
// Session (owned by the worker thread)
// ---------------------------------------------------------------------------

/// Opens a driver for a key. The production opener builds it from the registry.
pub type Opener = Box<dyn FnMut(&ConnKey) -> Result<Box<dyn Interface>, String>>;

/// The (at most one) opened adapter, reused across operations with equal keys.
pub struct Session {
    opener: Opener,
    cur: Option<(ConnKey, Box<dyn Interface>)>,
}

impl Session {
    pub fn new(opener: Opener) -> Self {
        Session { opener, cur: None }
    }

    /// Close and forget the opened adapter (next use re-opens it).
    pub fn invalidate(&mut self) {
        if let Some((_, mut d)) = self.cur.take() {
            let _ = d.close();
        }
    }

    /// The opened adapter for `key`, opening (or re-opening on a key change) it.
    pub fn interface(&mut self, key: &ConnKey) -> Result<&mut dyn Interface, String> {
        if self.cur.as_ref().map(|(k, _)| k != key).unwrap_or(true) {
            self.invalidate();
            let drv = (self.opener)(key)?;
            self.cur = Some((key.clone(), drv));
        }
        match self.cur.as_mut() {
            Some((_, d)) => Ok(d.as_mut()),
            None => Err("internal error: adapter not open".into()),
        }
    }

    /// Run `f` over the adapter's diagnostic transport for `key`. Any error
    /// drops the opened adapter so the next operation starts from a fresh open.
    pub fn with_diag<R>(
        &mut self,
        key: &ConnKey,
        timeout_ms: u64,
        f: impl FnOnce(&mut CoreDiag<'_>) -> Result<R, String>,
    ) -> Result<R, String> {
        let r = (|| {
            let drv = self.interface(key)?;
            let t = drv
                .diag()
                .ok_or_else(|| format!("driver '{}' has no diagnostic transport", key.driver))?;
            let mut diag = CoreDiag::new(t, timeout_ms);
            f(&mut diag)
        })();
        if r.is_err() {
            self.invalidate();
        }
        r
    }
}

// ---------------------------------------------------------------------------
// UDS session helpers for mutating operations
// ---------------------------------------------------------------------------

/// Enter the extended diagnostic session (`10 03`) on the bound module. Writes,
/// routines and actuator tests are rejected by most ECUs in the default session.
pub fn enter_extended_session(diag: &mut dyn RawDiag) -> Result<(), String> {
    let resp = diag
        .request(&[0x10, 0x03])
        .map_err(|e| format!("extended session (10 03): {e}"))?;
    match resp.as_slice() {
        [0x50, 0x03, ..] => Ok(()),
        [0x7F, 0x10, nrc, ..] => Err(format!(
            "module refused the extended diagnostic session (10 03): NRC 0x{nrc:02X}"
        )),
        other => Err(format!(
            "unexpected reply to extended session (10 03): {other:02X?}"
        )),
    }
}

/// Best-effort return to the default session (`10 01`) after a mutating flow.
pub fn leave_session(diag: &mut dyn RawDiag) {
    let _ = diag.request(&[0x10, 0x01]);
}

// ---------------------------------------------------------------------------
// Hub (app-wide busy lock + worker thread)
// ---------------------------------------------------------------------------

type Job = Box<dyn FnOnce(&mut Session) + Send>;

/// Tauri-managed handle to the adapter worker.
pub struct AdapterHub {
    jobs: Mutex<mpsc::Sender<Job>>,
    busy: Arc<Mutex<Option<String>>>,
}

/// RAII busy-slot claim; frees the slot on drop (also on error / panic).
struct BusyToken(Arc<Mutex<Option<String>>>);

impl Drop for BusyToken {
    fn drop(&mut self) {
        if let Ok(mut s) = self.0.lock() {
            *s = None;
        }
    }
}

impl AdapterHub {
    /// Spawn the worker. `make_opener` runs on the worker thread, so the opener
    /// itself need not be `Send`.
    pub fn spawn<F>(make_opener: F) -> Self
    where
        F: FnOnce() -> Opener + Send + 'static,
    {
        let (tx, rx) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("opentune-adapter".into())
            .spawn(move || {
                let mut session = Session::new(make_opener());
                loop {
                    match rx.recv_timeout(IDLE_RELEASE) {
                        Ok(job) => {
                            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                job(&mut session)
                            }));
                            if r.is_err() {
                                session.invalidate();
                            }
                        }
                        // Idle: release the port so other tools can use the adapter.
                        Err(mpsc::RecvTimeoutError::Timeout) => session.invalidate(),
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            session.invalidate();
                            break;
                        }
                    }
                }
            })
            .expect("spawn adapter worker thread");
        AdapterHub {
            jobs: Mutex::new(tx),
            busy: Arc::new(Mutex::new(None)),
        }
    }

    /// The hub used by the app: drivers come from the registry.
    pub fn with_registry() -> Self {
        Self::spawn(|| Box::new(|k: &ConnKey| crate::open_registry_driver(k)))
    }

    /// Name of the adapter operation currently running, if any.
    #[allow(dead_code)] // used by tests; handy for a status command
    pub fn current(&self) -> Option<String> {
        self.busy.lock().ok().and_then(|s| s.clone())
    }

    fn claim(&self, op: &str) -> Result<BusyToken, String> {
        let mut slot = self
            .busy
            .lock()
            .map_err(|_| "internal error: adapter lock poisoned".to_string())?;
        if let Some(cur) = slot.as_ref() {
            return Err(format!(
                "adapter busy: '{cur}' is using the adapter — wait for it to finish (or stop the live poll) and try again"
            ));
        }
        *slot = Some(op.to_string());
        Ok(BusyToken(self.busy.clone()))
    }

    /// Run one adapter operation exclusively on the worker thread and wait for
    /// its result. Fails immediately with "adapter busy" if another operation
    /// is in flight.
    pub fn run<R, F>(&self, op: &str, f: F) -> Result<R, String>
    where
        R: Send + 'static,
        F: FnOnce(&mut Session) -> Result<R, String> + Send + 'static,
    {
        let _token = self.claim(op)?;
        let (rtx, rrx) = mpsc::channel();
        let job: Job = Box::new(move |s: &mut Session| {
            let _ = rtx.send(f(s));
        });
        self.jobs
            .lock()
            .map_err(|_| "internal error: adapter queue poisoned".to_string())?
            .send(job)
            .map_err(|_| "adapter worker is not running".to_string())?;
        rrx.recv()
            .map_err(|_| format!("'{op}' aborted inside the adapter worker (internal error)"))?
    }
}

// ---------------------------------------------------------------------------
// Test kit: a multi-module simulated CAN bus behind a fake Interface
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod testkit {
    use super::*;
    use opentune_core::protocol::DiagTransport;
    use opentune_core::{Capabilities, EcuId, Error, InterfaceInfo, MemoryRegion, Progress};
    use opentune_udssim::server::UdsServer;
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::rc::Rc;

    /// One logged request: `(tx_id, bytes)`.
    pub type Frame = (u32, Vec<u8>);

    /// Simulated vehicle: one UDS server per (tx, rx) address, a wire log of
    /// `(tx_id, request)` and a log of every driver open's address.
    #[derive(Clone, Default)]
    pub struct SimBus {
        pub ecus: Rc<RefCell<BTreeMap<(u32, u32), UdsServer>>>,
        pub wire: Rc<RefCell<Vec<Frame>>>,
        pub opens: Rc<RefCell<Vec<Addr>>>,
        pub cleared: Rc<RefCell<Vec<(u32, u32)>>>,
    }

    impl SimBus {
        pub fn add(&self, tx: u32, rx: u32, server: UdsServer) {
            self.ecus.borrow_mut().insert((tx, rx), server);
        }

        pub fn opener(&self) -> Opener {
            let bus = self.clone();
            Box::new(move |k: &ConnKey| {
                let cfg = k.config()?;
                bus.opens.borrow_mut().push(Addr {
                    tx_id: cfg.tx_id,
                    rx_id: cfg.rx_id,
                });
                Ok(Box::new(SimIface {
                    route: SimRoute {
                        bus: bus.clone(),
                        tx: cfg.tx_id,
                        rx: cfg.rx_id,
                    },
                }) as Box<dyn Interface>)
            })
        }

        pub fn session(&self) -> Session {
            Session::new(self.opener())
        }

        /// Requests sent to `tx`, in order.
        pub fn sent_to(&self, tx: u32) -> Vec<Vec<u8>> {
            self.wire
                .borrow()
                .iter()
                .filter(|(t, _)| *t == tx)
                .map(|(_, r)| r.clone())
                .collect()
        }
    }

    pub struct SimRoute {
        bus: SimBus,
        tx: u32,
        rx: u32,
    }

    impl DiagTransport for SimRoute {
        fn request(&mut self, req: &[u8], timeout_ms: u64) -> opentune_core::Result<Vec<u8>> {
            self.bus.wire.borrow_mut().push((self.tx, req.to_vec()));
            let mut ecus = self.bus.ecus.borrow_mut();
            match ecus.get_mut(&(self.tx, self.rx)) {
                // udssim has no ClearDiagnosticInformation: acknowledge it here
                // and record which module was cleared.
                Some(_) if req.first() == Some(&0x14) => {
                    self.bus.cleared.borrow_mut().push((self.tx, self.rx));
                    Ok(vec![0x54])
                }
                Some(s) => Ok(s.handle(req)),
                None => Err(Error::Timeout(timeout_ms)), // nobody answers
            }
        }
    }

    pub struct SimIface {
        route: SimRoute,
    }

    impl Interface for SimIface {
        fn info(&self) -> InterfaceInfo {
            InterfaceInfo {
                id: "sim".into(),
                display_name: "Simulated bus".into(),
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
            Some(&mut self.route)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::SimBus;
    use super::*;
    use opentune_udssim::server::UdsServer;

    fn key(addr: Addr) -> ConnKey {
        ConnKey::new("sim", None, None, Some("hs"), addr)
    }

    #[test]
    fn resolve_prefers_ui_then_def_then_unknown() {
        let def = r#"{"id":"x","req_id":"0x726","resp_id":1838}"#;
        let ui = resolve(Some("760"), Some("768"), Some(def))
            .unwrap()
            .unwrap();
        assert_eq!((ui.tx_id, ui.rx_id), (0x760, 0x768));
        let d = resolve(None, Some(" "), Some(def)).unwrap().unwrap();
        assert_eq!((d.tx_id, d.rx_id), (0x726, 0x72E));
        assert_eq!(resolve(None, None, Some(r#"{"id":"x"}"#)).unwrap(), None);
        assert!(resolve(Some("7E0"), None, None).is_err());
        assert!(def_addr(r#"{"req_id":1}"#).is_err());
    }

    #[test]
    fn writes_refuse_an_unknown_target_reads_assume_engine() {
        assert!(write_addr(None, "coding write").is_err());
        assert_eq!(read_addr(None), (Addr::ENGINE, true));
        let a = Addr {
            tx_id: 0x726,
            rx_id: 0x72E,
        };
        assert_eq!(write_addr(Some(a), "x").unwrap(), a);
    }

    #[test]
    fn config_carries_addressing_and_rejects_29bit_on_elm() {
        let a = Addr {
            tx_id: 0x726,
            rx_id: 0x72E,
        };
        let cfg = ConnKey::new("elm327obd", Some("/dev/x"), None, Some("ms"), a)
            .config()
            .unwrap();
        assert_eq!((cfg.tx_id, cfg.rx_id), (0x726, 0x72E));
        assert_eq!(cfg.options.get("bus").map(String::as_str), Some("ms"));
        let big = Addr {
            tx_id: 0x18DA10F1,
            rx_id: 0x18DAF110,
        };
        assert!(ConnKey::new("elm327obd", None, None, None, big)
            .config()
            .is_err());
    }

    #[test]
    fn session_reuses_open_adapter_until_key_changes() {
        let bus = SimBus::default();
        bus.add(0x7E0, 0x7E8, UdsServer::new());
        let mut s = bus.session();
        let k = key(Addr::ENGINE);
        for _ in 0..3 {
            s.with_diag(&k, 100, |d| d.request(&[0x3E, 0x00])).unwrap();
        }
        assert_eq!(bus.opens.borrow().len(), 1, "polls reuse one open");
        let other = k.with_addr(Addr {
            tx_id: 0x760,
            rx_id: 0x768,
        });
        let _ = s.with_diag(&other, 100, |d| d.request(&[0x3E, 0x00]));
        assert_eq!(bus.opens.borrow().len(), 2, "new address re-binds");
        // The failed request (no module at 0x760) invalidated the session.
        s.with_diag(&other, 100, |_| Ok(())).unwrap();
        assert_eq!(bus.opens.borrow().len(), 3, "error forces a fresh open");
    }

    #[test]
    fn hub_rejects_a_second_operation_while_busy() {
        let hub = Arc::new(AdapterHub::spawn(|| {
            Box::new(|_k: &ConnKey| Err::<Box<dyn Interface>, _>("no adapter".to_string()))
        }));
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let h = hub.clone();
        let t = std::thread::spawn(move || {
            h.run("long scan", move |_s| {
                started_tx.send(()).unwrap();
                go_rx.recv().unwrap();
                Ok(1)
            })
        });
        started_rx.recv().unwrap();
        assert_eq!(hub.current().as_deref(), Some("long scan"));
        let e = hub.run("obd live", |_s| Ok(2)).unwrap_err();
        assert!(e.contains("adapter busy") && e.contains("long scan"), "{e}");
        go_tx.send(()).unwrap();
        assert_eq!(t.join().unwrap().unwrap(), 1);
        assert_eq!(hub.run("obd live", |_s| Ok(2)).unwrap(), 2, "slot freed");
        // A panicking job frees the slot and keeps the worker alive.
        let e = hub
            .run::<(), _>("boom", |_s| panic!("driver bug"))
            .unwrap_err();
        assert!(e.contains("boom"));
        assert_eq!(hub.run("after", |_s| Ok(3)).unwrap(), 3);
    }

    #[test]
    fn extended_session_helper_checks_the_reply() {
        let mut ok = |_r: &[u8]| -> Result<Vec<u8>, String> { Ok(vec![0x50, 0x03, 0, 0x32]) };
        enter_extended_session(&mut ok).unwrap();
        let mut nrc = |_r: &[u8]| -> Result<Vec<u8>, String> { Ok(vec![0x7F, 0x10, 0x22]) };
        assert!(enter_extended_session(&mut nrc)
            .unwrap_err()
            .contains("0x22"));
    }
}
