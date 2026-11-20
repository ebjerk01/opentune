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
//!
//! Operation shapes (see [`crate::ops`]): `read` / `mutate` / `actuate` /
//! `scan` / `raw` run as one [`AdapterHub::run`] each; `start` maps onto
//! [`AdapterHub::start_op`] and keeps the busy slot until its job returns.
//! Raw access is refused unless the channel is alpha / bench.

use crate::host::{
    bench_channel, Ack, Actuation, AdapterAccess, BackupReceipt, BuildChannel, Capabilities,
    DiagJob, DiagTarget, LinkInfo, LongJob, ModuleSession, RawBinding, RawInterface, RawJob,
    RestoreJob, ScanJob, Transport,
};
use opentune_core::resilient::LINK_LOST_PREFIX;
use opentune_core::{CoreDiag, RawDiag};
use opentune_hub::adapter::{AdapterHub, Addr, ConnKey, Session};
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
    /// The last probe of the opened adapter (cleared with the link).
    caps: Mutex<Option<Capabilities>>,
    /// Gates the Bench-tier operations (raw access) in this backend.
    channel: BuildChannel,
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
            caps: Mutex::new(None),
            channel: BuildChannel::compiled(),
        }
    }

    /// The same adapter on build channel `channel` (it decides whether the
    /// Bench-tier operations run).
    pub fn with_channel(mut self, channel: BuildChannel) -> Self {
        self.channel = channel;
        self
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
        *lock(&self.caps) = None;
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
        let key = ConnKey::new(
            &cfg.driver,
            Some(&cfg.port),
            baud.as_deref(),
            Some(&cfg.bus),
            Addr {
                tx_id: target.tx,
                rx_id: target.rx,
            },
        );
        // The transport is part of the confirmed target: a cable that cannot
        // carry it is refused here, before anything opens.
        use opentune_adaptation::kwp::{KLINE_DRIVERS, TP20_DRIVERS};
        match target.transport {
            Transport::IsoTp => Ok(key),
            Transport::Tp20 { addr } => {
                if !TP20_DRIVERS.contains(&cfg.driver.as_str()) {
                    return Err(format!(
                        "{target} needs a raw-CAN cable ({}); '{}' cannot carry TP2.0",
                        TP20_DRIVERS.join(", "),
                        cfg.driver
                    ));
                }
                Ok(key
                    .with_option("transport", "tp20")
                    .with_option("tp20_addr", &format!("0x{addr:02X}")))
            }
            Transport::KLine { .. } => {
                if !KLINE_DRIVERS.contains(&cfg.driver.as_str()) {
                    return Err(format!(
                        "{target} needs a K-line cable ({}); '{}' is not one",
                        KLINE_DRIVERS.join(", "),
                        cfg.driver
                    ));
                }
                Ok(key)
            }
        }
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
                let caps = d.capabilities();
                let mv = d.battery_millivolts().map_err(|e| e.to_string())?;
                Ok((ident, mv.map(|m| m as f32 / 1000.0), caps))
            })();
            if r.is_err() {
                s.invalidate();
            }
            r
        });
        let mut info = lock(&self.info);
        match &r {
            Ok((_, v, caps)) => {
                info.connected = true;
                info.battery_v = *v;
                *lock(&self.caps) = Some(*caps);
            }
            Err(e) if e.starts_with("adapter busy") => {}
            Err(_) => {
                info.connected = false;
                info.battery_v = None;
                *lock(&self.caps) = None;
            }
        }
        r.map(|(i, v, _)| (i, v))
    }

    /// Run `f(session, job)` as one hub operation. While the top bar's
    /// battery poll holds the hub, wait for it (briefly) instead of failing
    /// with "adapter busy"; `job` is handed over only once, when `f` runs.
    fn run_on_hub<J, R, F>(&self, op: &str, job: J, f: F) -> Result<R, String>
    where
        J: Send + 'static,
        R: Send + 'static,
        F: Fn(&mut Session, J) -> Result<R, String> + Clone + Send + 'static,
    {
        let slot = Arc::new(Mutex::new(Some(job)));
        let deadline = Instant::now() + POLL_WAIT;
        loop {
            let (sl, f) = (slot.clone(), f.clone());
            let r = self.hub.run(op, move |s| {
                let job = lock(&sl).take().ok_or("internal error: job already ran")?;
                f(s, job)
            });
            match r {
                Err(e) if self.retry_after_poll(&e, || lock(&slot).is_some(), deadline) => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                other => return other,
            }
        }
    }

    /// Is `e` the hub refusing because the battery poll is running, with the
    /// job not handed over yet and time left to wait?
    fn retry_after_poll(&self, e: &str, pending: impl Fn() -> bool, deadline: Instant) -> bool {
        e.starts_with("adapter busy")
            && pending()
            && Instant::now() < deadline
            && self.hub.current().is_none_or(|c| c == LINK_POLL)
    }

    fn run_job(
        &self,
        timeout_ms: u64,
        op: &str,
        target: DiagTarget,
        job: DiagJob,
    ) -> Result<Box<dyn Any + Send>, String> {
        let key = self.key(target)?;
        self.run_on_hub(op, job, move |s, job: DiagJob| {
            s.with_diag(&key, timeout_ms, |d| job(d))
        })
    }
}

