//! # Live datalogging panel (backend)
//!
//! Two Tauri commands that back the "Live log" panel. The live loop is driven
//! from the frontend, which calls [`log_poll_once`] on an interval. Each poll
//! goes through the app-wide adapter hub ([`crate::adapter`]), which reuses the
//! already-opened adapter between ticks (no full ELM327 re-init per tick) and
//! rejects the tick with "adapter busy" while another panel owns the adapter.
//!
//! Parameter definitions are RomRaider-style `logger.xml` files parsed by
//! [`opentune_logdef`], or a definition pack's `loggers/*.json` (the same
//! [`opentune_logdef::LogDef`] model, serialised as JSON). UDS `0x22` DIDs,
//! OBD-II mode-01 PIDs and UDS `0x23` memory reads are driven (see
//! [`opentune_logdef::poll_once`]); anything else comes back with an
//! explanatory `note` rather than a value, which the UI shows verbatim.
//!
//! ## Fast mode (UDS `0x2C`)
//!
//! [`log_start`] / [`log_tick`] / [`log_stop`] drive an
//! [`opentune_logdef::LiveSession`] (docs/LIVE-LOGGING.md): the channels are
//! packed into dynamically defined DIDs where the ECU allows it, with a
//! per-channel polling fallback and the reason. Because the dynamic DIDs live
//! in the ECU's diagnostic session, the session is **one long-running adapter
//! operation** ([`AdapterHub::start_op`]): it holds the adapter until it stops,
//! so another panel cannot change the session underneath it. The job owns the
//! `LiveSession` on the adapter worker and serves ticks one at a time over a
//! channel (ticks can never overlap). It stops — clearing the dynamic DIDs —
//! on [`log_stop`], when no tick arrives for [`IDLE_STOP`] (panel closed,
//! window gone) and when no channel answers for [`DEAD_TICKS`] ticks in a row
//! (connection dropped).

use crate::adapter::{AdapterHub, ConnKey, Session};
use opentune_logdef::{FastLogConfig, LiveSession, LiveStatus, Sample};
use serde_json::{json, Value};
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
/// dropped. When no parameter produced a value the cached adapter is dropped so
/// the next tick re-opens it (recovers from an unplugged / reset adapter).
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

    let samples = hub.run("live log", move |s| {
        let samples = s.with_diag(&key, timeout_ms, |d| {
            // poll_once wants &[&str]; borrow the owned ids.
            let id_refs: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
            Ok(opentune_logdef::poll_once(d, &def, &id_refs))
        })?;
        if !samples.is_empty() && !samples.iter().any(Sample::answered) {
            s.invalidate();
        }
        Ok(samples)
    })?;

    let rows: Vec<Value> = samples.iter().map(sample_json).collect();

    Ok(json!({
        "ts": chrono_millis(),
        "count": rows.len(),
        "samples": rows,
        "target": addr.json(assumed),
    }))
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
// Fast mode: a LiveSession held on the adapter hub
// ---------------------------------------------------------------------------

/// Busy-slot name other panels see while a live session holds the adapter.
pub const LIVE_OP: &str = "live log session";
/// A session with no tick for this long stops itself (the panel was closed).
pub const IDLE_STOP: Duration = Duration::from_secs(15);
/// Consecutive ticks in which no channel answered before the session treats
/// the connection as dropped and stops.
pub const DEAD_TICKS: u32 = 5;
/// How long `log_start` waits for the plan + define (probes every DID first).
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// How long `log_stop` waits for the dynamic DIDs to be cleared.
const STOP_TIMEOUT: Duration = Duration::from_secs(15);

/// A request to the session job.
enum Cmd {
    Tick(mpsc::Sender<Result<(Vec<Sample>, LiveStatus), String>>),
    Stop,
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
    channels: usize,
}

