//! Test kit of the N5 panels: SYNTHETIC modules behind the real
//! [`HubAdapter`] (the hub worker, its session and its read-only guards), with
//! every request logged by the module it was addressed to. The module state
//! lives behind a mutex, so a test inspects it directly (a written DID, a
//! cleared DTC store, a KWP simulator's state).
//!
//! Modules: an `opentune-udssim` [`UdsServer`], the adaptation crate's
//! synthetic KWP2000 module ([`SimKwpModule`]) or a scripted closure. No real
//! hardware, no real vehicle data.

use crate::host::{
    AdapterAccess, ConfirmRequest, ConfirmResult, Confirms, DiagTarget, Host, HostShared, Jobs,
};
use crate::hublink::{HubAdapter, LinkCfg};
use opentune_adaptation::sim::SimKwpModule;
use opentune_core::protocol::DiagTransport;
use opentune_core::{
    Capabilities, ConnectConfig, EcuId, Error, Interface, InterfaceInfo, MemoryRegion, Progress,
};
use opentune_hub::adapter::{AdapterHub, ConnKey};
use opentune_udssim::server::UdsServer;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// A scripted module: request → raw response (or a transport error).
pub(crate) type Script = Box<dyn FnMut(&[u8]) -> Result<Vec<u8>, String> + Send>;

pub(crate) enum Ecu {
    Uds(Box<UdsServer>),
    Kwp(Box<SimKwpModule>),
    Script(Script),
}

#[derive(Default)]
pub(crate) struct BusState {
    pub ecus: BTreeMap<(u32, u32), Ecu>,
    /// `(tx id the request was addressed to, request)`, in order.
    pub wire: Vec<(u32, Vec<u8>)>,
    /// The tx id of every driver open.
    pub opens: Vec<u32>,
}

/// The synthetic vehicle (cheap to clone; clones share it).
#[derive(Clone, Default)]
pub(crate) struct Bus {
    state: Arc<Mutex<BusState>>,
    /// Every request fails (an unplugged adapter) while set.
    pub dead: Arc<AtomicBool>,
}

impl Bus {
    pub(crate) fn lock(&self) -> MutexGuard<'_, BusState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn uds(self, tx: u32, rx: u32, s: UdsServer) -> Self {
        self.lock().ecus.insert((tx, rx), Ecu::Uds(Box::new(s)));
        self
    }

    pub(crate) fn kwp(self, tx: u32, rx: u32, m: SimKwpModule) -> Self {
        self.lock().ecus.insert((tx, rx), Ecu::Kwp(Box::new(m)));
        self
    }

    pub(crate) fn script(self, tx: u32, rx: u32, f: Script) -> Self {
        self.lock().ecus.insert((tx, rx), Ecu::Script(f));
        self
    }

    /// Run `f` on the UDS module at `(tx, rx)`.
    pub(crate) fn with_uds<R>(&self, tx: u32, rx: u32, f: impl FnOnce(&mut UdsServer) -> R) -> R {
        match self.lock().ecus.get_mut(&(tx, rx)) {
            Some(Ecu::Uds(s)) => f(s),
            _ => panic!("no UDS module at {tx:X}/{rx:X}"),
        }
    }

    /// Run `f` on the KWP module at `(tx, rx)`.
    pub(crate) fn with_kwp<R>(
        &self,
        tx: u32,
        rx: u32,
        f: impl FnOnce(&mut SimKwpModule) -> R,
    ) -> R {
        match self.lock().ecus.get_mut(&(tx, rx)) {
            Some(Ecu::Kwp(m)) => f(m),
            _ => panic!("no KWP module at {tx:X}/{rx:X}"),
        }
    }

    /// Every request, in order.
    pub(crate) fn wire(&self) -> Vec<(u32, Vec<u8>)> {
        self.lock().wire.clone()
    }

    /// Requests addressed to `tx`, in order.
    pub(crate) fn sent_to(&self, tx: u32) -> Vec<Vec<u8>> {
        self.lock()
            .wire
            .iter()
            .filter(|(t, _)| *t == tx)
            .map(|(_, r)| r.clone())
            .collect()
    }

    /// Requests with service id `sid`, as `(tx, request)`.
    pub(crate) fn with_sid(&self, sid: u8) -> Vec<(u32, Vec<u8>)> {
        self.lock()
            .wire
            .iter()
            .filter(|(_, r)| r.first() == Some(&sid))
            .cloned()
            .collect()
    }

    pub(crate) fn clear_wire(&self) {
        self.lock().wire.clear();
    }

    /// The real hub adapter on this bus, with cable `driver` connected.
    pub(crate) fn adapter(&self, driver: &str) -> Arc<HubAdapter> {
        let bus = self.clone();
        let hub = AdapterHub::spawn(move || {
            Box::new(move |k: &ConnKey| {
                let cfg = k.config()?;
                bus.lock().opens.push(cfg.tx_id);
                Ok(Box::new(Tap {
                    bus: bus.clone(),
                    tx: cfg.tx_id,
                    rx: cfg.rx_id,
                }) as Box<dyn Interface>)
            })
        });
        let a = Arc::new(HubAdapter::new(hub));
        a.set_link(Some(LinkCfg {
            driver: driver.into(),
            port: "sim0".into(),
            baud: 0,
            bus: String::new(),
        }));
        a
    }

    /// [`adapter`](Self::adapter) as the views see it.
    pub(crate) fn access(&self, driver: &str) -> Arc<dyn AdapterAccess> {
        self.adapter(driver)
    }
}

