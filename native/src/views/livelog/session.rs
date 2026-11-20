//! The live session's background job: `live_start`, then ticks (each one
//! blocks until the engine's next sample, so they run here and never in
//! `ui()`), then `live_stop`. Every tick updates the panel's [`Board`],
//! goes to the CSV recorder and to the Drive view's live feed.
//!
//! The job ends the session when the panel asks (Stop, view closed), when
//! the engine ends it (link lost without reconnect, idle), and when the
//! view is left: the app keeps drawing frames (each tick asks for one) but
//! the view's `ui()` no longer runs, unless the user chose to keep logging
//! in the background (e.g. to watch the Drive view).
//!
//! Nothing here builds a diagnostic request: the engine (trusted core code)
//! does, behind `AdapterAccess::live_*`.

use super::model::{self, Board, Phase, Row};
use crate::host::{AdapterAccess, DiagTarget, FeedEvent, LiveFeed, LiveSpec, ProgressSink};
use crate::widgets::csv::CsvRecordControl;
use crate::widgets::gps::GpsSource;
use eframe::egui;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Frames drawn without the view before the session counts the view as
/// left (a frame is in flight while the job looks).
const LEAVE_FRAMES: u64 = 3;
/// The longest single sleep between checks for Stop.
const SLICE: Duration = Duration::from_millis(20);
/// CSV rows held back while the recorder is busy (its save dialog is open)
/// before the oldest are dropped.
const PENDING_MAX: usize = 200_000;

pub(super) fn lock<T: ?Sized>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The panel's handle on a running session job.
#[derive(Default)]
pub(super) struct Ctl {
    stop: AtomicBool,
    why: Mutex<Option<String>>,
    /// `cumulative_frame_nr` of the last frame the view was drawn in.
    seen_frame: AtomicU64,
    /// Keep logging when the view is left.
    keep: AtomicBool,
    interval_ms: AtomicU64,
}

impl Ctl {
    pub(super) fn new(interval_ms: u64, keep: bool, frame: u64) -> Self {
        let c = Ctl::default();
        c.interval_ms.store(interval_ms, Ordering::Relaxed);
        c.keep.store(keep, Ordering::Relaxed);
        c.seen_frame.store(frame, Ordering::Relaxed);
        c
    }

    /// Ask the job to stop the session (the first reason wins).
    pub(super) fn stop(&self, why: &str) {
        lock(&self.why).get_or_insert_with(|| why.to_string());
        self.stop.store(true, Ordering::SeqCst);
    }

    pub(super) fn stopping(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// The view was drawn in frame `frame`.
    pub(super) fn seen(&self, frame: u64) {
        self.seen_frame.fetch_max(frame, Ordering::Relaxed);
    }

    pub(super) fn set_keep(&self, keep: bool) {
        self.keep.store(keep, Ordering::Relaxed);
    }

    pub(super) fn set_interval(&self, ms: u64) {
        self.interval_ms.store(ms, Ordering::Relaxed);
    }

    /// Has the app drawn frames without the view (and the user did not ask
    /// to keep logging)?
    fn left(&self, frames: Option<&egui::Context>) -> bool {
        let Some(ctx) = frames else {
            return false;
        };
        !self.keep.load(Ordering::Relaxed)
            && frame_nr(ctx) > self.seen_frame.load(Ordering::Relaxed) + LEAVE_FRAMES
    }
}

/// Frames the app's main window has completed (readable from any thread).
pub(super) fn frame_nr(ctx: &egui::Context) -> u64 {
    ctx.cumulative_frame_nr_for(egui::ViewportId::ROOT)
}

/// Where the session's CSV rows go (the panel's [`CsvRecordControl`]).
pub(super) trait RowSink: Send {
    fn recording(&self) -> bool;
    fn push_row(&mut self, row: &[String; 8]);
}

impl RowSink for CsvRecordControl {
    fn recording(&self) -> bool {
        self.is_recording()
    }
    fn push_row(&mut self, row: &[String; 8]) {
        self.push(row);
    }
}

/// Everything the job runs with.
pub(super) struct Run {
    pub adapter: Arc<dyn AdapterAccess>,
    pub spec: LiveSpec,
    pub target: DiagTarget,
    pub ctl: Arc<Ctl>,
    pub board: Arc<Mutex<Board>>,
    pub feed: LiveFeed,
    pub rec: Arc<Mutex<dyn RowSink>>,
    /// The app's GPS source: its channels join the CSV and the feed while it
    /// is open.
    pub gps: Option<GpsSource>,
    /// The app's context (leave detection); `None`: never counts as left.
    pub frames: Option<egui::Context>,
}

/// CSV rows on their way to the recorder.
#[derive(Default)]
struct Pending(Vec<[String; 8]>);

impl Pending {
    /// Queue `rows` at `ts` and hand what is queued to the recorder unless it
    /// is busy (`wait`: block for it, at the end of the session).
    fn push(&mut self, rec: &Mutex<dyn RowSink>, ts: u64, rows: &[Row], wait: bool) {
        self.0.extend(rows.iter().map(|r| r.csv(ts)));
        let guard = if wait {
            Some(lock(rec))
        } else {
            match rec.try_lock() {
                Ok(g) => Some(g),
                Err(std::sync::TryLockError::Poisoned(e)) => Some(e.into_inner()),
                Err(std::sync::TryLockError::WouldBlock) => None,
            }
        };
        match guard {
            Some(mut c) if c.recording() => {
                for row in self.0.drain(..) {
                    c.push_row(&row);
                }
            }
            // Not recording: nothing to keep.
            Some(_) => self.0.clear(),
            None if self.0.len() > PENDING_MAX => {
                let over = self.0.len() - PENDING_MAX;
                self.0.drain(..over);
            }
            None => {}
        }
    }
}

impl Run {
    fn board(&self) -> MutexGuard<'_, Board> {
        lock(&self.board)
    }

