//! A **resilient live-log loop** that survives a transport dropping out.
//!
//! A live log runs for minutes or hours over a link that can fail at any
//! moment: the adapter is unplugged, a sleeping bus wedges the transmitter,
//! a wireless adapter walks out of range. While it is gone every request
//! returns an I/O error or a timeout; it may come back on its own or only
//! after the adapter is re-opened. [`run_resilient`] drives a [`LogJob`]
//! (one sampling round at a time) through that:
//!
//! * A round in which requests were sent and **none** got any response
//!   ([`Liveness::dead`]) is a *dead round*. A negative response is an answer:
//!   the link is alive, the ECU just refused.
//! * After [`ReconnectPolicy::dead_after`] dead rounds in a row the link is
//!   **lost** ([`LinkEvent::Lost`]). With [`ReconnectPolicy::reconnect`] the
//!   transport is dropped (an owning wrapper such as
//!   [`InterfaceDiag`](crate::diag::InterfaceDiag) closes the driver) and the
//!   `open` hook is called again with exponential backoff until it succeeds
//!   ([`LinkEvent::Reconnected`], then [`LogJob::reopened`] re-initialises the
//!   job, e.g. re-defines dynamic DIDs) or the policy gives up
//!   ([`LinkEvent::GaveUp`]). Without it the loop keeps polling the same
//!   transport, backing off, and reports [`LinkEvent::Recovered`] when it
//!   answers again.
//! * It **never spins hot**: a dead round is followed by a pause of at least
//!   [`ReconnectPolicy::initial_backoff_ms`] (growing to
//!   [`ReconnectPolicy::max_backoff_ms`]), and every reopen attempt is preceded
//!   by its backoff sleep — even when the transport fails instantly.
//! * A clean stop (round limit, time limit, stop flag) calls [`LogJob::stop`]
//!   while the link is alive, so the job can clean up on the ECU. It is skipped
//!   while the link is lost: nothing would answer.
//!
//! Time comes from a [`Clock`]: [`SystemClock`] in applications,
//! [`VirtualClock`] in tests (sleeps advance virtual time instantly and are
//! recorded, so a test can count them and run an hour-long outage in
//! microseconds). [`faultlink`](crate::protocol::faultlink) is a CAN link that
//! can be killed and revived on cue, to drive the loop end to end.
//!
//! The loop knows nothing about what a round reads or writes: the
//! `opentune-logdef` `LiveSession` CSV writer and the CLI's OBD-II recorder
//! are both [`LogJob`]s over it.

use crate::diag::RawDiag;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Longest single sleep, so a stop flag is noticed within this many ms.
pub const SLEEP_SLICE_MS: u64 = 200;

/// Time source for [`run_resilient`].
pub trait Clock {
    /// Monotonic milliseconds since an arbitrary origin.
    fn now_ms(&self) -> u64;
    /// Wall-clock Unix milliseconds (row timestamps).
    fn unix_ms(&self) -> u64;
    /// Sleep `ms` milliseconds.
    fn sleep_ms(&self, ms: u64);
}

/// The real clock: [`Instant`] for intervals, [`std::time::SystemTime`] for
/// timestamps, [`std::thread::sleep`] for sleeping.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    t0: Instant,
}

impl SystemClock {
    /// A clock whose [`now_ms`](Clock::now_ms) starts at 0 now.
    pub fn new() -> Self {
        Self { t0: Instant::now() }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.t0.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
    fn unix_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0)
    }
    fn sleep_ms(&self, ms: u64) {
        std::thread::sleep(Duration::from_millis(ms));
    }
}

#[derive(Debug, Default)]
struct VirtualState {
    now: AtomicU64,
    base_unix_ms: u64,
    sleeps: Mutex<Vec<u64>>,
}

