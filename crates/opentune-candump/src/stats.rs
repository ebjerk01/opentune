//! Live capture statistics.
use crate::writer::hex;
use opentune_core::TimedFrame;
use serde::Serialize;
use std::collections::BTreeMap;

/// Per-identifier counters.
#[derive(Debug, Clone, Default)]
struct IdStat {
    count: u64,
    first_us: u64,
    last_us: u64,
    last: Vec<u8>,
}

/// Aggregates frames as they are captured.
#[derive(Debug, Clone, Default)]
pub struct CaptureStats {
    frames: u64,
    payload_bytes: u64,
    dropped: u64,
    /// Keyed by (id, extended) so 0x7E8 standard and 0x7E8 extended differ.
    ids: BTreeMap<(u32, bool), IdStat>,
}

/// One row of the per-identifier table.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IdSnapshot {
    /// CAN identifier.
    pub id: u32,
    /// Extended (29-bit) identifier.
    pub ext: bool,
    /// Frames seen with this id.
    pub count: u64,
    /// Average rate over the capture of this id: `(count - 1) / (last - first)`.
    /// 0 until two frames have been seen.
    pub hz: f64,
    /// Data length of the latest frame.
    pub dlc: usize,
    /// Latest payload, uppercase hex.
    pub last: String,
}

/// A point-in-time view of the statistics.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatsSnapshot {
    /// Frames captured (after filtering).
    pub frames: u64,
    /// Payload bytes captured.
    pub payload_bytes: u64,
    /// Average frames per second over `elapsed_s`.
    pub fps: f64,
    /// Capture duration so far, seconds.
    pub elapsed_s: f64,
    /// Frames / overflow events the adapter reported lost.
    pub dropped: u64,
    /// Distinct identifiers seen (may exceed `ids.len()` when truncated).
    pub unique_ids: usize,
    /// Per-id rows sorted by (id, ext), at most the requested limit.
    pub ids: Vec<IdSnapshot>,
}

impl CaptureStats {
    /// Empty statistics.
    pub fn new() -> Self {
        Self::default()
    }

    /// Account one captured frame.
    pub fn record(&mut self, f: &TimedFrame) {
        self.frames += 1;
        self.payload_bytes += f.data().len() as u64;
        let s = self.ids.entry((f.id(), f.extended)).or_default();
        if s.count == 0 {
            s.first_us = f.ts_us;
        }
        s.count += 1;
        s.last_us = f.ts_us;
        s.last.clear();
        s.last.extend_from_slice(f.data());
    }

    /// Set the adapter's cumulative dropped counter.
    pub fn set_dropped(&mut self, dropped: u64) {
        self.dropped = dropped;
    }

    /// Frames captured so far.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Payload bytes captured so far.
    pub fn payload_bytes(&self) -> u64 {
        self.payload_bytes
    }

    /// Dropped counter.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Distinct (id, ext) pairs seen.
    pub fn unique_ids(&self) -> usize {
        self.ids.len()
    }

    /// Snapshot at `elapsed_us` into the capture, listing at most `max_ids` ids.
    pub fn snapshot(&self, elapsed_us: u64, max_ids: usize) -> StatsSnapshot {
        let elapsed_s = elapsed_us as f64 / 1e6;
        let fps = if elapsed_s > 0.0 {
            self.frames as f64 / elapsed_s
        } else {
            0.0
        };
        let ids = self
            .ids
            .iter()
            .take(max_ids)
            .map(|(&(id, ext), s)| {
                let span = s.last_us.saturating_sub(s.first_us);
                let hz = if s.count >= 2 && span > 0 {
                    (s.count - 1) as f64 * 1e6 / span as f64
                } else {
                    0.0
                };
                IdSnapshot {
                    id,
                    ext,
                    count: s.count,
                    hz,
                    dlc: s.last.len(),
                    last: hex(&s.last),
                }
            })
            .collect();
        StatsSnapshot {
            frames: self.frames,
            payload_bytes: self.payload_bytes,
            fps,
            elapsed_s,
            dropped: self.dropped,
            unique_ids: self.ids.len(),
            ids,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregates_per_id() {
        let mut s = CaptureStats::new();
        // 0x280 every 10 ms (100 Hz), 11 frames over 100 ms.
        for i in 0..11u64 {
            s.record(&TimedFrame::classic(
                i * 10_000,
                0x280,
                false,
                vec![i as u8; 8],
            ));
        }
        // 0x1A0 every 20 ms, 6 frames.
        for i in 0..6u64 {
            s.record(&TimedFrame::classic(
                i * 20_000,
                0x1A0,
                false,
                vec![0xAB, i as u8],
            ));
        }
        // One extended frame with the same numeric id as a standard one.
        s.record(&TimedFrame::classic(50_000, 0x280, true, vec![]));
        s.set_dropped(3);

        let snap = s.snapshot(1_000_000, 256);
        assert_eq!(snap.frames, 18);
        assert_eq!(snap.payload_bytes, 11 * 8 + 6 * 2);
        assert!((snap.fps - 18.0).abs() < 1e-9);
        assert!((snap.elapsed_s - 1.0).abs() < 1e-9);
        assert_eq!(snap.dropped, 3);
        assert_eq!(snap.unique_ids, 3);

        let ids: Vec<(u32, bool)> = snap.ids.iter().map(|r| (r.id, r.ext)).collect();
        assert_eq!(ids, vec![(0x1A0, false), (0x280, false), (0x280, true)]);

        let r1a0 = &snap.ids[0];
        assert_eq!(r1a0.count, 6);
        assert!((r1a0.hz - 50.0).abs() < 1e-9);
        assert_eq!(r1a0.dlc, 2);
        assert_eq!(r1a0.last, "AB05");

        let r280 = &snap.ids[1];
        assert!((r280.hz - 100.0).abs() < 1e-9);
        assert_eq!(r280.last, "0A0A0A0A0A0A0A0A");

        let ext = &snap.ids[2];
        assert_eq!((ext.count, ext.hz, ext.dlc), (1, 0.0, 0));
    }

    #[test]
    fn snapshot_truncates_and_handles_zero_elapsed() {
        let mut s = CaptureStats::new();
        for id in 0..300u32 {
            s.record(&TimedFrame::classic(0, id, false, vec![1]));
        }
        let snap = s.snapshot(0, 256);
        assert_eq!(snap.ids.len(), 256);
        assert_eq!(snap.unique_ids, 300);
        assert_eq!(snap.fps, 0.0);
        assert_eq!(snap.ids[255].id, 255);
    }
}