    /// The GPS channels for this tick (with the speed error against the
    /// logged vehicle speed), when a GPS source is open.
    fn gps_rows(&self, rows: &[Row]) -> Vec<Row> {
        let Some(snap) = self.gps.as_ref().and_then(GpsSource::snapshot) else {
            return Vec::new();
        };
        let mut out: Vec<Row> = opentune_logdef::gps::gps_samples(&snap)
            .iter()
            .map(Row::from_sample)
            .collect();
        let veh = model::vehicle_speed(rows).map(Row::sample);
        out.push(Row::from_sample(&opentune_logdef::gps::speed_error_sample(
            veh.as_ref(),
            true,
            &snap,
        )));
        out
    }

    /// Why to stop now, if anything says so.
    fn should_stop(&self, p: &ProgressSink) -> Option<String> {
        if self.ctl.stopping() || p.cancelled() {
            return Some(
                lock(&self.ctl.why)
                    .clone()
                    .unwrap_or_else(|| "stopped".into()),
            );
        }
        if self.ctl.left(self.frames.as_ref()) {
            return Some("left the Live log view".into());
        }
        None
    }

    /// Sleep out the rest of the interval since `t0`, waking for Stop.
    fn pause(&self, p: &ProgressSink, t0: Instant) {
        loop {
            let interval = Duration::from_millis(self.ctl.interval_ms.load(Ordering::Relaxed));
            let left = interval.saturating_sub(t0.elapsed());
            if left.is_zero() || self.should_stop(p).is_some() {
                return;
            }
            std::thread::sleep(left.min(SLICE));
        }
    }
}

/// The job: start, tick until something ends it, stop. `Err` only when the
/// session did not start (the board says why, too).
pub(super) fn run(p: &ProgressSink, r: Run) -> Result<(), String> {
    p.set(0.0, "starting the live session");
    let start = match r.adapter.live_start(r.spec.clone(), r.target) {
        Ok(v) => v,
        Err(e) => {
            // A start that timed out may still be defining on the adapter
            // worker: stop it so it does not hold the adapter.
            if e.contains("did not start in time") {
                let _ = r.adapter.live_stop();
            }
            r.feed.publish(model::feed_error(&e));
            let mut b = r.board();
            b.phase = Phase::Done;
            b.error = Some(format!("start failed: {e}"));
            return Err(e);
        }
    };
    r.board().apply_start(&start);
    p.set(0.0, "live");

    let mut pending = Pending::default();
    let why = loop {
        if let Some(w) = r.should_stop(p) {
            break Some(w);
        }
        let t0 = Instant::now();
        match r.adapter.live_tick() {
            Ok(v) => {
                let ts = v
                    .get("ts")
                    .and_then(Value::as_u64)
                    .unwrap_or_else(model::now_ms);
                let rows = model::rows(v.get("samples").unwrap_or(&Value::Null));
                let gps = r.gps_rows(&rows);
                pending.push(&r.rec, ts, &rows, false);
                pending.push(&r.rec, ts, &gps, false);
                r.feed.publish(model::feed_event(&v, &rows, &gps));
                let ticks = {
                    let mut b = r.board();
                    b.apply_tick(&v, &rows);
                    b.ticks
                };
                p.set(0.0, format!("{ticks} tick(s)"));
                if v.get("running").and_then(Value::as_bool) == Some(false) {
                    break Some(
                        v.get("ended")
                            .and_then(Value::as_str)
                            .unwrap_or("the session ended")
                            .to_string(),
                    );
                }
            }
            Err(e) => {
                r.feed.publish(model::feed_error(&e));
                r.board().tick_error = Some(e.clone());
                // The engine is gone: nothing more will come.
                if e.contains("ended") || e.contains("no live log session") {
                    break Some(e);
                }
                // Else (a tick that timed out): carry on; a lost link is the
                // engine's to report.
            }
        }
        r.pause(p, t0);
    };

    r.board().phase = Phase::Stopping;
    p.set(0.0, "stopping (clearing dynamic DIDs)");
    match r.adapter.live_stop() {
        Ok(sum) => {
            let events = model::rows(sum.get("events").unwrap_or(&Value::Null));
            pending.push(&r.rec, model::now_ms(), &events, true);
            r.board().apply_stop(&sum);
        }
        Err(e) => {
            pending.push(&r.rec, model::now_ms(), &[], true);
            r.board().error = Some(format!("stop: {e}"));
        }
    }
    r.feed.publish(FeedEvent::Stop {
        source: model::SOURCE.into(),
    });
    let mut b = r.board();
    if let Some(w) = why {
        b.ended.get_or_insert(w);
    }
    b.phase = Phase::Done;
    Ok(())
}
