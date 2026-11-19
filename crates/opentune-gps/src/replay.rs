//! Deterministic file replay: one NMEA *epoch* (all sentences sharing one UTC
//! time) per [`ReplayFeed::next_epoch`] call. Used by the tests, by
//! `opentune log --gps <file>` (one epoch per logged sample) and by
//! `opentune gps <file>` for offline analysis. The replay clock follows the
//! NMEA timestamps, so ages and staleness are reproducible.

use crate::fix::{FixTracker, GpsSnapshot, LineAssembler, LineItem, DEFAULT_STALE_AFTER_MS};
use crate::nmea::parse_sentence;
use crate::GpsFeed;

const DAY_MS: u64 = 86_400_000;

/// How far the replay clock moves from NMEA time of day `cur` to `t` (ms).
/// Going backwards is the midnight rollover; a day that ended in a leap
/// second (`23:59:60.x`, at or past [`DAY_MS`]) is one second longer.
fn advance_ms(cur: u64, t: u64) -> u64 {
    if t >= cur {
        return t - cur;
    }
    let day = if cur >= DAY_MS { DAY_MS + 1000 } else { DAY_MS };
    (t + day).saturating_sub(cur)
}

/// Replays recorded NMEA, one epoch at a time.
#[derive(Debug, Clone)]
pub struct ReplayFeed {
    items: Vec<LineItem>,
    pos: usize,
    tracker: FixTracker,
    clock_ms: u64,
    epoch_time: Option<u64>,
    epochs: u64,
    looping: bool,
    label: String,
}

impl ReplayFeed {
    /// Replay `data` (a whole NMEA log; partial and garbage lines allowed).
    pub fn from_bytes(label: impl Into<String>, data: &[u8]) -> Self {
        let mut a = LineAssembler::new();
        let mut items = a.push(data);
        items.extend(a.finish());
        Self {
            items,
            pos: 0,
            tracker: FixTracker::new(DEFAULT_STALE_AFTER_MS),
            clock_ms: 0,
            epoch_time: None,
            epochs: 0,
            looping: false,
            label: label.into(),
        }
    }

    /// Replay a file.
    pub fn open(path: &std::path::Path) -> std::io::Result<Self> {
        let data = std::fs::read(path)?;
        Ok(Self::from_bytes(path.display().to_string(), &data))
    }

    /// Start over at the end of the data instead of ending (demo sources).
    pub fn looping(mut self, on: bool) -> Self {
        self.looping = on;
        self
    }

    /// Staleness limit in milliseconds of NMEA time.
    pub fn with_stale_after(mut self, ms: u64) -> Self {
        self.tracker.stale_after_ms = ms;
        self
    }

    /// Epochs replayed so far.
    pub fn epochs(&self) -> u64 {
        self.epochs
    }

    /// All data consumed (never true while looping over usable data).
    pub fn finished(&self) -> bool {
        self.pos >= self.items.len() && !(self.looping && self.epochs > 0)
    }

    /// Apply the next epoch and return the snapshot after it, or `None` once
    /// the data is exhausted.
    pub fn next_epoch(&mut self) -> Option<GpsSnapshot> {
        if self.pos >= self.items.len() {
            if !(self.looping && self.epochs > 0) {
                return None;
            }
            // Start over; keep the clock running so ages stay monotonic.
            self.pos = 0;
            self.epoch_time = None;
            self.clock_ms += 1000;
        }
        let mut consumed = false;
        while let Some(item) = self.items.get(self.pos) {
            let line = match item {
                LineItem::Overlong => {
                    self.tracker.count_overlong();
                    self.pos += 1;
                    continue;
                }
                LineItem::Line(l) => l,
            };
            if let Ok(s) = parse_sentence(line) {
                if let Some(t) = s.time().map(|t| t.ms_of_day()) {
                    match self.epoch_time {
                        Some(cur) if cur == t => {}
                        Some(_) if consumed => break, // next epoch starts here
                        Some(cur) => {
                            self.clock_ms += advance_ms(cur, t);
                            self.epoch_time = Some(t);
                        }
                        None => self.epoch_time = Some(t),
                    }
                }
                consumed = true;
            }
            self.tracker.ingest_line(line, self.clock_ms);
            self.pos += 1;
        }
        self.epochs += 1;
        Some(self.tracker.snapshot(self.clock_ms))
    }

    /// The snapshot once the data is exhausted: the last values, stale.
    fn ended_snapshot(&self) -> GpsSnapshot {
        let mut s = self.tracker.snapshot(self.clock_ms);
        s.good = false;
        s.stale = true;
        s.note = format!("replay ended after {} epoch(s)", self.epochs);
        s.ended = Some("end of replay data".into());
        s
    }
}

impl GpsFeed for ReplayFeed {
    fn snapshot(&mut self) -> GpsSnapshot {
        match self.next_epoch() {
            Some(s) => s,
            None => self.ended_snapshot(),
        }
    }

    fn describe(&self) -> String {
        format!("NMEA replay {}", self.label)
    }
}

impl Iterator for ReplayFeed {
    type Item = GpsSnapshot;
    /// One snapshot per epoch (never ends while looping).
    fn next(&mut self) -> Option<GpsSnapshot> {
        self.next_epoch()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nmea::checksum;

    fn gga(time: &str, quality: u8) -> String {
        let body = format!("GPGGA,{time},4807.038,N,01131.000,E,{quality},08,0.9,545.4,M,46.9,M,,");
        format!("${body}*{:02X}\r\n", checksum(&body))
    }

    fn ages(lines: &[String]) -> Vec<Option<u64>> {
        ReplayFeed::from_bytes("t", lines.concat().as_bytes())
            .with_stale_after(u64::MAX)
            .map(|s| s.age_ms)
            .collect()
    }

    #[test]
    fn midnight_rollover_after_a_fractional_leap_second() {
        // Regression: 23:59:60.500 is 86_400_500 ms of day, so the rollover
        // `t + DAY_MS - cur` underflowed (a panic in debug builds, a clock
        // jump of ~2^64 ms in release).
        let lines = [gga("235959.500", 1), gga("235960.500", 0), gga("000000.000", 0)];
        assert_eq!(ages(&lines), vec![Some(0), Some(1000), Some(1500)]);
        // An ordinary midnight.
        let lines = [gga("235959.000", 1), gga("000001.000", 0)];
        assert_eq!(ages(&lines), vec![Some(0), Some(2000)]);
    }
}
