//! Passive raw-bus capture ("CAN monitor"): the driver-side half of a full
//! CAN dump.
//!
//! A [`CanMonitor`] opens an adapter **for listening only** and hands back
//! every frame it sees on the bus, timestamped, until stopped. It is separate
//! from the diagnostic path ([`Interface::open`](crate::Interface::open) +
//! ISO-TP): a monitor is started with [`CanMonitor::start`] on a driver that was
//! *not* opened, because opening a diagnostic session may transmit (an ELM327
//! `0100` probe, a UDS session request, …) and a capture must never do that.
//!
//! Drivers expose their monitor through
//! [`Interface::can_monitor`](crate::Interface::can_monitor) (default `None`).
//!
//! ## Listen-only guarantees
//!
//! A monitor never writes a frame to the bus. Whether the *controller* stays
//! silent too (no ACK bits, no error frames on a wrong bitrate) depends on the
//! hardware; [`CanMonitor::listen_only`] reports which level a running monitor
//! achieved — see [`ListenOnly`]. A controller that is *not* silent and opened
//! at the wrong bitrate can disturb a live bus with error frames, so pick the
//! bitrate carefully on such adapters.
use crate::protocol::CanFrame;
use crate::Result;
use std::collections::BTreeMap;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Default CAN bitrate for the high-speed (OBD-II / powertrain) bus.
pub const HS_CAN_BITRATE: u32 = 500_000;
/// Default CAN bitrate for the medium-speed (comfort / body) bus.
pub const MS_CAN_BITRATE: u32 = 125_000;

/// One captured frame with its timestamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimedFrame {
    /// Microseconds since the monitor started (monotonic). Add
    /// [`CanMonitor::wall_base_us`] for wall-clock time.
    pub ts_us: u64,
    /// 29-bit (extended) identifier.
    pub extended: bool,
    /// Remote-transmission-request frame (no data; `frame.data` is empty).
    pub rtr: bool,
    /// The frame itself: id, payload and the CAN-FD / BRS flags.
    pub frame: CanFrame,
}

impl TimedFrame {
    /// A classic data frame.
    pub fn classic(ts_us: u64, id: u32, extended: bool, data: Vec<u8>) -> Self {
        Self {
            ts_us,
            extended,
            rtr: false,
            frame: CanFrame::classic(id, data),
        }
    }

    /// The CAN identifier (flag bits removed).
    pub fn id(&self) -> u32 {
        self.frame.id
    }

    /// The payload.
    pub fn data(&self) -> &[u8] {
        &self.frame.data
    }
}

/// How strongly a running monitor is guaranteed not to disturb the bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenOnly {
    /// The controller is in a silent / listen-only mode: it never transmits,
    /// not even ACK bits or error frames (slcan `L`, an ELM327 with `ATCSM1`
    /// accepted, a SocketCAN interface brought up with `listen-only on`).
    Hardware,
    /// The software never transmits, but the controller is in normal mode and
    /// still ACKs frames and may send error frames (SocketCAN / gs_usb without
    /// the `listen-only` control mode).
    Passive,
    /// No guarantee: the adapter refused silent mode or cannot report it.
    NotGuaranteed,
    /// Synthetic traffic, no bus at all (the mock driver).
    Simulated,
}

impl ListenOnly {
    /// Short stable label (`hardware`, `passive`, `not-guaranteed`, `simulated`).
    pub fn as_str(self) -> &'static str {
        match self {
            ListenOnly::Hardware => "hardware",
            ListenOnly::Passive => "passive",
            ListenOnly::NotGuaranteed => "not-guaranteed",
            ListenOnly::Simulated => "simulated",
        }
    }
}

/// Options for [`CanMonitor::start`].
///
/// Reuses the free-form option keys of
/// [`ConnectConfig::options`](crate::ConnectConfig): `port` (serial device, or
/// the SocketCAN interface name), `iface`, `baud` (serial baud) and `bus`
/// (`hs`/`ms`). Drivers document any extra keys they read.
#[derive(Debug, Clone, Default)]
pub struct MonitorOpts {
    /// CAN bitrate in bit/s. `0` means "derive from `bus`" — see
    /// [`MonitorOpts::resolved_bitrate`].
    pub bitrate: u32,
    /// Driver-specific options (`port`, `iface`, `baud`, `bus`, …).
    pub options: BTreeMap<String, String>,
    /// Only capture these identifiers. Empty captures everything. Drivers may
    /// program a hardware acceptance filter from it; the capture loop applies
    /// it exactly in software as well.
    pub filter_ids: Vec<u32>,
}

impl MonitorOpts {
    /// A non-empty option value.
    pub fn option(&self, key: &str) -> Option<&str> {
        self.options
            .get(key)
            .map(String::as_str)
            .filter(|v| !v.trim().is_empty())
    }