/// A virtual clock for tests: [`sleep_ms`](Clock::sleep_ms) records the sleep
/// and advances time instantly. Clones share one clock, so a simulated link
/// ([`FaultBus::with_clock`](crate::protocol::faultlink::FaultBus::with_clock))
/// and the loop agree on the time.
#[derive(Debug, Clone, Default)]
pub struct VirtualClock {
    st: Arc<VirtualState>,
}

impl VirtualClock {
    /// A clock at 0 whose [`unix_ms`](Clock::unix_ms) is `base_unix_ms` + now.
    pub fn new(base_unix_ms: u64) -> Self {
        Self {
            st: Arc::new(VirtualState {
                base_unix_ms,
                ..VirtualState::default()
            }),
        }
    }

    /// Move time forward without recording a sleep (e.g. a simulated wait).
    pub fn advance(&self, ms: u64) {
        self.st.now.fetch_add(ms, Ordering::SeqCst);
    }

    /// Every sleep so far, in order.
    pub fn sleeps(&self) -> Vec<u64> {
        self.st.sleeps.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// Total milliseconds slept.
    pub fn slept_ms(&self) -> u64 {
        self.sleeps().iter().sum()
    }
}

impl Clock for VirtualClock {
    fn now_ms(&self) -> u64 {
        self.st.now.load(Ordering::SeqCst)
    }
    fn unix_ms(&self) -> u64 {
        self.st.base_unix_ms + self.now_ms()
    }
    fn sleep_ms(&self, ms: u64) {
        if let Ok(mut s) = self.st.sleeps.lock() {
            s.push(ms);
        }
        self.advance(ms);
    }
}

/// When a link counts as lost and how hard to try to get it back.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ReconnectPolicy {
    /// Drop and re-open the transport once the link is lost. `false` keeps
    /// polling the same transport (with backoff) in case it comes back.
    pub reconnect: bool,
    /// Consecutive dead rounds before the link counts as lost (≥ 1).
    pub dead_after: u32,
    /// First backoff, ms (≥ 1): the pause after a dead round and before the
    /// first reopen attempt.
    pub initial_backoff_ms: u64,
    /// Backoff ceiling, ms; each further attempt doubles the pause up to it.
    pub max_backoff_ms: u64,
    /// Give up after this many failed reopen attempts (`None`: never).
    pub max_attempts: Option<u32>,
    /// Give up once the link has been down this long, ms (`None`: never).
    pub give_up_after_ms: Option<u64>,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            reconnect: false,
            dead_after: 3,
            initial_backoff_ms: 500,
            max_backoff_ms: 10_000,
            max_attempts: None,
            give_up_after_ms: None,
        }
    }
}

impl ReconnectPolicy {
    /// Reconnect enabled, never giving up, default timings.
    pub fn reconnecting() -> Self {
        Self {
            reconnect: true,
            ..Self::default()
        }
    }

    /// The pause before attempt `attempt` (1-based; also used for the pause
    /// after the `attempt`-th dead round in a row): the initial backoff,
    /// doubling, capped at the maximum, and never 0.
    pub fn backoff_ms(&self, attempt: u32) -> u64 {
        let shift = attempt.saturating_sub(1).min(32);
        self.initial_backoff_ms
            .max(1)
            .saturating_mul(1u64 << shift)
            .min(self.max_backoff_ms.max(self.initial_backoff_ms).max(1))
    }

    /// Check the policy (the loop must always be able to sleep).
    pub fn validate(&self) -> Result<(), String> {
        if self.dead_after == 0 {
            return Err("dead_after must be at least 1 round".into());
        }
        if self.initial_backoff_ms == 0 {
            return Err("initial backoff must be at least 1 ms (the loop never spins hot)".into());
        }
        if self.max_backoff_ms < self.initial_backoff_ms {
            return Err("max backoff must not be below the initial backoff".into());
        }
        if self.max_attempts == Some(0) {
            return Err("max_attempts 0 would give up without trying; use None or ≥ 1".into());
        }
        Ok(())
    }
}

