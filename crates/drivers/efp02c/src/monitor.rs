//! Raw CAN capture over an EFP02C ([`CanMonitor`]).
//!
//! No silent / listen-only mode of the box is known, so the controller runs in
//! normal mode: OpenTune never transmits a frame, but the controller ACKs
//! frames on the bus — reported as [`ListenOnly::Passive`]. Only 1 Mbit/s is
//! confirmed: any other bitrate (including the 500k / 125k a `bus=hs|ms`
//! option implies) is refused rather than risk error frames on a live bus.
//! With no bitrate and no `bus` given, 1 Mbit/s is used.
//!
//! Options: `termination` (on/off, default on), `usb_index` (default 0).
//! Timestamps are host arrival time; the record's 16-bit field that looks like
//! a timestamp is not one (see [`crate::proto::RxFrame::aux`]).
use crate::link::{Efp02cLink, LinkOptions};
use crate::proto::CONFIRMED_BITRATE;
use crate::transport::{self, Efp02cTransport};
use opentune_core::monitor::MonitorClock;
use opentune_core::{CanMonitor, Error, ListenOnly, MonitorOpts, Result, TimedFrame};

/// EFP02C bus monitor.
#[derive(Default)]
pub struct Efp02cMonitor {
    link: Option<Efp02cLink>,
    /// Pre-opened transport for tests, used by the next `start`.
    injected: Option<Box<dyn Efp02cTransport>>,
    /// Init timing override for the injected transport.
    timing: Option<crate::link::Timing>,
    clock: Option<MonitorClock>,
    describe: String,
}

/// The bitrate a monitor would run at, or why it is refused.
pub fn monitor_bitrate(opts: &MonitorOpts) -> Result<u32> {
    let explicit = opts.bitrate != 0 || opts.option("bus").is_some();
    let bitrate = if explicit {
        opts.resolved_bitrate()
    } else {
        CONFIRMED_BITRATE
    };
    crate::check_bitrate(bitrate)?;
    Ok(bitrate)
}

impl Efp02cMonitor {
    pub fn new() -> Self {
        Self::default()
    }

    /// A monitor over a pre-opened transport (tests, replays). Runs the init
    /// sequence with `timing`.
    pub fn with_transport(t: Box<dyn Efp02cTransport>, timing: crate::link::Timing) -> Self {
        Self {
            injected: Some(t),
            timing: Some(timing),
            ..Self::default()
        }
    }
}

impl CanMonitor for Efp02cMonitor {
    fn start(&mut self, opts: &MonitorOpts) -> Result<()> {
        self.stop()?;
        let bitrate = monitor_bitrate(opts)?;
        let mut lo = LinkOptions::from_options(&opts.options)?;
        // Identification is never needed for a capture.
        lo.identify = false;
        let t = match self.injected.take() {
            Some(t) => {
                if let Some(timing) = self.timing {
                    lo.timing = timing;
                }
                t
            }
            None => transport::open_usb(lo.usb_index)?,
        };
        let link = Efp02cLink::open(t, lo)?;
        self.describe = format!(
            "{} @ {} kbit/s, termination {}, listen-only: {} (driver UNVERIFIED)",
            link.describe(),
            bitrate / 1000,
            match link.termination() {
                Some(true) => "on",
                Some(false) => "off",
                None => "unknown",
            },
            ListenOnly::Passive.as_str()
        );
        self.link = Some(link);
        self.clock = Some(MonitorClock::start());
        Ok(())
    }

    fn next_frame(&mut self, timeout_ms: u64) -> Result<Option<TimedFrame>> {
        let clock = self.clock.ok_or(Error::NotConnected)?;
        let link = self.link.as_mut().ok_or(Error::NotConnected)?;
        Ok(link
            .next_rx(timeout_ms)?
            .map(|f| TimedFrame::classic(clock.now_us(), f.id, false, f.data)))
    }

    fn stop(&mut self) -> Result<()> {
        if let Some(mut l) = self.link.take() {
            l.shutdown();
        }
        self.clock = None;
        Ok(())
    }

    fn wall_base_us(&self) -> u64 {
        self.clock.map(|c| c.wall_base_us()).unwrap_or(0)
    }

    fn dropped(&self) -> u64 {
        self.link.as_ref().map(|l| l.stats().dropped).unwrap_or(0)
    }

    fn listen_only(&self) -> ListenOnly {
        ListenOnly::Passive
    }

    fn describe(&self) -> String {
        self.describe.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitrate_policy() {
        let mut o = MonitorOpts::default();
        assert_eq!(monitor_bitrate(&o).unwrap(), 1_000_000);
        o.bitrate = 1_000_000;
        assert_eq!(monitor_bitrate(&o).unwrap(), 1_000_000);
        o.bitrate = 500_000;
        assert!(matches!(monitor_bitrate(&o), Err(Error::Unsupported(_))));
        let mut o = MonitorOpts::default();
        o.options.insert("bus".into(), "hs".into());
        assert!(matches!(monitor_bitrate(&o), Err(Error::Unsupported(_))));
    }
}
