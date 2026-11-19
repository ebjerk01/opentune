//! # OBD-II inspection (backend)
//!
//! One Tauri command, [`obd_inspect`], over [`opentune_core::obd::inspect`]:
//! readiness monitors (PID `01`/`41`), stored / pending / permanent DTCs
//! (modes `03`/`07`/`0A`), freeze frame (mode `02`) and vehicle information
//! (mode `09`: VIN, CALIDs, CVNs, ECU name). `sections` picks what to read
//! (`readiness`, `dtcs`, `freeze`, `info`; empty = all).
//!
//! Like the dashboard ([`crate::feat_dash`]) all adapter I/O runs on the
//! app-wide [`AdapterHub`] (op name `"Inspection"`), so another panel using the
//! adapter makes this fail fast with "adapter busy". Addressing:
//!
//! - UI-entered `tx_id`/`rx_id` → that one module;
//! - else `all_ecus` → every ISO 15765-4 OBD address (`7E0/7E8` … `7E7/7EF`);
//!   the driver is re-opened per address (the ids are part of the connection
//!   key) and silent addresses are dropped from the result;
//! - else the engine ECU (`7E0/7E8`, reported as `assumed`).
//!
//! Everything here is read-only. The pseudo driver `obdsim` answers from a
//! simulated vehicle ([`SimVehicle::demo`]: engine + transmission) and never
//! touches the hub.

use crate::adapter::{self, AdapterHub, Addr, ConnKey, Session};
use opentune_core::obd::inspect::{inspect, obd_ecu_label, Inspection, Sections, OBD_ECU_IDS};
use opentune_core::obd::sim::SimVehicle;
use serde_json::{json, Value};

/// Per-request timeout for inspection exchanges, milliseconds.
const TIMEOUT_MS: u64 = 1000;
/// Pseudo driver id for the built-in simulated vehicle.
const SIM_DRIVER: &str = "obdsim";
/// Hub operation name (shown in "adapter busy" errors).
const OP: &str = "Inspection";

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Connection args as the UI sends them (`window.OT.connArgs()`).
#[derive(Debug, Clone, Default)]
pub(crate) struct Conn {
    pub(crate) driver: String,
    pub(crate) port: Option<String>,
    pub(crate) baud: Option<String>,
    pub(crate) bus: Option<String>,
    pub(crate) tx_id: Option<String>,
    pub(crate) rx_id: Option<String>,
}

/// What one [`obd_inspect`] call reads.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Request {
    pub(crate) sections: Sections,
    pub(crate) all_ecus: bool,
    pub(crate) frame: u8,
}

impl Conn {
    fn is_sim(&self) -> bool {
        self.driver.trim() == SIM_DRIVER
    }

    /// The modules to ask, each with its "assumed engine" flag.
    fn targets(&self, all_ecus: bool) -> Result<Vec<(Addr, bool)>, String> {
        match adapter::resolve(self.tx_id.as_deref(), self.rx_id.as_deref(), None)? {
            Some(a) => Ok(vec![(a, false)]),
            None if all_ecus => Ok(OBD_ECU_IDS
                .iter()
                .map(|&(tx_id, rx_id)| (Addr { tx_id, rx_id }, false))
                .collect()),
            None => Ok(vec![(Addr::ENGINE, true)]),
        }
    }

    fn key(&self, addr: Addr) -> ConnKey {
        ConnKey::new(
            &self.driver,
            self.port.as_deref(),
            self.baud.as_deref(),
            self.bus.as_deref(),
            addr,
        )
    }
}

/// One module's JSON: the [`Inspection`] plus `addr` (`{tx, rx, assumed}`) and
/// `label` (`engine`, `transmission`, `other`).
fn ecu_json(addr: Addr, assumed: bool, i: &Inspection) -> Value {
    let mut v = serde_json::to_value(i).unwrap_or_else(|_| json!({}));
    v["addr"] = addr.json(assumed);
    v["label"] = obd_ecu_label(addr.tx_id).into();
    v
}

/// The command's result: `{t_ms, ecus: [...], silent: ["7E2", ...]}`. With
/// `all_ecus`, modules that did not answer are listed in `silent` only; a
/// single target is always reported (with `responded: false` when silent).
fn result_json(all_ecus: bool, ecus: Vec<(Addr, bool, Inspection)>) -> Value {
    let mut list = Vec::new();
    let mut silent = Vec::new();
    for (addr, assumed, i) in ecus {
        if all_ecus && !i.responded {
            silent.push(format!("{:03X}", addr.tx_id));
        } else {
            list.push(ecu_json(addr, assumed, &i));
        }
    }
    json!({ "t_ms": now_ms(), "ecus": list, "silent": silent })
}

