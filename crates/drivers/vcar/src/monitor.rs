//! Synthetic bus traffic for the virtual car's [`CanMonitor`], derived from the
//! same engine model that feeds its OBD live data, so a CAN capture and the
//! dashboard agree.
//!
//! | id | rate | content (the virtual car's own illustrative layout) |
//! |---|---|---|
//! | `0x0C9` | 100 Hz | engine: rpm × 4 (BE u16), throttle %, load %, coolant + 40, running flag, –, counter |
//! | `0x1F5` | 50 Hz | speed: km/h × 100 (BE u16), brake flag, –, –, –, –, counter (low nibble) |
//! | `0x3D1` | 10 Hz | cluster: fuel %, outside temp + 40, odometer km (BE u24), –, –, counter |
//! | `0x4A0` | 5 Hz | body: ignition/running bits, doors, locks, battery V × 10, –, –, –, counter |
//!
//! Percentages are scaled 0..255. None of these ids or layouts describe a real
//! vehicle.
//!
//! Options read from [`MonitorOpts::options`]:
//! - `port` / `profile`: which virtual car to listen to (as for `open`).
//! - `vcar_realtime`: `0`/`false` runs on a virtual clock (no sleeping; frames
//!   come as fast as they are read, timestamps still follow the schedule) — for
//!   tests. Default: real time.
//! - `vcar_frames`: stop producing after this many frames.
use crate::vehicle::{engine_state, EngineState};
use crate::SharedVehicle;
use opentune_core::monitor::MonitorClock;
use opentune_core::{CanMonitor, Error, ListenOnly, MonitorOpts, Result, TimedFrame};
use std::time::Duration;

/// Engine frame id.
pub const ID_ENGINE: u32 = 0x0C9;
/// Vehicle-speed frame id.
pub const ID_SPEED: u32 = 0x1F5;
/// Cluster frame id.
pub const ID_CLUSTER: u32 = 0x3D1;
/// Body frame id.
pub const ID_BODY: u32 = 0x4A0;

const SCHEDULE: [(u32, u64); 4] = [
    (ID_ENGINE, 10_000),
    (ID_SPEED, 20_000),
    (ID_CLUSTER, 100_000),
    (ID_BODY, 200_000),
];

struct Schedule {
    id: u32,
    period_us: u64,
    next_us: u64,
    counter: u8,
}

/// Synthetic CAN bus monitor for the virtual car. Never touches hardware.
#[derive(Default)]
pub struct VcarMonitor {
    attached: Option<SharedVehicle>,
    vehicle: Option<SharedVehicle>,
    running: bool,
    clock: MonitorClock,
    realtime: bool,
    /// Seconds between the vehicle's epoch and this capture's `ts_us == 0`.
    offset_s: f64,
    limit: Option<u64>,
    emitted: u64,
    schedules: Vec<Schedule>,
    odometer_m: f64,
    last_ts_us: u64,
}

impl VcarMonitor {
    /// A monitor that always listens to `vehicle` (instead of resolving one
    /// from the start options).
    pub fn attached(vehicle: SharedVehicle) -> Self {
        Self {
            attached: Some(vehicle),
            ..Self::default()
        }
    }

    fn engine_at(&self, ts_us: u64) -> Result<EngineState> {
        let running = match &self.vehicle {
            Some(v) => v
                .lock()
                .map_err(|_| Error::Protocol("virtual car state poisoned".into()))?
                .engine_running(),
            None => false,
        };
        Ok(engine_state(running, self.offset_s + ts_us as f64 / 1e6))
    }

    fn payload(&mut self, idx: usize, ts_us: u64) -> Result<Vec<u8>> {
        let st = self.engine_at(ts_us)?;
        let dt_s = ts_us.saturating_sub(self.last_ts_us) as f64 / 1e6;
        self.last_ts_us = ts_us;
        self.odometer_m += st.speed_kmh / 3.6 * dt_s;
        let pct = |v: f64| ((v * 255.0 / 100.0).round().clamp(0.0, 255.0)) as u8;
        let s = &mut self.schedules[idx];
        s.counter = s.counter.wrapping_add(1);
        let c = s.counter;
        Ok(match s.id {
            ID_ENGINE => {
                let [hi, lo] = ((st.rpm * 4.0).round() as u16).to_be_bytes();
                vec![
                    hi,
                    lo,
                    pct(st.throttle_pct),
                    pct(st.load_pct),
                    (st.coolant_c + 40.0).round() as u8,
                    u8::from(st.running),
                    0x00,
                    c,
                ]
            }
            ID_SPEED => {
                let [hi, lo] = ((st.speed_kmh * 100.0).round() as u16).to_be_bytes();
                // Braking while the drive cycle slows from speed.
                let braking = st.running && st.speed_kmh > 5.0 && st.throttle_pct < 12.0;
                vec![hi, lo, u8::from(braking), 0, 0, 0, 0, c & 0x0F]
            }
            ID_CLUSTER => {
                let odo = 48_210u32 + (self.odometer_m / 1000.0) as u32;
                let [_, o2, o1, o0] = odo.to_be_bytes();
                vec![pct(62.0), 12 + 40, o2, o1, o0, 0, 0, c]
            }
            _ => {
                let ign = if st.running { 0x03 } else { 0x01 };
                let volts = (st.battery_mv / 100).min(255) as u8;
                vec![ign, 0x00, 0x01, volts, 0, 0, 0, c]
            }
        })
    }
}

impl CanMonitor for VcarMonitor {
    fn start(&mut self, opts: &MonitorOpts) -> Result<()> {
        let vehicle = match &self.attached {
            Some(v) => v.clone(),
            None => crate::shared_vehicle(crate::spec_from(|k| opts.option(k)))?,
        };
        let epoch = vehicle
            .lock()
            .map_err(|_| Error::Protocol("virtual car state poisoned".into()))?
            .epoch();
        self.realtime = !matches!(
            opts.option("vcar_realtime").map(str::trim),
            Some("0" | "false" | "no" | "off")
        );
        self.limit = match opts.option("vcar_frames") {
            Some(v) => Some(
                v.trim()
                    .parse()
                    .map_err(|_| Error::Config(format!("vcar_frames: bad number '{v}'")))?,
            ),
            None => None,
        };
        self.schedules = SCHEDULE
            .into_iter()
            .filter(|(id, _)| opts.accepts(*id))
            .enumerate()
            // Stagger the first frames a little, like independent ECUs.
            .map(|(i, (id, period_us))| Schedule {
                id,
                period_us,
                next_us: 1_000 + 2_500 * i as u64,
                counter: 0,
            })
            .collect();
        self.clock = MonitorClock::start();
        self.offset_s = epoch.elapsed().as_secs_f64();
        self.vehicle = Some(vehicle);
        self.emitted = 0;
        self.odometer_m = 0.0;
        self.last_ts_us = 0;
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
        }
        let data = self.payload(idx, due)?;
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
        "vcar0".into()
    }

    fn describe(&self) -> String {
        "Virtual car (synthetic traffic: 0x0C9 @100 Hz, 0x1F5 @50 Hz, 0x3D1 @10 Hz, 0x4A0 @5 Hz)"
            .into()
    }
}
