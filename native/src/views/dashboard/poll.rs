//! The Dashboard's polling job: discovery (supported PIDs, VIN), then one
//! mode-01 read per interval until stopped. Each poll is ONE short read
//! operation on the app's adapter (`AdapterAccess::read`, the read-only
//! allow-list path), so other panels can use the adapter between polls (a
//! poll that finds it busy is reported and retried). Every poll updates the
//! board the view draws, goes to the CSV recorder and to the Drive view's
//! live feed.
//!
//! The learned [`ObdSession`] (supported PIDs, multi-PID support, PIDs that
//! never answer) is kept between polls and cached per connection by the
//! view, so a restart does not re-learn ECU quirks. A poll where nothing
//! answered forgets the learned quirks (a dropped link must not mark every
//! PID unsupported).

use super::model::{self, Track};
use crate::host::{AdapterAccess, DiagTarget, FeedEvent, LiveFeed, ProgressSink};
use crate::widgets::csv::CsvRecordControl;
use opentune_core::obd::{self, ObdSession, ObdValue};
use opentune_core::resilient::Liveness;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Adapter operation name (shown in "adapter busy" messages).
pub const OP: &str = "Dashboard";

pub fn lock<T: ?Sized>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Where the job is.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Phase {
    #[default]
    Idle,
    Discovering,
    Polling,
    Stopped,
    Failed(String),
}

/// What the view draws; the job updates it.
#[derive(Clone, Debug, Default)]
pub struct Board {
    pub phase: Phase,
    pub supported: Option<BTreeSet<u8>>,
    pub vin: Option<String>,
    pub multi_pid: Option<bool>,
    pub values: BTreeMap<String, Option<ObdValue>>,
    pub errors: BTreeMap<String, String>,
    pub tracks: BTreeMap<String, Track>,
    pub polls: u64,
    /// Polls per second over the last few polls.
    pub rate_hz: Option<f64>,
    /// The last poll's failure (busy, link), cleared by the next good poll.
    pub poll_error: Option<String>,
    /// The last poll got no answer at all.
    pub silent: bool,
    pub last_ms: Option<u64>,
}

/// The view's handle on the running job.
pub struct Ctl {
    stop: AtomicBool,
    interval_ms: AtomicU64,
    keys: Mutex<Vec<String>>,
}

impl Ctl {
    pub fn new(interval_ms: u64, keys: Vec<String>) -> Self {
        Self {
            stop: AtomicBool::new(false),
            interval_ms: AtomicU64::new(interval_ms.max(50)),
            keys: Mutex::new(keys),
        }
    }
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
    pub fn set_interval(&self, ms: u64) {
        self.interval_ms.store(ms.max(50), Ordering::SeqCst);
    }
    /// The tile keys to poll (the gauges and the Drive keys are added).
    pub fn set_keys(&self, keys: Vec<String>) {
        *lock(&self.keys) = keys;
    }
    fn keys(&self) -> Vec<String> {
        lock(&self.keys).clone()
    }
    fn interval(&self) -> Duration {
        Duration::from_millis(self.interval_ms.load(Ordering::SeqCst))
    }
}

/// Everything the job runs with.
pub struct Run {
    pub adapter: Arc<dyn AdapterAccess>,
    pub target: DiagTarget,
    pub ctl: Arc<Ctl>,
    pub board: Arc<Mutex<Board>>,
    pub feed: LiveFeed,
    pub rec: Arc<Mutex<CsvRecordControl>>,
    /// The connection's learned session from an earlier run (cache), and
    /// where the job leaves it when it ends.
    pub session: Arc<Mutex<Option<ObdSession>>>,
}

/// Discovery: the supported PIDs (cached in the session) and the VIN.
pub fn discover(
    adapter: &Arc<dyn AdapterAccess>,
    target: DiagTarget,
) -> Result<(ObdSession, BTreeSet<u8>, Option<String>), String> {
    adapter.read(OP, target, |d| {
        let mut s = ObdSession::new();
        let pids = s.discover(d)?;
        let vin = obd::read_vin(d);
        Ok((s, pids, vin))
    })
}

/// One poll's outcome.
pub struct Poll {
    pub session: ObdSession,
    pub readout: obd::Readout,
    /// Nothing answered (or the link reported itself lost).
    pub silent: bool,
    pub link_error: Option<String>,
}