/// When a run ends on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RunLimits {
    /// Stop after this many rounds (`None`: no round limit).
    pub rounds: Option<u64>,
    /// Stop after this long, ms (`None`: no time limit).
    pub duration_ms: Option<u64>,
    /// Pause between live rounds, ms (0 = as fast as possible).
    pub interval_ms: u64,
}

/// What happened to the link. Written into the log as marker rows and
/// reported to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkEvent {
    /// `dead_rounds` rounds in a row got no response at all.
    Lost {
        /// Dead rounds in a row.
        dead_rounds: u32,
        /// The last transport error seen.
        error: String,
    },
    /// The same transport answered again (no reopen), `after_ms` after it was
    /// lost.
    Recovered {
        /// Downtime, ms.
        after_ms: u64,
    },
    /// Reopen attempt `attempt` failed; the next one follows in `retry_in_ms`.
    ReopenFailed {
        /// 1-based attempt number.
        attempt: u32,
        /// Why the open (or the first request on it) failed.
        error: String,
        /// Pause before the next attempt, ms.
        retry_in_ms: u64,
    },
    /// Reopen attempt `attempt` succeeded, `after_ms` after the link was lost.
    Reconnected {
        /// 1-based attempt number.
        attempt: u32,
        /// Downtime, ms.
        after_ms: u64,
    },
    /// The policy gave up after `attempts` attempts and `after_ms` down.
    GaveUp {
        /// Failed attempts.
        attempts: u32,
        /// Downtime, ms.
        after_ms: u64,
        /// The last error.
        error: String,
    },
}

impl LinkEvent {
    /// Short machine-readable state: `lost`, `recovered`, `reopen-failed`,
    /// `reconnected`, `gave-up`.
    pub fn state(&self) -> &'static str {
        match self {
            LinkEvent::Lost { .. } => "lost",
            LinkEvent::Recovered { .. } => "recovered",
            LinkEvent::ReopenFailed { .. } => "reopen-failed",
            LinkEvent::Reconnected { .. } => "reconnected",
            LinkEvent::GaveUp { .. } => "gave-up",
        }
    }

    /// One human-readable line.
    pub fn describe(&self) -> String {
        let s = |ms: &u64| format!("{:.1} s", *ms as f64 / 1000.0);
        match self {
            LinkEvent::Lost { dead_rounds, error } => {
                format!("link lost after {dead_rounds} dead round(s): {error}")
            }
            LinkEvent::Recovered { after_ms } => {
                format!("link answered again after {} (no reopen)", s(after_ms))
            }
            LinkEvent::ReopenFailed {
                attempt,
                error,
                retry_in_ms,
            } => format!(
                "reopen attempt {attempt} failed: {error}; retrying in {}",
                s(retry_in_ms)
            ),
            LinkEvent::Reconnected { attempt, after_ms } => {
                format!("reconnected after {} (attempt {attempt})", s(after_ms))
            }
            LinkEvent::GaveUp {
                attempts,
                after_ms,
                error,
            } => format!(
                "gave up reconnecting after {attempts} attempt(s) and {} down: {error}",
                s(after_ms)
            ),
        }
    }
}

/// Why a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EndReason {
    /// The round limit was reached.
    #[default]
    Rounds,
    /// The time limit was reached.
    Duration,
    /// The stop flag was set (e.g. Ctrl-C).
    Stopped,
    /// The reconnect policy gave up.
    GaveUp,
}

/// What a run did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunReport {
    /// Rounds run (live and dead).
    pub rounds: u64,
    /// Rounds in which nothing answered.
    pub dead_rounds: u64,
    /// Times the link was lost.
    pub drops: u32,
    /// Successful reopens.
    pub reconnects: u32,
    /// Times the same transport came back without a reopen.
    pub recoveries: u32,
    /// Reopen attempts (successful or not).
    pub reopen_attempts: u32,
    /// Total time the link was lost, ms.
    pub downtime_ms: u64,
    /// Run time, ms.
    pub elapsed_ms: u64,
    /// Why the run ended.
    pub ended: EndReason,
    /// [`LogJob::stop`] ran (the link was alive at the end).
    pub stopped_cleanly: bool,
}

