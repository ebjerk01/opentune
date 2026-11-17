//! Synthetic CAN traffic for the mock driver's [`CanMonitor`], so a full CAN
//! dump (CLI `candump`, the desktop panel) can be exercised offline.
//!
//! Three periodic frames at typical powertrain-bus rates:
//!
//! | id | rate | content (illustrative, not an OEM layout) |
//! |---|---|---|
//! | `0x280` | 100 Hz | engine: torque, rpm × 4 (LE), pedal, counter |
//! | `0x1A0` | 50 Hz | brake: brake-switch flag, speed × 100 (LE), counter |
//! | `0x5A0` | 20 Hz | speed: km/h × 128 (LE), distance counter |
//!
//! Options read from [`MonitorOpts::options`]:
//! - `mock_realtime`: `0`/`false` runs on a virtual clock (no sleeping; frames
//!   come as fast as they are read, timestamps still follow the schedule) — for
//!   tests. Default: real time.
//! - `mock_frames`: stop producing after this many frames.
use opentune_core::monitor::MonitorClock;
use opentune_core::{CanMonitor, Error, ListenOnly, MonitorOpts, Result, TimedFrame};
use std::time::Duration;

struct Schedule {
    id: u32,
    period_us: u64,
    next_us: u64,
    counter: u8,
}

/// Synthetic CAN bus monitor. Never touches hardware.
pub struct MockMonitor {
    running: bool,
    clock: MonitorClock,
    realtime: bool,
    virtual_us: u64,
    limit: Option<u64>,
    emitted: u64,
    schedules: Vec<Schedule>,
    distance: u32,
}

impl Default for MockMonitor {
    fn default() -> Self {
        Self {
            running: false,
            clock: MonitorClock::start(),
            realtime: true,
            virtual_us: 0,
            limit: None,
            emitted: 0,
            schedules: Vec::new(),
            distance: 0,
        }
    }
}

/// A slow 0..1..0 triangle wave with the given period in seconds.
fn tri(t_s: f64, period_s: f64) -> f64 {
    let p = (t_s / period_s).fract();
    if p < 0.5 {
        p * 2.0
    } else {
        2.0 - p * 2.0
    }
}

impl MockMonitor {
    fn payload(&mut self, idx: usize, ts_us: u64) -> Vec<u8> {
        let t = ts_us as f64 / 1e6;
        let rpm = 800.0 + 3200.0 * tri(t, 12.0);
        let kmh = 120.0 * tri(t, 30.0);
        let s = &mut self.schedules[idx];
        s.counter = s.counter.wrapping_add(1);
        let c = s.counter;
        match s.id {
            0x280 => {
                let raw = (rpm * 4.0) as u16;
                let torque = (20.0 + 60.0 * tri(t, 12.0)) as u8;
                let pedal = (100.0 * tri(t, 12.0)) as u8;
                let [lo, hi] = raw.to_le_bytes();
                vec![0x01, torque, lo, hi, pedal, 0x00, 0x00, c]
            }
            0x1A0 => {
                let braking = tri(t, 30.0) > 0.9 && (t / 30.0).fract() > 0.5;
                let [lo, hi] = ((kmh * 100.0) as u16).to_le_bytes();
                vec![
                    0x00,
                    u8::from(braking) << 3,
                    lo,
                    hi,
                    0xFE,
                    0xFE,
                    0x00,
                    c & 0x0F,
                ]
            }
            _ => {
                self.distance = self.distance.wrapping_add((kmh / 3.6 * 0.05) as u32);
                let [lo, hi] = ((kmh * 128.0) as u16).to_le_bytes();
                let d = self.distance.to_le_bytes();
                vec![0xFF, lo, hi, d[0], d[1], d[2], 0x00, c]
            }
        }
    }
}

impl CanMonitor for MockMonitor {
    fn start(&mut self, opts: &MonitorOpts) -> Result<()> {
        self.realtime = !matches!(
            opts.option("mock_realtime").map(str::trim),
            Some("0" | "false" | "no" | "off")
        );
        self.limit = match opts.option("mock_frames") {
            Some(v) => Some(
                v.trim()
                    .parse()
                    .map_err(|_| Error::Config(format!("mock_frames: bad number '{v}'")))?,
            ),
            None => None,
        };
        self.schedules = [(0x280u32, 10_000u64), (0x1A0, 20_000), (0x5A0, 50_000)]
            .into_iter()
            .filter(|(id, _)| opts.accepts(*id))
            .enumerate()
            // Stagger the first frames a little, like independent ECUs.
            .map(|(i, (id, period_us))| Schedule {
                id,
                period_us,
                next_us: 1_000 + 3_000 * i as u64,
                counter: 0,
            })
            .collect();
        self.clock = MonitorClock::start();
        self.virtual_us = 0;
        self.emitted = 0;
        self.distance = 0;
        self.running = true;
        Ok(())
    }

