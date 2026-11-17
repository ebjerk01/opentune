//! Passive full-bus capture over SocketCAN ([`CanMonitor`]).
//!
//! Opens a raw CAN-FD-capable socket on the interface (`iface`, else `port`,
//! default `can0`) and only ever reads from it, with kernel receive timestamps
//! (`SO_TIMESTAMP`) when available. Classic, remote and CAN-FD frames are
//! captured; error frames are ignored.
//!
//! ## Listen-only
//! The socket never transmits, but the CAN controller follows the interface's
//! control mode, which only `ip link` (CAP_NET_ADMIN) can change. The monitor
//! reads it over netlink: an interface brought up with
//! `ip link set can0 type can bitrate 500000 listen-only on` reports
//! [`ListenOnly::Hardware`]; otherwise the controller still ACKs frames (and
//! sends error frames at a wrong bitrate) and the monitor reports
//! [`ListenOnly::Passive`]. The bitrate is likewise the interface's own;
//! [`MonitorOpts::bitrate`] is not applied.
//!
//! The same monitor serves the gs_usb/candleLight driver, whose kernel driver
//! exposes the adapter as a SocketCAN interface (and maps `listen-only on` to
//! the gs_usb `GS_CAN_MODE_LISTEN_ONLY` flag).
use opentune_core::{CanMonitor, ListenOnly, MonitorOpts, Result, TimedFrame};

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use opentune_core::monitor::MonitorClock;
    use opentune_core::protocol::CanFrame;
    use opentune_core::Error;
    use socketcan::{
        CanAnyFrame, CanCtrlMode, CanFdSocket, CanInterface, EmbeddedFrame, Socket, SocketOptions,
    };
    use std::time::{Duration, Instant, SystemTime};

    /// SocketCAN bus monitor (also used by the gs_usb driver).
    pub struct SocketCanMonitor {
        label: &'static str,
        sock: Option<CanFdSocket>,
        clock: Option<MonitorClock>,
        iface: String,
        kernel_ts: bool,
        listen: ListenOnly,
        describe: String,
    }

    impl Default for SocketCanMonitor {
        fn default() -> Self {
            Self::new("SocketCAN")
        }
    }

    /// Listen-only level and bitrate of `iface`, read over netlink.
    fn iface_state(iface: &str) -> (ListenOnly, Option<u32>) {
        let details = CanInterface::open(iface)
            .ok()
            .and_then(|i| i.details().ok());
        let Some(d) = details else {
            return (ListenOnly::Passive, None);
        };
        let listen = match d.can.ctrl_mode {
            Some(m) if m.has_mode(CanCtrlMode::ListenOnly) => ListenOnly::Hardware,
            _ => ListenOnly::Passive,
        };
        (listen, d.can.bit_timing.map(|t| t.bitrate))
    }

    fn convert(frame: CanAnyFrame) -> Option<(bool, bool, CanFrame)> {
        match frame {
            CanAnyFrame::Normal(f) => Some((
                f.is_extended(),
                false,
                CanFrame::classic(socketcan::Frame::raw_id(&f), f.data().to_vec()),
            )),
            CanAnyFrame::Remote(f) => Some((
                f.is_extended(),
                true,
                CanFrame::classic(socketcan::Frame::raw_id(&f), Vec::new()),
            )),
            CanAnyFrame::Fd(f) => Some((
                f.is_extended(),
                false,
                CanFrame::fd(socketcan::Frame::raw_id(&f), f.data().to_vec(), f.is_brs()),
            )),
            CanAnyFrame::Error(_) => None,
        }
    }

    impl SocketCanMonitor {
        /// A monitor whose description starts with `label` (e.g. `gs_usb`).
        pub fn new(label: &'static str) -> Self {
            Self {
                label,
                sock: None,
                clock: None,
                iface: "can0".into(),
                kernel_ts: false,
                listen: ListenOnly::Passive,
                describe: String::new(),
            }
        }
    }

    impl CanMonitor for SocketCanMonitor {
        fn start(&mut self, opts: &MonitorOpts) -> Result<()> {
            let iface = opts
                .option("iface")
                .or_else(|| opts.option("port"))
                .unwrap_or("can0")
                .trim()
                .to_string();
            let sock = CanFdSocket::open(&iface)
                .map_err(|e| Error::Config(format!("cannot open CAN interface {iface}: {e}")))?;
            self.kernel_ts = sock.set_recv_timestamp(true).is_ok();
            let (listen, bitrate) = iface_state(&iface);
            self.listen = listen;
            self.describe = format!(
                "{} {iface}{}, listen-only: {}{}",
                self.label,
                bitrate
                    .map(|b| format!(" @ {} kbit/s", b / 1000))
                    .unwrap_or_default(),
                listen.as_str(),
                if listen == ListenOnly::Passive {
                    " (bring the interface up with `listen-only on` for a silent controller)"
                } else {
                    ""
                }
            );
            self.iface = iface;
            self.sock = Some(sock);
            self.clock = Some(MonitorClock::start());
            Ok(())
        }

        fn next_frame(&mut self, timeout_ms: u64) -> Result<Option<TimedFrame>> {
            let clock = self.clock.ok_or(Error::NotConnected)?;
            let sock = self.sock.as_ref().ok_or(Error::NotConnected)?;
            let deadline = Instant::now() + Duration::from_millis(timeout_ms);
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Ok(None);
                }
                sock.set_read_timeout(left).map_err(Error::Io)?;
                let read = if self.kernel_ts {
                    sock.read_frame_with_timestamp()
                } else {
                    sock.read_frame().map(|f| (f, SystemTime::now()))
                };
                let (frame, t) = match read {
                    Ok(x) => x,
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        return Ok(None)
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::InvalidData && self.kernel_ts => {
                        // No timestamp cmsg delivered: fall back to host time.
                        self.kernel_ts = false;
                        continue;
                    }
                    Err(e) => return Err(Error::Io(e)),
                };
                if let Some((extended, rtr, frame)) = convert(frame) {
                    return Ok(Some(TimedFrame {
                        ts_us: clock.since_start(t),
                        extended,
                        rtr,
                        frame,
                    }));
                }
            }
        }

        fn stop(&mut self) -> Result<()> {
            self.sock = None;
            self.clock = None;
            Ok(())
        }

        fn wall_base_us(&self) -> u64 {
            self.clock.map(|c| c.wall_base_us()).unwrap_or(0)
        }

        fn listen_only(&self) -> ListenOnly {
            self.listen
        }

        fn channel(&self) -> String {
            self.iface.clone()
        }

        fn describe(&self) -> String {
            self.describe.clone()
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::*;
    use opentune_core::Error;

    /// SocketCAN is Linux-only; the monitor refuses to start elsewhere.
    pub struct SocketCanMonitor;

    impl Default for SocketCanMonitor {
        fn default() -> Self {
            Self
        }
    }

    impl SocketCanMonitor {
        /// API parity with the Linux monitor.
        pub fn new(_label: &'static str) -> Self {
            Self
        }
    }

    impl CanMonitor for SocketCanMonitor {
        fn start(&mut self, _opts: &MonitorOpts) -> Result<()> {
            Err(Error::Unsupported(
                "SocketCAN capture is only available on Linux".into(),
            ))
        }
        fn next_frame(&mut self, _timeout_ms: u64) -> Result<Option<TimedFrame>> {
            Err(Error::NotConnected)
        }
        fn stop(&mut self) -> Result<()> {
            Ok(())
        }
        fn wall_base_us(&self) -> u64 {
            0
        }
        fn listen_only(&self) -> ListenOnly {
            ListenOnly::NotGuaranteed
        }
    }
}

