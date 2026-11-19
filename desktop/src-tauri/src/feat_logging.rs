//! # Live datalogging panel (backend)
//!
//! Two Tauri commands that back the "Live log" panel. The live loop is driven
//! from the frontend, which calls [`log_poll_once`] on an interval. Each poll
//! goes through the app-wide adapter hub ([`crate::adapter`]), which reuses the
//! already-opened adapter between ticks (no full ELM327 re-init per tick) and
//! rejects the tick with "adapter busy" while another panel owns the adapter.
//! A poll in which nothing answered at the transport (or the adapter reset)
//! drops the opened adapter so the next poll re-opens it; a poll in which the
//! ECU only sent negative responses is alive ([`crate::adapter::PollLink`]).
//!
//! Parameter definitions are RomRaider-style `logger.xml` files parsed by
//! [`opentune_logdef`], or a definition pack's `loggers/*.json` (the same
//! [`opentune_logdef::LogDef`] model, serialised as JSON). UDS `0x22` DIDs,
//! OBD-II mode-01 PIDs and UDS `0x23` memory reads are driven (see
//! [`opentune_logdef::poll_once`]); anything else comes back with an
//! explanatory `note` rather than a value, which the UI shows verbatim.
//!
//! ## Live session (UDS `0x2C` fast mode, Subaru SSM)
//!
//! [`log_start`] / [`log_tick`] / [`log_stop`] drive an
//! [`opentune_logdef::LiveSession`] (docs/LIVE-LOGGING.md): the channels are
//! packed into dynamically defined DIDs where the ECU allows it, with a
//! per-channel polling fallback and the reason; or, with an SSM transport,
//! the module is identified (`BF`), the definition reduced to what it
//! supports and the channels read in batched `A8` requests
//! ([`opentune_logdef::ssm`], the same code as `opentune log --transport
//! ssm|ssm-can`). Because the dynamic DIDs live in the ECU's diagnostic
//! session, the session is **one long-running adapter operation**
//! ([`AdapterHub::start_op`]): it holds the adapter until it stops, so another
//! panel cannot change the session underneath it. The job owns the
//! `LiveSession` on the adapter worker and serves ticks one at a time over a
//! channel (ticks can never overlap).
//!
//! The job runs on the core resilient loop
//! ([`opentune_core::resilient::run_resilient`]), one round per panel tick,
//! so it has the CLI's link semantics: a tick in which requests were sent and
//! none got any response is *dead* (a tick of negative responses is alive);
//! after [`DEAD_TICKS`] dead ticks in a row, or at once on an adapter reset,
//! the link is **lost**. Without reconnect the session then stops, as it
//! always did. With reconnect (opt-in, see [`live_policy`]) the adapter is
//! closed and re-opened **through this session's own hub `Session`** with a
//! growing backoff, and `LiveSession::start` runs again so the dynamic DIDs
//! are re-defined. Every link event becomes a marker sample (`param_id`
//! `link`, [`opentune_logdef::link_event_sample`]) in the next tick's
//! samples, and each tick reports the link state ([`LinkView`]). The session
//! stops — clearing the dynamic DIDs while the link is alive — on
//! [`log_stop`] and when no tick arrives for [`IDLE_STOP`] (panel closed,
//! window gone).

use crate::adapter::{AdapterHub, ConnKey, PollLink, Session};
use opentune_core::resilient::{
    run_resilient, Clock, EndReason, LinkEvent, Liveness, LogJob, ReconnectPolicy, RunLimits,
    RunOptions, SystemClock, LINK_LOST_PREFIX,
};
use opentune_core::{CoreDiag, RawDiag};
use opentune_logdef::ssm::{SsmPrepared, SsmWire};
use opentune_logdef::{
    link_event_sample, FastLogConfig, LiveSession, LiveStatus, Sample, SsmPollConfig, SsmTarget,
};
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Read and parse the logger definition at `path` into a
/// [`opentune_logdef::LogDef`]: a pack's JSON `LogDef` when the text is a JSON
/// object, otherwise a RomRaider-style `logger.xml`.
fn load_def(path: &str) -> Result<opentune_logdef::LogDef, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("read '{path}': {e}"))?;
    if text.trim_start().starts_with('{') {
        return serde_json::from_str(&text)
            .map_err(|e| format!("parse '{path}' (JSON LogDef): {e}"));
    }
    opentune_logdef::parse(&text).map_err(|e| format!("parse '{path}': {e}"))
}

/// Parse a `logger.xml` definition and return the list of available parameters.
///
/// Each entry is `{ id, name, units }`. A definition can list the same parameter
/// id under several ECU variants; the UI only needs one checkbox per id, so
/// duplicates are collapsed to the first occurrence while preserving order.
#[tauri::command]
pub fn log_params(def_path: String) -> Result<Value, String> {
    let def = load_def(&def_path)?;

    let mut seen = std::collections::HashSet::new();
    let mut params = Vec::new();
    for (_proto, _ecu, p) in def.params() {
        if !seen.insert(p.id.clone()) {
            continue;
        }
        params.push(json!({
            "id": p.id,
            "name": p.name,
            "units": p.units,
        }));
    }

    Ok(json!({
        "path": def_path,
        "version": def.version,
        "count": params.len(),
        "params": params,
    }))
}

/// Poll the selected parameters **once** and return the samples. An empty
/// `ids` selects every enabled parameter in the definition.
///
/// This is the per-tick worker for the live loop. Reads go to the target module
/// (UI `tx_id`/`rx_id`, else the engine ECU). Every parameter always yields a
/// sample; a failed or unsupported read is carried in `note`, never silently
/// dropped. When nothing answered at the transport, or the adapter reset, the
/// cached adapter is dropped so the next tick re-opens it (recovers from an
/// unplugged / reset adapter); `link` says which (`up`, `silent`, `lost`).
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn log_poll_once(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    tx_id: Option<String>,
    rx_id: Option<String>,
    def_path: String,
    timeout_ms: u64,
    ids: Vec<String>,
    hub: tauri::State<'_, crate::adapter::AdapterHub>,
) -> Result<Value, String> {
    let def = load_def(&def_path)?;
    let (addr, assumed) = crate::adapter::read_addr(crate::adapter::resolve(
        tx_id.as_deref(),
        rx_id.as_deref(),
        None,
    )?);
    let key = crate::adapter::ConnKey::new(
        &driver,
        port.as_deref(),
        baud.as_deref(),
        bus.as_deref(),
        addr,
    );
    let (samples, link) = hub.run("live log", move |s| poll_on(s, &key, timeout_ms, &def, &ids))?;
    let rows: Vec<Value> = samples.iter().map(sample_json).collect();

    Ok(json!({
        "ts": chrono_millis(),
        "count": rows.len(),
        "samples": rows,
        "target": addr.json(assumed),
        "link": link,
    }))
}

/// One stateless poll on the hub worker: the samples and the link's
/// liveness. A silent or lost link drops the opened adapter.
fn poll_on(
    s: &mut Session,
    key: &ConnKey,
    timeout_ms: u64,
    def: &opentune_logdef::LogDef,
    ids: &[String],
) -> Result<(Vec<Sample>, PollLink), String> {
    let (samples, link) = s.with_diag(key, timeout_ms, |d| {
        let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let mut lv = Liveness::new(d);
        let samples = opentune_logdef::poll_once(&mut lv, def, &id_refs);
        Ok((samples, PollLink::of(&lv)))
    })?;
    if link.reopen() {
        s.invalidate();
    }
    Ok((samples, link))
}

/// One live sample as the panel's table expects it (same keys as
/// [`log_poll_once`]).
fn sample_json(s: &Sample) -> Value {
    json!({
        "paramId": s.param_id,
        "name": s.name,
        "raw": s.raw,
        "value": s.value,
        "units": s.units,
        "note": s.note,
        // Text channels (identification DIDs): `value`/`raw` are null.
        "text": s.text,
    })
}

// ---------------------------------------------------------------------------
// Live session: a LiveSession held on the adapter hub
// ---------------------------------------------------------------------------

/// Busy-slot name other panels see while a live session holds the adapter.
pub const LIVE_OP: &str = "live log session";
/// A session with no tick for this long stops itself (the panel was closed).
pub const IDLE_STOP: Duration = Duration::from_secs(15);
/// Consecutive ticks in which nothing answered at the transport before the
/// link counts as lost ([`ReconnectPolicy::dead_after`]).
pub const DEAD_TICKS: u32 = 5;
/// With reconnect: give up once the link has been down this long.
pub const RECONNECT_GIVE_UP: Duration = Duration::from_secs(300);
/// With reconnect: stop after this many re-opens in a row after which nothing
/// answered (the adapter opens but the ECU stays silent, e.g. ignition off),
/// so a session left running never keeps waking a bus that went to sleep.
pub const MAX_HOLLOW_RECONNECTS: u32 = 3;
/// The pseudo-driver id of the simulated SSM module (offline, synthetic).
pub const SSM_SIM_DRIVER: &str = crate::drivers::SSM_SIM_DRIVER;
/// How long `log_start` waits for the plan + define (probes every DID first).
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// How long `log_stop` waits for the dynamic DIDs to be cleared.
const STOP_TIMEOUT: Duration = Duration::from_secs(15);

