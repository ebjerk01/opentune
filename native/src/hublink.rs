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
/// Per-request diagnostic timeout.
const TIMEOUT_MS: u64 = 2500;

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
                s.with_diag(&k, TIMEOUT_MS, |d| job(d))
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
        self.run_job(op, target, job)
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
        self.run_job(op, target, job)
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
}