/// Inspect every target on the hub worker. With a single target an adapter
/// failure is an error; while scanning all addresses it marks that address
/// silent. When nothing answered at all the opened adapter is dropped.
fn inspect_on(
    s: &mut Session,
    conn: &Conn,
    targets: &[(Addr, bool)],
    req: Request,
) -> Result<Vec<(Addr, bool, Inspection)>, String> {
    let mut out = Vec::new();
    for &(addr, assumed) in targets {
        let key = conn.key(addr);
        let r = s.with_diag(&key, TIMEOUT_MS, |d| {
            Ok(inspect(d, req.sections, req.frame))
        });
        let i = match r {
            Ok(i) => i,
            Err(e) if targets.len() == 1 => return Err(e),
            Err(e) => {
                let mut i = Inspection::default();
                i.errors.insert("module".into(), e);
                i
            }
        };
        out.push((addr, assumed, i));
    }
    if !out.iter().any(|(_, _, i)| i.responded) {
        s.invalidate();
    }
    Ok(out)
}

/// The simulated vehicle: modules missing from it stay silent.
fn inspect_sim(targets: &[(Addr, bool)], req: Request) -> Vec<(Addr, bool, Inspection)> {
    let mut v = SimVehicle::demo(now_ms() / 100);
    targets
        .iter()
        .map(|&(addr, assumed)| {
            let i = match v.ecus.get_mut(&addr.tx_id) {
                Some(ecu) => inspect(ecu, req.sections, req.frame),
                None => {
                    let mut silent =
                        |_: &[u8]| -> Result<Vec<u8>, String> { Err("NO DATA".into()) };
                    inspect(&mut silent, req.sections, req.frame)
                }
            };
            (addr, assumed, i)
        })
        .collect()
}

pub(crate) fn inspect_impl(hub: &AdapterHub, conn: Conn, req: Request) -> Result<Value, String> {
    let targets = conn.targets(req.all_ecus)?;
    let all = targets.len() > 1;
    if conn.is_sim() {
        return Ok(result_json(all, inspect_sim(&targets, req)));
    }
    let ecus = hub.run(OP, move |s| inspect_on(s, &conn, &targets, req))?;
    Ok(result_json(all, ecus))
}