impl LiveRun {
    fn info(&self) -> std::sync::MutexGuard<'_, RunInfo> {
        self.info.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set_status(&self, st: &LiveStatus) {
        self.info().status = Some(st.clone());
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
            "channels": self.channels,
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
        })
    }
}

/// Tauri-managed live-session state: the current (or last) fast session.
#[derive(Default)]
pub struct LiveLogState {
    cur: Mutex<Option<Arc<LiveRun>>>,
}

impl LiveLogState {
    fn current(&self) -> Option<Arc<LiveRun>> {
        self.cur.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
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
}

/// Validate, then dispatch the session onto the hub. Returns the channel that
/// reports the status after plan + define (or the start error). Never blocks
/// on the adapter.
pub fn begin_live(
    hub: &AdapterHub,
    state: &LiveLogState,
    req: LiveRequest,
) -> Result<mpsc::Receiver<Result<LiveStatus, String>>, String> {
    let mut slot = state.cur.lock().unwrap_or_else(|e| e.into_inner());
    if slot.as_ref().is_some_and(|r| r.info().running) {
        return Err("a live log session is already running — stop it first".into());
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
            ..RunInfo::default()
        }),
        done: (Mutex::new(false), Condvar::new()),
        target: req.target.clone(),
        timeout_ms: req.timeout_ms,
        channels: live.params().len(),
    });
    let (stx, srx) = mpsc::channel();
    let job_run = run.clone();
    let job = Job {
        key: req.key,
        timeout_ms: req.timeout_ms,
        idle: req.idle,
    };
    hub.start_op(LIVE_OP, move |s: &mut Session| {
        live_job(s, job_run, job, live, rx, stx)
    })?;
    *slot = Some(run);
    Ok(srx)
}