/// One opened "driver": routes requests to the module at `(tx, rx)`.
struct Tap {
    bus: Bus,
    tx: u32,
    rx: u32,
}

impl DiagTransport for Tap {
    fn request(&mut self, req: &[u8], timeout_ms: u64) -> opentune_core::Result<Vec<u8>> {
        if self.bus.dead.load(Ordering::SeqCst) {
            return Err(Error::Io(std::io::Error::other("sim: unplugged")));
        }
        let mut st = self.bus.lock();
        st.wire.push((self.tx, req.to_vec()));
        match st.ecus.get_mut(&(self.tx, self.rx)) {
            Some(Ecu::Uds(s)) => Ok(s.handle(req)),
            Some(Ecu::Kwp(m)) => Ok(m.answer(req)),
            Some(Ecu::Script(f)) => f(req).map_err(Error::Protocol),
            None => Err(Error::Timeout(timeout_ms)),
        }
    }
}

impl Interface for Tap {
    fn info(&self) -> InterfaceInfo {
        InterfaceInfo {
            id: "sim".into(),
            display_name: "Synthetic bus (tests)".into(),
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

// ---------------------------------------------------------------------------
// Host fixture
// ---------------------------------------------------------------------------

/// What a `Host` borrows, owned by a test.
pub(crate) struct Fixture {
    pub jobs: Jobs,
    pub confirms: Confirms,
    pub actions: Vec<crate::host::HostAction>,
    pub status: Option<String>,
    pub keys: opentune_hub::seedkeys::SeedKeys,
    pub shared: HostShared,
    pub adapter: Option<Arc<dyn AdapterAccess>>,
    pub dir: PathBuf,
}

impl Fixture {
    pub(crate) fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "ot-n5-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self {
            jobs: Jobs::new(eframe::egui::Context::default()),
            confirms: Confirms::default(),
            actions: Vec::new(),
            status: None,
            keys: Default::default(),
            shared: HostShared::new(
                crate::host::BuildChannel::new(crate::host::BuildChannel::BENCH),
                None,
            ),
            adapter: None,
            dir,
        }
    }

    /// Run `f` against a host whose app target is `target`.
    pub(crate) fn host<R>(&mut self, target: DiagTarget, f: impl FnOnce(&mut Host<'_>) -> R) -> R {
        let mut h = Host {
            tokens: crate::theme::DARK,
            image_path: None,
            image: None,
            image_edited: false,
            defs: &[],
            def_idx: None,
            map_idx: 0,
            checksums: &[],
            adapter: self.adapter.as_ref(),
            diagnostics: None,
            seedkeys: &self.keys,
            target: Ok(target),
            jobs: &self.jobs,
            confirms: &mut self.confirms,
            actions: &mut self.actions,
            status: &mut self.status,
            shared: &self.shared,
        };
        f(&mut h)
    }

    /// An ECU confirmation the user accepted, for the app target `target`.
    pub(crate) fn ecu_ack(&mut self, target: DiagTarget) -> crate::host::Ack {
        let req = ConfirmRequest {
            key: "test-ecu".into(),
            title: "t".into(),
            body: "b".into(),
            typed: None,
            confirm_label: "ok".into(),
        };
        let r = self.host(target, |h| h.confirm_ecu(req.clone()));
        assert!(matches!(r, ConfirmResult::Pending));
        self.confirms.answer("test-ecu", true);
        match self.host(target, |h| h.confirm_ecu(req)) {
            ConfirmResult::Accepted(a) => a,
            other => panic!("expected Accepted, got {other:?}"),
        }
    }

    /// A vault backup of `data` in the fixture's folder.
    pub(crate) fn backup(&mut self, data: &[u8]) -> crate::host::BackupReceipt {
        let dir = self.dir.clone();
        self.host(DiagTarget::ENGINE, |h| h.backup(&dir, "test", data))
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A test-only seed/key resolver: id `xor-5a`, key = seed XOR 0x5A (the
/// udssim `XorSecurity::new(0x5A)` relationship). Not any real algorithm.
pub(crate) fn xor_keys() -> opentune_hub::seedkeys::SeedKeys {
    struct Xor;
    impl opentune_core::protocol::seedkey::SeedKey for Xor {
        fn id(&self) -> &str {
            "xor-5a"
        }
        fn compute(&self, seed: &[u8], _level: u8) -> opentune_core::Result<Vec<u8>> {
            Ok(seed.iter().map(|b| b ^ 0x5A).collect())
        }
    }
    fn resolver(id: &str) -> Option<Box<dyn opentune_core::protocol::seedkey::SeedKey>> {
        (id == "xor-5a")
            .then(|| Box::new(Xor) as Box<dyn opentune_core::protocol::seedkey::SeedKey>)
    }
    opentune_hub::seedkeys::SeedKeys::new(vec![resolver])
}
