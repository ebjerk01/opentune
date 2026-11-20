//! The live feed: Dashboard and Live log publish every poll here, and the
//! Drive view subscribes, so the Drive view never opens an adapter
//! operation of its own (the Tauri app's `OTDrive.feed`).
//!
//! Publishers and subscribers only share this type, so the panels that
//! publish and the one that reads stay in separate files. Delivery never
//! blocks a publisher: a subscriber whose queue is full misses frames (it
//! shows the next one), and a dropped subscriber is forgotten.

use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

/// One channel value in a frame.
#[derive(Clone, Debug, PartialEq)]
pub struct FeedValue {
    /// Stable key (a PID / parameter id; `key#i` for multi-sensor values).
    pub key: String,
    pub name: String,
    pub unit: String,
    /// `None`: the channel had no (numeric) value in this poll.
    pub value: Option<f64>,
}

/// What a publisher sends.
#[derive(Clone, Debug, PartialEq)]
pub enum FeedEvent {
    /// One poll's values. `source` names the publisher ("dashboard",
    /// "livelog", "gps"); `t_ms` is milliseconds since the Unix epoch.
    Frame {
        source: String,
        t_ms: u64,
        values: Vec<FeedValue>,
    },
    /// A poll failed (`busy`: another operation holds the adapter, the link
    /// itself may be fine).
    Error {
        source: String,
        message: String,
        busy: bool,
    },
    /// The publisher stopped polling.
    Stop { source: String },
}

/// Frames a subscriber may have queued before it starts missing them.
pub const FEED_QUEUE: usize = 256;

/// The app-wide live feed. Cheap to clone; publish from any thread.
#[derive(Clone, Default)]
pub struct LiveFeed {
    subs: Arc<Mutex<Vec<SyncSender<FeedEvent>>>>,
    repaint: Option<eframe::egui::Context>,
}

impl LiveFeed {
    /// A feed that asks `ctx` to repaint when something is published.
    pub fn new(ctx: eframe::egui::Context) -> Self {
        Self {
            subs: Default::default(),
            repaint: Some(ctx),
        }
    }

    /// Start receiving events (from now on). Drop the receiver to stop.
    pub fn subscribe(&self) -> Receiver<FeedEvent> {
        let (tx, rx) = mpsc::sync_channel(FEED_QUEUE);
        if let Ok(mut s) = self.subs.lock() {
            s.push(tx);
        }
        rx
    }

    /// Live subscribers (a publisher may skip work nobody reads).
    pub fn subscribers(&self) -> usize {
        self.subs.lock().map(|s| s.len()).unwrap_or(0)
    }

    /// Deliver `ev` to every subscriber without blocking.
    pub fn publish(&self, ev: FeedEvent) {
        if let Ok(mut s) = self.subs.lock() {
            s.retain(|tx| !matches!(tx.try_send(ev.clone()), Err(TrySendError::Disconnected(_))));
        }
        if let Some(c) = &self.repaint {
            c.request_repaint();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(n: u64) -> FeedEvent {
        FeedEvent::Frame {
            source: "dashboard".into(),
            t_ms: n,
            values: vec![FeedValue {
                key: "0C".into(),
                name: "RPM".into(),
                unit: "rpm".into(),
                value: Some(800.0),
            }],
        }
    }

    #[test]
    fn publishers_never_block_and_dropped_subscribers_are_forgotten() {
        let feed = LiveFeed::default();
        feed.publish(frame(0)); // nobody listens: fine
        let a = feed.subscribe();
        let b = feed.subscribe();
        assert_eq!(feed.subscribers(), 2);
        feed.publish(frame(1));
        assert_eq!(a.try_recv().unwrap(), frame(1));
        drop(b);
        // A full queue drops frames for that subscriber only.
        for i in 0..(FEED_QUEUE as u64 + 10) {
            feed.publish(frame(i));
        }
        assert_eq!(feed.subscribers(), 1);
        assert_eq!(a.try_iter().count(), FEED_QUEUE);
        feed.publish(FeedEvent::Stop {
            source: "dashboard".into(),
        });
        assert!(matches!(a.try_recv().unwrap(), FeedEvent::Stop { .. }));
    }
}