/// One live-logging job driven by [`run_resilient`].
pub trait LogJob {
    /// Read and record one round over `diag`. `ts_ms` is the round's Unix
    /// timestamp. An I/O error (e.g. writing the CSV) ends the run.
    fn round(&mut self, diag: &mut dyn RawDiag, round: u64, ts_ms: u64) -> std::io::Result<()>;
    /// Record a link event (e.g. as a marker row). `round` is the index the
    /// next round will get.
    fn event(&mut self, ev: &LinkEvent, round: u64, ts_ms: u64) -> std::io::Result<()>;
    /// A fresh transport after a reopen: re-initialise whatever the ECU or
    /// the job forgot (dynamic DIDs, learned PID support, …).
    fn reopened(&mut self, diag: &mut dyn RawDiag);
    /// Clean up at a clean stop while the link is alive.
    fn stop(&mut self, diag: &mut dyn RawDiag);
}

/// A [`RawDiag`] wrapper that counts answered and failed requests: the
/// loop's liveness probe, usable on its own.
pub struct Liveness<'a> {
    inner: &'a mut dyn RawDiag,
    /// Requests that got any response (positive or negative).
    pub answered: u64,
    /// Requests that failed at the transport (timeout, I/O, no response).
    pub failed: u64,
    /// The last transport error.
    pub last_error: Option<String>,
}

impl<'a> Liveness<'a> {
    /// Wrap `inner`.
    pub fn new(inner: &'a mut dyn RawDiag) -> Self {
        Self {
            inner,
            answered: 0,
            failed: 0,
            last_error: None,
        }
    }

    /// Requests were sent and none got any response.
    pub fn dead(&self) -> bool {
        self.answered == 0 && self.failed > 0
    }
}

impl RawDiag for Liveness<'_> {
    fn request(&mut self, req: &[u8]) -> Result<Vec<u8>, String> {
        let r = self.inner.request(req);
        match &r {
            Ok(_) => self.answered += 1,
            Err(e) => {
                self.failed += 1;
                self.last_error = Some(e.clone());
            }
        }
        r
    }
}

/// Everything about a run except the job and the link.
pub struct RunOptions<'a> {
    /// Round / time limits and the live interval.
    pub limits: RunLimits,
    /// Loss detection and reconnect policy.
    pub policy: ReconnectPolicy,
    /// Set to stop the run cleanly (e.g. from a Ctrl-C handler).
    pub stop: Option<&'a AtomicBool>,
    /// Time source.
    pub clock: &'a dyn Clock,
}

enum Link<'l> {
    Up(Box<dyn RawDiag + 'l>),
    Down,
}

