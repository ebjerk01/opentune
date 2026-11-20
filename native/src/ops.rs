//! Adapter operation shapes beyond `read` / `mutate` (see
//! [`AdapterAccess`](crate::host::AdapterAccess)):
//!
//! * **actuate**: a NON-persistent ECU action (an output test, a mode toggle)
//!   with an automatic hand-back: `restore` always runs after `run`, on the
//!   same session, even when `run` fails or panics ([`run_then_restore`]).
//! * **start**: a long-running operation (live polling, a capture) that
//!   holds the adapter until its job returns; the caller keeps an
//!   [`OpHandle`] with `stop()` and `poll()` / `join()`.
//! * **scan**: one READ-ONLY operation that re-binds to several modules in
//!   turn ([`ModuleSession::bind`]), for auto-scan and inspection.
//! * **raw**: Bench-tier access to the driver itself ([`RawInterface`]) for
//!   CAN monitoring, TP2.0 and the SSM wire. The wrapper refuses every
//!   flash-writing method, so raw access cannot bypass `mutate`.
//!
//! Every read-only path (`read`, `start`, `scan`, and the raw interface's
//! diagnostic transport) admits only the services in [`READ_SERVICES`]
//! ([`ReadOnlyDiag`] / [`refuse_ecu_changing`]); anything else is refused
//! before a byte is sent. An allow-list, so an unknown service is refused
//! too. A guard rail on the service id, not a protocol parser.

use opentune_core::protocol::DiagTransport;
use opentune_core::{
    Capabilities, ConnectConfig, EcuId, Interface, InterfaceInfo, MemoryRegion, Progress, RawDiag,
};
use std::any::Any;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

/// What the default bodies of the newer [`AdapterAccess`](crate::host::AdapterAccess)
/// methods return: an adapter written before them simply does not offer the
/// operation.
pub const UNSUPPORTED: &str = "unsupported by this adapter";

// ---------------------------------------------------------------------------
// Read-only guard
// ---------------------------------------------------------------------------

/// The services a READ-ONLY path admits (an allow-list: everything else is
/// refused unsent, so no ECU change is possible without a confirmation).
///
/// * UDS / KWP2000: session control `10` (any session: it changes no data,
///   and every write service stays refused in it), security access `27`,
///   read by identifier `22`, read memory by address `23`, read DTCs `19`,
///   tester present `3E`, and the upload trio `35` requestUpload / `36`
///   transferData / `37` requestTransferExit (`34` requestDownload is
///   refused, so `36` can never carry a download).
/// * KWP2000 reads: `1A` read ECU identification, `21` read data by local
///   identifier, `17` / `18` read DTCs (status / by status), `12` read
///   freeze frame.
/// * OBD-II read modes: `01` `02` `03` `06` `07` `09` `0A` (never `04`
///   clear DTCs or `08` control of on-board systems).
/// * SSM (Subaru): `A0` read memory block, `A8` read address, `BF` ECU init
///   / identification.
///
/// Refused, for example: `2E` `3B` `3D` writes, `34` download, `31` / KWP `32`
/// routines, `14` / OBD `04` clear DTCs, `2F` / KWP `30` I/O control, `11`
/// reset, `28` communication control, `85` DTC setting, `2C` dynamic DID
/// definition, SSM `B0` / `B8` writes.
pub const READ_SERVICES: &[u8] = &[
    // UDS / KWP2000
    0x10, 0x27, 0x22, 0x23, 0x19, 0x3E, 0x35, 0x36, 0x37, // KWP2000 reads
    0x1A, 0x21, 0x17, 0x18, 0x12, // OBD-II read modes
    0x01, 0x02, 0x03, 0x06, 0x07, 0x09, 0x0A, // SSM reads
    0xA0, 0xA8, 0xBF,
];

/// `Err` (nothing to send) unless `req` is a read-path service; see
/// [`READ_SERVICES`]. Public so an embedder's own adapter can apply the same
/// rule on its read-only paths.
pub fn refuse_ecu_changing(req: &[u8]) -> Result<(), String> {
    match req {
        [] => Err("empty request".into()),
        [sid, ..] if READ_SERVICES.contains(sid) => Ok(()),
        [sid, ..] => Err(format!(
            "read-only operation: service 0x{sid:02X} is not a read service and was refused; \
             nothing was sent (ECU changes go through confirm_ecu + mutate or actuate)"
        )),
    }
}

/// A [`RawDiag`] that admits only [`READ_SERVICES`]: what every read-only
/// adapter path hands its job.
pub struct ReadOnlyDiag<'a>(pub &'a mut dyn RawDiag);

