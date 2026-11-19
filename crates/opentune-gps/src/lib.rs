//! # opentune-gps
//!
//! An optional GPS/GNSS source for OpenTune's datalogger and dashboard.
//!
//! A u-blox style receiver enumerates as a plain USB CDC-ACM serial port
//! (`/dev/ttyACM0` on Linux, `COMx` on Windows) and streams NMEA 0183. This
//! crate reads it directly — no gpsd:
//!
//! * [`nmea`] — a dependency-light parser for `$--RMC`, `$--GGA` and `$--VTG`
//!   from any talker (`GP`, `GN`, `GL`, `GA`, `GB`/`BD`, …), checksum
//!   verified, never panicking.
//! * [`FixTracker`] — merges sentences into the latest [`GpsFix`] and counts
//!   every rejected line in [`NmeaStats`] (bad checksum, partial, malformed,
//!   noise, overlong) instead of failing.
//! * [`GpsReader`] — a background thread over a serial port (or any
//!   `Read`) with latest-fix state and staleness.
//! * [`ReplayFeed`] — deterministic file replay, one NMEA epoch per call, for
//!   tests and offline log analysis.
//!
//! Both sources implement [`GpsFeed`], which is all the logger needs.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod fix;
pub mod nmea;
mod reader;
mod reopen;
mod replay;

pub use fix::{
    FixTracker, GpsFix, GpsSnapshot, LineAssembler, LineItem, NmeaStats, DEFAULT_STALE_AFTER_MS,
    MAX_LINE,
};
pub use nmea::{parse_sentence, NmeaError, Sentence};
pub use reader::GpsReader;
pub use reopen::{OpenFn, ReopenFeed};
pub use replay::ReplayFeed;

/// A source of GPS snapshots for the logger: call [`snapshot`](Self::snapshot)
/// once per logged sample. A live receiver returns its current state; a
/// replay advances one epoch per call.
pub trait GpsFeed: Send {
    /// The state to log with the current sample.
    fn snapshot(&mut self) -> GpsSnapshot;
    /// A short human-readable description of the source.
    fn describe(&self) -> String;
    /// Wait (up to `timeout`) until a live source has heard its first
    /// sentence, so a log does not start with "no NMEA received yet" rows.
    /// A replay is always ready.
    fn wait_ready(&mut self, timeout: std::time::Duration) {
        let _ = timeout;
    }
}

/// Default baud rate for UART receivers (USB CDC-ACM ignores it).
pub const DEFAULT_BAUD: u32 = 9600;

/// Open `spec` as a GPS source: an existing regular file is replayed (one
/// epoch per snapshot), anything else is opened as a serial port.
#[cfg(feature = "serial")]
pub fn open_source(spec: &str, baud: u32) -> Result<Box<dyn GpsFeed>, String> {
    let path = std::path::Path::new(spec);
    if path.is_file() {
        let r = ReplayFeed::open(path).map_err(|e| format!("read NMEA file '{spec}': {e}"))?;
        return Ok(Box::new(r));
    }
    Ok(Box::new(GpsReader::open_serial(spec, baud)?))
}

/// [`open_source`], but a **serial** source re-opens itself with backoff
/// (`initial_ms` doubling up to `max_ms`) after it ends, e.g. when the USB
/// receiver is unplugged and plugged back in ([`ReopenFeed`]). A recorded
/// file is replayed once and never re-opened.
#[cfg(feature = "serial")]
pub fn open_source_reopening(
    spec: &str,
    baud: u32,
    initial_ms: u64,
    max_ms: u64,
) -> Result<Box<dyn GpsFeed>, String> {
    if std::path::Path::new(spec).is_file() {
        return open_source(spec, baud);
    }
    let port = spec.to_string();
    let open: OpenFn = Box::new(move || {
        GpsReader::open_serial(&port, baud).map(|r| Box::new(r) as Box<dyn GpsFeed>)
    });
    Ok(Box::new(ReopenFeed::new(
        format!("serial {spec} (auto-reopen)"),
        open,
        initial_ms,
        max_ms,
    )?))
}

/// One serial port that might be a GNSS receiver.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PortInfo {
    /// Port name to open (`/dev/ttyACM0`, `COM5`).
    pub name: String,
    /// USB vendor id, when the port is a USB device.
    pub vid: Option<u16>,
    /// USB product id, when the port is a USB device.
    pub pid: Option<u16>,
    /// USB product string, when reported.
    pub product: Option<String>,
    /// Looks like a GNSS receiver (u-blox vendor id, or GPS/GNSS in the
    /// product string).
    pub likely_gnss: bool,
}

/// u-blox AG's USB vendor id.
pub const UBLOX_VID: u16 = 0x1546;

/// Serial ports on this machine, likely GNSS receivers first.
#[cfg(feature = "serial")]
pub fn list_ports() -> Vec<PortInfo> {
    let mut out: Vec<PortInfo> = serialport::available_ports()
        .unwrap_or_default()
        .into_iter()
        .map(|p| {
            let (vid, pid, product) = match p.port_type {
                serialport::SerialPortType::UsbPort(u) => (Some(u.vid), Some(u.pid), u.product),
                _ => (None, None, None),
            };
            let likely_gnss = vid == Some(UBLOX_VID)
                || product.as_deref().is_some_and(|s| {
                    let s = s.to_ascii_uppercase();
                    s.contains("GNSS") || s.contains("GPS")
                });
            PortInfo {
                name: p.port_name,
                vid,
                pid,
                product,
                likely_gnss,
            }
        })
        .collect();
    out.sort_by(|a, b| b.likely_gnss.cmp(&a.likely_gnss).then(a.name.cmp(&b.name)));
    out
}
