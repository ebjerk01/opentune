//! Structured, timestamped session transcript for a flash.
//!
//! A [`Transcript`] collects [`FlashEvent`]s as a flash runs and can be written
//! out two ways next to the vault backup:
//!
//! - `<stem>.log`   — human-readable, one timestamped line per event.
//! - `<stem>.jsonl` — machine-readable, one JSON object per line.
//!
//! The transcript is purely a record; it never drives the flash. Callers hand a
//! `&mut Transcript` to [`crate::flash_calibration_resumable_logged`], then
//! persist it with [`Transcript::write`] / [`Transcript::write_next_to`].
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// One thing that happened during a flash session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum FlashEvent {
    /// Session opened: ECU identity + target region, and whether this run
    /// resumed an existing checkpoint.
    SessionStart {
        ecu: Option<String>,
        address: u32,
        length: u32,
        resumed: bool,
    },
    /// Battery voltage read at the voltage gate.
    Voltage { millivolts: Option<u32> },
    /// Pre-flash backup of the original persisted.
    BackupSaved {
        path: String,
        crc32: u32,
        len: usize,
    },
    /// A chunk was processed. `reused` = the ECU already held the intended
    /// bytes (verified without rewriting), e.g. after an interrupted run.
    ChunkWrite {
        index: usize,
        offset: u32,
        len: usize,
        reused: bool,
    },
    /// Read-back verification of a chunk.
    ChunkVerify { index: usize, ok: bool },
    /// Final whole-region read-back verification.
    FinalVerify { ok: bool },
    /// An error aborted the session.
    Error { message: String },
    /// Session closed.
    SessionEnd {
        ok: bool,
        bytes_written: usize,
        chunks_total: usize,
        chunks_reused: usize,
    },
}

/// A transcript entry: an event plus the wall-clock time it was recorded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptEntry {
    /// Milliseconds since the Unix epoch.
    pub t_ms: u128,
    #[serde(flatten)]
    pub event: FlashEvent,
}

/// An ordered, timestamped log of a flash session.
#[derive(Debug, Clone, Default)]
pub struct Transcript {
    entries: Vec<TranscriptEntry>,
}

impl Transcript {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append `event`, stamped with the current time.
    pub fn record(&mut self, event: FlashEvent) {
        let t_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        self.entries.push(TranscriptEntry { t_ms, event });
    }

    pub fn entries(&self) -> &[TranscriptEntry] {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Render the human-readable `.log` body.
    pub fn to_log(&self) -> String {
        let mut s = String::new();
        for e in &self.entries {
            s.push_str(&format!(
                "[{}] {}\n",
                fmt_utc_ms(e.t_ms),
                describe(&e.event)
            ));
        }
        s
    }

    /// Render the machine-readable `.jsonl` body (one object per line).
    pub fn to_jsonl(&self) -> String {
        let mut s = String::new();
        for e in &self.entries {
            // Each entry is a flat struct; serialization cannot fail for these
            // plain types, but never unwrap — fall back to a skip marker.
            match serde_json::to_string(e) {
                Ok(line) => {
                    s.push_str(&line);
                    s.push('\n');
                }
                Err(_) => s.push_str("{\"error\":\"serialize_failed\"}\n"),
            }
        }
        s
    }

    /// Write `<stem>.log` and `<stem>.jsonl` into `dir`. Returns their paths.
    pub fn write(&self, dir: &Path, stem: &str) -> std::io::Result<(PathBuf, PathBuf)> {
        std::fs::create_dir_all(dir)?;
        let log = dir.join(format!("{stem}.log"));
        let jsonl = dir.join(format!("{stem}.jsonl"));
        std::fs::write(&log, self.to_log())?;
        std::fs::write(&jsonl, self.to_jsonl())?;
        Ok((log, jsonl))
    }

    /// Write the transcript next to a vault backup image, reusing its stem.
    /// e.g. `.../edc16_00000000_123.bin` → `edc16_00000000_123.{log,jsonl}`.
    pub fn write_next_to(&self, backup_path: &Path) -> std::io::Result<(PathBuf, PathBuf)> {
        let dir = backup_path.parent().unwrap_or_else(|| Path::new("."));
        let stem = backup_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("session");
        self.write(dir, stem)
    }
}

/// One-line human description of an event (used by the `.log` renderer).
fn describe(e: &FlashEvent) -> String {
    match e {
        FlashEvent::SessionStart {
            ecu,
            address,
            length,
            resumed,
        } => format!(
            "session start ecu={} region=0x{:08X} +{} resumed={}",
            ecu.as_deref().unwrap_or("?"),
            address,
            length,
            resumed
        ),
        FlashEvent::Voltage { millivolts } => match millivolts {
            Some(mv) => format!("voltage {mv} mV"),
            None => "voltage unreadable".into(),
        },
        FlashEvent::BackupSaved { path, crc32, len } => {
            format!("backup saved {path} ({len} bytes, crc32=0x{crc32:08X})")
        }
        FlashEvent::ChunkWrite {
            index,
            offset,
            len,
            reused,
        } => format!(
            "chunk {index} @+0x{offset:X} {len} bytes {}",
            if *reused {
                "already correct (reused)"
            } else {
                "written"
            }
        ),
        FlashEvent::ChunkVerify { index, ok } => {
            format!("chunk {index} verify {}", if *ok { "ok" } else { "FAILED" })
        }
        FlashEvent::FinalVerify { ok } => {
            format!("final verify {}", if *ok { "ok" } else { "FAILED" })
        }
        FlashEvent::Error { message } => format!("ERROR {message}"),
        FlashEvent::SessionEnd {
            ok,
            bytes_written,
            chunks_total,
            chunks_reused,
        } => format!(
            "session end {} bytes_written={bytes_written} chunks={chunks_total} reused={chunks_reused}",
            if *ok { "ok" } else { "ABORTED" }
        ),
    }
}

/// Format epoch-milliseconds as `YYYY-MM-DDTHH:MM:SS.mmmZ` (UTC), no deps.
fn fmt_utc_ms(ms: u128) -> String {
    let secs = (ms / 1000) as i64;
    let millis = (ms % 1000) as u32;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (hh, mm, ss) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{millis:03}Z")
}

/// Days since 1970-01-01 → (year, month, day). Hinnant's civil-from-days.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonl_round_trips() {
        let mut t = Transcript::new();
        t.record(FlashEvent::SessionStart {
            ecu: Some("edc16".into()),
            address: 0x8000,
            length: 256,
            resumed: false,
        });
        t.record(FlashEvent::ChunkWrite {
            index: 0,
            offset: 0,
            len: 128,
            reused: false,
        });
        let jsonl = t.to_jsonl();
        let lines: Vec<&str> = jsonl.lines().collect();
        assert_eq!(lines.len(), 2);
        // Each line parses back into an entry with the right tag.
        let first: TranscriptEntry = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(
            first.event,
            FlashEvent::SessionStart {
                ecu: Some("edc16".into()),
                address: 0x8000,
                length: 256,
                resumed: false,
            }
        );
        assert!(lines[0].contains("\"event\":\"session_start\""));
    }

    #[test]
    fn log_is_human_readable() {
        let mut t = Transcript::new();
        t.record(FlashEvent::Voltage {
            millivolts: Some(12_600),
        });
        let log = t.to_log();
        assert!(log.contains("voltage 12600 mV"));
        assert!(log.starts_with('['));
    }

    #[test]
    fn civil_epoch() {
        // 2026-11-16 was day 20773 since the epoch.
        assert_eq!(fmt_utc_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(31), (1970, 2, 1));
    }
}
