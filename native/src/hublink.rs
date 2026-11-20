//! The built-in adapter: one `opentune-hub` session owns the hardware.
//!
//! The top bar's link (connect, battery poll) and every view's diagnostics
//! ([`AdapterAccess`]) run on the same hub worker, so they never fight over
//! a serial port: the hub serializes operations and reuses the opened driver
//! (no re-handshake per request). The link poll only opens the driver (its
//! adapter set-up) and reads the battery voltage; it sends nothing to an ECU.
//!
//! A view's operation that starts while a battery poll is running waits for
//! it (briefly) instead of failing with "adapter busy"; any other running
//! operation still makes it fail fast, as in the hub.

use crate::host::{Ack, AdapterAccess, BackupReceipt, DiagJob, DiagTarget, LinkInfo};
use opentune_hub::adapter::{AdapterHub, Addr, ConnKey};
use std::any::Any;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Hub operation name of the top bar's battery poll.
pub const LINK_POLL: &str = "adapter link";
/// How long a view's operation waits for a running battery poll.
const POLL_WAIT: Duration = Duration::from_secs(3);
/// Per-response timeout of a read-only operation (an absent module fails
/// fast).
const READ_TIMEOUT_MS: u64 = 2500;
/// Per-response timeout of an ECU-changing operation: the UDS P2* budget
/// (the longest an ECU may stay silent between responsePending replies,
/// e.g. during an erase routine or a transfer exit) plus a margin, so a slow
/// but healthy routine is never abandoned halfway.
const MUTATE_TIMEOUT_MS: u64 = opentune_core::protocol::uds::P2_STAR_CLIENT_MS + 1000;

/// The cable the user connected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkCfg {
    pub driver: String,
    pub port: String,
    /// Serial baud (0 = the driver's default).
    pub baud: u32,
    /// `hs` / `ms` (dual-bus ELM327), empty = default.
    pub bus: String,
}