/// Read OBD-II inspection data. `sections`: any of `readiness`, `dtcs`,
/// `freeze`, `info` (`null`/empty = all). `all_ecus`: ask every OBD address
/// 7E0..7E7 (ignored when `tx_id`/`rx_id` are set). `frame`: freeze-frame
/// number (default 0). Returns `{t_ms, ecus, silent}`; each `ecus` entry is
/// `{addr, label, responded, readiness, dtcs, freeze_frame, info, errors}`
/// with `null` for a section not read (its error, if any, in `errors`).
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn obd_inspect(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    tx_id: Option<String>,
    rx_id: Option<String>,
    sections: Option<Vec<String>>,
    all_ecus: Option<bool>,
    frame: Option<u8>,
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
    let req = Request {
        sections: Sections::parse(&sections.unwrap_or_default())?,
        all_ecus: all_ecus.unwrap_or(false),
        frame: frame.unwrap_or(0),
    };
    inspect_impl(&hub, conn, req)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::Opener;
    use opentune_core::obd::sim::SimEcu;
    use opentune_core::protocol::DiagTransport;
    use opentune_core::{
        Capabilities, ConnectConfig, EcuId, Error, Interface, InterfaceInfo, MemoryRegion,
        Progress, RawDiag,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc, Mutex};

    /// A fake adapter bound to one simulated module (or a silent address).
    struct SimIface {
        ecu: Option<SimEcu>,
        requests: Arc<AtomicUsize>,
    }

    impl DiagTransport for SimIface {
        fn request(&mut self, req: &[u8], timeout_ms: u64) -> opentune_core::Result<Vec<u8>> {
            self.requests.fetch_add(1, Ordering::SeqCst);
            match self.ecu.as_mut() {
                Some(e) => e.request(req).map_err(Error::Protocol),
                None => Err(Error::Timeout(timeout_ms)),
            }
        }
    }

    impl Interface for SimIface {
        fn info(&self) -> InterfaceInfo {
            InterfaceInfo {
                id: "sim".into(),
                display_name: "Simulated OBD vehicle".into(),
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
            Some(self)
        }
    }

    /// A hub over [`SimVehicle::demo`] (or an empty vehicle when `alive` is
    /// false). Records each open's tx id and counts requests.
    struct Rig {
        hub: AdapterHub,
        opens: Arc<Mutex<Vec<u32>>>,
        requests: Arc<AtomicUsize>,
    }

    fn rig(alive: bool) -> Rig {
        let opens = Arc::new(Mutex::new(Vec::new()));
        let requests = Arc::new(AtomicUsize::new(0));
        let (o, r) = (opens.clone(), requests.clone());
        let hub = AdapterHub::spawn(move || -> Opener {
            let vehicle = if alive {
                SimVehicle::demo(0)
            } else {
                SimVehicle::default()
            };
            Box::new(move |k: &ConnKey| {
                o.lock().unwrap().push(k.addr.tx_id);
                Ok(Box::new(SimIface {
                    ecu: vehicle.ecus.get(&k.addr.tx_id).cloned(),
                    requests: r.clone(),
                }) as Box<dyn Interface>)
            })
        });
        Rig {
            hub,
            opens,
            requests,
        }
    }

    fn conn(driver: &str) -> Conn {
        Conn {
            driver: driver.into(),
            port: Some("/dev/test".into()),
            ..Conn::default()
        }
    }

    fn req(sections: &[&str], all_ecus: bool) -> Request {
        Request {
            sections: Sections::parse(sections).unwrap(),
            all_ecus,
            frame: 0,
        }
    }

    #[test]
    fn obdsim_works_without_hardware() {
        let r = rig(false);
        let v = inspect_impl(&r.hub, conn(SIM_DRIVER), req(&[], false)).unwrap();
        assert!(v["t_ms"].as_u64().unwrap() > 0);
        let e = &v["ecus"][0];
        assert_eq!(e["addr"]["tx"], "7E0");
        assert_eq!(e["addr"]["assumed"], true);
        assert_eq!(e["label"], "engine");
        assert_eq!(e["responded"], true);
        assert_eq!(e["readiness"]["status"]["mil"], true);
        assert_eq!(e["readiness"]["status"]["dtc_count"], 2);
        assert_eq!(e["readiness"]["status"]["ignition"], "compression");
        assert_eq!(e["dtcs"]["stored"][1]["code"], "P2002");
        assert_eq!(e["dtcs"]["permanent"][0]["code"], "P0401");
        assert_eq!(e["freeze_frame"]["dtc"]["code"], "P0401");
        assert!(e["freeze_frame"]["values"].as_array().unwrap().len() >= 9);
        assert_eq!(e["info"]["calids"][0], "OT-DEMO-CAL-0001");
        assert_eq!(e["info"]["cvns"][1], "0000BEEF");
        assert_eq!(e["info"]["ecu_name"], "ECM-EngineControl");
        assert_eq!(r.opens.lock().unwrap().len(), 0, "sim never opens a driver");

        // All ECUs: engine + transmission, the other six silent.
        let v = inspect_impl(&r.hub, conn(SIM_DRIVER), req(&["info"], true)).unwrap();
        let ecus = v["ecus"].as_array().unwrap();
        assert_eq!(ecus.len(), 2);
        assert_eq!(ecus[1]["label"], "transmission");
        assert_eq!(ecus[1]["info"]["ecu_name"], "TCM-TransmissionCtl");
        assert!(ecus[1]["readiness"].is_null());
        assert_eq!(v["silent"].as_array().unwrap().len(), 6);
    }

    #[test]
    fn hub_engine_default_and_explicit_module() {
        let r = rig(true);
        let v = inspect_impl(&r.hub, conn("elm327obd"), req(&["dtcs"], false)).unwrap();
        assert_eq!(v["ecus"][0]["dtcs"]["pending"][0]["code"], "P0299");
        assert_eq!(*r.opens.lock().unwrap(), vec![0x7E0]);
        // Exactly the three DTC requests: no readiness / info traffic.
        assert_eq!(r.requests.load(Ordering::SeqCst), 3);

        let mut c = conn("elm327obd");
        c.tx_id = Some("7E1".into());
        c.rx_id = Some("7E9".into());
        let v = inspect_impl(&r.hub, c.clone(), req(&["readiness"], true)).unwrap();
        // Explicit ids win over all_ecus.
        let ecus = v["ecus"].as_array().unwrap();
        assert_eq!(ecus.len(), 1);
        assert_eq!(ecus[0]["addr"]["tx"], "7E1");
        assert_eq!(ecus[0]["addr"]["assumed"], false);
        assert_eq!(ecus[0]["readiness"]["status"]["mil"], false);
        c.rx_id = None;
        assert!(inspect_impl(&r.hub, c, req(&[], false)).is_err());
    }

    #[test]
    fn hub_all_ecus_scan_drops_silent_addresses() {
        let r = rig(true);
        let v = inspect_impl(&r.hub, conn("elm327obd"), req(&["readiness"], true)).unwrap();
        let tx: Vec<&str> = v["ecus"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["addr"]["tx"].as_str().unwrap())
            .collect();
        assert_eq!(tx, ["7E0", "7E1"]);
        assert_eq!(
            v["silent"],
            json!(["7E2", "7E3", "7E4", "7E5", "7E6", "7E7"])
        );
        assert_eq!(r.opens.lock().unwrap().len(), 8, "one open per address");
    }

    #[test]
    fn silent_single_module_is_reported_and_reopened() {
        let r = rig(false);
        let v = inspect_impl(&r.hub, conn("elm327obd"), req(&[], false)).unwrap();
        let e = &v["ecus"][0];
        assert_eq!(e["responded"], false);
        assert!(e["errors"]["module"].is_string());
        assert_eq!(r.requests.load(Ordering::SeqCst), 1, "gave up after one");
        inspect_impl(&r.hub, conn("elm327obd"), req(&[], false)).unwrap();
        assert_eq!(
            r.opens.lock().unwrap().len(),
            2,
            "invalidated after silence"
        );
    }

    #[test]
    fn bad_section_is_an_error() {
        assert!(Sections::parse(&["nope"]).is_err());
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
        let e = inspect_impl(&r.hub, conn("elm327obd"), req(&[], false)).unwrap_err();
        assert!(e.contains("adapter busy") && e.contains("long scan"), "{e}");
        go_tx.send(()).unwrap();
        t.join().unwrap().unwrap();
    }
}