/// The link policy of a desktop live session: [`DEAD_TICKS`] dead ticks
/// make a lost link; with `reconnect` it is re-opened with the core's
/// backoff (0.5 s doubling to 10 s) for at most [`RECONNECT_GIVE_UP`].
///
/// Reconnect is **opt-in** (the panel's *Reconnect* box, off by default, as
/// the CLI's `--reconnect`): a dropped link is very often the ignition being
/// switched off, and re-opening means re-initialising the adapter, switching
/// the diagnostic session and re-defining dynamic DIDs on a bus that is going
/// to sleep — which can keep modules awake and wedge some adapters. Losing
/// the rest of a log is recoverable (press Start); those are not. Opted in,
/// the retries are bounded by [`RECONNECT_GIVE_UP`] and
/// [`MAX_HOLLOW_RECONNECTS`].
pub fn live_policy(reconnect: bool) -> ReconnectPolicy {
    ReconnectPolicy {
        reconnect,
        dead_after: DEAD_TICKS,
        give_up_after_ms: Some(u64::try_from(RECONNECT_GIVE_UP.as_millis()).unwrap_or(u64::MAX)),
        ..ReconnectPolicy::default()
    }
}

/// A tick request: the reply carries the samples (link markers included).
struct TickReq {
    reply: mpsc::Sender<Result<Vec<Sample>, String>>,
    /// The panel stops waiting at this instant; a later reply is wasted.
    deadline: Instant,
}

/// A request to the session job.
enum Cmd {
    Tick(TickReq),
    Stop,
}

/// The link as the panel shows it, updated by the session job.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LinkView {
    /// `up`, `silent` (dead ticks, not yet lost), `lost`, `reconnecting`,
    /// `reconnected` or `gave-up`.
    pub state: &'static str,
    /// Reconnect is on for this session.
    pub reconnect: bool,
    /// Dead ticks in a row.
    pub dead_ticks: u32,
    /// Dead ticks that make a lost link.
    pub dead_after: u32,
    /// Re-open attempt of the current outage (1-based; 0 before the first).
    pub attempt: u32,
    /// Pause before the next attempt, ms.
    pub retry_in_ms: Option<u64>,
    /// The last transport or re-open error.
    pub error: Option<String>,
    /// Times the link was lost.
    pub drops: u32,
    /// Successful re-opens.
    pub reconnects: u32,
    /// Total time the link was down, ms (closed outages).
    pub downtime_ms: u64,
    /// The last link event, one line.
    pub last_event: Option<String>,
}

impl LinkView {
    fn new(policy: &ReconnectPolicy) -> Self {
        LinkView {
            state: "up",
            reconnect: policy.reconnect,
            dead_ticks: 0,
            dead_after: policy.dead_after,
            attempt: 0,
            retry_in_ms: None,
            error: None,
            drops: 0,
            reconnects: 0,
            downtime_ms: 0,
            last_event: None,
        }
    }

    /// The link is down: ticks get the link state, no samples.
    fn down(&self) -> bool {
        matches!(self.state, "lost" | "reconnecting" | "gave-up")
    }

    fn tick(&mut self, dead: bool, error: Option<String>) {
        if dead {
            self.dead_ticks += 1;
            self.state = "silent";
            self.error = error;
        } else {
            self.dead_ticks = 0;
            self.state = "up";
            self.error = None;
            self.attempt = 0;
            self.retry_in_ms = None;
        }
    }

    fn event(&mut self, ev: &LinkEvent) {
        self.last_event = Some(ev.describe());
        match ev {
            LinkEvent::Lost { error, .. } => {
                self.state = "lost";
                self.drops += 1;
                self.attempt = 0;
                self.error = Some(error.clone());
            }
            LinkEvent::ReopenFailed {
                attempt,
                error,
                retry_in_ms,
            } => {
                self.state = "reconnecting";
                self.attempt = *attempt;
                self.error = Some(error.clone());
                self.retry_in_ms = Some(*retry_in_ms);
            }
            LinkEvent::Reconnected { attempt, after_ms } => {
                self.state = "reconnected";
                self.attempt = *attempt;
                self.reconnects += 1;
                self.downtime_ms += after_ms;
                self.dead_ticks = 0;
                self.error = None;
                self.retry_in_ms = None;
            }
            LinkEvent::Recovered { after_ms } => {
                self.state = "up";
                self.downtime_ms += after_ms;
                self.dead_ticks = 0;
            }
            LinkEvent::GaveUp {
                after_ms, error, ..
            } => {
                self.state = "gave-up";
                self.downtime_ms += after_ms;
                self.error = Some(error.clone());
                self.retry_in_ms = None;
            }
        }
    }
}

#[derive(Default)]
struct RunInfo {
    running: bool,
    status: Option<LiveStatus>,
    /// Set once the session started (after plan + define).
    t0: Option<Instant>,
    /// Final duration, once finished.
    final_elapsed: Option<f64>,
    /// Why the session ended on its own (idle, connection lost, error).
    ended: Option<String>,
    /// The link state.
    link: Option<LinkView>,
    /// Link markers no tick picked up before the session ended.
    final_events: Vec<Sample>,
    /// SSM: identification, selection notes and what was left out.
    ssm: Option<Value>,
    /// `{ id, name, units }` of the channels actually logged.
    params: Vec<Value>,
}

/// One live session's shared handle (the job serves it, commands talk to it).
pub struct LiveRun {
    tx: Mutex<mpsc::Sender<Cmd>>,
    info: Mutex<RunInfo>,
    done: (Mutex<bool>, Condvar),
    /// `{ tx, rx, assumed }` of the module being logged.
    target: Value,
    /// Per-request timeout (bounds how long a tick may take).
    timeout_ms: u64,
    /// Longest backoff pause a tick may wait behind, ms.
    max_pause_ms: u64,
    channels: AtomicUsize,
}

impl LiveRun {
    fn info(&self) -> std::sync::MutexGuard<'_, RunInfo> {
        self.info.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set_status(&self, st: &LiveStatus) {
        self.info().status = Some(st.clone());
    }

    fn link(&self) -> Option<LinkView> {
        self.info().link.clone()
    }

    fn update_link(&self, f: impl FnOnce(&mut LinkView)) {
        if let Some(l) = self.info().link.as_mut() {
            f(l);
        }
    }

    fn elapsed_s(&self) -> f64 {
        let i = self.info();
        i.final_elapsed
            .or_else(|| i.t0.map(|t| t.elapsed().as_secs_f64()))
            .unwrap_or(0.0)
    }

    fn finish(&self, ended: Option<String>) {
        {
            let mut i = self.info();
            i.running = false;
            if i.final_elapsed.is_none() {
                i.final_elapsed = Some(i.t0.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0));
            }
            if ended.is_some() {
                i.ended = ended;
            }
        }
        let (m, cv) = &self.done;
        *m.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cv.notify_all();
    }

    fn wait_done(&self, timeout: Duration) -> bool {
        let (m, cv) = &self.done;
        let g = m.lock().unwrap_or_else(|e| e.into_inner());
        let (g, _) = cv
            .wait_timeout_while(g, timeout, |done| !*done)
            .unwrap_or_else(|e| e.into_inner());
        *g
    }

    /// `{ ticks, elapsed_s, samples_per_s, requests_per_sample }`.
    fn rate(&self) -> Value {
        let elapsed = self.elapsed_s();
        let i = self.info();
        let (ticks, rps) = i
            .status
            .as_ref()
            .map(|s| (s.ticks, s.requests_per_tick()))
            .unwrap_or((0, 0.0));
        json!({
            "ticks": ticks,
            "elapsed_s": elapsed,
            "samples_per_s": if elapsed > 0.0 { ticks as f64 / elapsed } else { 0.0 },
            "requests_per_sample": rps,
            "channels": self.channels.load(Ordering::Relaxed),
        })
    }

    /// The `log_stop` / idle-state JSON.
    fn summary(&self) -> Value {
        let rate = self.rate();
        let i = self.info();
        json!({
            "running": i.running,
            "status": i.status,
            "ended": i.ended,
            "rate": rate,
            "target": self.target,
            "link": i.link,
            "events": i.final_events.iter().map(sample_json).collect::<Vec<_>>(),
            "ssm": i.ssm,
        })
    }
}

/// Tauri-managed live-session state: the current (or last) live session.
#[derive(Default)]
pub struct LiveLogState {
    cur: Mutex<Option<Arc<LiveRun>>>,
}