    /// True when the `bus` option selects medium-speed CAN (`ms`/`mscan`/`medium`).
    pub fn is_ms_bus(&self) -> bool {
        matches!(
            self.option("bus")
                .map(|b| b.trim().to_ascii_lowercase())
                .as_deref(),
            Some("ms" | "mscan" | "ms-can" | "medium")
        )
    }

    /// The bitrate to use: an explicit [`bitrate`](MonitorOpts::bitrate), else
    /// 125 kbit/s for `bus=ms`, else 500 kbit/s.
    pub fn resolved_bitrate(&self) -> u32 {
        if self.bitrate != 0 {
            self.bitrate
        } else if self.is_ms_bus() {
            MS_CAN_BITRATE
        } else {
            HS_CAN_BITRATE
        }
    }

    /// True if a frame with identifier `id` passes [`filter_ids`](MonitorOpts::filter_ids).
    pub fn accepts(&self, id: u32) -> bool {
        self.filter_ids.is_empty() || self.filter_ids.contains(&id)
    }
}

/// Passive raw-bus capture. See the [module docs](self).
///
/// Lifecycle: [`start`](CanMonitor::start) → repeated
/// [`next_frame`](CanMonitor::next_frame) → [`stop`](CanMonitor::stop). A monitor
/// may be started again after `stop`.
pub trait CanMonitor {
    /// Open the adapter in listen-only mode (where the hardware allows) and
    /// begin capturing. Never transmits a CAN frame.
    fn start(&mut self, opts: &MonitorOpts) -> Result<()>;

    /// The next frame, waiting at most `timeout_ms`. `Ok(None)` is a quiet
    /// interval (no frame within the timeout), not an error.
    fn next_frame(&mut self, timeout_ms: u64) -> Result<Option<TimedFrame>>;

    /// Stop capturing and release the adapter.
    fn stop(&mut self) -> Result<()>;

    /// Unix time in microseconds at which `ts_us == 0`.
    fn wall_base_us(&self) -> u64;

    /// Frames (or overflow events, for adapters that cannot count frames) lost
    /// by the adapter or the host since `start` — e.g. ELM327 `BUFFER FULL`.
    fn dropped(&self) -> u64 {
        0
    }

    /// The listen-only level achieved by the running monitor.
    fn listen_only(&self) -> ListenOnly {
        ListenOnly::NotGuaranteed
    }

    /// Channel name written into logs (candump's interface column).
    fn channel(&self) -> String {
        "can0".into()
    }

    /// Human description of the adapter (firmware banner etc.), for the UI.
    fn describe(&self) -> String {
        String::new()
    }
}

/// Monotonic capture clock: microseconds since construction, plus the Unix
/// time at construction. Shared helper for monitor implementations.
#[derive(Debug, Clone, Copy)]
pub struct MonitorClock {
    start: Instant,
    wall_base_us: u64,
}

impl MonitorClock {
    /// Start the clock now.
    pub fn start() -> Self {
        Self {
            start: Instant::now(),
            wall_base_us: unix_us(SystemTime::now()),
        }
    }

    /// Microseconds since [`start`](MonitorClock::start).
    pub fn now_us(&self) -> u64 {
        self.start.elapsed().as_micros() as u64
    }

    /// Unix time in microseconds at `now_us() == 0`.
    pub fn wall_base_us(&self) -> u64 {
        self.wall_base_us
    }

    /// Convert an absolute (e.g. kernel) timestamp into capture time,
    /// clamped at 0.
    pub fn since_start(&self, t: SystemTime) -> u64 {
        unix_us(t).saturating_sub(self.wall_base_us)
    }
}

impl Default for MonitorClock {
    fn default() -> Self {
        Self::start()
    }
}

/// Microseconds since the Unix epoch (0 for times before it).
pub fn unix_us(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitrate_resolution() {
        let mut o = MonitorOpts::default();
        assert_eq!(o.resolved_bitrate(), 500_000);
        o.options.insert("bus".into(), "MS".into());
        assert!(o.is_ms_bus());
        assert_eq!(o.resolved_bitrate(), 125_000);
        o.bitrate = 250_000;
        assert_eq!(o.resolved_bitrate(), 250_000);
    }

    #[test]
    fn filter_accepts() {
        let mut o = MonitorOpts::default();
        assert!(o.accepts(0x7E8));
        o.filter_ids = vec![0x7E8, 0x7E0];
        assert!(o.accepts(0x7E0));
        assert!(!o.accepts(0x280));
    }

    #[test]
    fn empty_option_is_none() {
        let mut o = MonitorOpts::default();
        o.options.insert("port".into(), "  ".into());
        assert_eq!(o.option("port"), None);
    }

    #[test]
    fn clock_maps_absolute_time() {
        let c = MonitorClock::start();
        let later = UNIX_EPOCH + std::time::Duration::from_micros(c.wall_base_us() + 1500);
        assert_eq!(c.since_start(later), 1500);
        assert_eq!(c.since_start(UNIX_EPOCH), 0);
    }
}