impl RawDiag for ReadOnlyDiag<'_> {
    fn request(&mut self, req: &[u8]) -> Result<Vec<u8>, String> {
        refuse_ecu_changing(req)?;
        self.0.request(req)
    }
}

// ---------------------------------------------------------------------------
// actuate
// ---------------------------------------------------------------------------

/// The hand-back half of an actuation: puts the module back in normal
/// control (return control to ECU, stop the routine, leave the mode).
pub type RestoreJob = Box<dyn FnOnce(&mut dyn RawDiag) -> Result<(), String> + Send + 'static>;

/// The outcome of [`actuate`](crate::host::AdapterAccess::actuate_job):
/// what the action returned and whether control was handed back. Both
/// halves always ran; check `restored` before telling the user the module is
/// back to normal.
#[derive(Debug)]
pub struct Actuation<R> {
    /// What `run` returned (an `Err` too when it panicked).
    pub result: Result<R, String>,
    /// What `restore` returned (an `Err` too when it panicked).
    pub restored: Result<(), String>,
}

impl<R> Actuation<R> {
    /// The result, or an error that says plainly when control may NOT have
    /// been handed back.
    pub fn into_result(self) -> Result<R, String> {
        match (self.result, self.restored) {
            (Ok(v), Ok(())) => Ok(v),
            (Err(e), Ok(())) => Err(format!("{e} (control was handed back)")),
            (Ok(_), Err(r)) => Err(format!(
                "the action ran, but handing control back FAILED: {r}; the module may still be \
                 in the actuated state"
            )),
            (Err(e), Err(r)) => Err(format!(
                "{e}; handing control back also FAILED: {r}; the module may still be in the \
                 actuated state"
            )),
        }
    }

    pub(crate) fn map<T>(self, f: impl FnOnce(R) -> Result<T, String>) -> Actuation<T> {
        Actuation {
            result: self.result.and_then(f),
            restored: self.restored,
        }
    }
}

fn panic_text(p: &(dyn Any + Send)) -> String {
    p.downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panic".into())
}

/// Run `run`, then `restore`, on the same link. `restore` runs whatever
/// `run` did (returned, failed or panicked), and its own panic is caught too.
/// Every [`AdapterAccess::actuate_job`](crate::host::AdapterAccess::actuate_job)
/// implementation runs its two halves through this.
pub fn run_then_restore<R>(
    d: &mut dyn RawDiag,
    run: impl FnOnce(&mut dyn RawDiag) -> Result<R, String>,
    restore: RestoreJob,
) -> Actuation<R> {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&mut *d)))
        .unwrap_or_else(|p| Err(format!("the action panicked: {}", panic_text(&*p))));
    let restored = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| restore(&mut *d)))
        .unwrap_or_else(|p| Err(format!("the hand-back panicked: {}", panic_text(&*p))));
    Actuation { result, restored }
}

// ---------------------------------------------------------------------------
// start: long-running operations
// ---------------------------------------------------------------------------

/// Asks a long-running operation to end at its next safe point. Clones share
/// the flag.
#[derive(Clone, Debug, Default)]
pub struct StopFlag(Arc<AtomicBool>);