impl LiveLogState {
    fn current(&self) -> Option<Arc<LiveRun>> {
        self.cur.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// SSM instead of UDS: which wire, which module, how big a packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SsmRequest {
    pub wire: SsmWire,
    pub target: SsmTarget,
    pub max_packet_len: usize,
}

/// What `log_start` asks for.
pub struct LiveRequest {
    pub key: ConnKey,
    /// `{ tx, rx, assumed }` of `key`'s module, echoed to the UI.
    pub target: Value,
    pub timeout_ms: u64,
    pub def: opentune_logdef::LogDef,
    pub ids: Vec<String>,
    pub cfg: FastLogConfig,
    /// Stop when no tick arrives for this long ([`IDLE_STOP`] in the app).
    pub idle: Duration,
    /// Subaru SSM (identify + select + batched `A8`), else UDS / OBD-II.
    pub ssm: Option<SsmRequest>,
    /// Loss detection and reconnect ([`live_policy`]).
    pub policy: ReconnectPolicy,
    /// Time source of the loop (a `VirtualClock` in tests).
    pub clock: Arc<dyn Clock + Send + Sync>,
}

/// What a started session reports: its status, the channels it logs and,
/// for SSM, the module's identification and what was left out.
#[derive(Debug, Clone)]
pub struct Started {
    pub status: LiveStatus,
    pub params: Vec<Value>,
    pub ssm: Option<Value>,
}

/// Validate, then dispatch the session onto the hub. Returns the channel that
/// reports the start (after identify / plan + define) or the start error.
/// Never blocks on the adapter.
pub fn begin_live(
    hub: &AdapterHub,
    state: &LiveLogState,
    req: LiveRequest,
) -> Result<mpsc::Receiver<Result<Started, String>>, String> {
    let mut slot = state.cur.lock().unwrap_or_else(|e| e.into_inner());
    if slot.as_ref().is_some_and(|r| r.info().running) {
        return Err("a live log session is already running — stop it first".into());
    }
    req.policy.validate()?;
    if req.ssm.is_none() && req.key.driver.trim() == SSM_SIM_DRIVER {
        return Err(format!(
            "the {SSM_SIM_DRIVER} simulator speaks SSM: choose an SSM transport"
        ));
    }
    let id_refs: Vec<&str> = req.ids.iter().map(String::as_str).collect();
    let live = LiveSession::new(&req.def, &id_refs, req.cfg.clone())?;
    if live.params().is_empty() {
        return Err("no parameter selected (or none enabled in the definition)".into());
    }
    let (tx, rx) = mpsc::channel::<Cmd>();
    let run = Arc::new(LiveRun {
        tx: Mutex::new(tx),
        info: Mutex::new(RunInfo {
            running: true,
            link: Some(LinkView::new(&req.policy)),
            ..RunInfo::default()
        }),
        done: (Mutex::new(false), Condvar::new()),
        target: req.target.clone(),
        timeout_ms: req.timeout_ms,
        max_pause_ms: req.policy.max_backoff_ms,
        channels: AtomicUsize::new(live.params().len()),
    });
    let (stx, srx) = mpsc::channel();
    let job_run = run.clone();
    let job = Job {
        key: req.key,
        timeout_ms: req.timeout_ms,
        idle: req.idle,
        ssm: req.ssm,
        policy: req.policy,
        clock: req.clock,
        def: req.def,
        ids: req.ids,
        cfg: req.cfg,
    };
    hub.start_op(LIVE_OP, move |s: &mut Session| {
        live_job(s, job_run, job, live, rx, stx)
    })?;
    *slot = Some(run);
    Ok(srx)
}

/// Wait (blocking) for the session's start result.
pub fn await_live_start(rx: mpsc::Receiver<Result<Started, String>>) -> Result<Started, String> {
    match rx.recv_timeout(START_TIMEOUT) {
        Ok(r) => r,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            Err("the live session did not start in time (adapter or ECU not answering)".into())
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err("live session aborted inside the adapter worker (internal error)".into())
        }
    }
}

/// Everything a session job runs with besides its session.
struct Job {
    key: ConnKey,
    timeout_ms: u64,
    idle: Duration,
    ssm: Option<SsmRequest>,
    policy: ReconnectPolicy,
    clock: Arc<dyn Clock + Send + Sync>,
    def: opentune_logdef::LogDef,
    ids: Vec<String>,
    cfg: FastLogConfig,
}

/// Open the adapter for `key` through the hub's session — re-using the one
/// already open unless `fresh`, which closes and re-opens it — and get it
/// ready for the wire (SSM: the raw K-line at 4800 baud, or the CAN
/// transport; UDS: the diagnostic transport).
fn open_link(
    cell: &RefCell<&mut Session>,
    key: &ConnKey,
    ssm: Option<SsmRequest>,
    fresh: bool,
) -> Result<(), String> {
    let mut s = cell.borrow_mut();
    if fresh {
        s.invalidate();
    }
    let r = (|| {
        let drv = s.interface(key)?;
        match ssm {
            Some(q) => q.wire.prepare(drv),
            None => drv.diag().map(|_| ()).ok_or_else(|| {
                format!("driver '{}' has no diagnostic transport", key.driver)
            }),
        }
    })();
    if r.is_err() {
        s.invalidate();
    }
    r
}

/// The session's link: every request goes through the hub's [`Session`]
/// (the one adapter binding this operation holds), never a second path.
/// Errors that mean the adapter is gone (it could not be opened, it reset)
/// carry [`LINK_LOST_PREFIX`].
struct HubDiag<'c, 's> {
    s: &'c RefCell<&'s mut Session>,
    key: &'c ConnKey,
    timeout_ms: u64,
    ssm: Option<SsmRequest>,
}

impl RawDiag for HubDiag<'_, '_> {
    fn request(&mut self, req: &[u8]) -> Result<Vec<u8>, String> {
        let mut s = self.s.borrow_mut();
        let drv = s
            .interface(self.key)
            .map_err(|e| format!("{LINK_LOST_PREFIX}{e}"))?;
        let t = self.timeout_ms;
        match self.ssm {
            None => match drv.diag() {
                Some(tr) => CoreDiag::new(tr, t).request(req),
                None => Err(format!(
                    "{LINK_LOST_PREFIX}driver no longer exposes a diagnostic transport"
                )),
            },
            Some(r) => r
                .wire
                .with_transport(drv, r.target, |tr| CoreDiag::new(tr, t).request(req))
                .map_err(|e| format!("{LINK_LOST_PREFIX}{e}"))?,
        }
    }
}

/// The command channel, seen from the job: ticks waiting for a round, link
/// markers waiting for a tick, and the reply of the last round (sent once
/// the loop has recorded that round's link events, so a "lost" marker rides
/// on the tick that lost the link).
struct Inbox {
    rx: mpsc::Receiver<Cmd>,
    run: Arc<LiveRun>,
    idle: Duration,
    stop: AtomicBool,
    ended: RefCell<Option<String>>,
    /// Last command received or reply sent (idle detection).
    last_seen: Cell<Instant>,
    held: RefCell<VecDeque<TickReq>>,
    pending: RefCell<Vec<Sample>>,
    deferred: RefCell<Option<(TickReq, Vec<Sample>)>>,
}

impl Inbox {
    fn new(rx: mpsc::Receiver<Cmd>, run: Arc<LiveRun>, idle: Duration) -> Self {
        Inbox {
            rx,
            run,
            idle,
            stop: AtomicBool::new(false),
            ended: RefCell::new(None),
            last_seen: Cell::new(Instant::now()),
            held: RefCell::new(VecDeque::new()),
            pending: RefCell::new(Vec::new()),
            deferred: RefCell::new(None),
        }
    }

    /// Stop the run; `why` is the reason when it ends on its own.
    fn end(&self, why: Option<String>) {
        if why.is_some() && self.ended.borrow().is_none() {
            *self.ended.borrow_mut() = why;
        }
        self.stop.store(true, Ordering::Relaxed);
    }

    fn idle_message(&self) -> String {
        format!(
            "no tick for {:.1} s (panel closed?) — session stopped",
            self.idle.as_secs_f64()
        )
    }

    fn take_pending(&self) -> Vec<Sample> {
        std::mem::take(&mut *self.pending.borrow_mut())
    }

    fn send(&self, t: TickReq, samples: Vec<Sample>) {
        self.last_seen.set(Instant::now());
        let _ = t.reply.send(Ok(samples));
    }

    /// Send the last round's reply with the link markers recorded since.
    fn flush_deferred(&self) {
        let d = self.deferred.borrow_mut().take();
        if let Some((t, mut samples)) = d {
            samples.extend(self.take_pending());
            self.send(t, samples);
        }
    }

