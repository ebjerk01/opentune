//! What the diagnostics upload keeps on disk, under
//! `<app data dir>/diagnostics/`:
//!
//! * `state.json` — the user's settings ("Allow sending diagnostics", the
//!   auto-mode off switch, the notice seen), the send times of the last 24
//!   hours (the rate limit survives a restart), the crash already reported,
//!   the upload history index and the offline queue index;
//! * `history/<id>.json` — the exact payload of each of the last
//!   [`HISTORY_LEN`] uploads;
//! * `queue/<id>.json` — payloads waiting for the next allowed slot (auto
//!   mode only), at most [`QUEUE_CAP`] bytes, the oldest dropped first.
//!
//! No token or endpoint is ever written here.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Upload history entries kept (each with its exact payload).
pub const HISTORY_LEN: usize = 20;
/// Total size of the offline queue.
pub const QUEUE_CAP: usize = 2 * 1024 * 1024;
/// Automatic sends: at most one per this many seconds ...
pub const MIN_GAP_S: u64 = 10 * 60;
/// ... and at most this many per 24 hours (manual sends count too).
pub const PER_DAY: usize = 20;
const DAY_S: u64 = 24 * 60 * 60;

/// The user's choices.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// "Allow sending diagnostics" (manual mode; off by default).
    pub allow_send: bool,
    /// The global off switch for an embedder's automatic mode.
    pub auto_off: bool,
    /// The upload host whose first-start notice was shown.
    pub notice_seen: Option<String>,
    /// The crashed session (its header time) whose send offer was dismissed.
    pub crash_dismissed: Option<String>,
}

/// One upload attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: String,
    /// UTC, RFC 3339.
    pub ts: String,
    /// `manual`, `crash`, `error` or `periodic`.
    pub trigger: String,
    /// Payload size in bytes.
    pub bytes: usize,
    /// `sent (200)`, `queued: …`, `failed: …`, `rejected (401)`, `dropped: …`.
    pub status: String,
}

/// One payload waiting in the offline queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueEntry {
    pub id: String,
    pub trigger: String,
    pub created: String,
    pub bytes: usize,
    #[serde(default)]
    pub tries: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Persist {
    settings: Settings,
    /// Unix seconds of the send attempts of the last 24 hours.
    sends: Vec<u64>,
    /// Header time of the crashed session already reported.
    crash_done: Option<String>,
    history: Vec<HistoryEntry>,
    queue: Vec<QueueEntry>,
}

/// The on-disk state (or an in-memory one when there is no data dir).
#[derive(Debug)]
pub struct Store {
    dir: Option<PathBuf>,
    p: Persist,
}

/// When the next send may go out: `None` = now, `Some(t)` = at unix time `t`.
/// `gap` is the minimum time since the last attempt (0 for a manual send).
pub fn next_slot(sends: &[u64], now: u64, gap: u64) -> Option<u64> {
    let recent: Vec<u64> = sends.iter().copied().filter(|&t| t + DAY_S > now).collect();
    let mut at = 0;
    if let Some(&last) = recent.iter().max() {
        at = at.max(last + gap);
    }
    if recent.len() >= PER_DAY {
        let mut sorted = recent.clone();
        sorted.sort_unstable();
        // The oldest one that has to age out before a new send fits.
        at = at.max(sorted[recent.len() - PER_DAY] + DAY_S);
    }
    (at > now).then_some(at)
}

fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    if let Some(d) = path.parent() {
        fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, data)?;
    fs::rename(&tmp, path)
}