impl StopFlag {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn stop(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_stopped(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

type AnyResult = Result<Box<dyn Any + Send>, String>;
type LongFn = Box<dyn FnOnce(&mut dyn RawDiag, &StopFlag) -> AnyResult + Send + 'static>;

/// The job of a long-running operation, as an adapter receives it in
/// [`AdapterAccess::start_job`](crate::host::AdapterAccess::start_job). The
/// adapter calls exactly one of [`run`](Self::run) (with the opened link) or
/// [`fail`](Self::fail) (the link could not be opened); either delivers the
/// outcome to the caller's [`OpHandle`].
pub struct LongJob {
    f: LongFn,
    stop: StopFlag,
    reply: mpsc::Sender<AnyResult>,
}

impl LongJob {
    pub(crate) fn new<R, F>(f: F) -> (LongJob, OpHandle<R>)
    where
        R: Send + 'static,
        F: FnOnce(&mut dyn RawDiag, &StopFlag) -> Result<R, String> + Send + 'static,
    {
        let stop = StopFlag::new();
        let (tx, rx) = mpsc::channel();
        let job = LongJob {
            f: Box::new(move |d: &mut dyn RawDiag, s: &StopFlag| {
                f(d, s).map(|r| Box::new(r) as Box<dyn Any + Send>)
            }),
            stop: stop.clone(),
            reply: tx,
        };
        let handle = OpHandle {
            stop,
            rx,
            result: None,
            _t: std::marker::PhantomData,
        };
        (job, handle)
    }

    /// The flag the caller's [`OpHandle::stop`] sets.
    pub fn stop_flag(&self) -> &StopFlag {
        &self.stop
    }

    /// Run the job on `d` until it returns, and deliver its result. Returns
    /// the job's error too (a copy), so the adapter can drop a session that
    /// failed. A panic in the job is caught and reported as an error.
    pub fn run(self, d: &mut dyn RawDiag) -> Result<(), String> {
        let LongJob { f, stop, reply } = self;
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(d, &stop)))
            .unwrap_or_else(|p| Err(format!("the operation panicked: {}", panic_text(&*p))));
        let out = r.as_ref().map(|_| ()).map_err(Clone::clone);
        let _ = reply.send(r);
        out
    }

    /// The operation could not start (e.g. the adapter would not open):
    /// deliver `why` to the caller.
    pub fn fail(self, why: String) {
        let _ = self.reply.send(Err(why));
    }
}

/// The caller's side of a long-running operation.
///
/// Dropping the handle stops the operation (sets its [`StopFlag`]), so a
/// view that forgets one cannot keep the adapter busy forever.
pub struct OpHandle<R> {
    stop: StopFlag,
    rx: mpsc::Receiver<AnyResult>,
    result: Option<Result<R, String>>,
    _t: std::marker::PhantomData<fn() -> R>,
}

impl<R: 'static> OpHandle<R> {
    /// Ask the operation to end at its next safe point.
    pub fn stop(&self) {
        self.stop.stop();
    }

    /// The operation's stop flag (e.g. to share with a UI toggle).
    pub fn stop_flag(&self) -> StopFlag {
        self.stop.clone()
    }

    fn land(&mut self, r: Result<AnyResult, mpsc::RecvError>) {
        self.result = Some(match r {
            Ok(r) => crate::host::unbox(r),
            Err(_) => Err("the operation ended without a result".into()),
        });
    }

    /// The result once the operation has ended (never blocks).
    pub fn poll(&mut self) -> Option<&Result<R, String>> {
        if self.result.is_none() {
            match self.rx.try_recv() {
                Ok(r) => self.land(Ok(r)),
                Err(mpsc::TryRecvError::Disconnected) => self.land(Err(mpsc::RecvError)),
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        self.result.as_ref()
    }

    pub fn is_running(&mut self) -> bool {
        self.poll().is_none()
    }

    /// Take the result out once the operation has ended.
    pub fn take(&mut self) -> Option<Result<R, String>> {
        self.poll();
        self.result.take()
    }

    /// Block until the operation ends and return its result. For background
    /// jobs and tests: never call it from `ui()`.
    pub fn join(mut self) -> Result<R, String> {
        if self.result.is_none() {
            let r = self.rx.recv();
            self.land(r);
        }
        self.result
            .take()
            .unwrap_or_else(|| Err("the result was already taken".into()))
    }
}

impl<R> Drop for OpHandle<R> {
    fn drop(&mut self) {
        self.stop.stop();
    }
}

// ---------------------------------------------------------------------------
// scan: per-module rebinding inside one read-only operation
// ---------------------------------------------------------------------------

use crate::host::DiagTarget;

/// One READ-ONLY operation over several modules (auto-scan, inspection).
/// The operation holds the adapter throughout; [`bind`](Self::bind) points
/// the link at another module (re-opening the adapter for it when needed).
///
/// Read-only by contract: requests are checked with [`refuse_ecu_changing`]
/// and an ECU-changing one is refused unsent. Clearing DTCs or any other
/// change runs as its own confirmed `mutate` / `actuate` per module.
pub trait ModuleSession {
    /// Bind to module `target` and return its diagnostic link. An error means
    /// the adapter could not be bound to it (nothing was sent); the scan may
    /// go on with the next module.
    fn bind(&mut self, target: DiagTarget) -> Result<&mut dyn RawDiag, String>;
    /// The module currently bound, if any.
    fn bound(&self) -> Option<DiagTarget>;
}

// ---------------------------------------------------------------------------
// raw: bench-only interface access
// ---------------------------------------------------------------------------

/// How a raw operation binds the adapter: the module ids plus extra driver
/// options (e.g. `transport=tp20`, `tp20_addr=0x0A`, or `init=none` for a
/// raw K-line).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawBinding {
    pub target: DiagTarget,
    pub options: Vec<(String, String)>,
}

impl RawBinding {
    pub fn new(target: DiagTarget) -> Self {
        Self {
            target,
            options: Vec::new(),
        }
    }

