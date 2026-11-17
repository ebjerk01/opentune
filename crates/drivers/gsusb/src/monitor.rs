//! Passive full-bus capture for gs_usb / candleLight adapters ([`CanMonitor`]).
//!
//! On Linux the kernel `gs_usb` driver exposes the adapter as a SocketCAN
//! interface, so capture is the SocketCAN monitor
//! ([`opentune_driver_socketcan::monitor`]) labelled `gs_usb`. Listen-only is
//! the interface's control mode: bring it up with
//! `ip link set can0 type can bitrate 500000 listen-only on` and the kernel
//! starts the device with [`GS_CAN_MODE_LISTEN_ONLY`]; the monitor then reports
//! [`ListenOnly::Hardware`](opentune_core::ListenOnly::Hardware), otherwise
//! `Passive` (never transmits, but the controller ACKs).
use opentune_core::{CanMonitor, ListenOnly, MonitorOpts, Result, TimedFrame};
use opentune_driver_socketcan::monitor::SocketCanMonitor;

/// gs_usb `struct gs_device_mode` flag selecting listen-only (silent) mode.
/// Provided for a future raw-libusb backend; the kernel path sets it itself.
pub const GS_CAN_MODE_LISTEN_ONLY: u32 = 1 << 0;

/// gs_usb bus monitor (SocketCAN path).
pub struct GsUsbMonitor(SocketCanMonitor);

impl Default for GsUsbMonitor {
    fn default() -> Self {
        Self(SocketCanMonitor::new("gs_usb"))
    }
}

impl CanMonitor for GsUsbMonitor {
    fn start(&mut self, opts: &MonitorOpts) -> Result<()> {
        self.0.start(opts)
    }
    fn next_frame(&mut self, timeout_ms: u64) -> Result<Option<TimedFrame>> {
        self.0.next_frame(timeout_ms)
    }
    fn stop(&mut self) -> Result<()> {
        self.0.stop()
    }
    fn wall_base_us(&self) -> u64 {
        self.0.wall_base_us()
    }
    fn dropped(&self) -> u64 {
        self.0.dropped()
    }
    fn listen_only(&self) -> ListenOnly {
        self.0.listen_only()
    }
    fn channel(&self) -> String {
        self.0.channel()
    }
    fn describe(&self) -> String {
        self.0.describe()
    }
}