impl Store {
    /// Load `dir/state.json` (a missing or unreadable file starts fresh).
    pub fn open(dir: Option<PathBuf>) -> Store {
        let p = dir
            .as_ref()
            .and_then(|d| fs::read(d.join("state.json")).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Store { dir, p }
    }

    fn save(&self) -> Result<(), String> {
        let Some(d) = &self.dir else { return Ok(()) };
        let data = serde_json::to_vec_pretty(&self.p).map_err(|e| e.to_string())?;
        write_atomic(&d.join("state.json"), &data)
            .map_err(|e| format!("cannot save the diagnostics settings: {e}"))
    }

    fn file(&self, kind: &str, id: &str) -> Option<PathBuf> {
        // Ids are generated here (digits and dashes), never user input.
        let ok = !id.is_empty() && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-');
        self.dir
            .as_ref()
            .filter(|_| ok)
            .map(|d| d.join(kind).join(format!("{id}.json")))
    }

    pub fn settings(&self) -> &Settings {
        &self.p.settings
    }

    /// Change the settings and save them.
    pub fn update_settings(&mut self, f: impl FnOnce(&mut Settings)) -> Result<(), String> {
        f(&mut self.p.settings);
        self.save()
    }

    pub fn sends(&self) -> &[u64] {
        &self.p.sends
    }

    /// Count one send attempt at `now` (older than a day are forgotten).
    pub fn note_send(&mut self, now: u64) {
        self.p.sends.retain(|&t| t + DAY_S > now);
        self.p.sends.push(now);
        let _ = self.save();
    }

    pub fn crash_done(&self) -> Option<&str> {
        self.p.crash_done.as_deref()
    }

    pub fn set_crash_done(&mut self, ts: &str) {
        self.p.crash_done = Some(ts.to_string());
        let _ = self.save();
    }

    pub fn history(&self) -> &[HistoryEntry] {
        &self.p.history
    }

    /// Add one attempt with its exact payload; keeps the last [`HISTORY_LEN`].
    pub fn add_history(&mut self, e: HistoryEntry, body: &[u8]) {
        if let Some(f) = self.file("history", &e.id) {
            let _ = write_atomic(&f, body);
        }
        self.p.history.push(e);
        while self.p.history.len() > HISTORY_LEN {
            let old = self.p.history.remove(0);
            if let Some(f) = self.file("history", &old.id) {
                let _ = fs::remove_file(f);
            }
        }
        let _ = self.save();
    }

    /// The exact payload of a history entry.
    pub fn history_payload(&self, id: &str) -> Option<Vec<u8>> {
        self.p.history.iter().find(|e| e.id == id)?;
        fs::read(self.file("history", id)?).ok()
    }

    pub fn queue(&self) -> &[QueueEntry] {
        &self.p.queue
    }

    pub fn queue_bytes(&self) -> usize {
        self.p.queue.iter().map(|q| q.bytes).sum()
    }

    /// Queue a payload; the oldest entries are dropped while the queue is
    /// over [`QUEUE_CAP`]. Returns the ids dropped. Without a data dir
    /// nothing can be queued (returns the entry's own id).
    pub fn enqueue(&mut self, e: QueueEntry, body: &[u8]) -> Vec<String> {
        let Some(f) = self.file("queue", &e.id) else {
            return vec![e.id];
        };
        if body.len() > QUEUE_CAP || write_atomic(&f, body).is_err() {
            return vec![e.id];
        }
        self.p.queue.push(e);
        let mut dropped = Vec::new();
        while self.queue_bytes() > QUEUE_CAP && self.p.queue.len() > 1 {
            let old = self.p.queue.remove(0);
            if let Some(f) = self.file("queue", &old.id) {
                let _ = fs::remove_file(f);
            }
            dropped.push(old.id);
        }
        let _ = self.save();
        dropped
    }

    /// The oldest queued payload.
    pub fn queue_front(&self) -> Option<(QueueEntry, Vec<u8>)> {
        let e = self.p.queue.first()?.clone();
        let body = fs::read(self.file("queue", &e.id)?).ok()?;
        Some((e, body))
    }

    /// Remove a queued payload.
    pub fn dequeue(&mut self, id: &str) {
        if let Some(f) = self.file("queue", id) {
            let _ = fs::remove_file(f);
        }
        self.p.queue.retain(|q| q.id != id);
        let _ = self.save();
    }

    /// One more failed try of a queued payload.
    pub fn queue_tried(&mut self, id: &str) {
        if let Some(q) = self.p.queue.iter_mut().find(|q| q.id == id) {
            q.tries += 1;
        }
        let _ = self.save();
    }

    /// A queue entry whose payload file is gone is dropped.
    pub fn drop_broken_front(&mut self) {
        if let Some(e) = self.p.queue.first().cloned() {
            if self.queue_front().is_none() {
                self.dequeue(&e.id);
            }
        }
    }
}