    fn receive(&self, c: Cmd) {
        self.last_seen.set(Instant::now());
        match c {
            Cmd::Tick(t) => self.held.borrow_mut().push_back(t),
            Cmd::Stop => self.end(None),
        }
    }

    /// The next tick to run a round for (blocking), or `None` when the run
    /// should stop (stop requested, idle, handle gone).
    fn next_tick(&self) -> Option<TickReq> {
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return None;
            }
            let held = self.held.borrow_mut().pop_front();
            if let Some(t) = held {
                if t.deadline > Instant::now() {
                    return Some(t);
                }
                continue; // the panel gave up on it
            }
            let left = self.idle.saturating_sub(self.last_seen.get().elapsed());
            match self.rx.recv_timeout(left) {
                Ok(c) => self.receive(c),
                Err(mpsc::RecvTimeoutError::Timeout) => self.end(Some(self.idle_message())),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.end(Some("session handle dropped — session stopped".into()))
                }
            }
        }
    }

    /// While the loop sleeps (backoff) or the link is down: deliver the last
    /// reply, take commands, and answer waiting ticks with the link state
    /// when the link is down (no samples, the markers so far).
    fn serve_waiting(&self) {
        self.flush_deferred();
        loop {
            match self.rx.try_recv() {
                Ok(c) => self.receive(c),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.end(Some("session handle dropped — session stopped".into()));
                    break;
                }
            }
        }
        if self.run.link().is_some_and(|l| l.down()) {
            loop {
                let t = self.held.borrow_mut().pop_front();
                let Some(t) = t else { break };
                let markers = self.take_pending();
                self.send(t, markers);
            }
        }
        if self.last_seen.get().elapsed() >= self.idle {
            self.end(Some(self.idle_message()));
        }
    }
}

/// The loop's clock: real (or virtual) time, serving the command channel
/// around every sleep so ticks, Stop and idle are handled while it backs off.
struct ServingClock<'a> {
    inner: &'a dyn Clock,
    inbox: &'a Inbox,
}

impl Clock for ServingClock<'_> {
    fn now_ms(&self) -> u64 {
        self.inner.now_ms()
    }
    fn unix_ms(&self) -> u64 {
        self.inner.unix_ms()
    }
    fn sleep_ms(&self, ms: u64) {
        self.inbox.serve_waiting();
        self.inner.sleep_ms(ms);
        self.inbox.serve_waiting();
    }
}

/// The session as a [`LogJob`]: one round per panel tick.
struct DeskJob<'a> {
    live: &'a mut LiveSession,
    inbox: &'a Inbox,
    run: Arc<LiveRun>,
    reconnect: bool,
    /// A round answered since the last (re)open.
    answered_since_open: bool,
    /// Re-opens in a row after which nothing answered.
    hollow: u32,
}

impl DeskJob<'_> {
    fn end_lost(&self, ev: &LinkEvent) -> std::io::Error {
        let why = match ev {
            LinkEvent::Lost { dead_rounds, .. } if *dead_rounds >= DEAD_TICKS => format!(
                "no channel answered for {DEAD_TICKS} ticks in a row (connection dropped or \
                 ignition off) — session stopped"
            ),
            LinkEvent::Lost { error, .. } => {
                format!("link lost ({error}) — connection dropped, session stopped")
            }
            other => other.describe(),
        };
        std::io::Error::other(format!(
            "{why} (turn on Reconnect to keep logging through a drop)"
        ))
    }
}

impl LogJob for DeskJob<'_> {
    fn round(&mut self, diag: &mut dyn RawDiag, _round: u64, _ts: u64) -> std::io::Result<()> {
        self.inbox.flush_deferred();
        let Some(t) = self.inbox.next_tick() else {
            return Ok(()); // the loop sees the stop flag next
        };
        let mut lv = Liveness::new(diag);
        let samples = self.live.tick(&mut lv);
        let dead = lv.dead();
        let error = lv.last_error.clone();
        self.run.set_status(self.live.status());
        self.run.update_link(|l| l.tick(dead, error));
        if !dead {
            self.answered_since_open = true;
            self.hollow = 0;
        }
        let mut out = self.inbox.take_pending();
        out.extend(samples);
        *self.inbox.deferred.borrow_mut() = Some((t, out));
        Ok(())
    }

    fn event(&mut self, ev: &LinkEvent, _round: u64, _ts: u64) -> std::io::Result<()> {
        self.inbox.pending.borrow_mut().push(link_event_sample(ev));
        self.run.update_link(|l| l.event(ev));
        let mut stop = None;
        match ev {
            LinkEvent::Lost { .. } if !self.reconnect => stop = Some(self.end_lost(ev)),
            LinkEvent::Lost { .. } if !self.answered_since_open => {
                self.hollow += 1;
                if self.hollow >= MAX_HOLLOW_RECONNECTS {
                    stop = Some(std::io::Error::other(format!(
                        "the link was re-opened {} time(s) in a row but nothing answered \
                         (ignition off?) — session stopped",
                        self.hollow
                    )));
                }
            }
            LinkEvent::Reconnected { .. } => self.answered_since_open = false,
            _ => {}
        }
        self.inbox.flush_deferred();
        match stop {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn reopened(&mut self, diag: &mut dyn RawDiag) {
        let st = self.live.start(diag).clone();
        self.run.set_status(&st);
    }

    fn stop(&mut self, diag: &mut dyn RawDiag) {
        let st = self.live.stop(diag).clone();
        self.run.set_status(&st);
    }
}

/// `{ id, name, units }` of a session's channels.
fn params_json(live: &LiveSession) -> Vec<Value> {
    live.params()
        .iter()
        .map(|p| json!({ "id": p.id, "name": p.name, "units": p.units }))
        .collect()
}

/// The panel's SSM summary: identification, what is logged and what was left
/// out (with the reasons).
fn ssm_json(r: &SsmRequest, p: &SsmPrepared, logged: usize) -> Value {
    json!({
        "target": r.target,
        "wire": r.wire,
        "max_packet_len": r.max_packet_len,
        "ssm_id": p.init.ssm_id_hex(),
        "rom_id": p.init.rom_id_hex(),
        "capability_bytes": p.init.capabilities.len(),
        "logged": logged,
        "notes": p.notes,
        "not_logged": p.not_logged,
    })
}

/// Identify (SSM), plan and define on the opened link.
fn start_on(
    link: &mut dyn RawDiag,
    job: &Job,
    live: LiveSession,
) -> Result<(LiveSession, Option<Value>), String> {
    let Some(r) = job.ssm else {
        return Ok((live, None));
    };
    let ids: Vec<&str> = job.ids.iter().map(String::as_str).collect();
    let p = opentune_logdef::ssm::prepare(
        link,
        &job.def,
        job.cfg.clone(),
        r.target,
        r.max_packet_len,
        &ids,
    )?;
    let live = LiveSession::new(&p.def, &ids, p.cfg.clone())?;
    if live.params().is_empty() {
        return Err(format!(
            "none of the selected parameters is supported by this module (ROM ID {}): {}",
            p.init.rom_id_hex(),
            p.not_logged.join(", ")
        ));
    }
    let info = ssm_json(&r, &p, live.params().len());
    Ok((live, Some(info)))
}

/// Runs on the adapter worker for the whole session.
fn live_job(
    s: &mut Session,
    run: Arc<LiveRun>,
    job: Job,
    live: LiveSession,
    rx: mpsc::Receiver<Cmd>,
    started: mpsc::Sender<Result<Started, String>>,
) {
    let cell = RefCell::new(s);
    let hub_diag = || HubDiag {
        s: &cell,
        key: &job.key,
        timeout_ms: job.timeout_ms,
        ssm: job.ssm,
    };
    let begun = open_link(&cell, &job.key, job.ssm, false).and_then(|()| {
        let mut d = hub_diag();
        let (mut live, ssm) = start_on(&mut d, &job, live)?;
        let st = live.start(&mut d).clone();
        Ok((live, ssm, st))
    });
    let mut live = match begun {
        Ok((live, ssm, st)) => {
            let params = params_json(&live);
            run.channels.store(live.params().len(), Ordering::Relaxed);
            {
                let mut i = run.info();
                i.status = Some(st.clone());
                i.t0 = Some(Instant::now());
                i.ssm = ssm.clone();
                i.params = params.clone();
            }
            let _ = started.send(Ok(Started {
                status: st,
                params,
                ssm,
            }));
            live
        }
        Err(e) => {
            cell.borrow_mut().invalidate();
            // Finished BEFORE the error is reported, so a status read right
            // after a failed start never still says "running".
            run.finish(Some(e.clone()));
            let _ = started.send(Err(e));
            return;
        }
    };

    let inbox = Inbox::new(rx, run.clone(), job.idle);
    let clock = ServingClock {
        inner: job.clock.as_ref(),
        inbox: &inbox,
    };
    let mut desk = DeskJob {
        live: &mut live,
        inbox: &inbox,
        run: run.clone(),
        reconnect: job.policy.reconnect,
        answered_since_open: false, // until a tick answers
        hollow: 0,
    };
    let mut reopen = || {
        run.update_link(|l| {
            l.state = "reconnecting";
            l.attempt += 1;
            l.retry_in_ms = None;
        });
        open_link(&cell, &job.key, job.ssm, true)?;
        Ok::<_, String>(boxed(hub_diag()))
    };
    let opts = RunOptions {
        limits: RunLimits::default(),
        policy: job.policy.clone(),
        stop: Some(&inbox.stop),
        clock: &clock,
    };
    let res = run_resilient(&mut desk, boxed(hub_diag()), &mut reopen, &opts);
    drop(desk);
    let ended = match res {
        Ok(rep) if rep.ended == EndReason::GaveUp => Some(format!(
            "{} — session stopped",
            run.link()
                .and_then(|l| l.last_event)
                .unwrap_or_else(|| "gave up reconnecting".into())
        )),
        Ok(_) => inbox.ended.borrow_mut().take(),
        Err(e) => Some(e.to_string()),
    };
    inbox.flush_deferred();
    let leftover = inbox.take_pending();
    let link_up = run.link().is_some_and(|l| !l.down());
    if ended.is_some() || !link_up {
        // Start the next operation from a fresh open.
        cell.borrow_mut().invalidate();
    }
    run.info().final_events = leftover;
    run.finish(ended);
    // The inbox is dropped on return, with any queued tick: its caller sees
    // "ended".
}

/// A [`HubDiag`] as the loop's link.
fn boxed<'c, 's>(d: HubDiag<'c, 's>) -> Box<dyn RawDiag + 'c> {
    Box::new(d)
}