/// [`AdapterAccess`] over an `opentune-hub` session.
pub struct HubAdapter {
    hub: AdapterHub,
    cfg: Mutex<Option<LinkCfg>>,
    target: Mutex<DiagTarget>,
    info: Mutex<LinkInfo>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl HubAdapter {
    /// The app's adapter: drivers from the registry.
    pub fn with_registry() -> Self {
        Self::new(AdapterHub::with_registry())
    }

    pub fn new(hub: AdapterHub) -> Self {
        Self {
            hub,
            cfg: Mutex::new(None),
            target: Mutex::new(DiagTarget::ENGINE),
            info: Mutex::new(LinkInfo::default()),
        }
    }

    /// The app's global target (the link poll keeps the session bound to it,
    /// so a view's operation on the same module reuses the opened adapter).
    pub fn set_target(&self, t: DiagTarget) {
        *lock(&self.target) = t;
    }

    /// Bind (Some) or release (None) the cable. Releasing closes the driver.
    pub fn set_link(&self, cfg: Option<LinkCfg>) {
        {
            let mut info = lock(&self.info);
            *info = match &cfg {
                Some(c) => LinkInfo {
                    driver: c.driver.clone(),
                    port: c.port.clone(),
                    ..Default::default()
                },
                None => LinkInfo::default(),
            };
        }
        let release = cfg.is_none();
        *lock(&self.cfg) = cfg;
        if release {
            // Best effort: the hub also closes an idle driver on its own.
            let _ = self.hub.run("disconnect", |s| {
                s.invalidate();
                Ok(())
            });
        }
    }

    pub fn link_cfg(&self) -> Option<LinkCfg> {
        lock(&self.cfg).clone()
    }

    /// The operation running on the hub, if any.
    pub fn busy(&self) -> Option<String> {
        self.hub.current()
    }

    fn key(&self, target: DiagTarget) -> Result<ConnKey, String> {
        let cfg = self
            .link_cfg()
            .ok_or("no adapter connected: pick a cable and connect it in the top bar")?;
        let baud = (cfg.baud != 0).then(|| cfg.baud.to_string());
        Ok(ConnKey::new(
            &cfg.driver,
            Some(&cfg.port),
            baud.as_deref(),
            Some(&cfg.bus),
            Addr {
                tx_id: target.tx,
                rx_id: target.rx,
            },
        ))
    }

    /// Open (or reuse) the driver for the current target and read the
    /// battery: `(adapter identification, volts)`. Updates [`LinkInfo`].
    pub fn poll_link(&self) -> Result<(String, Option<f32>), String> {
        let key = self.key(*lock(&self.target))?;
        let r = self.hub.run(LINK_POLL, move |s| {
            let r = (|| {
                let d = s.interface(&key)?;
                let i = d.info();
                let ident = match i.firmware.filter(|f| !f.trim().is_empty()) {
                    Some(fw) => format!("{} ({})", i.display_name, fw.trim()),
                    None => i.display_name,
                };
                let mv = d.battery_millivolts().map_err(|e| e.to_string())?;
                Ok((ident, mv.map(|m| m as f32 / 1000.0)))
            })();
            if r.is_err() {
                s.invalidate();
            }
            r
        });
        let mut info = lock(&self.info);
        match &r {
            Ok((_, v)) => {
                info.connected = true;
                info.battery_v = *v;
            }
            Err(e) if e.starts_with("adapter busy") => {}
            Err(_) => {
                info.connected = false;
                info.battery_v = None;
            }
        }
        r
    }

    fn run_job(
        &self,
        timeout_ms: u64,
        op: &str,
        target: DiagTarget,
        job: DiagJob,
    ) -> Result<Box<dyn Any + Send>, String> {
        let key = self.key(target)?;
        let slot = Arc::new(Mutex::new(Some(job)));
        let deadline = Instant::now() + POLL_WAIT;
        loop {
            let (k, sl) = (key.clone(), slot.clone());
            let r = self.hub.run(op, move |s| {
                let job = lock(&sl).take().ok_or("internal error: job already ran")?;
                s.with_diag(&k, timeout_ms, |d| job(d))
            });
            let waiting_on_poll = || self.hub.current().is_none_or(|c| c == LINK_POLL);
            match r {
                Err(e)
                    if e.starts_with("adapter busy")
                        && lock(&slot).is_some()
                        && Instant::now() < deadline
                        && waiting_on_poll() =>
                {
                    std::thread::sleep(Duration::from_millis(25));
                }
                other => return other,
            }
        }
    }
}

impl AdapterAccess for HubAdapter {
    fn link(&self) -> LinkInfo {
        let mut l = lock(&self.info).clone();
        l.target = Some(*lock(&self.target));
        l
    }

    fn read_job(
        &self,
        op: &str,
        target: DiagTarget,
        job: DiagJob,
    ) -> Result<Box<dyn Any + Send>, String> {
        self.run_job(READ_TIMEOUT_MS, op, target, job)
    }