/// Run `job` over `link` until a limit, the stop flag or the policy ends it,
/// surviving transport drops as the module docs describe. `open` re-opens
/// the transport (only called with [`ReconnectPolicy::reconnect`]). The
/// caller has already set the job up on `link` (e.g. `LiveSession::start`).
///
/// Errors only on an invalid policy or a job I/O error (after a best-effort
/// [`LogJob::stop`] while the link is alive).
pub fn run_resilient<'l>(
    job: &mut dyn LogJob,
    link: Box<dyn RawDiag + 'l>,
    open: &mut dyn FnMut() -> Result<Box<dyn RawDiag + 'l>, String>,
    opts: &RunOptions<'_>,
) -> std::io::Result<RunReport> {
    let policy = &opts.policy;
    policy
        .validate()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let clock = opts.clock;
    let t0 = clock.now_ms();
    let deadline = opts.limits.duration_ms.map(|d| t0.saturating_add(d));
    let stopped = || opts.stop.is_some_and(|s| s.load(Ordering::Relaxed));
    let mut report = RunReport::default();
    let mut link = Link::Up(link);
    let mut round: u64 = 0;
    let mut streak: u32 = 0; // dead rounds in a row
    let mut lost_at: Option<u64> = None; // link counted as lost since
    let mut last_error = String::new();

    let end = loop {
        if stopped() {
            break EndReason::Stopped;
        }
        if deadline.is_some_and(|d| clock.now_ms() >= d) {
            break EndReason::Duration;
        }
        match &mut link {
            Link::Up(diag) => {
                if opts.limits.rounds.is_some_and(|n| round >= n) {
                    break EndReason::Rounds;
                }
                let ts = clock.unix_ms();
                let mut live = Liveness::new(diag.as_mut());
                let res = job.round(&mut live, round, ts);
                let dead = live.dead();
                if let Some(e) = live.last_error.take() {
                    last_error = e;
                }
                round += 1;
                report.rounds += 1;
                if let Err(e) = res {
                    if streak < policy.dead_after && !dead {
                        job.stop(diag.as_mut());
                    }
                    return Err(e);
                }
                if dead {
                    report.dead_rounds += 1;
                    streak = streak.saturating_add(1);
                } else {
                    if let Some(at) = lost_at.take() {
                        let after_ms = clock.now_ms().saturating_sub(at);
                        report.recoveries += 1;
                        report.downtime_ms += after_ms;
                        job.event(&LinkEvent::Recovered { after_ms }, round, clock.unix_ms())?;
                    }
                    streak = 0;
                }
                if streak == policy.dead_after && lost_at.is_none() {
                    lost_at = Some(clock.now_ms());
                    report.drops += 1;
                    let ev = LinkEvent::Lost {
                        dead_rounds: streak,
                        error: last_error.clone(),
                    };
                    job.event(&ev, round, clock.unix_ms())?;
                    if policy.reconnect {
                        link = Link::Down; // drops (and closes) the transport
                        continue;
                    }
                }
                let pause = if dead {
                    opts.limits.interval_ms.max(policy.backoff_ms(streak))
                } else {
                    opts.limits.interval_ms
                };
                let more = !matches!(opts.limits.rounds, Some(n) if round >= n);
                if pause > 0 && more {
                    pause_for(clock, pause, &stopped, deadline);
                }
            }
            Link::Down => {
                let since = lost_at.unwrap_or(t0);
                let mut attempt = 0u32;
                let outcome = loop {
                    attempt += 1;
                    if !pause_for(clock, policy.backoff_ms(attempt), &stopped, deadline) {
                        break None; // stop flag or time limit while waiting
                    }
                    report.reopen_attempts += 1;
                    let error = match open() {
                        Ok(mut d) => {
                            job.reopened(d.as_mut());
                            break Some(Ok((d, attempt)));
                        }
                        Err(e) => e,
                    };
                    let down = clock.now_ms().saturating_sub(since);
                    if policy.max_attempts.is_some_and(|n| attempt >= n)
                        || policy.give_up_after_ms.is_some_and(|t| down >= t)
                    {
                        break Some(Err((attempt, down, error)));
                    }
                    let ev = LinkEvent::ReopenFailed {
                        attempt,
                        error,
                        retry_in_ms: policy.backoff_ms(attempt + 1),
                    };
                    job.event(&ev, round, clock.unix_ms())?;
                };
                match outcome {
                    None => {
                        report.downtime_ms += clock.now_ms().saturating_sub(since);
                        lost_at = None;
                        // Re-checked at the top: stop flag or deadline.
                    }
                    Some(Ok((d, attempt))) => {
                        let after_ms = clock.now_ms().saturating_sub(since);
                        report.reconnects += 1;
                        report.downtime_ms += after_ms;
                        lost_at = None;
                        streak = 0;
                        link = Link::Up(d);
                        job.event(
                            &LinkEvent::Reconnected { attempt, after_ms },
                            round,
                            clock.unix_ms(),
                        )?;
                    }
                    Some(Err((attempts, after_ms, error))) => {
                        report.downtime_ms += after_ms;
                        job.event(
                            &LinkEvent::GaveUp {
                                attempts,
                                after_ms,
                                error,
                            },
                            round,
                            clock.unix_ms(),
                        )?;
                        break EndReason::GaveUp;
                    }
                }
            }
        }
    };

    if let Some(at) = lost_at {
        if matches!(link, Link::Up(_)) {
            report.downtime_ms += clock.now_ms().saturating_sub(at);
        }
    }
    if let Link::Up(diag) = &mut link {
        if streak < policy.dead_after {
            job.stop(diag.as_mut());
            report.stopped_cleanly = true;
        }
    }
    report.ended = end;
    report.elapsed_ms = clock.now_ms().saturating_sub(t0);
    Ok(report)
}