/// One tick of the running session (blocking until the job answers).
pub fn tick_live(state: &LiveLogState) -> Result<Value, String> {
    let run = state
        .current()
        .ok_or_else(|| "no live log session — press Start".to_string())?;
    if !run.info().running {
        let why = run.info().ended.clone();
        return Err(match why {
            Some(w) => format!("the live session has ended: {w}"),
            None => "the live session has stopped".into(),
        });
    }
    // A tick sends at most one request per channel plus a re-define, and may
    // wait behind one backoff pause of a silent link.
    let channels = run.channels.load(Ordering::Relaxed) as u64;
    let wait = Duration::from_millis(
        run.timeout_ms
            .saturating_mul(channels + 8)
            .saturating_add(run.max_pause_ms),
    ) + Duration::from_secs(2);
    let (rtx, rrx) = mpsc::channel();
    run.tx
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .send(Cmd::Tick(TickReq {
            reply: rtx,
            deadline: Instant::now() + wait,
        }))
        .map_err(|_| "the live session has ended".to_string())?;
    let samples = match rrx.recv_timeout(wait) {
        Ok(r) => r?,
        // Re-opening the adapter (and re-defining) can outlast a tick: the
        // panel gets the link state instead of an error.
        Err(mpsc::RecvTimeoutError::Timeout) if run.link().is_some_and(|l| l.down()) => {
            Vec::new()
        }
        Err(mpsc::RecvTimeoutError::Timeout) => return Err("live tick timed out".into()),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let why = run.info().ended.clone().unwrap_or_default();
            return Err(format!("the live session has ended: {why}"));
        }
    };
    let rows: Vec<Value> = samples.iter().map(sample_json).collect();
    let rate = run.rate();
    let (running, ended, status, link) = {
        let i = run.info();
        (i.running, i.ended.clone(), i.status.clone(), i.link.clone())
    };
    Ok(json!({
        "ts": chrono_millis(),
        "count": rows.len(),
        "samples": rows,
        "status": status,
        "rate": rate,
        "running": running,
        "ended": ended,
        "target": run.target,
        "link": link,
    }))
}

/// Stop the session (clears the dynamic DIDs) and wait for the adapter to be
/// released. Works whatever state the session is in.
pub fn stop_live(hub: &AdapterHub, state: &LiveLogState) -> Result<Value, String> {
    let run = state
        .current()
        .ok_or_else(|| "no live log session has been started".to_string())?;
    let _ = run
        .tx
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .send(Cmd::Stop);
    if run.wait_done(STOP_TIMEOUT) {
        let t = Instant::now();
        while hub.current().as_deref() == Some(LIVE_OP) && t.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    Ok(run.summary())
}

/// Parse a hex byte/word typed in the UI (`"F200"`, `"0xF200"`).
fn parse_hex_u16(s: &str, what: &str) -> Result<u16, String> {
    let t = s.trim();
    let h = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    u16::from_str_radix(h, 16).map_err(|_| format!("invalid {what} '{s}' (hex)"))
}

/// The fast-mode policy from the panel's options.
pub fn fast_config(
    fast: bool,
    dynamic_did: Option<&str>,
    max_record: Option<usize>,
    fast_session: Option<&str>,
) -> Result<FastLogConfig, String> {
    let mut cfg = FastLogConfig {
        enabled: fast,
        ..FastLogConfig::default()
    };
    if let Some(d) = dynamic_did.map(str::trim).filter(|d| !d.is_empty()) {
        cfg.first_did = parse_hex_u16(d, "dynamic DID")?;
    }
    if let Some(n) = max_record {
        if n == 0 {
            return Err("max record must be at least 1 byte".into());
        }
        cfg.max_record_len = n;
    }
    if let Some(sess) = fast_session.map(str::trim).filter(|d| !d.is_empty()) {
        let v = parse_hex_u16(sess, "diagnostic session")?;
        cfg.session = Some(u8::try_from(v).map_err(|_| format!("session '{sess}' is not a byte"))?);
    }
    cfg.validate()?;
    Ok(cfg)
}

/// The panel's transport choice: `uds` (default), `ssm` (K-line) or
/// `ssm-can`, with the SSM module (`ecu`/`tcu`) and packet limit.
pub fn ssm_request(
    transport: Option<&str>,
    target: Option<&str>,
    max_packet: Option<usize>,
) -> Result<Option<SsmRequest>, String> {
    let wire = match transport.map(str::trim).unwrap_or("") {
        "" | "uds" => return Ok(None),
        "ssm" | "ssm-kline" | "k-line" => SsmWire::KLine,
        "ssm-can" => SsmWire::Can,
        other => return Err(format!("unknown transport '{other}' (uds, ssm or ssm-can)")),
    };
    let target = match target.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => t.parse()?,
        None => SsmTarget::Ecu,
    };
    let max_packet_len =
        max_packet.unwrap_or(opentune_core::protocol::ssm::DEFAULT_MAX_PACKET_LEN);
    SsmPollConfig { max_packet_len }.validate()?;
    Ok(Some(SsmRequest {
        wire,
        target,
        max_packet_len,
    }))
}

/// The hub key and target JSON of an SSM session: the driver config the
/// CLI uses for the same wire ([`SsmWire::configure`]: K-line address,
/// 4800 baud, no wake-up; or the module's ISO-TP ids).
pub fn ssm_key(
    driver: &str,
    port: Option<&str>,
    baud: Option<&str>,
    bus: Option<&str>,
    r: &SsmRequest,
) -> (ConnKey, Value) {
    let mut cfg = opentune_core::ConnectConfig::default();
    r.wire.configure(&mut cfg, r.target);
    let addr = crate::adapter::Addr {
        tx_id: cfg.tx_id,
        rx_id: cfg.rx_id,
    };
    let mut key = ConnKey::new(driver, port, baud, bus, addr);
    for (k, v) in &cfg.options {
        key = key.with_option(k, v);
    }
    let mut target = addr.json(false);
    target["ssm"] = json!(r.target);
    target["wire"] = json!(r.wire);
    (key, target)
}

