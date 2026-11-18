//! # CAN capture panel (backend)
//!
//! Records every frame on the bus to a file (candump log / CSV / Vector ASC)
//! through the driver's passive [`CanMonitor`](opentune_core::CanMonitor),
//! listen-only where the adapter supports it.
//!
//! A capture is one long-running operation on the app-wide adapter hub
//! ([`AdapterHub::start_op`]): it holds the adapter for its whole duration, so
//! every other panel gets "adapter busy" until it ends. The driver is created
//! fresh from the registry and **never `open`ed** — opening a diagnostic
//! session could transmit; the monitor opens the hardware itself. Live
//! counters live in [`CandumpState`], outside the hub, so `candump_status` and
//! `candump_stop` never wait on the busy adapter; `candump_stop` ends the
//! capture through a stop flag the capture loop checks every 100 ms.
//!
//! Commands (all `async`):
//! - `candump_start({driver, port?, baud?, bus?, path, format, filterIds?,
//!   durationS?, bitrate?}) -> {ok: true}`
//! - `candump_status() -> {running, path, format, frames, bytes, fps, elapsed_s,
//!   dropped, ids: [{id, ext, count, hz, dlc, last}], error, adapter, …}`
//! - `candump_stop() -> {path, frames, bytes, elapsed_s, dropped}`

use crate::adapter::{AdapterHub, Session};
use opentune_candump::{
    monitor_opts, run_capture, CaptureParams, CaptureStats, Format, FrameWriter,
};
use opentune_core::{Interface, MonitorOpts};
use serde_json::{json, Value};
use std::io::BufWriter;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Busy-slot name shown to other panels while a capture runs.
pub const CAPTURE_OP: &str = "CAN capture";
/// Most per-id rows `candump_status` returns.
pub const MAX_IDS: usize = 256;
/// How long `candump_start` waits for the adapter to come up (ELM327 reset +
/// AT setup takes a few seconds).
const START_TIMEOUT: Duration = Duration::from_secs(20);
/// How long `candump_stop` waits for the capture loop to finish the file.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Builds an *unopened* driver by id.
pub type MakeDriver = fn(&str) -> Option<Box<dyn Interface>>;

fn registry_driver(id: &str) -> Option<Box<dyn Interface>> {
    crate::drivers::registry().make(id)
}

#[derive(Default)]
struct Live {
    running: bool,
    /// Set once the monitor is capturing.
    t0: Option<Instant>,
    /// Final duration, once finished.
    final_elapsed: Option<f64>,
    bytes: u64,
    error: Option<String>,
    adapter: String,
    listen_only: String,
}

/// One capture's shared state (the capture job writes, commands read).
pub struct Capture {
    path: String,
    format: Format,
    stop: AtomicBool,
    stats: Mutex<CaptureStats>,
    live: Mutex<Live>,
    done: (Mutex<bool>, Condvar),
}

impl Capture {
    fn live(&self) -> std::sync::MutexGuard<'_, Live> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn elapsed_s(&self) -> f64 {
        let l = self.live();
        l.final_elapsed
            .or_else(|| l.t0.map(|t| t.elapsed().as_secs_f64()))
            .unwrap_or(0.0)
    }

    fn finish(&self, error: Option<String>) {
        {
            let mut l = self.live();
            l.running = false;
            if l.final_elapsed.is_none() {
                l.final_elapsed = Some(l.t0.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0));
            }
            if error.is_some() {
                l.error = error;
            }
        }
        let (m, cv) = &self.done;
        *m.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cv.notify_all();
    }

    /// Wait until the capture job has finished, up to `timeout`.
    fn wait_done(&self, timeout: Duration) -> bool {
        let (m, cv) = &self.done;
        let g = m.lock().unwrap_or_else(|e| e.into_inner());
        let (g, _) = cv
            .wait_timeout_while(g, timeout, |done| !*done)
            .unwrap_or_else(|e| e.into_inner());
        *g
    }

    /// The `candump_status` JSON.
    pub fn status(&self) -> Value {
        let elapsed = self.elapsed_s();
        let snap = self
            .stats
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot((elapsed * 1e6) as u64, MAX_IDS);
        let l = self.live();
        json!({
            "running": l.running,
            "path": self.path,
            "format": self.format.as_str(),
            "frames": snap.frames,
            "bytes": l.bytes,
            "payload_bytes": snap.payload_bytes,
            "fps": snap.fps,
            "elapsed_s": elapsed,
            "dropped": snap.dropped,
            "unique_ids": snap.unique_ids,
            "ids": snap.ids,
            "error": l.error,
            "adapter": l.adapter,
            "listen_only": l.listen_only,
        })
    }

    /// The `candump_stop` JSON.
    fn summary(&self) -> Value {
        let s = self.status();
        json!({
            "path": s["path"],
            "frames": s["frames"],
            "bytes": s["bytes"],
            "elapsed_s": s["elapsed_s"],
            "dropped": s["dropped"],
            "running": s["running"],
            "error": s["error"],
        })
    }
}