    fn next_frame(&mut self, timeout_ms: u64) -> Result<Option<TimedFrame>> {
        if !self.running {
            return Err(Error::NotConnected);
        }
        let exhausted = self.limit.is_some_and(|l| self.emitted >= l);
        let next = self
            .schedules
            .iter()
            .enumerate()
            .min_by_key(|(_, s)| s.next_us)
            .map(|(i, s)| (i, s.next_us));
        let (idx, due) = match next {
            Some(n) if !exhausted => n,
            _ => {
                // Nothing (more) to send: behave like a quiet bus.
                let ms = if self.realtime { timeout_ms } else { 1 };
                std::thread::sleep(Duration::from_millis(ms));
                return Ok(None);
            }
        };
        if self.realtime {
            let now = self.clock.now_us();
            if due > now {
                let wait = due - now;
                if wait > timeout_ms * 1000 {
                    std::thread::sleep(Duration::from_millis(timeout_ms));
                    return Ok(None);
                }
                std::thread::sleep(Duration::from_micros(wait));
            }
        } else {
            self.virtual_us = due;
        }
        let data = self.payload(idx, due);
        let s = &mut self.schedules[idx];
        s.next_us += s.period_us;
        self.emitted += 1;
        Ok(Some(TimedFrame::classic(due, s.id, false, data)))
    }

    fn stop(&mut self) -> Result<()> {
        self.running = false;
        Ok(())
    }

    fn wall_base_us(&self) -> u64 {
        self.clock.wall_base_us()
    }

    fn listen_only(&self) -> ListenOnly {
        ListenOnly::Simulated
    }

    fn channel(&self) -> String {
        "vcan0".into()
    }

    fn describe(&self) -> String {
        "Mock driver (synthetic traffic: 0x280 @100 Hz, 0x1A0 @50 Hz, 0x5A0 @20 Hz)".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn virtual_opts() -> MonitorOpts {
        let mut o = MonitorOpts::default();
        o.options.insert("mock_realtime".into(), "0".into());
        o
    }

    #[test]
    fn produces_periodic_frames_in_order() {
        let mut m = MockMonitor::default();
        assert!(m.next_frame(1).is_err(), "not started");
        m.start(&virtual_opts()).unwrap();
        let frames: Vec<TimedFrame> = (0..200)
            .map(|_| m.next_frame(10).unwrap().unwrap())
            .collect();
        assert!(frames.windows(2).all(|w| w[0].ts_us <= w[1].ts_us));
        let count = |id| frames.iter().filter(|f| f.id() == id).count();
        // 100 : 50 : 20 Hz
        let (a, b, c) = (count(0x280), count(0x1A0), count(0x5A0));
        assert_eq!(a + b + c, 200);
        assert!(a > b && b > c, "{a} {b} {c}");
        assert!(frames.iter().all(|f| f.data().len() == 8 && !f.extended));
        m.stop().unwrap();
        assert!(m.next_frame(1).is_err());
    }

    #[test]
    fn honours_filter_and_limit() {
        let mut o = virtual_opts();
        o.filter_ids = vec![0x1A0];
        o.options.insert("mock_frames".into(), "5".into());
        let mut m = MockMonitor::default();
        m.start(&o).unwrap();
        for _ in 0..5 {
            assert_eq!(m.next_frame(10).unwrap().unwrap().id(), 0x1A0);
        }
        assert_eq!(m.next_frame(10).unwrap(), None);
    }

    #[test]
    fn realtime_respects_timeout() {
        // 0x5A0 alone: first frame due 1 ms after start.
        let o = MonitorOpts {
            filter_ids: vec![0x5A0],
            ..Default::default()
        };
        let mut m = MockMonitor::default();
        m.start(&o).unwrap();
        assert!(m.next_frame(200).unwrap().is_some());
        // The next 0x5A0 is 50 ms away: a 1 ms wait returns quiet.
        assert_eq!(m.next_frame(1).unwrap(), None);
    }
}