/// Start a live session over the selected parameters: fast (`0x2C`) when
/// `fast` and the ECU allows it, else per-channel polling (with the reason);
/// with `transport` `ssm`/`ssm-can`, Subaru SSM to `ssm_target`. `reconnect`
/// re-opens a lost link ([`live_policy`]). Holds the adapter until
/// [`log_stop`]. Returns `{ status, channels, params, ssm, target, link }`.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn log_start(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    tx_id: Option<String>,
    rx_id: Option<String>,
    def_path: String,
    timeout_ms: u64,
    ids: Vec<String>,
    fast: bool,
    dynamic_did: Option<String>,
    max_record: Option<usize>,
    fast_session: Option<String>,
    transport: Option<String>,
    ssm_target: Option<String>,
    ssm_max_packet: Option<usize>,
    reconnect: Option<bool>,
    hub: tauri::State<'_, AdapterHub>,
    state: tauri::State<'_, LiveLogState>,
) -> Result<Value, String> {
    let def = load_def(&def_path)?;
    let ssm = ssm_request(
        transport.as_deref(),
        ssm_target.as_deref(),
        ssm_max_packet,
    )?;
    let (key, target, cfg) = match &ssm {
        Some(r) => {
            if fast {
                return Err("Fast (0x2C) is UDS; SSM always batches its reads".into());
            }
            let (key, target) = ssm_key(
                &driver,
                port.as_deref(),
                baud.as_deref(),
                bus.as_deref(),
                r,
            );
            let cfg = FastLogConfig {
                ssm: Some(SsmPollConfig {
                    max_packet_len: r.max_packet_len,
                }),
                ..fast_config(false, None, None, None)?
            };
            (key, target, cfg)
        }
        None => {
            let cfg = fast_config(
                fast,
                dynamic_did.as_deref(),
                max_record,
                fast_session.as_deref(),
            )?;
            let (addr, assumed) = crate::adapter::read_addr(crate::adapter::resolve(
                tx_id.as_deref(),
                rx_id.as_deref(),
                None,
            )?);
            let key = ConnKey::new(
                &driver,
                port.as_deref(),
                baud.as_deref(),
                bus.as_deref(),
                addr,
            );
            (key, addr.json(assumed), cfg)
        }
    };
    let req = LiveRequest {
        key,
        target: target.clone(),
        timeout_ms,
        def,
        ids,
        cfg,
        idle: IDLE_STOP,
        ssm,
        policy: live_policy(reconnect.unwrap_or(false)),
        clock: Arc::new(SystemClock::new()),
    };
    let rx = begin_live(&hub, &state, req)?;
    let st = tauri::async_runtime::spawn_blocking(move || await_live_start(rx))
        .await
        .map_err(|e| format!("live start task failed: {e}"))??;
    let link = state.current().and_then(|r| r.link());
    Ok(json!({
        "status": st.status,
        "channels": st.params.len(),
        "params": st.params,
        "ssm": st.ssm,
        "target": target,
        "link": link,
    }))
}

/// One sample of every channel from the running session:
/// `{ ts, samples, status, rate, running, ended, target, link }`. Link
/// events arrive as marker samples (`paramId` `link`).
#[tauri::command]
pub async fn log_tick(state: tauri::State<'_, LiveLogState>) -> Result<Value, String> {
    tick_live(&state)
}

/// Stop the running session, clear its dynamic DIDs and release the adapter:
/// `{ running, status, ended, rate, target, link, events, ssm }` (`events`:
/// link markers no tick picked up).
#[tauri::command]
pub async fn log_stop(
    hub: tauri::State<'_, AdapterHub>,
    state: tauri::State<'_, LiveLogState>,
) -> Result<Value, String> {
    stop_live(&hub, &state)
}