/// One poll of `pids` with the learned `session`.
pub fn poll(
    adapter: &Arc<dyn AdapterAccess>,
    target: DiagTarget,
    session: ObdSession,
    pids: Vec<u8>,
) -> Result<Poll, String> {
    adapter.read(OP, target, move |d| {
        let mut session = session;
        let mut lv = Liveness::new(d);
        let readout = session.read(&mut lv, &pids);
        let silent = !pids.is_empty() && (lv.dead() || lv.link_lost);
        if silent {
            session.forget_learned();
        }
        let link_error = lv.last_error.clone();
        Ok(Poll {
            session,
            readout,
            silent,
            link_error,
        })
    })
}

/// Sleep `d` in short steps; false when stopped meanwhile.
fn nap(d: Duration, stop: &dyn Fn() -> bool) -> bool {
    let until = Instant::now() + d;
    loop {
        if stop() {
            return false;
        }
        let now = Instant::now();
        if now >= until {
            return true;
        }
        std::thread::sleep((until - now).min(Duration::from_millis(20)));
    }
}

/// The job body: discover (unless the cached session already knows the
/// supported PIDs), then poll until stopped.
pub fn run(p: &ProgressSink, r: Run) -> Result<(), String> {
    let stop = || p.cancelled() || r.ctl.stopped();
    run_with(&r, &stop)
}

/// [`run`] with an explicit stop test (tests run it without a job).
pub fn run_with(r: &Run, stop: &dyn Fn() -> bool) -> Result<(), String> {
    let out = run_inner(r, stop);
    lock(&r.board).phase = match &out {
        Ok(()) => Phase::Stopped,
        Err(e) => Phase::Failed(e.clone()),
    };
    if let Err(e) = &out {
        r.feed.publish(model::feed_error(e));
    }
    r.feed.publish(FeedEvent::Stop {
        source: model::SOURCE.into(),
    });
    out
}

fn run_inner(r: &Run, stop: &dyn Fn() -> bool) -> Result<(), String> {
    let cached = lock(&r.session).take();
    let mut session = match cached.filter(|s| s.supported().is_some()) {
        Some(s) => {
            let mut b = lock(&r.board);
            b.supported = s.supported().cloned();
            b.multi_pid = s.multi_pid();
            s
        }
        None => {
            lock(&r.board).phase = Phase::Discovering;
            let (s, pids, vin) = discover(&r.adapter, r.target)?;
            let mut b = lock(&r.board);
            b.supported = Some(pids);
            b.vin = vin;
            s
        }
    };
    *lock(&r.session) = Some(session.clone());
    lock(&r.board).phase = Phase::Polling;
    let start = Instant::now();
    let mut stamps: std::collections::VecDeque<Instant> = Default::default();
    while !stop() {
        let t0 = Instant::now();
        let sup = lock(&r.board).supported.clone();
        let pids = model::pids_for(&r.ctl.keys(), sup.as_ref());
        match poll(&r.adapter, r.target, session.clone(), pids) {
            Ok(pl) => {
                session = pl.session;
                *lock(&r.session) = Some(session.clone());
                let ts = now_ms();
                let t = start.elapsed().as_secs_f64();
                stamps.push_back(t0);
                while stamps.len() > 8 {
                    stamps.pop_front();
                }
                let rate = match (stamps.front(), stamps.back()) {
                    (Some(a), Some(b)) if stamps.len() > 1 && *b > *a => {
                        Some((stamps.len() - 1) as f64 / (*b - *a).as_secs_f64())
                    }
                    _ => None,
                };
                {
                    let mut b = lock(&r.board);
                    model::track_poll(&mut b.tracks, t, &pl.readout);
                    b.values = pl.readout.values.clone();
                    b.errors = pl.readout.errors.clone();
                    b.multi_pid = session.multi_pid();
                    b.polls += 1;
                    b.rate_hz = rate;
                    b.silent = pl.silent;
                    b.poll_error = pl
                        .silent
                        .then(|| pl.link_error.clone().unwrap_or_else(|| "no answer".into()));
                    b.last_ms = Some(ts);
                }
                if pl.silent {
                    r.feed
                        .publish(model::feed_error("no answer from the ECU (link down?)"));
                } else {
                    r.feed.publish(model::feed_frame(ts, &pl.readout));
                }
                let mut rec = lock(&r.rec);
                if rec.is_recording() {
                    for row in model::csv_rows(ts, &pl.readout) {
                        rec.push(&row);
                    }
                }
            }
            Err(e) => {
                let first = lock(&r.board).polls == 0;
                lock(&r.board).poll_error = Some(e.clone());
                // No adapter / no link before any poll: nothing to retry.
                if first && !model::is_busy(&e) {
                    return Err(e);
                }
                r.feed.publish(model::feed_error(&e));
            }
        }
        let wait = r.ctl.interval().saturating_sub(t0.elapsed());
        if !nap(wait, stop) {
            break;
        }
    }
    Ok(())
}