    /// The same binding with an extra driver option (replacing one of the
    /// same key).
    pub fn with_option(mut self, key: &str, value: &str) -> Self {
        self.options.retain(|(k, _)| k != key);
        self.options.push((key.into(), value.into()));
        self
    }
}

/// The driver, as a raw operation sees it. Low level and **Bench tier**:
/// the CAN monitor and the raw K-line pass through unfiltered (frames and
/// bytes are not inspected), so a panel using them is a Bench panel.
///
/// What it refuses, so raw access can never bypass the `mutate` guard
/// (confirm + backup):
/// * [`write_memory`](Interface::write_memory) (block flash writes);
/// * [`set_seed_key`](Interface::set_seed_key) (the unlock for block reads
///   and writes);
/// * [`open`](Interface::open) / [`close`](Interface::close) (the hub owns
///   the binding; it re-opens the driver after every raw operation);
/// * through [`diag`](Interface::diag), every ECU-changing request
///   ([`refuse_ecu_changing`]).
///
/// [`capabilities`](Interface::capabilities) reports `write_memory` and
/// `programming_voltage` as false accordingly.
pub struct RawInterface<'a> {
    inner: &'a mut dyn Interface,
}

impl<'a> RawInterface<'a> {
    /// Wrap an opened driver. An adapter implementation hands its raw jobs
    /// only this wrapper, never the driver itself.
    pub fn guard(inner: &'a mut dyn Interface) -> Self {
        Self { inner }
    }
}

fn refused(what: &str) -> opentune_core::Error {
    opentune_core::Error::Safety(format!(
        "raw interface access is read-only: {what} is refused (ECU writes go through a \
         confirmed adapter.mutate)"
    ))
}

impl DiagTransport for RawInterface<'_> {
    fn request(&mut self, req: &[u8], timeout_ms: u64) -> opentune_core::Result<Vec<u8>> {
        refuse_ecu_changing(req).map_err(opentune_core::Error::Safety)?;
        match self.inner.diag() {
            Some(t) => t.request(req, timeout_ms),
            None => Err(opentune_core::Error::Unsupported(
                "this driver has no diagnostic transport".into(),
            )),
        }
    }
}