pub use imp::SocketCanMonitor;

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn missing_interface_is_a_config_error() {
        let mut m = SocketCanMonitor::default();
        let mut o = MonitorOpts::default();
        o.options
            .insert("iface".into(), "opentune-no-such-can9".into());
        assert!(matches!(m.start(&o), Err(opentune_core::Error::Config(_))));
        assert!(m.next_frame(1).is_err());
    }

    /// Live loopback check on a virtual CAN interface, when one exists
    /// (`ip link add dev vcan0 type vcan && ip link set up vcan0`).
    #[test]
    fn captures_from_vcan_when_present() {
        use socketcan::{CanFrame, CanSocket, EmbeddedFrame, Socket, StandardId};
        let Ok(tx) = CanSocket::open("vcan0") else {
            return; // no vcan0 on this machine: nothing to test
        };
        let mut m = SocketCanMonitor::default();
        let mut o = MonitorOpts::default();
        o.options.insert("iface".into(), "vcan0".into());
        m.start(&o).unwrap();
        let f = CanFrame::new(StandardId::new(0x123).unwrap(), &[1, 2, 3]).unwrap();
        tx.write_frame(&f).unwrap();
        let got = m.next_frame(500).unwrap().expect("frame");
        assert_eq!((got.id(), got.data()), (0x123, &[1u8, 2, 3][..]));
        assert_eq!(m.channel(), "vcan0");
        m.stop().unwrap();
    }
}