/// Sleep `ms` in slices of at most [`SLEEP_SLICE_MS`], stopping early at the
/// deadline or when `stopped()`. Returns `false` when cut short.
fn pause_for(clock: &dyn Clock, ms: u64, stopped: &dyn Fn() -> bool, deadline: Option<u64>) -> bool {
    let mut left = ms;
    let mut cut = false;
    if let Some(d) = deadline {
        let until = d.saturating_sub(clock.now_ms());
        if until < left {
            left = until;
            cut = true;
        }
    }
    while left > 0 {
        if stopped() {
            return false;
        }
        let s = left.min(SLEEP_SLICE_MS);
        clock.sleep_ms(s);
        left -= s;
    }
    !cut && !stopped()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A transport that is dead while the shared flag says so.
    struct Switch {
        dead: Rc<RefCell<bool>>,
    }
    impl RawDiag for Switch {
        fn request(&mut self, req: &[u8]) -> Result<Vec<u8>, String> {
            if *self.dead.borrow() {
                Err("io error: unplugged".into())
            } else {
                Ok(vec![req[0] + 0x40])
            }
        }
    }

    #[derive(Default)]
    struct Job {
        rounds: Vec<(u64, bool)>,
        events: Vec<String>,
        reopened: u32,
        stops: u32,
        /// (round index, closure) — run before that round.
        hook: Option<Box<dyn FnMut(u64)>>,
    }
    impl LogJob for Job {
        fn round(&mut self, d: &mut dyn RawDiag, round: u64, _ts: u64) -> std::io::Result<()> {
            if let Some(h) = self.hook.as_mut() {
                h(round);
            }
            let ok = d.request(&[0x22]).is_ok();
            self.rounds.push((round, ok));
            Ok(())
        }
        fn event(&mut self, ev: &LinkEvent, _r: u64, _ts: u64) -> std::io::Result<()> {
            self.events.push(ev.state().to_string());
            Ok(())
        }
        fn reopened(&mut self, _d: &mut dyn RawDiag) {
            self.reopened += 1;
        }
        fn stop(&mut self, _d: &mut dyn RawDiag) {
            self.stops += 1;
        }
    }

    fn opts<'a>(clock: &'a VirtualClock, rounds: Option<u64>, policy: ReconnectPolicy) -> RunOptions<'a> {
        RunOptions {
            limits: RunLimits {
                rounds,
                duration_ms: None,
                interval_ms: 0,
            },
            policy,
            stop: None,
            clock,
        }
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let p = ReconnectPolicy {
            initial_backoff_ms: 100,
            max_backoff_ms: 1000,
            ..ReconnectPolicy::default()
        };
        let b: Vec<u64> = (1..=6).map(|a| p.backoff_ms(a)).collect();
        assert_eq!(b, [100, 200, 400, 800, 1000, 1000]);
        assert_eq!(p.backoff_ms(u32::MAX), 1000);
        assert!(ReconnectPolicy {
            initial_backoff_ms: 0,
            ..p.clone()
        }
        .validate()
        .is_err());
        assert!(ReconnectPolicy { dead_after: 0, ..p }.validate().is_err());
    }

    #[test]
    fn healthy_run_stops_cleanly_without_sleeping() {
        let clock = VirtualClock::new(0);
        let dead = Rc::new(RefCell::new(false));
        let mut job = Job::default();
        let mut open = || -> Result<Box<dyn RawDiag>, String> { Err("unused".into()) };
        let r = run_resilient(
            &mut job,
            Box::new(Switch { dead: dead.clone() }),
            &mut open,
            &opts(&clock, Some(5), ReconnectPolicy::reconnecting()),
        )
        .unwrap();
        assert_eq!((r.rounds, r.drops, r.ended), (5, 0, EndReason::Rounds));
        assert!(r.stopped_cleanly && job.stops == 1);
        assert!(clock.sleeps().is_empty());
    }

    #[test]
    fn without_reconnect_a_dead_link_backs_off_and_recovers() {
        let clock = VirtualClock::new(0);
        let dead = Rc::new(RefCell::new(false));
        let d2 = dead.clone();
        let mut job = Job {
            hook: Some(Box::new(move |r| *d2.borrow_mut() = (2..7).contains(&r))),
            ..Job::default()
        };
        let mut open = || -> Result<Box<dyn RawDiag>, String> { panic!("no reopen") };
        let policy = ReconnectPolicy {
            initial_backoff_ms: 100,
            max_backoff_ms: 400,
            ..ReconnectPolicy::default()
        };
        let r = run_resilient(
            &mut job,
            Box::new(Switch { dead: dead.clone() }),
            &mut open,
            &opts(&clock, Some(10), policy),
        )
        .unwrap();
        assert_eq!(r.dead_rounds, 5);
        assert_eq!(job.events, ["lost", "recovered"]);
        assert_eq!((r.drops, r.recoveries, r.reconnects), (1, 1, 0));
        // One pause per dead round, growing to the cap (100, 200, 400, 400,
        // 400; sliced into ≤ 200 ms sleeps): never a hot spin.
        assert_eq!(clock.slept_ms(), 1500);
        assert!(clock.sleeps().len() >= 5);
        assert!(clock.sleeps().iter().all(|&s| s > 0 && s <= SLEEP_SLICE_MS));
        assert!(r.stopped_cleanly);
    }

    #[test]
    fn reconnect_drops_reopens_and_resumes() {
        let clock = VirtualClock::new(0);
        let dead = Rc::new(RefCell::new(false));
        let d2 = dead.clone();
        let mut job = Job {
            hook: Some(Box::new(move |r| {
                if r == 3 {
                    *d2.borrow_mut() = true;
                }
            })),
            ..Job::default()
        };
        let mut tries = 0;
        let d3 = dead.clone();
        let mut open = || -> Result<Box<dyn RawDiag>, String> {
            tries += 1;
            if tries < 3 {
                return Err("no such device".into());
            }
            *d3.borrow_mut() = false;
            Ok(Box::new(Switch { dead: d3.clone() }))
        };
        let policy = ReconnectPolicy {
            initial_backoff_ms: 50,
            max_backoff_ms: 1000,
            dead_after: 2,
            ..ReconnectPolicy::reconnecting()
        };
        let r = run_resilient(
            &mut job,
            Box::new(Switch { dead: dead.clone() }),
            &mut open,
            &opts(&clock, Some(8), policy),
        )
        .unwrap();
        assert_eq!(
            job.events,
            ["lost", "reopen-failed", "reopen-failed", "reconnected"]
        );
        assert_eq!(job.reopened, 1);
        assert_eq!((r.drops, r.reconnects, r.reopen_attempts), (1, 1, 3));
        assert_eq!(r.rounds, 8);
        assert!(job.rounds[5..].iter().all(|(_, ok)| *ok));
        // dead round 3 → pause 50; round 4 dead → lost; attempts after 50/100/200.
        assert_eq!(clock.sleeps(), [50, 50, 100, 200]);
        assert!(r.stopped_cleanly);
    }

    #[test]
    fn gives_up_after_the_attempt_limit_without_stop() {
        let clock = VirtualClock::new(0);
        let dead = Rc::new(RefCell::new(true));
        let mut job = Job::default();
        let mut open = || -> Result<Box<dyn RawDiag>, String> { Err("gone".into()) };
        let policy = ReconnectPolicy {
            initial_backoff_ms: 10,
            max_backoff_ms: 40,
            dead_after: 1,
            max_attempts: Some(4),
            ..ReconnectPolicy::reconnecting()
        };
        let r = run_resilient(
            &mut job,
            Box::new(Switch { dead }),
            &mut open,
            &opts(&clock, None, policy),
        )
        .unwrap();
        assert_eq!(r.ended, EndReason::GaveUp);
        assert_eq!(r.reopen_attempts, 4);
        assert_eq!(job.events.last().map(String::as_str), Some("gave-up"));
        assert_eq!(job.stops, 0);
        assert_eq!(clock.sleeps(), [10, 20, 40, 40]);
    }

    #[test]
    fn gives_up_after_the_time_limit_and_honours_duration_and_stop() {
        let clock = VirtualClock::new(0);
        let mut job = Job::default();
        let mut open = || -> Result<Box<dyn RawDiag>, String> { Err("gone".into()) };
        let policy = ReconnectPolicy {
            initial_backoff_ms: 1000,
            max_backoff_ms: 1000,
            dead_after: 1,
            give_up_after_ms: Some(5000),
            ..ReconnectPolicy::reconnecting()
        };
        let r = run_resilient(
            &mut job,
            Box::new(Switch {
                dead: Rc::new(RefCell::new(true)),
            }),
            &mut open,
            &opts(&clock, None, policy.clone()),
        )
        .unwrap();
        assert_eq!((r.ended, r.reopen_attempts), (EndReason::GaveUp, 5));

        // "Never give up" ends only on the duration.
        let clock = VirtualClock::new(0);
        let mut o = opts(
            &clock,
            None,
            ReconnectPolicy {
                give_up_after_ms: None,
                ..policy
            },
        );
        o.limits.duration_ms = Some(60_000);
        let r = run_resilient(
            &mut Job::default(),
            Box::new(Switch {
                dead: Rc::new(RefCell::new(true)),
            }),
            &mut open,
            &o,
        )
        .unwrap();
        assert_eq!(r.ended, EndReason::Duration);
        assert_eq!(clock.now_ms(), 60_000);
        assert!(clock.sleeps().iter().all(|&s| s > 0 && s <= SLEEP_SLICE_MS));

        // A set stop flag ends before the first round.
        let flag = AtomicBool::new(true);
        let clock = VirtualClock::new(0);
        let mut o = opts(&clock, None, ReconnectPolicy::default());
        o.stop = Some(&flag);
        let r = run_resilient(
            &mut Job::default(),
            Box::new(Switch {
                dead: Rc::new(RefCell::new(false)),
            }),
            &mut open,
            &o,
        )
        .unwrap();
        assert_eq!((r.ended, r.rounds), (EndReason::Stopped, 0));
        assert!(r.stopped_cleanly);
    }

    #[test]
    fn negative_responses_are_alive_and_no_requests_is_not_dead() {
        let mut nrc = |_: &[u8]| -> Result<Vec<u8>, String> { Ok(vec![0x7F, 0x22, 0x31]) };
        let mut l = Liveness::new(&mut nrc);
        l.request(&[0x22]).unwrap();
        assert!(!l.dead());
        let mut never = |_: &[u8]| -> Result<Vec<u8>, String> { Err("x".into()) };
        let l = Liveness::new(&mut never);
        assert!(!l.dead(), "a round that sent nothing is not dead");
    }
}