/// Tauri-managed capture state: the current (or last) capture.
#[derive(Default)]
pub struct CandumpState {
    cur: Mutex<Option<Arc<Capture>>>,
}

impl CandumpState {
    fn current(&self) -> Option<Arc<Capture>> {
        self.cur.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// What `candump_start` asks for.
pub struct StartRequest {
    pub driver: String,
    pub opts: MonitorOpts,
    pub path: String,
    pub format: Format,
    pub duration: Option<Duration>,
}

/// Validate, create the file and dispatch the capture onto the hub. Returns
/// the channel that reports the adapter description once the monitor runs
/// (or the start error). Never blocks on the adapter.
pub fn begin_capture(
    hub: &AdapterHub,
    state: &CandumpState,
    req: StartRequest,
    make: MakeDriver,
) -> Result<mpsc::Receiver<Result<String, String>>, String> {
    let mut slot = state.cur.lock().unwrap_or_else(|e| e.into_inner());
    if slot.as_ref().is_some_and(|c| c.live().running) {
        return Err("a CAN capture is already running — stop it first".into());
    }
    if req.path.trim().is_empty() {
        return Err("choose a file to save the capture to".into());
    }
    let file =
        std::fs::File::create(&req.path).map_err(|e| format!("create '{}': {e}", req.path))?;
    let cap = Arc::new(Capture {
        path: req.path.clone(),
        format: req.format,
        stop: AtomicBool::new(false),
        stats: Mutex::new(CaptureStats::new()),
        live: Mutex::new(Live {
            running: true,
            ..Live::default()
        }),
        done: (Mutex::new(false), Condvar::new()),
    });
    let (tx, rx) = mpsc::channel();
    let job_cap = cap.clone();
    let dispatched = hub.start_op(CAPTURE_OP, move |s: &mut Session| {
        capture_job(s, job_cap, req, file, make, tx)
    });
    if let Err(e) = dispatched {
        let _ = std::fs::remove_file(&cap.path);
        return Err(e);
    }
    *slot = Some(cap);
    Ok(rx)
}

/// Wait for the capture job's start result (blocking).
pub fn await_start(rx: mpsc::Receiver<Result<String, String>>) -> Result<String, String> {
    match rx.recv_timeout(START_TIMEOUT) {
        Ok(r) => r,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            Err("the adapter did not start capturing in time".into())
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err("capture aborted inside the adapter worker (internal error)".into())
        }
    }
}

/// Runs on the adapter worker for the whole capture.
fn capture_job(
    session: &mut Session,
    cap: Arc<Capture>,
    req: StartRequest,
    file: std::fs::File,
    make: MakeDriver,
    started: mpsc::Sender<Result<String, String>>,
) {
    // Release any cached diagnostic session: the monitor opens the port itself.
    session.invalidate();
    // Mark the capture finished BEFORE reporting the error, so a status read
    // right after `candump_start` fails never still says "running".
    let fail = |msg: String| {
        let _ = std::fs::remove_file(&cap.path);
        cap.finish(Some(msg.clone()));
        let _ = started.send(Err(msg));
    };
    let Some(mut drv) = make(&req.driver) else {
        drop(file);
        return fail(format!("unknown driver '{}'", req.driver));
    };
    let Some(mon) = drv.can_monitor() else {
        drop(file);
        return fail(format!(
            "driver '{}' cannot capture raw CAN traffic (use socketcan, gsusb, slcan, elm327obd, efp02c or mock)",
            req.driver
        ));
    };
    if let Err(e) = mon.start(&req.opts) {
        drop(file);
        return fail(format!("start capture on {}: {e}", req.driver));
    }
    let adapter = if mon.describe().is_empty() {
        req.driver.clone()
    } else {
        mon.describe()
    };
    {
        let mut l = cap.live();
        l.adapter = adapter.clone();
        l.listen_only = mon.listen_only().as_str().into();
        l.t0 = Some(Instant::now());
    }
    let _ = started.send(Ok(adapter));

    let mut writer = FrameWriter::new(
        BufWriter::new(file),
        req.format,
        &mon.channel(),
        mon.wall_base_us(),
    );
    let params = CaptureParams {
        opts: req.opts,
        duration: req.duration,
    };
    let tick_cap = cap.clone();
    let mut tick = move |bytes: u64| tick_cap.live().bytes = bytes;
    let res = run_capture(mon, &mut writer, &cap.stats, &cap.stop, &params, &mut tick);
    cap.live().bytes = writer.bytes_written();
    cap.finish(res.err().map(|e| format!("capture stopped: {e}")));
}

/// Request a stop and wait (blocking) for the file to be finished and the
/// adapter released. Works whatever state the capture is in.
pub fn stop_capture(hub: &AdapterHub, state: &CandumpState) -> Result<Value, String> {
    let cap = state
        .current()
        .ok_or_else(|| "no CAN capture has been started".to_string())?;
    cap.stop.store(true, Ordering::Relaxed);
    if cap.wait_done(STOP_TIMEOUT) {
        // The busy slot is freed right after the job returns; wait for it so
        // the next panel operation does not race into "adapter busy".
        let t = Instant::now();
        while hub.current().as_deref() == Some(CAPTURE_OP) && t.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    Ok(cap.summary())
}

/// Status of the current (or last) capture; an idle shape before the first.
pub fn capture_status(state: &CandumpState) -> Value {
    match state.current() {
        Some(c) => c.status(),
        None => json!({
            "running": false,
            "path": null,
            "format": null,
            "frames": 0,
            "bytes": 0,
            "payload_bytes": 0,
            "fps": 0.0,
            "elapsed_s": 0.0,
            "dropped": 0,
            "unique_ids": 0,
            "ids": [],
            "error": null,
            "adapter": "",
            "listen_only": null,
        }),
    }
}

/// Start a capture to `path`. Refused while another capture (or any other
/// adapter operation) is running. Returns once the adapter is capturing.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn candump_start(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    path: String,
    format: String,
    filter_ids: Option<Vec<u32>>,
    duration_s: Option<u32>,
    bitrate: Option<u32>,
    hub: tauri::State<'_, AdapterHub>,
    state: tauri::State<'_, CandumpState>,
) -> Result<Value, String> {
    let format: Format = format.parse()?;
    let filter = filter_ids.unwrap_or_default();
    if let Some(bad) = filter.iter().find(|&&id| id > 0x1FFF_FFFF) {
        return Err(format!("filter id {bad:X} is larger than 29 bits"));
    }
    let req = StartRequest {
        opts: monitor_opts(
            port.as_deref(),
            baud.as_deref(),
            bus.as_deref(),
            bitrate.filter(|&b| b > 0),
            filter,
        ),
        driver: driver.trim().to_string(),
        path,
        format,
        duration: duration_s
            .filter(|&d| d > 0)
            .map(|d| Duration::from_secs(d.into())),
    };
    let rx = begin_capture(&hub, &state, req, registry_driver)?;
    let adapter = tauri::async_runtime::spawn_blocking(move || await_start(rx))
        .await
        .map_err(|e| format!("capture start task failed: {e}"))??;
    Ok(json!({ "ok": true, "adapter": adapter }))
}

/// Live counters of the current (or last) capture. Never touches the adapter.
#[tauri::command]
pub async fn candump_status(state: tauri::State<'_, CandumpState>) -> Result<Value, String> {
    Ok(capture_status(&state))
}

/// Stop the capture, finish the file and release the adapter.
#[tauri::command]
pub async fn candump_stop(
    hub: tauri::State<'_, AdapterHub>,
    state: tauri::State<'_, CandumpState>,
) -> Result<Value, String> {
    let cap = state
        .current()
        .ok_or_else(|| "no CAN capture has been started".to_string())?;
    cap.stop.store(true, Ordering::Relaxed);
    // Wait off the async runtime; status/stop never need the hub's busy slot.
    tauri::async_runtime::spawn_blocking(move || cap.wait_done(STOP_TIMEOUT))
        .await
        .map_err(|e| format!("capture stop task failed: {e}"))?;
    stop_capture(&hub, &state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::testkit::SimBus;

    fn tmp(name: &str) -> String {
        let dir =
            std::env::temp_dir().join(format!("opentune-candump-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name).to_string_lossy().into_owned()
    }

    fn mock_request(path: &str, format: Format) -> StartRequest {
        StartRequest {
            driver: "mock".into(),
            opts: MonitorOpts::default(),
            path: path.into(),
            format,
            duration: None,
        }
    }

    #[test]
    fn capture_holds_the_adapter_until_stopped() {
        let hub = AdapterHub::spawn(|| SimBus::default().opener());
        let state = CandumpState::default();
        assert_eq!(capture_status(&state)["running"], false);
        assert!(stop_capture(&hub, &state).is_err());

        let path = tmp("busy.log");
        let rx = begin_capture(
            &hub,
            &state,
            mock_request(&path, Format::Candump),
            registry_driver,
        )
        .unwrap();
        let adapter = await_start(rx).unwrap();
        assert!(adapter.contains("Mock"), "{adapter}");

        // Another panel's operation is refused while the capture runs…
        let err = hub.run("OBD live", |_s| Ok(())).unwrap_err();
        assert!(
            err.contains("adapter busy") && err.contains(CAPTURE_OP),
            "{err}"
        );
        // …and so is a second capture.
        let again = begin_capture(
            &hub,
            &state,
            mock_request(&tmp("b2.log"), Format::Csv),
            registry_driver,
        );
        assert!(again.unwrap_err().contains("already running"));

        std::thread::sleep(Duration::from_millis(350));
        let st = capture_status(&state);
        assert_eq!(st["running"], true);
        assert_eq!(st["format"], "candump");
        assert!(st["frames"].as_u64().unwrap() > 10, "{st}");
        let ids: Vec<u64> = st["ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_u64().unwrap())
            .collect();
        assert!(
            ids.contains(&0x280) && ids.windows(2).all(|w| w[0] < w[1]),
            "{ids:?}"
        );
        assert!(st["ids"][0]["last"].is_string() && st["ids"][0]["ext"] == false);

        let sum = stop_capture(&hub, &state).unwrap();
        assert_eq!(sum["running"], false);
        assert!(sum["error"].is_null(), "{sum}");
        let frames = sum["frames"].as_u64().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count() as u64, frames);
        assert_eq!(sum["bytes"].as_u64().unwrap(), text.len() as u64);

        // The adapter is free again.
        assert_eq!(hub.run("OBD live", |_s| Ok(7)).unwrap(), 7);
        // Stop is idempotent and a new capture may start.
        assert_eq!(stop_capture(&hub, &state).unwrap()["frames"], frames);
        let rx = begin_capture(
            &hub,
            &state,
            mock_request(&tmp("c.asc"), Format::Asc),
            registry_driver,
        )
        .unwrap();
        await_start(rx).unwrap();
        stop_capture(&hub, &state).unwrap();
        assert!(std::fs::read_to_string(tmp("c.asc"))
            .unwrap()
            .ends_with("End TriggerBlock\n"));
    }

    #[test]
    fn duration_ends_the_capture_and_frees_the_adapter() {
        let hub = AdapterHub::spawn(|| SimBus::default().opener());
        let state = CandumpState::default();
        let mut req = mock_request(&tmp("dur.csv"), Format::Csv);
        req.duration = Some(Duration::from_millis(200));
        req.opts.filter_ids = vec![0x5A0];
        await_start(begin_capture(&hub, &state, req, registry_driver).unwrap()).unwrap();
        let cap = state.current().unwrap();
        assert!(cap.wait_done(Duration::from_secs(5)));
        let st = capture_status(&state);
        assert_eq!(st["running"], false);
        assert_eq!(st["ids"].as_array().unwrap().len(), 1);
        let t = Instant::now();
        while hub.current().is_some() && t.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(hub.run("probe", |_s| Ok(())).is_ok());
    }

    #[test]
    fn start_errors_are_reported_and_release_the_adapter() {
        let hub = AdapterHub::spawn(|| SimBus::default().opener());
        let state = CandumpState::default();
        // kkl is K-line only: no raw CAN capture.
        let mut req = mock_request(&tmp("kkl.log"), Format::Candump);
        req.driver = "kkl".into();
        let err =
            await_start(begin_capture(&hub, &state, req, registry_driver).unwrap()).unwrap_err();
        assert!(err.contains("cannot capture"), "{err}");
        let st = capture_status(&state);
        assert_eq!(st["running"], false);
        assert!(st["error"].as_str().unwrap().contains("cannot capture"));
        assert!(!std::path::Path::new(&tmp("kkl.log")).exists());
        let t = Instant::now();
        while hub.current().is_some() && t.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(hub.run("probe", |_s| Ok(())).is_ok());
        // Unknown format / empty path are rejected up front.
        assert!("pcap".parse::<Format>().is_err());
        let mut req = mock_request("", Format::Candump);
        req.path = " ".into();
        assert!(begin_capture(&hub, &state, req, registry_driver).is_err());
    }
}