    fn mutate_job(
        &self,
        op: &str,
        ack: Ack,
        backup: &BackupReceipt,
        job: DiagJob,
    ) -> Result<Box<dyn Any + Send>, String> {
        // The module the user confirmed, not whatever is selected now.
        let target = ack.ecu_target()?;
        if !backup.path.is_file() {
            return Err(format!(
                "{op}: the backup {} is missing; nothing was sent",
                backup.path.display()
            ));
        }
        self.run_job(MUTATE_TIMEOUT_MS, op, target, job)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{AdapterAccess, ConfirmRequest};
    use opentune_hub::adapter::testkit::SimBus;
    use opentune_udssim::server::UdsServer;

    /// A simulated engine (7E0) + TCU (7E1) behind the hub. The bus is not
    /// Send, so it is built on the worker; `opens` logs the tx id of every
    /// driver open (the module the session was bound to).
    fn sim() -> (Arc<Mutex<Vec<u32>>>, Arc<HubAdapter>) {
        let opens = Arc::new(Mutex::new(Vec::new()));
        let log = opens.clone();
        let hub = AdapterHub::spawn(move || {
            let bus = SimBus::default();
            bus.add(0x7E0, 0x7E8, UdsServer::new());
            bus.add(0x7E1, 0x7E9, UdsServer::new());
            let mut inner = bus.opener();
            Box::new(move |k: &ConnKey| {
                lock(&log).push(k.addr.tx_id);
                inner(k)
            })
        });
        let a = Arc::new(HubAdapter::new(hub));
        a.set_link(Some(LinkCfg {
            driver: "sim".into(),
            port: String::new(),
            baud: 0,
            bus: "hs".into(),
        }));
        (opens, a)
    }

    #[test]
    fn reads_go_to_the_given_module_and_return_values() {
        let (opens, a) = sim();
        let a: Arc<dyn AdapterAccess> = a;
        let r: Vec<u8> = a
            .read("tester present", DiagTarget::ENGINE, |d| {
                d.request(&[0x3E, 0x00])
            })
            .unwrap();
        assert_eq!(r[0], 0x7E);
        let tcu = DiagTarget {
            tx: 0x7E1,
            rx: 0x7E9,
        };
        a.read("tcu", tcu, |d| d.request(&[0x3E, 0x00])).unwrap();
        a.read("tcu again", tcu, |d| d.request(&[0x3E, 0x00]))
            .unwrap();
        assert_eq!(
            *lock(&opens),
            vec![0x7E0, 0x7E1],
            "re-bound once, then reused"
        );
        assert_eq!(a.link().target, Some(DiagTarget::ENGINE));
    }

    /// End to end through the real driver registry with the offline virtual
    /// car: the link poll opens it, a view's read reuses the session.
    #[test]
    fn registry_vcar_link_and_read() {
        let a = Arc::new(HubAdapter::with_registry());
        a.set_link(Some(LinkCfg {
            driver: "vcar".into(),
            port: String::new(),
            baud: 0,
            bus: String::new(),
        }));
        let (ident, _v) = a.poll_link().expect("vcar opens");
        assert!(!ident.is_empty());
        assert!(a.link().connected);
        let a: Arc<dyn AdapterAccess> = a;
        let r: Vec<u8> = a
            .read("tester present", DiagTarget::ENGINE, |d| {
                d.request(&[0x3E, 0x00])
            })
            .expect("vcar answers");
        assert!(!r.is_empty());
    }

    #[test]
    fn mutations_wait_the_full_p2_star_budget() {
        use opentune_core::protocol::uds::{P2_CLIENT_MS, P2_STAR_CLIENT_MS};
        // Checked at compile time: a mutation outwaits an ECU's longest
        // silence between 0x78s; a read stays near P2 to fail fast.
        const _: () = assert!(MUTATE_TIMEOUT_MS > P2_STAR_CLIENT_MS);
        const _: () =
            assert!(READ_TIMEOUT_MS >= P2_CLIENT_MS && READ_TIMEOUT_MS < P2_STAR_CLIENT_MS);
    }

    #[test]
    fn no_cable_no_operation() {
        let a = HubAdapter::new(AdapterHub::spawn(|| {
            Box::new(|_k: &ConnKey| Err("no hardware in tests".to_string()))
        }));
        let a: Arc<dyn AdapterAccess> = Arc::new(a);
        let e = a
            .read("x", DiagTarget::ENGINE, |d| d.request(&[0x3E]))
            .unwrap_err();
        assert!(e.contains("no adapter connected"), "{e}");
    }

    #[test]
    fn mutations_address_the_confirmed_module_and_need_the_backup() {
        let (opens, a) = sim();
        let ctx = eframe::egui::Context::default();
        let jobs = crate::host::Jobs::new(ctx);
        let mut confirms = crate::host::Confirms::default();
        let (mut actions, mut status) = (Vec::new(), None);
        let tcu = DiagTarget {
            tx: 0x7E1,
            rx: 0x7E9,
        };
        let req = || ConfirmRequest {
            key: "w".into(),
            title: "t".into(),
            body: "b".into(),
            typed: None,
            confirm_label: "ok".into(),
        };
        let dir = std::env::temp_dir().join(format!("ot-hublink-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut ask = |target, confirms: &mut crate::host::Confirms| {
            let mut h = crate::host::Host {
                tokens: crate::theme::DARK,
                image_path: None,
                image: None,
                image_edited: false,
                defs: &[],
                def_idx: None,
                map_idx: 0,
                checksums: &[],
                adapter: None,
                diagnostics: None,
                seedkeys: &opentune_hub::seedkeys::SeedKeys::default(),
                target: Ok(target),
                jobs: &jobs,
                confirms,
                actions: &mut actions,
                status: &mut status,
            };
            let r = h.confirm_ecu(req());
            let b = h.backup(&dir, "before write", b"OLD").unwrap();
            (r, b)
        };
        ask(tcu, &mut confirms);
        confirms.answer("w", true);
        // The global target moved to the engine before the click landed.
        a.set_target(DiagTarget::ENGINE);
        let (r, backup) = ask(DiagTarget::ENGINE, &mut confirms);
        let crate::host::ConfirmResult::Accepted(ack) = r else {
            panic!("expected Accepted")
        };
        let a: Arc<dyn AdapterAccess> = a;
        a.mutate("write", ack, &backup, |d| d.request(&[0x3E, 0x00]))
            .unwrap();
        assert_eq!(
            *lock(&opens),
            vec![0x7E1],
            "the confirmed TCU, never the engine"
        );
        // A missing backup file refuses before anything is sent.
        let (r, gone) = ask(tcu, &mut confirms);
        assert!(matches!(r, crate::host::ConfirmResult::Pending));
        confirms.answer("w", true);
        let (r, _) = ask(tcu, &mut confirms);
        let crate::host::ConfirmResult::Accepted(ack) = r else {
            panic!("expected Accepted")
        };
        std::fs::remove_file(&gone.path).unwrap();
        let e = a
            .mutate("write", ack, &gone, |d| d.request(&[0x3E, 0x00]))
            .unwrap_err();
        assert!(e.contains("missing"), "{e}");
        assert_eq!(lock(&opens).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_running_battery_poll_is_waited_for() {
        let (_bus, a) = sim();
        // Hold the hub with a fake "poll" for a moment.
        let h = a.clone();
        let t = std::thread::spawn(move || {
            h.hub.run(LINK_POLL, |_s| {
                std::thread::sleep(Duration::from_millis(150));
                Ok(())
            })
        });
        std::thread::sleep(Duration::from_millis(30));
        let a2: Arc<dyn AdapterAccess> = a.clone();
        let r: Result<Vec<u8>, String> = a2.read("after poll", DiagTarget::ENGINE, |d| {
            d.request(&[0x3E, 0x00])
        });
        assert!(r.is_ok(), "{r:?}");
        t.join().unwrap().unwrap();
        // Another (non-poll) operation still fails fast.
        let h = a.clone();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let t = std::thread::spawn(move || {
            h.hub.run("long scan", move |_s| {
                go_rx.recv().ok();
                Ok(())
            })
        });
        std::thread::sleep(Duration::from_millis(30));
        let e = a2
            .read("x", DiagTarget::ENGINE, |d| d.request(&[0x3E, 0x00]))
            .unwrap_err();
        assert!(e.contains("long scan"), "{e}");
        go_tx.send(()).unwrap();
        t.join().unwrap().unwrap();
    }

    // -----------------------------------------------------------------------
    // Link loss, reopen, target changes, panics
    // -----------------------------------------------------------------------

    use opentune_hub::adapter::testkit::SimFault;
    use std::sync::atomic::Ordering;

    const TCU: DiagTarget = DiagTarget {
        tx: 0x7E1,
        rx: 0x7E9,
    };

    /// Like [`sim`], plus the bus's fault switches (the bus lives on the
    /// worker; its `SimFault` is shared, so it is handed back from there).
    fn sim_faulty() -> (Arc<HubAdapter>, Arc<SimFault>, Arc<Mutex<Vec<u32>>>) {
        let opens = Arc::new(Mutex::new(Vec::new()));
        let log = opens.clone();
        let (ftx, frx) = std::sync::mpsc::channel();
        let hub = AdapterHub::spawn(move || {
            let bus = SimBus::default();
            bus.add(0x7E0, 0x7E8, UdsServer::new());
            bus.add(0x7E1, 0x7E9, UdsServer::new());
            ftx.send(bus.fault.clone()).unwrap();
            let mut inner = bus.opener();
            Box::new(move |k: &ConnKey| {
                lock(&log).push(k.addr.tx_id);
                inner(k)
            })
        });
        let fault = frx.recv_timeout(Duration::from_secs(5)).unwrap();
        let a = Arc::new(HubAdapter::new(hub));
        a.set_link(Some(LinkCfg {
            driver: "sim".into(),
            port: String::new(),
            baud: 0,
            bus: "hs".into(),
        }));
        (a, fault, opens)
    }

    fn opened(f: &SimFault) -> u32 {
        f.opened.load(Ordering::SeqCst)
    }

    fn tester_present(a: &Arc<dyn AdapterAccess>, t: DiagTarget) -> Result<Vec<u8>, String> {
        a.read("tester present", t, |d| d.request(&[0x3E, 0x00]))
    }

    #[test]
    fn a_read_after_an_adapter_reset_reopens() {
        let (a, fault, _) = sim_faulty();
        let a: Arc<dyn AdapterAccess> = a;
        tester_present(&a, DiagTarget::ENGINE).unwrap();
        assert_eq!(opened(&fault), 1);
        // The adapter power-cycles: this read fails as a lost link ...
        fault.reset_next.store(true, Ordering::SeqCst);
        let e = tester_present(&a, DiagTarget::ENGINE).unwrap_err();
        assert!(
            e.starts_with(opentune_core::resilient::LINK_LOST_PREFIX),
            "{e}"
        );
        assert_eq!(opened(&fault), 1, "no reopen inside the failed read");
        // ... and the next one reopens the driver and works.
        assert_eq!(tester_present(&a, DiagTarget::ENGINE).unwrap()[0], 0x7E);
        assert_eq!(opened(&fault), 2);
        tester_present(&a, DiagTarget::ENGINE).unwrap();
        assert_eq!(opened(&fault), 2, "then reused");
    }

    #[test]
    fn an_unplugged_adapter_recovers_once_it_opens_again() {
        let (a, fault, _) = sim_faulty();
        let a: Arc<dyn AdapterAccess> = a;
        tester_present(&a, DiagTarget::ENGINE).unwrap();
        fault.dead_until_reopen.store(true, Ordering::SeqCst);
        fault.refuse_opens.store(2, Ordering::SeqCst);
        let e = tester_present(&a, DiagTarget::ENGINE).unwrap_err();
        assert!(e.contains("unplugged"), "{e}");
        // The device is not back yet: two refused opens.
        for _ in 0..2 {
            let e = tester_present(&a, DiagTarget::ENGINE).unwrap_err();
            assert!(e.contains("no such device"), "{e}");
        }
        assert_eq!(opened(&fault), 1);
        tester_present(&a, DiagTarget::ENGINE).unwrap();
        assert_eq!(opened(&fault), 2);
        assert!(!fault.dead_until_reopen.load(Ordering::SeqCst));
    }

    #[test]
    fn poll_link_drops_connected_on_a_real_error_but_not_when_busy() {
        let (a, fault, _) = sim_faulty();
        let (ident, volts) = a.poll_link().unwrap();
        assert_eq!(ident, "Simulated bus");
        assert_eq!(volts, None);
        assert!(a.link().connected);

        // Another operation holds the hub: "adapter busy" keeps the state.
        let h = a.clone();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let t = std::thread::spawn(move || {
            h.hub.run("long scan", move |_s| {
                go_rx.recv().ok();
                Ok(())
            })
        });
        let t0 = Instant::now();
        while a.busy().is_none() {
            assert!(t0.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(2));
        }
        let e = a.poll_link().unwrap_err();
        assert!(e.starts_with("adapter busy"), "{e}");
        assert!(a.link().connected, "busy is not a lost link");
        go_tx.send(()).unwrap();
        t.join().unwrap().unwrap();

        // The driver is closed and will not open: a real error.
        a.hub
            .run("close", |s| {
                s.invalidate();
                Ok(())
            })
            .unwrap();
        fault.refuse_opens.store(1, Ordering::SeqCst);
        let e = a.poll_link().unwrap_err();
        assert!(e.contains("no such device"), "{e}");
        let l = a.link();
        assert!(!l.connected && l.battery_v.is_none());
        assert_eq!(l.driver, "sim", "the configured cable stays");
        // It comes back.
        a.poll_link().unwrap();
        assert!(a.link().connected);
    }

    #[test]
    fn set_link_none_means_no_adapter_connected() {
        let (a, fault, _) = sim_faulty();
        a.poll_link().unwrap();
        assert_eq!(opened(&fault), 1);
        a.set_link(None);
        assert_eq!(a.link_cfg(), None);
        let l = a.link();
        assert!(!l.connected && l.driver.is_empty() && l.battery_v.is_none());
        assert_eq!(l.target, Some(DiagTarget::ENGINE), "the target is kept");
        let e = a.poll_link().unwrap_err();
        assert!(e.contains("no adapter connected"), "{e}");
        assert!(!a.link().connected);
        let d: Arc<dyn AdapterAccess> = a.clone();
        let e = tester_present(&d, DiagTarget::ENGINE).unwrap_err();
        assert!(e.contains("no adapter connected"), "{e}");
        let dir = std::env::temp_dir().join(format!("ot-hublink-none-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let backup = crate::host::test_backup(&dir, b"OLD");
        let e = d
            .mutate("w", crate::host::test_ack(TCU), &backup, |d| {
                d.request(&[0x3E, 0x00])
            })
            .unwrap_err();
        assert!(e.contains("no adapter connected"), "{e}");
        assert_eq!(opened(&fault), 1, "nothing was opened");
        // Releasing closed the driver: reconnecting opens it afresh.
        a.set_link(Some(LinkCfg {
            driver: "sim".into(),
            port: String::new(),
            baud: 0,
            bus: "hs".into(),
        }));
        assert!(!a.link().connected, "not connected until a poll succeeds");
        tester_present(&d, DiagTarget::ENGINE).unwrap();
        assert_eq!(opened(&fault), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_absent_module_times_out_with_the_operation_budget() {
        let (a, fault, opens) = sim_faulty();
        let d: Arc<dyn AdapterAccess> = a;
        let absent = DiagTarget {
            tx: 0x7E5,
            rx: 0x7ED,
        };
        let e = tester_present(&d, absent).unwrap_err();
        assert!(
            e.contains(&format!("timed out after {READ_TIMEOUT_MS} ms")),
            "{e}"
        );
        // A mutation waits the longer budget.
        let dir = std::env::temp_dir().join(format!("ot-hublink-absent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let backup = crate::host::test_backup(&dir, b"OLD");
        let e = d
            .mutate("w", crate::host::test_ack(absent), &backup, |d| {
                d.request(&[0x3E, 0x00])
            })
            .unwrap_err();
        assert!(
            e.contains(&format!("timed out after {MUTATE_TIMEOUT_MS} ms")),
            "{e}"
        );
        // The present modules still answer afterwards.
        tester_present(&d, DiagTarget::ENGINE).unwrap();
        assert_eq!(*lock(&opens), vec![0x7E5, 0x7E5, 0x7E0]);
        assert_eq!(opened(&fault), 3, "each failure dropped the session");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_target_change_rebinds_the_session_exactly_once() {
        let (a, _fault, opens) = sim_faulty();
        a.poll_link().unwrap();
        a.poll_link().unwrap();
        assert_eq!(*lock(&opens), vec![0x7E0]);
        a.set_target(TCU);
        assert_eq!(a.link().target, Some(TCU));
        a.poll_link().unwrap();
        a.poll_link().unwrap();
        let d: Arc<dyn AdapterAccess> = a.clone();
        tester_present(&d, TCU).unwrap();
        tester_present(&d, TCU).unwrap();
        assert_eq!(*lock(&opens), vec![0x7E0, 0x7E1], "one rebind, then reuse");
        // Setting the same target again changes nothing.
        a.set_target(TCU);
        a.poll_link().unwrap();
        assert_eq!(lock(&opens).len(), 2);
    }

    #[test]
    fn a_panicking_job_keeps_the_hub_usable() {
        let (a, fault, _) = sim_faulty();
        let d: Arc<dyn AdapterAccess> = a.clone();
        tester_present(&d, DiagTarget::ENGINE).unwrap();
        let e = d
            .read("boom", DiagTarget::ENGINE, |_d| -> Result<(), String> {
                panic!("synthetic panic in an adapter job")
            })
            .unwrap_err();
        assert!(e.contains("boom"), "{e}");
        assert_eq!(a.busy(), None, "the busy slot is released");
        // The worker survives; the session was dropped and reopens.
        tester_present(&d, DiagTarget::ENGINE).unwrap();
        assert_eq!(opened(&fault), 2);
        a.poll_link().unwrap();
        assert!(a.link().connected);
    }

    #[test]
    fn the_mutation_marker_does_not_leak_on_the_hub_worker() {
        let (a, _fault, _) = sim_faulty();
        let d: Arc<dyn AdapterAccess> = a;
        let worker = || std::thread::current().name().map(str::to_string);
        let dir = std::env::temp_dir().join(format!("ot-hublink-scope-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let backup = crate::host::test_backup(&dir, b"OLD");
        let (in_mut, thread) = d
            .mutate("w", crate::host::test_ack(TCU), &backup, move |_d| {
                Ok((crate::host::in_mutation(), worker()))
            })
            .unwrap();
        assert!(in_mut);
        assert_eq!(thread.as_deref(), Some("opentune-adapter"));
        let (in_mut, thread2) = d
            .read("r", DiagTarget::ENGINE, move |_d| {
                Ok((crate::host::in_mutation(), worker()))
            })
            .unwrap();
        assert!(!in_mut, "not set during a read");
        assert_eq!(thread2, thread, "the same worker thread");
        // A mutation that panics on the worker: the next job is clean.
        let e = d
            .mutate(
                "boom",
                crate::host::test_ack(TCU),
                &backup,
                |_d| -> Result<(), String> { panic!("synthetic panic in a mutation") },
            )
            .unwrap_err();
        assert!(e.contains("boom"), "{e}");
        let (in_mut, thread3) = d
            .read("r", DiagTarget::ENGINE, move |_d| {
                Ok((crate::host::in_mutation(), worker()))
            })
            .unwrap();
        assert!(!in_mut, "restored after the panic");
        assert_eq!(thread3, thread);
        // The calling thread never was in a mutation.
        assert!(!crate::host::in_mutation());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