/// Wait (blocking) for the session's start result.
pub fn await_live_start(
    rx: mpsc::Receiver<Result<LiveStatus, String>>,
) -> Result<LiveStatus, String> {
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

/// The adapter binding and timing a session job runs with.
struct Job {
    key: ConnKey,
    timeout_ms: u64,
    idle: Duration,
}

/// Runs on the adapter worker for the whole session.
fn live_job(
    s: &mut Session,
    run: Arc<LiveRun>,
    job: Job,
    mut live: LiveSession,
    rx: mpsc::Receiver<Cmd>,
    started: mpsc::Sender<Result<LiveStatus, String>>,
) {
    let Job {
        key,
        timeout_ms,
        idle,
    } = job;
    match s.with_diag(&key, timeout_ms, |d| Ok(live.start(d).clone())) {
        Ok(st) => {
            {
                let mut i = run.info();
                i.status = Some(st.clone());
                i.t0 = Some(Instant::now());
            }
            let _ = started.send(Ok(st));
        }
        Err(e) => {
            // Finished BEFORE the error is reported, so a status read right
            // after a failed start never still says "running".
            run.finish(Some(e.clone()));
            let _ = started.send(Err(e));
            return;
        }
    }
    let mut dead = 0u32;
    let ended = loop {
        match rx.recv_timeout(idle) {
            Ok(Cmd::Tick(reply)) => match s.with_diag(&key, timeout_ms, |d| Ok(live.tick(d))) {
                Ok(samples) => {
                    let st = live.status().clone();
                    run.set_status(&st);
                    let silent = !samples.is_empty() && !samples.iter().any(Sample::answered);
                    dead = if silent { dead + 1 } else { 0 };
                    let _ = reply.send(Ok((samples, st)));
                    if dead >= DEAD_TICKS {
                        break Some(format!(
                            "no channel answered for {DEAD_TICKS} ticks in a row (connection \
                             dropped or ignition off) — session stopped"
                        ));
                    }
                }
                Err(e) => {
                    let _ = reply.send(Err(e.clone()));
                    break Some(format!("adapter error: {e} — session stopped"));
                }
            },
            Ok(Cmd::Stop) => break None,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                break Some(format!(
                    "no tick for {:.1} s (panel closed?) — session stopped",
                    idle.as_secs_f64()
                ))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break Some("session handle dropped — session stopped".into())
            }
        }
    };
    // Clear every dynamic DID this session defined (best effort).
    if let Ok(st) = s.with_diag(&key, timeout_ms, |d| Ok(live.stop(d).clone())) {
        run.set_status(&st);
    }
    if ended.is_some() {
        // Start the next operation from a fresh open.
        s.invalidate();
    }
    run.finish(ended);
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
    let (rtx, rrx) = mpsc::channel();
    run.tx
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .send(Cmd::Tick(rtx))
        .map_err(|_| "the live session has ended".to_string())?;
    // A tick sends at most one request per channel plus a re-define.
    let wait = Duration::from_millis(run.timeout_ms.saturating_mul(run.channels as u64 + 8))
        + Duration::from_secs(2);
    let (samples, status) = match rrx.recv_timeout(wait) {
        Ok(r) => r?,
        Err(mpsc::RecvTimeoutError::Timeout) => return Err("live tick timed out".into()),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let why = run.info().ended.clone().unwrap_or_default();
            return Err(format!("the live session has ended: {why}"));
        }
    };
    let rows: Vec<Value> = samples.iter().map(sample_json).collect();
    let rate = run.rate();
    let (running, ended) = {
        let i = run.info();
        (i.running, i.ended.clone())
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

/// Start a live session over the selected parameters: fast (`0x2C`) when
/// `fast` and the ECU allows it, else per-channel polling (with the reason).
/// Holds the adapter until [`log_stop`]. Returns `{ status, channels, target }`.
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
    hub: tauri::State<'_, AdapterHub>,
    state: tauri::State<'_, LiveLogState>,
) -> Result<Value, String> {
    let def = load_def(&def_path)?;
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
    let target = addr.json(assumed);
    let req = LiveRequest {
        key,
        target: target.clone(),
        timeout_ms,
        def,
        ids,
        cfg,
        idle: IDLE_STOP,
    };
    let rx = begin_live(&hub, &state, req)?;
    let status = tauri::async_runtime::spawn_blocking(move || await_live_start(rx))
        .await
        .map_err(|e| format!("live start task failed: {e}"))??;
    let channels = state.current().map(|r| r.channels).unwrap_or(0);
    Ok(json!({ "status": status, "channels": channels, "target": target }))
}

/// One sample of every channel from the running session:
/// `{ ts, samples, status, rate, running, ended, target }`.
#[tauri::command]
pub async fn log_tick(state: tauri::State<'_, LiveLogState>) -> Result<Value, String> {
    tick_live(&state)
}

/// Stop the running session, clear its dynamic DIDs and release the adapter:
/// `{ running, status, ended, rate, target }`.
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

    use crate::adapter::testkit::SimBus;
    use crate::adapter::Addr;
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
        AdapterHub::spawn(move || {
            let bus = SimBus::default();
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
        }
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
        let st = await_live_start(
            begin_live(&hub, &state, request(true, Duration::from_secs(10))).unwrap(),
        )
        .unwrap();
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
        let st = await_live_start(
            begin_live(&hub, &state, request(true, Duration::from_secs(10))).unwrap(),
        )
        .unwrap();
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
        let st = await_live_start(
            begin_live(&hub, &state, request(false, Duration::from_secs(10))).unwrap(),
        )
        .unwrap();
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
        for _ in 0..DEAD_TICKS {
            let t = tick_live(&state).unwrap();
            assert!(!t["samples"][0]["note"].as_str().unwrap_or("").is_empty());
        }
        let run = state.current().unwrap();
        assert!(run.wait_done(Duration::from_secs(5)));
        let e = tick_live(&state).unwrap_err();
        assert!(e.contains("connection dropped"), "{e}");
        let t = Instant::now();
        while hub.current().is_some() && t.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(hub.run("probe", |_s| Ok(())).is_ok(), "adapter released");

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
}
