//! The Drive view's live state, fed by the app's live feed (the Dashboard
//! and Live log publish their polls there; this view never opens an adapter
//! operation of its own). Pure apart from the clock the caller passes.

use super::core::{self, Chan, Hist, Link, Peak, HIST_S};
use crate::host::FeedEvent;
use std::collections::BTreeMap;

/// A live channel.
#[derive(Clone, Debug, Default)]
pub struct LiveChan {
    pub chan: Chan,
    /// `(value, seconds)` of the last valid value.
    pub last: Option<(f64, f64)>,
    pub hist: Hist,
    pub peak: Peak,
}

/// What the live tiles show (one source at a time).
#[derive(Clone, Debug)]
pub struct LiveState {
    /// The source shown (`dashboard`, `livelog`).
    pub src: Option<String>,
    pub last_frame: Option<f64>,
    /// Smoothed update interval, seconds.
    pub interval: f64,
    /// Consecutive failed polls (busy ones do not count).
    pub errors: u32,
    pub last_error: String,
    pub stopped: bool,
    /// In first-seen order.
    pub chans: Vec<LiveChan>,
    index: BTreeMap<String, usize>,
    /// New channels arrived (the view re-picks its default tiles).
    pub fresh: bool,
}

impl Default for LiveState {
    fn default() -> Self {
        Self {
            src: None,
            last_frame: None,
            interval: 0.5,
            errors: 0,
            last_error: String::new(),
            stopped: false,
            chans: Vec::new(),
            index: BTreeMap::new(),
            fresh: false,
        }
    }
}

impl LiveState {
    pub fn chan(&self, key: &str) -> Option<&LiveChan> {
        self.index.get(key).map(|&i| &self.chans[i])
    }

    pub fn list(&self) -> Vec<Chan> {
        self.chans.iter().map(|c| c.chan.clone()).collect()
    }

    pub fn stale_s(&self) -> f64 {
        core::stale_after(self.interval)
    }

    /// The link at `now` (two failed polls in a row count as lost).
    pub fn link(&self, now: f64) -> Link {
        core::link_state(
            self.stopped,
            self.errors >= 2,
            self.last_frame,
            now,
            self.stale_s(),
        )
    }

    /// Take one feed event at `now` (seconds on the view's clock).
    pub fn apply(&mut self, ev: &FeedEvent, now: f64) {
        match ev {
            FeedEvent::Frame { source, values, .. } => {
                // Two sources at once: keep the one showing while it is fresh.
                if let (Some(src), Some(t)) = (&self.src, self.last_frame) {
                    if src != source && !self.stopped && now - t < self.stale_s() {
                        return;
                    }
                }
                if self.src.as_deref() != Some(source) {
                    *self = LiveState {
                        src: Some(source.clone()),
                        fresh: true,
                        ..Default::default()
                    };
                }
                if let Some(t) = self.last_frame {
                    self.interval = (0.7 * self.interval + 0.3 * (now - t)).clamp(0.05, 10.0);
                }
                self.last_frame = Some(now);
                self.errors = 0;
                self.last_error.clear();
                self.stopped = false;
                for v in values {
                    let i = match self.index.get(&v.key) {
                        Some(&i) => i,
                        None => {
                            self.chans.push(LiveChan {
                                chan: Chan {
                                    key: v.key.clone(),
                                    name: v.name.clone(),
                                    unit: v.unit.clone(),
                                },
                                ..Default::default()
                            });
                            self.index.insert(v.key.clone(), self.chans.len() - 1);
                            self.fresh = true;
                            self.chans.len() - 1
                        }
                    };
                    let c = &mut self.chans[i];
                    let val = v.value.filter(|x| x.is_finite());
                    if let Some(x) = val {
                        c.last = Some((x, now));
                        c.peak.add(Some(x), now);
                    }
                    c.hist.push(now, val, HIST_S);
                }
            }
            FeedEvent::Error {
                source,
                message,
                busy,
            } => {
                if self.src.as_ref().is_some_and(|s| s != source) {
                    return;
                }
                if !busy {
                    self.errors += 1;
                }
                self.last_error = message.clone();
            }
            FeedEvent::Stop { source } => {
                if self.src.as_ref().is_none_or(|s| s == source) {
                    self.stopped = true;
                }
            }
        }
    }

    /// Forget min / max.
    pub fn reset_peaks(&mut self) {
        for c in &mut self.chans {
            c.peak = Peak::default();
        }
    }
}