/// Milliseconds since the Unix epoch, for a per-tick timestamp. Avoids pulling in
/// a date crate: the frontend only uses it to order/stamp rows.
fn chrono_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::adapter::testkit::{SimBus, SimFault};
    use crate::adapter::Addr;
    use opentune_core::resilient::VirtualClock;
    use std::sync::atomic::Ordering as AtOrd;
    use opentune_core::ecudef::Endian;
    use opentune_logdef::{
        Addressing, Conversion, ConversionFit, EcuGroup, LogDef, LogParam, ProtocolGroup,
        ProtocolKind, SourceFormat,
    };
    use opentune_udssim::server::{DddidConfig, ServerConfig, UdsServer};

    fn did_param(id: &str, did: u16, factor: f64) -> LogParam {
        LogParam {
            id: id.into(),
            name: id.into(),
            description: String::new(),
            units: "u".into(),
            protocol: ProtocolKind::UdsMode22,
            addressing: Addressing::UdsDid { did },
            storagetype: None,
            endian: Endian::Big,
            value_kind: Default::default(),
            conversion: Some(Conversion {
                units: "u".into(),
                expr: "X".into(),
                format: None,
                fit: ConversionFit::Linear {
                    factor,
                    offset: 0.0,
                },
                gauge_min: None,
                gauge_max: None,
                gauge_step: None,
            }),
            conversions: Vec::new(),
            min: None,
            max: None,
            alternates: Vec::new(),
            depends_on: Vec::new(),
            convert_group: None,
            enabled: None,
            ssm: None,
        }
    }

    fn def3() -> LogDef {
        LogDef {
            format: SourceFormat::RomRaiderLogger,
            version: None,
            convert_factors: Vec::new(),
            protocols: vec![ProtocolGroup {
                id: "uds".into(),
                kind: ProtocolKind::UdsMode22,
                baud: None,
                ecus: vec![EcuGroup {
                    id: None,
                    name: None,
                    ecu_type: None,
                    params: vec![
                        did_param("rpm", 0x1000, 0.25),
                        did_param("ect", 0x1001, 1.0),
                        did_param("map", 0x1002, 1.0),
                    ],
                }],
            }],
            warnings: Vec::new(),
        }
    }

    /// A hub whose worker owns a simulated engine ECU (udssim, which
    /// implements `0x2C`), optionally without `0x2C`, or no ECU at all.
    fn hub(ecu: Option<bool>) -> AdapterHub {
        hub_with(ecu, Arc::new(SimFault::default()))
    }

    /// [`hub`] with faults the test can inject.
    fn hub_with(ecu: Option<bool>, fault: Arc<SimFault>) -> AdapterHub {
        AdapterHub::spawn(move || {
            let bus = SimBus {
                fault,
                ..SimBus::default()
            };
            if let Some(dddid) = ecu {
                let mut s = UdsServer::with_config(ServerConfig {
                    dddid: DddidConfig {
                        supported: dddid,
                        ..DddidConfig::default()
                    },
                    ..ServerConfig::default()
                });
                s.set_did(0x1000, vec![0x1A, 0xF8]); // 6904 x 0.25 = 1726
                s.set_did(0x1001, vec![0x5A]);
                s.set_did(0x1002, vec![0x00, 0x65]);
                bus.add(0x7E0, 0x7E8, s);
            }
            bus.opener()
        })
    }

    fn key() -> ConnKey {
        ConnKey::new("sim", None, None, Some("hs"), Addr::ENGINE)
    }

    fn request(fast: bool, idle: Duration) -> LiveRequest {
        LiveRequest {
            key: key(),
            target: Addr::ENGINE.json(false),
            timeout_ms: 50,
            def: def3(),
            ids: Vec::new(),
            cfg: fast_config(fast, None, None, None).unwrap(),
            idle,
            ssm: None,
            policy: live_policy(false),
            clock: Arc::new(VirtualClock::new(1_700_000_000_000)),
        }
    }

    /// Like [`request`] with reconnect on and a short backoff, on `clock`.
    fn reconnecting(fast: bool, clock: &VirtualClock) -> LiveRequest {
        LiveRequest {
            policy: ReconnectPolicy {
                initial_backoff_ms: 100,
                max_backoff_ms: 400,
                ..live_policy(true)
            },
            clock: Arc::new(clock.clone()),
            ..request(fast, Duration::from_secs(10))
        }
    }

    fn start(hub: &AdapterHub, state: &LiveLogState, req: LiveRequest) -> Started {
        await_live_start(begin_live(hub, state, req).unwrap()).unwrap()
    }

    /// The link markers (`paramId` `link`) of a tick: their states.
    fn markers(v: &Value) -> Vec<String> {
        v["samples"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|s| s["paramId"] == opentune_logdef::LINK_PARAM_ID)
                    .map(|s| s["text"].as_str().unwrap_or("").to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Wait until the hub's busy slot is free again.
    fn released(hub: &AdapterHub) -> bool {
        let t = Instant::now();
        while hub.current().is_some() && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        hub.run("probe", |_s| Ok(())).is_ok()
    }

    fn value(v: &Value, id: &str) -> f64 {
        v["samples"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["paramId"] == id)
            .and_then(|s| s["value"].as_f64())
            .unwrap_or(f64::NAN)
    }

    #[test]
    fn fast_session_holds_the_adapter_and_clears_its_dynamic_dids() {
        let hub = hub(Some(true));
        let state = LiveLogState::default();
        assert!(tick_live(&state).is_err(), "no session yet");
        let st = start(&hub, &state, request(true, Duration::from_secs(10))).status;
        assert_eq!(st.mode, opentune_logdef::LogMode::Fast2c, "{:?}", st.reason);
        assert_eq!(st.dynamic_dids.len(), 1);
        assert_eq!(st.dynamic_dids[0].did, 0xF200);
        // The session owns the adapter: other panels get "adapter busy".
        let e = hub.run("OBD live", |_s| Ok(())).unwrap_err();
        assert!(e.contains("adapter busy") && e.contains(LIVE_OP), "{e}");
        // A second start is refused while one runs.
        assert!(begin_live(&hub, &state, request(true, Duration::from_secs(10))).is_err());

        for _ in 0..3 {
            let t = tick_live(&state).unwrap();
            assert_eq!(t["count"], 3);
            assert!((value(&t, "rpm") - 1726.0).abs() < 1e-9, "{t}");
            assert_eq!(value(&t, "ect"), 90.0);
            assert_eq!(value(&t, "map"), 101.0);
            assert_eq!(t["status"]["mode"], "fast-2c");
            assert_eq!(t["rate"]["requests_per_sample"], 1.0, "one 0x22 per sample");
        }
        let sum = stop_live(&hub, &state).unwrap();
        assert_eq!(sum["running"], false);
        assert_eq!(sum["rate"]["ticks"], 3);
        assert!(sum["ended"].is_null(), "a requested stop is not an error");
        assert!(tick_live(&state).is_err(), "ticks after stop are refused");
        // The adapter is free again and the dynamic DID is gone on the ECU.
        let r = hub
            .run("check", |s| {
                s.with_diag(&key(), 50, |d| {
                    opentune_core::RawDiag::request(d, &[0x22, 0xF2, 0x00])
                })
            })
            .unwrap();
        assert_eq!(r, vec![0x7F, 0x22, 0x31], "DDDID cleared by stop");
    }

    #[test]
    fn ecu_without_0x2c_falls_back_to_polling_with_a_reason() {
        let hub = hub(Some(false));
        let state = LiveLogState::default();
        let st = start(&hub, &state, request(true, Duration::from_secs(10))).status;
        assert_eq!(st.mode, opentune_logdef::LogMode::Polling);
        let why = st.reason.clone().unwrap_or_default();
        assert!(why.contains("0x11"), "{why}");
        let t = tick_live(&state).unwrap();
        assert!((value(&t, "rpm") - 1726.0).abs() < 1e-9);
        assert_eq!(t["status"]["mode"], "polling");
        assert_eq!(t["rate"]["requests_per_sample"], 3.0);
        stop_live(&hub, &state).unwrap();
        assert!(hub.run("after", |_s| Ok(1)).is_ok());
    }

    #[test]
    fn plain_polling_session_when_fast_is_off() {
        let hub = hub(Some(true));
        let state = LiveLogState::default();
        let st = start(&hub, &state, request(false, Duration::from_secs(10))).status;
        assert_eq!(st.reason.as_deref(), Some("fast mode not requested"));
        stop_live(&hub, &state).unwrap();
    }

    #[test]
    fn dropped_connection_and_idle_panel_stop_the_session() {
        // Nobody answers: start falls back to polling, ticks carry notes only,
        // and after DEAD_TICKS silent ticks the session ends and frees the adapter.
        let hub = hub(None);
        let state = LiveLogState::default();
        await_live_start(begin_live(&hub, &state, request(true, Duration::from_secs(10))).unwrap())
            .unwrap();
        for n in 1..=DEAD_TICKS {
            let t = tick_live(&state).unwrap();
            assert!(!t["samples"][0]["note"].as_str().unwrap_or("").is_empty());
            if n < DEAD_TICKS {
                assert_eq!(t["link"]["state"], "silent", "{t}");
                assert_eq!(t["link"]["dead_ticks"], n);
            } else {
                // The tick that lost the link carries the marker.
                assert_eq!(markers(&t), ["lost"], "{t}");
            }
        }
        let run = state.current().unwrap();
        assert!(run.wait_done(Duration::from_secs(5)));
        let e = tick_live(&state).unwrap_err();
        assert!(e.contains("connection dropped"), "{e}");
        assert!(e.contains("Reconnect"), "{e}");
        assert!(released(&hub), "adapter released");

        // A panel that stops ticking (closed) ends the session on its own.
        let hub = hub_idle();
        let state = LiveLogState::default();
        await_live_start(
            begin_live(&hub, &state, request(true, Duration::from_millis(150))).unwrap(),
        )
        .unwrap();
        assert!(state.current().unwrap().wait_done(Duration::from_secs(5)));
        let sum = stop_live(&hub, &state).unwrap();
        assert!(
            sum["ended"].as_str().unwrap().contains("panel closed"),
            "{sum}"
        );
    }

    fn hub_idle() -> AdapterHub {
        hub(Some(true))
    }

    #[test]
    fn fast_options_are_validated() {
        let c = fast_config(true, Some("0xF210"), Some(16), Some("03")).unwrap();
        assert_eq!(
            (c.first_did, c.max_record_len, c.session),
            (0xF210, 16, Some(3))
        );
        assert!(fast_config(true, Some("F190"), None, None)
            .unwrap_err()
            .contains("outside"));
        assert!(fast_config(true, Some("zz"), None, None).is_err());
        assert!(fast_config(true, None, Some(0), None).is_err());
        assert!(fast_config(true, None, None, Some("1FF")).is_err());
        assert!(!fast_config(false, None, None, None).unwrap().enabled);
    }

    /// A definition pack's JSON logger loads through the same path as an XML
    /// `logger.xml`, so the Live log panel can open `packs/*/loggers/*.json`.
    #[test]
    fn loads_a_pack_json_logger() {
        let p = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../packs/generic-obd2/loggers/obd2-mode01.json"
        );
        let def = load_def(p).unwrap();
        assert!(def.param_count() > 20);
        let v = log_params(p.to_string()).unwrap();
        assert!(v.to_string().contains("obd01_0c_rpm"));
    }

    /// An ECU that refuses every channel (NRC 0x31) answers: the ticks are
    /// alive, the link never counts as lost and the session keeps running.
    #[test]
    fn all_nrc_ticks_are_alive() {
        let hub = AdapterHub::spawn(|| {
            let bus = SimBus::default();
            bus.add(0x7E0, 0x7E8, UdsServer::new()); // no DIDs: 7F 22 31
            bus.opener()
        });
        let state = LiveLogState::default();
        start(&hub, &state, request(false, Duration::from_secs(10)));
        for _ in 0..DEAD_TICKS + 2 {
            let t = tick_live(&state).unwrap();
            assert!(t["samples"][0]["value"].is_null());
            assert_eq!(t["link"]["state"], "up", "{t}");
            assert!(markers(&t).is_empty());
        }
        assert!(state.current().unwrap().info().running);
        stop_live(&hub, &state).unwrap();
    }

    /// The stateless poll path: an all-NRC poll keeps the opened adapter, a
    /// silent one drops it, an adapter reset is reported as `lost`.
    #[test]
    fn stateless_poll_counts_nrcs_as_alive() {
        let fault = Arc::new(SimFault::default());
        let f2 = fault.clone();
        let hub = AdapterHub::spawn(move || {
            let bus = SimBus {
                fault: f2,
                ..SimBus::default()
            };
            bus.add(0x7E0, 0x7E8, UdsServer::new());
            bus.opener()
        });
        let poll = |hub: &AdapterHub| {
            hub.run("poll", |s| poll_on(s, &key(), 50, &def3(), &[]))
                .unwrap()
        };
        for _ in 0..3 {
            let (samples, link) = poll(&hub);
            assert!(samples.iter().all(|s| !s.answered()), "NRCs only");
            assert_eq!(link.state, "up");
        }
        assert_eq!(fault.opened.load(AtOrd::SeqCst), 1, "NRCs keep the adapter");
        fault.reset_next.store(true, AtOrd::SeqCst);
        let (_, link) = poll(&hub);
        assert_eq!(link.state, "lost");
        assert!(link.error.unwrap().contains("adapter reset"));
        fault.dead_until_reopen.store(true, AtOrd::SeqCst);
        fault.refuse_opens.store(0, AtOrd::SeqCst);
        let (_, link) = poll(&hub); // re-opens (clears the fault), answers
        assert_eq!(link.state, "up");
        assert_eq!(fault.opened.load(AtOrd::SeqCst), 2);
    }

    /// Reconnect: the adapter is unplugged mid-session; after DEAD_TICKS dead
    /// ticks the link is lost, re-opened through the hub (two refused opens,
    /// then success) with backoff on the virtual clock, the dynamic DID is
    /// defined again and values resume. Every event reaches the panel as a
    /// marker sample, in order.
    #[test]
    fn reconnect_reopens_through_the_hub_and_redefines() {
        let fault = Arc::new(SimFault::default());
        let hub = hub_with(Some(true), fault.clone());
        let state = LiveLogState::default();
        let clock = VirtualClock::new(1_700_000_000_000);
        let st = start(&hub, &state, reconnecting(true, &clock)).status;
        assert_eq!(st.mode, opentune_logdef::LogMode::Fast2c);
        let t = tick_live(&state).unwrap();
        assert!((value(&t, "rpm") - 1726.0).abs() < 1e-9);
        assert_eq!(t["link"]["reconnect"], true);

        fault.refuse_opens.store(2, AtOrd::SeqCst);
        fault.dead_until_reopen.store(true, AtOrd::SeqCst);
        let mut seen = Vec::new();
        let mut states = Vec::new();
        let mut resumed = false;
        for _ in 0..60 {
            let t = tick_live(&state).unwrap();
            seen.extend(markers(&t));
            states.push(t["link"]["state"].as_str().unwrap_or("").to_string());
            if seen.contains(&"reconnected".to_string()) && value(&t, "rpm") > 0.0 {
                resumed = true;
                assert_eq!(t["status"]["mode"], "fast-2c", "re-defined after the reopen");
                assert_eq!(t["link"]["state"], "up");
                assert_eq!(t["link"]["reconnects"], 1);
                assert_eq!(t["link"]["drops"], 1);
                break;
            }
        }
        assert!(resumed, "values resumed: markers {seen:?}, states {states:?}");
        assert_eq!(seen, ["lost", "reopen-failed", "reopen-failed", "reconnected"]);
        assert!(states.iter().any(|s| s == "silent"), "{states:?}");
        assert_eq!(fault.opened.load(AtOrd::SeqCst), 2, "one initial open + one reopen");
        // Backoff on the virtual clock: never a hot spin.
        assert!(clock.sleeps().iter().all(|&ms| ms > 0));
        assert!(clock.slept_ms() >= 100 + 200 + 400, "{:?}", clock.sleeps());
        let sum = stop_live(&hub, &state).unwrap();
        assert!(sum["ended"].is_null(), "{sum}");
        assert_eq!(sum["link"]["reconnects"], 1);
        let r = hub
            .run("check", |s| {
                s.with_diag(&key(), 50, |d| {
                    opentune_core::RawDiag::request(d, &[0x22, 0xF2, 0x00])
                })
            })
            .unwrap();
        assert_eq!(r, vec![0x7F, 0x22, 0x31], "DDDID cleared by stop");
    }

    /// An adapter reset counts as lost at once; without reconnect the session
    /// stops on that tick, which carries the marker.
    #[test]
    fn adapter_reset_without_reconnect_stops_at_once() {
        let fault = Arc::new(SimFault::default());
        let hub = hub_with(Some(true), fault.clone());
        let state = LiveLogState::default();
        start(&hub, &state, request(true, Duration::from_secs(10)));
        tick_live(&state).unwrap();
        fault.reset_next.store(true, AtOrd::SeqCst);
        let t = tick_live(&state).unwrap();
        assert_eq!(markers(&t), ["lost"], "{t}");
        assert!(state.current().unwrap().wait_done(Duration::from_secs(5)));
        let e = tick_live(&state).unwrap_err();
        assert!(e.contains("adapter reset") && e.contains("session stopped"), "{e}");
        assert!(released(&hub));
    }

    /// Reconnect is bounded: when the adapter opens but nothing ever answers
    /// (ignition off), the session stops after MAX_HOLLOW_RECONNECTS.
    #[test]
    fn hollow_reconnects_end_the_session() {
        let hub = hub(None);
        let state = LiveLogState::default();
        let clock = VirtualClock::new(0);
        start(&hub, &state, reconnecting(false, &clock));
        let mut seen = Vec::new();
        let mut err = None;
        for _ in 0..200 {
            match tick_live(&state) {
                Ok(t) => seen.extend(markers(&t)),
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        let e = err.expect("the session ended");
        assert!(e.contains("re-opened 3 time(s)"), "{e}");
        let lost = seen.iter().filter(|m| *m == "lost").count();
        assert_eq!(lost as u32, MAX_HOLLOW_RECONNECTS, "{seen:?}");
        assert!(released(&hub));
    }

    fn ssm_def() -> opentune_logdef::LogDef {
        load_def(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/opentune-logdef/tests/fixtures/ssm_demo_logger.xml"
        ))
        .unwrap()
    }

    /// The production opener with the `ssmsim` pseudo-driver: offline.
    fn ssm_hub() -> AdapterHub {
        AdapterHub::spawn(|| -> crate::adapter::Opener {
            Box::new(|k: &ConnKey| crate::drivers::open_registry_driver(k))
        })
    }

    fn ssm_req(transport: &str, target: &str, ids: &[&str]) -> LiveRequest {
        let r = ssm_request(Some(transport), Some(target), None)
            .unwrap()
            .unwrap();
        let (key, target) = ssm_key(SSM_SIM_DRIVER, None, None, None, &r);
        LiveRequest {
            key,
            target,
            def: ssm_def(),
            ids: ids.iter().map(|s| s.to_string()).collect(),
            cfg: FastLogConfig {
                ssm: Some(SsmPollConfig {
                    max_packet_len: r.max_packet_len,
                }),
                ..FastLogConfig::default()
            },
            ssm: Some(r),
            ..request(false, Duration::from_secs(10))
        }
    }

    /// SSM over the simulated module, K-line and CAN: BF identification,
    /// the capability selection with its reasons, batched A8 values.
    #[test]
    fn ssm_session_over_the_simulator() {
        for transport in ["ssm", "ssm-can"] {
            let hub = ssm_hub();
            let state = LiveLogState::default();
            let st = start(&hub, &state, ssm_req(transport, "ecu", &[]));
            assert_eq!(st.status.mode, opentune_logdef::LogMode::Ssm, "{transport}");
            let ssm = st.ssm.clone().unwrap();
            assert_eq!(ssm["rom_id"], "5A5A000001", "{ssm}");
            assert_eq!(ssm["target"], "ecu");
            let notes = ssm["notes"].to_string();
            assert!(notes.contains("capability bit clear): P_NOPE"), "{notes}");
            assert!(notes.contains("other module"), "{notes}");
            let ids: Vec<&str> = st.params.iter().filter_map(|p| p["id"].as_str()).collect();
            assert!(ids.contains(&"P_RPM") && !ids.contains(&"P_NOPE"), "{ids:?}");
            let t = tick_live(&state).unwrap();
            assert_eq!(t["status"]["mode"], "ssm-a8");
            assert!(value(&t, "P_RPM").is_finite(), "{t}");
            assert_eq!(t["link"]["state"], "up");
            let sum = stop_live(&hub, &state).unwrap();
            assert!(sum["ended"].is_null(), "{sum}");
            assert_eq!(sum["ssm"]["rom_id"], "5A5A000001");
        }
    }

    /// The TCU over CAN, and the requested ids that are not logged.
    #[test]
    fn ssm_tcu_and_not_logged_ids() {
        let hub = ssm_hub();
        let state = LiveLogState::default();
        let st = start(&hub, &state, ssm_req("ssm-can", "tcu", &["P_TCU", "P_ECT"]));
        let ssm = st.ssm.unwrap();
        assert_eq!(ssm["target"], "tcu");
        assert_eq!(ssm["not_logged"], json!(["P_ECT"]), "{ssm}");
        assert_eq!(st.params.len(), 1);
        assert_eq!(st.params[0]["id"], "P_TCU");
        stop_live(&hub, &state).unwrap();

        // Nothing selected is supported: a start error that says why.
        let e = await_live_start(
            begin_live(&hub, &state, ssm_req("ssm", "ecu", &["P_NOPE"])).unwrap(),
        )
        .unwrap_err();
        assert!(e.contains("P_NOPE") && e.contains("5A5A000001"), "{e}");
        assert!(released(&hub));
    }

    #[test]
    fn ssm_options_are_checked() {
        assert_eq!(ssm_request(None, None, None).unwrap(), None);
        assert_eq!(ssm_request(Some("uds"), Some("tcu"), None).unwrap(), None);
        let r = ssm_request(Some("ssm"), None, None).unwrap().unwrap();
        assert_eq!((r.wire, r.target), (SsmWire::KLine, SsmTarget::Ecu));
        assert!(ssm_request(Some("kwp"), None, None).is_err());
        assert!(ssm_request(Some("ssm"), Some("abs"), None).is_err());
        assert!(ssm_request(Some("ssm"), None, Some(9)).is_err());
        let (k, t) = ssm_key("kkl", Some("/dev/ttyUSB0"), None, None, &r);
        let cfg = k.config().unwrap();
        assert_eq!(cfg.tx_id, 0x10);
        assert_eq!(cfg.options.get("init").map(String::as_str), Some("none"));
        assert_eq!(cfg.options.get("baud").map(String::as_str), Some("4800"));
        assert_eq!(t["wire"], "k-line");
        // The simulator speaks SSM only.
        let hub = ssm_hub();
        let state = LiveLogState::default();
        let mut req = request(false, Duration::from_secs(1));
        req.key = ConnKey::new(SSM_SIM_DRIVER, None, None, None, Addr::ENGINE);
        assert!(begin_live(&hub, &state, req)
            .unwrap_err()
            .contains("SSM transport"));
    }
}