/// The link a [`ModuleSession`] hands out: requests go to the bound module
/// through the hub's session (re-opened on a rebind), ECU-changing ones are
/// refused unsent.
struct HubModules<'s> {
    s: &'s mut Session,
    base: ConnKey,
    timeout_ms: u64,
    bound: Option<(DiagTarget, ConnKey)>,
}

impl ModuleSession for HubModules<'_> {
    fn bind(&mut self, target: DiagTarget) -> Result<&mut dyn RawDiag, String> {
        let key = self.base.with_addr(Addr {
            tx_id: target.tx,
            rx_id: target.rx,
        });
        self.bound = None;
        let r = self.s.interface(&key).and_then(|d| {
            d.diag()
                .map(|_| ())
                .ok_or_else(|| format!("driver '{}' has no diagnostic transport", key.driver))
        });
        if let Err(e) = r {
            self.s.invalidate();
            return Err(e);
        }
        self.bound = Some((target, key));
        Ok(self)
    }

    fn bound(&self) -> Option<DiagTarget> {
        self.bound.as_ref().map(|(t, _)| *t)
    }
}

impl RawDiag for HubModules<'_> {
    fn request(&mut self, req: &[u8]) -> Result<Vec<u8>, String> {
        crate::ops::refuse_ecu_changing(req)?;
        let key = match &self.bound {
            Some((_, k)) => k.clone(),
            None => return Err("no module bound: call bind(target) first".into()),
        };
        let drv = self
            .s
            .interface(&key)
            .map_err(|e| format!("{LINK_LOST_PREFIX}{e}"))?;
        let r = match drv.diag() {
            Some(t) => CoreDiag::new(t, self.timeout_ms).request(req),
            None => Err(format!(
                "{LINK_LOST_PREFIX}driver no longer exposes a diagnostic transport"
            )),
        };
        // As `Session::with_diag`: a failed exchange drops the adapter, the
        // next request (or bind) opens it afresh.
        if r.is_err() {
            self.s.invalidate();
        }
        r
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
        // Also for a direct `read_job` call: only READ_SERVICES go out.
        let job: DiagJob =
            Box::new(move |d: &mut dyn RawDiag| job(&mut crate::ops::ReadOnlyDiag(d)));
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
        // SAFETY INVARIANT: a receipt means a real, NON-EMPTY backup. The
        // typed `mutate` wrapper refuses an empty one too; this covers a
        // direct `mutate_job` call.
        crate::host::refuse_empty_receipt(op, backup)?;
        self.run_job(MUTATE_TIMEOUT_MS, op, target, job)
    }

    fn capabilities(&self) -> Option<Capabilities> {
        *lock(&self.caps)
    }

    fn actuate_job(
        &self,
        op: &str,
        ack: Ack,
        run: DiagJob,
        restore: RestoreJob,
    ) -> Result<Actuation<Box<dyn Any + Send>>, String> {
        // The module the user confirmed. No MutationScope: an actuation is
        // not a mutation, so the write helpers refuse inside it.
        let target = ack.ecu_target()?;
        let key = self.key(target)?;
        self.run_on_hub(
            op,
            (run, restore),
            move |s, (run, restore): (DiagJob, RestoreJob)| {
                let a = s.with_diag(&key, MUTATE_TIMEOUT_MS, |d| {
                    Ok(crate::ops::run_then_restore(d, run, restore))
                })?;
                if a.result.is_err() || a.restored.is_err() {
                    s.invalidate();
                }
                Ok(a)
            },
        )
    }

    fn start_job(&self, op: &str, target: DiagTarget, job: LongJob) -> Result<(), String> {
        let key = self.key(target)?;
        let slot = Arc::new(Mutex::new(Some(job)));
        let deadline = Instant::now() + POLL_WAIT;
        loop {
            let (k, sl) = (key.clone(), slot.clone());
            // `start_op` claims the busy slot before it takes the closure, so
            // a refusal leaves the job in `slot` for the next attempt.
            let r = self.hub.start_op(op, move |s| {
                let mut job = lock(&sl).take();
                let r = s.with_diag(&k, READ_TIMEOUT_MS, |d| match job.take() {
                    // Also for a direct `start_job` call: read services only.
                    Some(j) => j.run(&mut crate::ops::ReadOnlyDiag(d)),
                    None => Err("internal error: job already ran".into()),
                });
                if let (Err(e), Some(j)) = (r, job) {
                    j.fail(e); // the adapter did not open: the job never ran
                }
            });
            match r {
                Err(e) if self.retry_after_poll(&e, || lock(&slot).is_some(), deadline) => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                other => return other,
            }
        }
    }

    fn scan_job(&self, op: &str, job: ScanJob) -> Result<Box<dyn Any + Send>, String> {
        // The cable and bus of the link; `bind` sets the module per step.
        let base = self.key(*lock(&self.target))?;
        self.run_on_hub(op, job, move |s, job: ScanJob| {
            let mut m = HubModules {
                s,
                base: base.clone(),
                timeout_ms: READ_TIMEOUT_MS,
                bound: None,
            };
            job(&mut m)
        })
    }

    fn raw_job(
        &self,
        op: &str,
        bind: RawBinding,
        job: RawJob,
    ) -> Result<Box<dyn Any + Send>, String> {
        if !bench_channel(&self.channel) {
            return Err(format!(
                "{op}: raw adapter access is a Bench-tier feature (alpha / bench builds); \
                 refused on the '{}' channel",
                self.channel.as_str()
            ));
        }
        let key = bind
            .options
            .iter()
            .fold(self.key(bind.target)?, |k, (o, v)| k.with_option(o, v));
        self.run_on_hub(op, job, move |s, job: RawJob| {
            let r = match s.interface(&key) {
                Ok(d) => job(&mut RawInterface::guard(d)),
                Err(e) => Err(e),
            };
            // Raw use leaves the driver in an unknown state (baud, an open
            // TP2.0 channel): the next operation opens it afresh.
            s.invalidate();
            r
        })
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

    /// `read` and `start` (typed or direct) only send read services: a write
    /// is refused by the guard before the wire, and the same module still
    /// answers reads afterwards.
    #[test]
    fn transport_targets_bind_with_their_driver_options() {
        let a = HubAdapter::new(AdapterHub::spawn(|| {
            Box::new(|_k: &ConnKey| Err("no hardware in tests".to_string()))
        }));
        let cable = |driver: &str| LinkCfg {
            driver: driver.into(),
            port: "can0".into(),
            baud: 0,
            bus: String::new(),
        };
        a.set_link(Some(cable("socketcan")));
        let k = a.key(DiagTarget::tp20(0x02)).unwrap();
        assert_eq!((k.addr.tx_id, k.addr.rx_id), (0x02, 0x300));
        assert!(k.options.contains(&("transport".into(), "tp20".into())));
        assert!(k.options.contains(&("tp20_addr".into(), "0x02".into())));
        // ISO-TP carries no transport option.
        assert!(a.key(DiagTarget::ENGINE).unwrap().options.is_empty());
        // A cable that cannot carry the confirmed transport is refused here,
        // before anything is opened.
        let e = a.key(DiagTarget::kline(0x01)).unwrap_err();
        assert!(e.contains("K-line") && e.contains("socketcan"), "{e}");
        a.set_link(Some(cable("elm327obd")));
        let e = a.key(DiagTarget::tp20(0x02)).unwrap_err();
        assert!(e.contains("TP2.0") && e.contains("elm327obd"), "{e}");
        a.set_link(Some(cable("kkl")));
        let k = a.key(DiagTarget::kline(0x01)).unwrap();
        assert_eq!((k.addr.tx_id, k.addr.rx_id), (0x01, 0));
    }

    #[test]
    fn read_paths_refuse_ecu_changing_services_unsent() {
        let (_opens, a) = sim();
        let dynamic: Arc<dyn AdapterAccess> = a.clone();
        let refused = |e: String| assert!(e.contains("not a read service"), "{e}");
        // Typed read: a write, a routine, a DTC clear, a download.
        for req in [
            vec![0x2E, 0xF1, 0x90, 0x41],
            vec![0x31, 0x01, 0xFF, 0x00],
            vec![0x14, 0xFF, 0xFF, 0xFF],
            vec![0x34, 0x00, 0x44, 0, 0, 0, 0, 0, 0, 0, 1],
        ] {
            refused(
                dynamic
                    .read("sneaky", DiagTarget::ENGINE, move |d| d.request(&req))
                    .unwrap_err(),
            );
        }
        // A direct read_job (no typed wrapper) is guarded by the adapter.
        let job: DiagJob = Box::new(|d: &mut dyn RawDiag| {
            d.request(&[0x2E, 0xF1, 0x90, 0x41])
                .map(|r| Box::new(r) as Box<dyn Any + Send>)
        });
        match a.read_job("direct", DiagTarget::ENGINE, job) {
            Err(e) => refused(e),
            Ok(_) => panic!("a write went through read_job"),
        }
        // A long operation is read-only too.
        let h = dynamic
            .start("long", DiagTarget::ENGINE, |d, _stop| {
                d.request(&[0x2E, 0xF1, 0x90, 0x41])
            })
            .unwrap();
        refused(h.join().unwrap_err());
        // The long op's busy slot is released just after its result lands.
        let t0 = Instant::now();
        while a.busy().is_some() {
            assert!(t0.elapsed() < Duration::from_secs(5), "slot never freed");
            std::thread::sleep(Duration::from_millis(5));
        }
        // Reads (incl. a session change and security access) still work.
        let r: Vec<u8> = dynamic
            .read("reads", DiagTarget::ENGINE, |d| {
                d.request(&[0x10, 0x03])?;
                d.request(&[0x3E, 0x00])
            })
            .unwrap();
        assert_eq!(r[0], 0x7E);
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
        let tcu = DiagTarget::can(0x7E1, 0x7E9);
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
        let tcu = DiagTarget::can(0x7E1, 0x7E9);
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
                shared: &crate::host::HostShared::default(),
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

    const TCU: DiagTarget = DiagTarget::can(0x7E1, 0x7E9);

    /// Like [`sim`], plus the bus's fault switches (the bus lives on the
    /// worker; its `SimFault` is shared, so it is handed back from there).
    fn sim_faulty() -> (Arc<HubAdapter>, Arc<SimFault>, Arc<Mutex<Vec<u32>>>) {
        sim_on(BuildChannel::compiled())
    }

    /// [`sim_faulty`] on build channel `channel`.
    fn sim_on(channel: BuildChannel) -> (Arc<HubAdapter>, Arc<SimFault>, Arc<Mutex<Vec<u32>>>) {
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
        let a = Arc::new(HubAdapter::new(hub).with_channel(channel));
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
        let absent = DiagTarget::can(0x7E5, 0x7ED);
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

    // -----------------------------------------------------------------------
    // actuate / start / scan / raw
    // -----------------------------------------------------------------------

    type Log = Arc<Mutex<Vec<String>>>;

    fn note(log: &Log, s: &str) {
        lock(log).push(s.to_string());
    }

    #[test]
    fn actuate_hands_back_after_success_error_and_panic_on_the_confirmed_module() {
        let (a, fault, opens) = sim_faulty();
        let d: Arc<dyn AdapterAccess> = a.clone();
        let log: Log = Default::default();
        let (l1, l2) = (log.clone(), log.clone());
        // The action and its hand-back go to the Ack's module (the TCU),
        // not the app's global target (the engine), on one open.
        let act = d
            .actuate(
                "output test",
                crate::host::test_ack(TCU),
                move |dg| {
                    note(&l1, "run");
                    dg.request(&[0x3E, 0x00])
                },
                move |dg| {
                    note(&l2, "restore");
                    dg.request(&[0x3E, 0x00]).map(|_| ())
                },
            )
            .unwrap();
        assert_eq!(act.result.as_deref(), Ok(&[0x7E, 0x00][..]));
        assert!(act.restored.is_ok());
        assert_eq!(*lock(&opens), vec![0x7E1]);
        assert_eq!(opened(&fault), 1, "run and restore share one session");

        // `run` fails: the hand-back still runs.
        let l = log.clone();
        let act = d
            .actuate(
                "x",
                crate::host::test_ack(TCU),
                |_dg| -> Result<(), String> { Err("NRC 0x22".into()) },
                move |_dg| {
                    note(&l, "restore after error");
                    Ok(())
                },
            )
            .unwrap();
        assert!(act.restored.is_ok());
        assert!(act.into_result().unwrap_err().contains("handed back"));

        // `run` panics: the hand-back still runs, the hub stays usable.
        let l = log.clone();
        let act = d
            .actuate(
                "x",
                crate::host::test_ack(TCU),
                |_dg| -> Result<(), String> { panic!("synthetic panic in an action") },
                move |dg| {
                    note(&l, "restore after panic");
                    dg.request(&[0x3E, 0x00]).map(|_| ())
                },
            )
            .unwrap();
        assert!(act.result.unwrap_err().contains("synthetic"));
        assert!(act.restored.is_ok());
        assert_eq!(a.busy(), None);
        assert_eq!(
            *lock(&log),
            [
                "run",
                "restore",
                "restore after error",
                "restore after panic"
            ]
        );

        // A failed hand-back is reported as such.
        let act = d
            .actuate(
                "x",
                crate::host::test_ack(TCU),
                |_dg| Ok(()),
                |_dg| Err("return control refused".into()),
            )
            .unwrap();
        let e = act.into_result().unwrap_err();
        assert!(e.contains("FAILED") && e.contains("actuated state"), "{e}");

        // A file-write Ack (no module) is refused before anything opens.
        let n = opened(&fault);
        let e = d
            .actuate(
                "x",
                crate::host::test_file_ack(),
                |_dg| Ok(()),
                |_dg| Ok(()),
            )
            .unwrap_err();
        assert!(e.contains("confirm_ecu"), "{e}");
        assert_eq!(opened(&fault), n);
    }

    #[test]
    fn actuate_is_not_a_mutation_so_downloads_refuse_inside_it() {
        use crate::transfer::{download, Download, NoProgress, TransferError};
        let (a, _fault, _) = sim_faulty();
        let d: Arc<dyn AdapterAccess> = a;
        // Count what reaches the link through the download.
        let attempt = |dg: &mut dyn RawDiag| {
            let mut sent = 0u32;
            let mut counting = |r: &[u8]| {
                sent += 1;
                dg.request(r)
            };
            let r = download(
                &mut counting,
                &Download::new(0x8000, &[1, 2, 3]),
                &NoProgress,
            );
            (r, sent, crate::host::in_mutation())
        };
        let act = d
            .actuate(
                "x",
                crate::host::test_ack(TCU),
                move |dg| Ok(attempt(dg)),
                move |dg| {
                    let (r, sent, in_mut) = attempt(dg);
                    assert!(matches!(r, Err(TransferError::NotStarted { .. })));
                    assert_eq!((sent, in_mut), (0, false));
                    Ok(())
                },
            )
            .unwrap();
        assert!(act.restored.is_ok());
        let (r, sent, in_mut) = act.result.unwrap();
        let e = r.unwrap_err().to_string();
        assert!(e.contains("adapter.mutate"), "{e}");
        assert_eq!(sent, 0, "nothing was sent");
        assert!(!in_mut, "an actuation is not marked as a mutation");
    }

    #[test]
    fn a_long_operation_holds_the_adapter_until_stopped() {
        let (a, fault, opens) = sim_faulty();
        let d: Arc<dyn AdapterAccess> = a.clone();
        let mut h = d
            .start("live poll", TCU, |dg, stop| {
                let mut polls = 0u32;
                while !stop.is_stopped() {
                    dg.request(&[0x3E, 0x00])?;
                    polls += 1;
                    std::thread::sleep(Duration::from_millis(2));
                }
                Ok(polls)
            })
            .unwrap();
        let t0 = Instant::now();
        while a.busy().is_none() {
            assert!(t0.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(h.is_running());
        // Everything else gets "adapter busy" meanwhile.
        let e = tester_present(&d, DiagTarget::ENGINE).unwrap_err();
        assert!(e.contains("live poll"), "{e}");
        std::thread::sleep(Duration::from_millis(30));
        h.stop();
        let polls = h.join().unwrap();
        assert!(polls > 0);
        let t0 = Instant::now();
        while a.busy().is_some() {
            assert!(t0.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(2));
        }
        tester_present(&d, DiagTarget::ENGINE).unwrap();
        assert_eq!(*lock(&opens), vec![0x7E1, 0x7E0]);

        // A link that will not open reaches the handle as the error.
        a.hub
            .run("close", |s| {
                s.invalidate();
                Ok(())
            })
            .unwrap();
        fault.refuse_opens.store(1, Ordering::SeqCst);
        let h = d.start("live poll", TCU, |_dg, _s| Ok(())).unwrap();
        let e = h.join().unwrap_err();
        assert!(e.contains("no such device"), "{e}");

        // Dropping the handle stops a running operation.
        let h = d
            .start("forgotten", TCU, |_dg, stop| {
                while !stop.is_stopped() {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Ok(())
            })
            .unwrap();
        drop(h);
        let t0 = Instant::now();
        while a.busy().is_some() {
            assert!(t0.elapsed() < Duration::from_secs(5), "not stopped");
            std::thread::sleep(Duration::from_millis(2));
        }
        // No cable: refused before anything is queued.
        a.set_link(None);
        let e = d.start("x", TCU, |_dg, _s| Ok(())).err().unwrap();
        assert!(e.contains("no adapter connected"), "{e}");
    }

    #[test]
    fn a_scan_rebinds_per_module_and_stays_read_only() {
        let (a, fault, opens) = sim_faulty();
        let d: Arc<dyn AdapterAccess> = a.clone();
        let absent = DiagTarget::can(0x7E5, 0x7ED);
        let found = d
            .scan("auto-scan", move |m| {
                assert!(m.bound().is_none());
                let mut found = Vec::new();
                for t in [DiagTarget::ENGINE, absent, TCU] {
                    let link = m.bind(t)?;
                    if link.request(&[0x3E, 0x00]).is_ok() {
                        found.push(t);
                    }
                    assert_eq!(m.bound(), Some(t));
                }
                // Read-only: a write is refused unsent (the session stays).
                let e = m.bind(TCU)?.request(&[0x2E, 0xF1, 0x90, 0x00]).unwrap_err();
                assert!(e.contains("read-only"), "{e}");
                let e = m.bind(TCU)?.request(&[0x14, 0xFF, 0xFF, 0xFF]).unwrap_err();
                assert!(e.contains("read-only"), "{e}");
                Ok(found)
            })
            .unwrap();
        assert_eq!(found, vec![DiagTarget::ENGINE, TCU]);
        // One open per module; the absent one timed out and was dropped.
        assert_eq!(*lock(&opens), vec![0x7E0, 0x7E5, 0x7E1]);
        assert_eq!(opened(&fault), 3, "the refused writes reopened nothing");
        assert_eq!(a.busy(), None);
    }

    #[test]
    fn raw_access_is_bench_only_and_refuses_flash_writes() {
        // Stable: refused in the backend, nothing opened.
        let (a, fault, _) = sim_on(BuildChannel::new(BuildChannel::STABLE));
        let d: Arc<dyn AdapterAccess> = a;
        let e = d
            .raw("can monitor", RawBinding::new(TCU), |_i| Ok(()))
            .unwrap_err();
        assert!(e.contains("Bench-tier") && e.contains("stable"), "{e}");
        assert_eq!(opened(&fault), 0);

        // Bench: the job gets the guarded driver.
        let (a, fault, opens) = sim_on(BuildChannel::new(BuildChannel::BENCH));
        let d: Arc<dyn AdapterAccess> = a;
        let bind = RawBinding::new(TCU).with_option("transport", "tp20");
        let (id, reply, write, diag_write, reopen, caps) = d
            .raw("raw", bind, |i| {
                use opentune_core::{Interface, MemoryRegion};
                let id = i.info().id;
                let t = i.diag().ok_or("no diag")?;
                let reply = t.request(&[0x3E, 0x00], 500).map_err(|e| e.to_string())?;
                let diag_write = t
                    .request(&[0x34, 0x00, 0x44, 0, 0, 0, 0, 0, 0, 0, 16], 500)
                    .unwrap_err()
                    .to_string();
                let write = i
                    .write_memory(MemoryRegion::new(0, 4), &[0; 4], &mut |_| {})
                    .unwrap_err()
                    .to_string();
                let reopen = i.open(&Default::default()).is_err() && i.close().is_err();
                Ok((id, reply, write, diag_write, reopen, i.capabilities()))
            })
            .unwrap();
        assert_eq!(id, "sim");
        assert_eq!(reply, vec![0x7E, 0x00]);
        assert!(write.contains("write_memory is refused"), "{write}");
        assert!(diag_write.contains("read-only"), "{diag_write}");
        assert!(reopen, "open / close are the hub's");
        assert!(!caps.write_memory);
        assert_eq!(*lock(&opens), vec![0x7E1]);
        // The raw use dropped the session: the next operation reopens.
        tester_present(&d, TCU).unwrap();
        assert_eq!(opened(&fault), 2);
    }

    // -----------------------------------------------------------------------
    // Invariant: a BackupReceipt means a real, NON-EMPTY backup was written
    // -----------------------------------------------------------------------

    #[test]
    fn mutate_job_refuses_a_zero_length_receipt_and_actuate_needs_none() {
        let (a, fault, opens) = sim_faulty();
        let dir = std::env::temp_dir().join(format!("ot-hublink-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut empty = crate::host::test_backup(&dir, b"OLD");
        empty.len = 0; // the file exists; the receipt claims 0 bytes
                       // Straight at the trait method (bypassing the typed wrapper).
        let job: DiagJob = Box::new(|d: &mut dyn RawDiag| {
            d.request(&[0x2E, 0xF1, 0x90, 0x00])
                .map(|r| Box::new(r) as Box<dyn Any + Send>)
        });
        let e = a
            .mutate_job("write", crate::host::test_ack(TCU), &empty, job)
            .unwrap_err();
        assert!(e.contains("empty (0 bytes)"), "{e}");
        assert!(
            lock(&opens).is_empty() && opened(&fault) == 0,
            "nothing sent"
        );
        // actuate takes no receipt at all.
        let d: Arc<dyn AdapterAccess> = a;
        let act = d
            .actuate(
                "output test",
                crate::host::test_ack(TCU),
                |dg| dg.request(&[0x3E, 0x00]),
                |_dg| Ok(()),
            )
            .unwrap();
        assert!(act.into_result().is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A representative PERSISTENT write, end to end through the host's
    /// seams: confirm_ecu (accepted) -> read the old value from the module
    /// -> Host::backup -> adapter.mutate(ack, &backup, write) over the hub's
    /// simulated bus. The old bytes are in the vault, the write reached the
    /// confirmed module only, and the backup is still there afterwards.
    #[test]
    fn a_persistent_write_end_to_end_confirm_backup_mutate() {
        const DID: u16 = 0xF1A0;
        let hub = AdapterHub::spawn(|| {
            let bus = SimBus::default();
            let mut engine = UdsServer::new();
            engine.set_did(DID, b"ENGINE-CODING".to_vec());
            let mut tcu = UdsServer::new();
            tcu.set_did(DID, b"OLD-CODING".to_vec());
            bus.add(0x7E0, 0x7E8, engine);
            bus.add(0x7E1, 0x7E9, tcu);
            bus.opener()
        });
        let a = Arc::new(HubAdapter::new(hub));
        a.set_link(Some(LinkCfg {
            driver: "sim".into(),
            port: String::new(),
            baud: 0,
            bus: "hs".into(),
        }));
        let d: Arc<dyn AdapterAccess> = a.clone();
        let read_did = |t: DiagTarget| {
            d.read("read coding", t, |dg| {
                let r = dg.request(&[0x22, (DID >> 8) as u8, DID as u8])?;
                match r.first() {
                    Some(0x62) => Ok(r[3..].to_vec()),
                    _ => Err(format!("negative reply {r:02X?}")),
                }
            })
        };
        let dir = std::env::temp_dir().join(format!("ot-hublink-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // The app's global target is the engine; the user works on the TCU.
        a.set_target(DiagTarget::ENGINE);
        let jobs = crate::host::Jobs::new(eframe::egui::Context::default());
        let mut confirms = crate::host::Confirms::default();
        let (mut actions, mut status) = (Vec::new(), None);
        let shared = crate::host::HostShared::default();
        let keys = opentune_hub::seedkeys::SeedKeys::default();
        let req = || ConfirmRequest {
            key: "write-coding".into(),
            title: "Write coding?".into(),
            body: "b".into(),
            typed: None,
            confirm_label: "Write".into(),
        };
        let mut frame = |confirms: &mut crate::host::Confirms| {
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
                seedkeys: &keys,
                target: Ok(TCU),
                jobs: &jobs,
                confirms,
                actions: &mut actions,
                status: &mut status,
                shared: &shared,
            };
            match h.confirm_ecu(req()) {
                crate::host::ConfirmResult::Accepted(ack) => {
                    // 1. read the value the write will replace, 2. back it up.
                    let old = read_did(ack.ecu_target().unwrap()).unwrap();
                    let backup = h.backup(&dir, "coding before write", &old).unwrap();
                    Some((ack, backup, old))
                }
                _ => None,
            }
        };
        assert!(frame(&mut confirms).is_none(), "the dialog opens");
        confirms.answer("write-coding", true);
        let (ack, backup, old) = frame(&mut confirms).expect("accepted");
        assert_eq!(old, b"OLD-CODING");
        assert_eq!(std::fs::read(&backup.path).unwrap(), b"OLD-CODING");
        assert!(backup.path.starts_with(dir.join("opentune-vault")));

        // 3. the write, on the module the user confirmed.
        let reply: Vec<u8> = d
            .mutate("write coding", ack, &backup, |dg| {
                let mut req = vec![0x2E, (DID >> 8) as u8, DID as u8];
                req.extend_from_slice(b"NEW-CODING");
                assert!(crate::host::in_mutation());
                dg.request(&req)
            })
            .unwrap();
        assert_eq!(reply, vec![0x6E, 0xF1, 0xA0]);
        assert_eq!(read_did(TCU).unwrap(), b"NEW-CODING");
        assert_eq!(
            read_did(DiagTarget::ENGINE).unwrap(),
            b"ENGINE-CODING",
            "the engine (the global target) was not written"
        );
        assert!(backup.path.is_file(), "the backup is kept");
        assert_eq!(std::fs::read(&backup.path).unwrap(), b"OLD-CODING");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_link_poll_records_the_adapters_capabilities() {
        let (a, _fault, _) = sim_faulty();
        assert_eq!(a.capabilities(), None);
        a.poll_link().unwrap();
        assert_eq!(a.capabilities(), Some(Capabilities::default()));
        a.set_link(None);
        assert_eq!(a.capabilities(), None);
    }
}