impl Interface for RawInterface<'_> {
    fn info(&self) -> InterfaceInfo {
        self.inner.info()
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            write_memory: false,
            programming_voltage: false,
            ..self.inner.capabilities()
        }
    }
    fn open(&mut self, _cfg: &ConnectConfig) -> opentune_core::Result<()> {
        Err(refused("re-opening the driver"))
    }
    fn close(&mut self) -> opentune_core::Result<()> {
        Err(refused("closing the driver"))
    }
    fn battery_millivolts(&mut self) -> opentune_core::Result<Option<u32>> {
        self.inner.battery_millivolts()
    }
    fn read_ecu_id(&mut self) -> opentune_core::Result<EcuId> {
        self.inner.read_ecu_id()
    }
    fn read_memory(
        &mut self,
        region: MemoryRegion,
        progress: &mut dyn FnMut(Progress),
    ) -> opentune_core::Result<Vec<u8>> {
        self.inner.read_memory(region, progress)
    }
    fn write_memory(
        &mut self,
        _region: MemoryRegion,
        _data: &[u8],
        _progress: &mut dyn FnMut(Progress),
    ) -> opentune_core::Result<()> {
        Err(refused("write_memory"))
    }
    fn plan_read_memory(
        &mut self,
        region: MemoryRegion,
    ) -> Option<opentune_core::Result<Vec<String>>> {
        self.inner.plan_read_memory(region)
    }
    fn diag(&mut self) -> Option<&mut dyn DiagTransport> {
        if self.inner.diag().is_some() {
            Some(self)
        } else {
            None
        }
    }
    fn can_monitor(&mut self) -> Option<&mut dyn opentune_core::monitor::CanMonitor> {
        self.inner.can_monitor()
    }
    fn kline(&mut self) -> Option<&mut dyn opentune_core::protocol::kline::KLineRaw> {
        self.inner.kline()
    }
    fn set_seed_key(
        &mut self,
        _seed_key: Box<dyn opentune_core::protocol::seedkey::SeedKey>,
    ) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_read_only_guard_admits_only_read_services() {
        for ok in [
            &[0x22, 0xF1, 0x90][..],
            &[0x19, 0x02, 0xFF],
            &[0x10, 0x03],
            &[0x10, 0x02], // any session: it changes no data
            &[0x3E, 0x00],
            &[0x27, 0x01],
            &[0x23, 0x24, 0x00, 0x08, 0x00, 0x00, 0x01, 0x00],
            &[0x35, 0x00, 0x44],
            &[0x36, 0x01],
            &[0x37],
            &[0x21, 0x01],
            &[0x1A, 0x9B],
            &[0x01, 0x0C],
            &[0x09, 0x02],
            &[0xA8, 0x00],
        ] {
            refuse_ecu_changing(ok).unwrap();
        }
        for bad in [
            &[0x2E, 0xF1, 0x90, 0x00][..],
            &[0x14, 0xFF, 0xFF, 0xFF],
            &[0x04],
            &[0x08, 0x01],
            &[0x31, 0x01, 0xFF, 0x00],
            &[0x32, 0x01],
            &[0x34, 0x00, 0x44],
            &[0x3D, 0x14],
            &[0x2F, 0x00, 0x01, 0x03],
            &[0x30, 0x01],
            &[0x11, 0x01],
            &[0x28, 0x00, 0x01],
            &[0x85, 0x02],
            &[0x2C, 0x03, 0xF2, 0x00],
            &[0x3B, 0x01],
            &[0xB0, 0x00],
            &[0xB8, 0x00],
            &[0x99], // unknown: refused too (allow-list)
            &[],
        ] {
            assert!(refuse_ecu_changing(bad).is_err(), "{bad:02X?}");
        }
        // ReadOnlyDiag never forwards a refused request.
        let mut sent: Vec<Vec<u8>> = Vec::new();
        let mut inner = |r: &[u8]| -> Result<Vec<u8>, String> {
            sent.push(r.to_vec());
            Ok(vec![r[0] + 0x40])
        };
        {
            let mut ro = ReadOnlyDiag(&mut inner);
            assert!(ro.request(&[0x2E, 0xF1, 0x90, 0x00]).is_err());
            assert_eq!(ro.request(&[0x22, 0xF1, 0x90]).unwrap(), vec![0x62]);
        }
        assert_eq!(sent, vec![vec![0x22, 0xF1, 0x90]]);
    }

    #[test]
    fn restore_runs_after_success_error_and_panic() {
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<u8>>::new()));
        let l = log.clone();
        let mut d = move |r: &[u8]| {
            l.lock().unwrap().push(r.to_vec());
            Ok::<_, String>(vec![r[0] + 0x40])
        };
        let restore = || -> RestoreJob { Box::new(|d| d.request(&[0x2F, 0x00]).map(|_| ())) };
        let a = run_then_restore(&mut d, |d| d.request(&[0x2F, 0x03]), restore());
        assert_eq!(a.result.unwrap(), vec![0x6F]);
        assert!(a.restored.is_ok());
        let a = run_then_restore(
            &mut d,
            |_d| -> Result<(), String> { Err("NRC 0x22".into()) },
            restore(),
        );
        assert!(a.restored.is_ok());
        assert!(a.into_result().unwrap_err().contains("handed back"));
        let a = run_then_restore(
            &mut d,
            |_d| -> Result<(), String> { panic!("synthetic") },
            restore(),
        );
        assert!(a.result.unwrap_err().contains("synthetic"));
        assert!(a.restored.is_ok());
        // Every action was followed by exactly one hand-back.
        let sent = log.lock().unwrap().clone();
        assert_eq!(sent.iter().filter(|r| r == &&vec![0x2F, 0x00]).count(), 3);
        // A failing (or panicking) hand-back is reported loudly.
        let a = run_then_restore(
            &mut d,
            |_d| Ok(1u8),
            Box::new(|_d| -> Result<(), String> { panic!("restore boom") }),
        );
        let e = a.into_result().unwrap_err();
        assert!(e.contains("FAILED") && e.contains("restore boom"), "{e}");
    }

    #[test]
    fn a_dropped_handle_stops_its_operation() {
        let (job, h) = LongJob::new(|_d, stop| {
            let t0 = std::time::Instant::now();
            while !stop.is_stopped() {
                assert!(t0.elapsed() < std::time::Duration::from_secs(5));
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            Ok(7u32)
        });
        let flag = job.stop_flag().clone();
        drop(h);
        assert!(flag.is_stopped());
        let mut d = |r: &[u8]| Ok::<_, String>(r.to_vec());
        job.run(&mut d).unwrap();
    }

    #[test]
    fn a_failed_start_reaches_the_handle() {
        let (job, h) = LongJob::new(|_d, _s| Ok(()));
        job.fail("no such device".into());
        assert_eq!(h.join().unwrap_err(), "no such device");
        let (job, mut h) = LongJob::new(|_d, _s| Ok(()));
        drop(job);
        assert!(h
            .poll()
            .unwrap()
            .as_ref()
            .unwrap_err()
            .contains("without a result"));
    }
}
