//! A live source that re-opens itself after it ends: a USB receiver that is
//! unplugged mid-log ends its [`GpsReader`](crate::GpsReader) with a read
//! error; [`ReopenFeed`] then tries to open it again on later snapshots,
//! with backoff, and carries on once it is back. Until then the snapshots are
//! not good and say why, so the log keeps going with empty GPS channels.
//!
//! It never sleeps: [`GpsFeed::snapshot`] is called once per logged sample,
//! and an attempt is only made once the backoff has passed, so a missing port
//! costs one failed open per backoff period, not one per sample.

use crate::fix::GpsSnapshot;
use crate::GpsFeed;
use std::time::Instant;

/// Opens (or re-opens) the underlying source.
pub type OpenFn = Box<dyn FnMut() -> Result<Box<dyn GpsFeed>, String> + Send>;

/// A [`GpsFeed`] that re-opens its source after it ends.
pub struct ReopenFeed {
    open: OpenFn,
    inner: Option<Box<dyn GpsFeed>>,
    label: String,
    now: Box<dyn Fn() -> u64 + Send>,
    initial_ms: u64,
    max_ms: u64,
    backoff_ms: u64,
    next_try_ms: u64,
    attempts: u32,
    reopens: u32,
    ended: Option<String>,
    last_error: Option<String>,
}

impl ReopenFeed {
    /// Open the source now (an error if that fails) and re-open it after it
    /// ends: first after `initial_ms`, then doubling up to `max_ms`.
    pub fn new(
        label: impl Into<String>,
        mut open: OpenFn,
        initial_ms: u64,
        max_ms: u64,
    ) -> Result<Self, String> {
        let inner = open()?;
        let t0 = Instant::now();
        Ok(Self {
            open,
            inner: Some(inner),
            label: label.into(),
            now: Box::new(move || u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX)),
            initial_ms: initial_ms.max(1),
            max_ms: max_ms.max(initial_ms).max(1),
            backoff_ms: initial_ms.max(1),
            next_try_ms: 0,
            attempts: 0,
            reopens: 0,
            ended: None,
            last_error: None,
        })
    }

    /// Use `now` (milliseconds) instead of the wall clock (tests).
    pub fn with_time(mut self, now: impl Fn() -> u64 + Send + 'static) -> Self {
        self.now = Box::new(now);
        self
    }

    /// Successful re-opens so far.
    pub fn reopens(&self) -> u32 {
        self.reopens
    }

    /// Re-open attempts so far (successful or not).
    pub fn attempts(&self) -> u32 {
        self.attempts
    }
}

impl GpsFeed for ReopenFeed {
    fn snapshot(&mut self) -> GpsSnapshot {
        let now = (self.now)();
        let snap = self.inner.as_mut().map(|f| f.snapshot());
        match &snap {
            Some(s) if s.ended.is_none() => {
                self.backoff_ms = self.initial_ms;
                return snap.unwrap_or_default();
            }
            Some(s) => {
                if self.ended.is_none() {
                    // First time we see it end: wait one backoff, then try.
                    self.ended = s.ended.clone();
                    self.next_try_ms = now.saturating_add(self.initial_ms);
                }
            }
            None => {}
        }
        if now >= self.next_try_ms {
            self.attempts += 1;
            self.inner = None; // release the old port first
            match (self.open)() {
                Ok(mut f) => {
                    self.reopens += 1;
                    self.ended = None;
                    self.last_error = None;
                    self.backoff_ms = self.initial_ms;
                    let mut s = f.snapshot();
                    self.inner = Some(f);
                    if !s.good {
                        s.note = format!("{} (re-opened, attempt {})", s.note, self.attempts);
                    }
                    return s;
                }
                Err(e) => {
                    self.last_error = Some(e);
                    self.next_try_ms = now.saturating_add(self.backoff_ms);
                    self.backoff_ms = self.backoff_ms.saturating_mul(2).min(self.max_ms);
                }
            }
        }
        let mut s = snap.unwrap_or_else(|| GpsSnapshot {
            stale: true,
            ..GpsSnapshot::default()
        });
        s.good = false;
        s.stale = true;
        let why = self.ended.as_deref().unwrap_or("source ended");
        let retry = self.next_try_ms.saturating_sub(now) as f64 / 1000.0;
        s.note = match &self.last_error {
            Some(e) => format!(
                "GPS source lost ({why}); re-open attempt {} failed: {e}; next in {retry:.1} s",
                self.attempts
            ),
            None => format!("GPS source lost ({why}); re-opening in {retry:.1} s"),
        };
        s.ended = self.ended.clone();
        s
    }

    fn describe(&self) -> String {
        self.label.clone()
    }

    fn wait_ready(&mut self, timeout: std::time::Duration) {
        if let Some(f) = self.inner.as_mut() {
            f.wait_ready(timeout);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ReplayFeed;
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    use std::sync::Arc;

    /// One line of data: the replayed "port" yields one epoch, then ends.
    const EPOCH: &[u8] = b"$GPTXT,01,01,02,synthetic\r\n";

    #[test]
    fn reopens_after_the_source_ends_with_backoff() {
        let opened = Arc::new(AtomicU32::new(0));
        let up = Arc::new(AtomicU32::new(1)); // 1 = the port exists
        let (o, u) = (Arc::clone(&opened), Arc::clone(&up));
        let open: OpenFn = Box::new(move || {
            if u.load(Ordering::SeqCst) == 0 {
                return Err("no such port".into());
            }
            o.fetch_add(1, Ordering::SeqCst);
            // One epoch, then the "port" ends.
            Ok(Box::new(ReplayFeed::from_bytes("port", EPOCH)) as Box<dyn GpsFeed>)
        });
        let t = Arc::new(AtomicU64::new(0));
        let tt = Arc::clone(&t);
        let mut f = ReopenFeed::new("gps", open, 1000, 4000)
            .unwrap()
            .with_time(move || tt.load(Ordering::SeqCst));
        assert!(f.snapshot().ended.is_none()); // the one epoch
        up.store(0, Ordering::SeqCst); // unplugged
        let s = f.snapshot(); // ended: wait one backoff
        assert!(!s.good && s.note.contains("re-opening in 1.0 s"), "{}", s.note);
        let mut tried = 0;
        for ms in (0..=7000).step_by(100) {
            t.store(ms, Ordering::SeqCst);
            let s = f.snapshot();
            assert!(!s.good && s.note.starts_with("GPS source lost"), "{}", s.note);
            tried = f.attempts();
        }
        // Attempts at 1.0, 2.0, 4.0 and 8.0 (not yet) s: backoff, not per sample.
        assert_eq!(tried, 3);
        up.store(1, Ordering::SeqCst);
        t.store(20_000, Ordering::SeqCst);
        let s = f.snapshot();
        assert!(s.ended.is_none(), "{s:?}");
        assert_eq!((f.reopens(), opened.load(Ordering::SeqCst)), (1, 2));
    }
}
