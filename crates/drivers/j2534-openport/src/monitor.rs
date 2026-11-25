//! Receive-only raw-CAN capture on the OpenPort 2.0 (`opentune candump
//! --driver openport2`).
//!
//! The session is the driver's own (`\r\n\r\n`, `ati`, `atz`, `ata`,
//! `ato5 <flags> <rate> 0`), then a **pass-all** PASS filter (`atf5 1 <txflags>
//! 4` + a zero mask and a zero pattern: every id matches) instead of the
//! response-id filter, then nothing but reads. Records are decoded as the
//! driver decodes them: transmit indications (`0x10`), loopback echoes
//! (`0x20`) and bare START indications (`0x80`) are dropped; status bit `0x02`
//! marks a 29-bit id (bisak §7.4).
//!
//! No transmit path, structurally: [`Op2Monitor`] has no send method, and its
//! session is marked receive-only, so [`crate::Session::command`] refuses
//! `att`/`atm`/`aty`/`atw`/`atv`/`atx`/`atp` before writing a byte. The cable
//! still acknowledges frames on the bus (it has no listen-only mode that
//! stops that: `SNIFF_MODE` is accepted but frames are still acknowledged,
//! bisak §10), so the listen-only level is reported as not guaranteed.
//!
//! Options ([`MonitorOpts`]): `port` (default `/dev/ttyACM0`), the bit rate
//! (`--bitrate`, else `--bus hs|ms` → 500k/125k; one of the driver's
//! [`BITRATES`](crate::BITRATES)), `can_ids` = `11` (default: connect flags 0,
//! the form the driver's CAN session uses), `29` (`CAN_29BIT_ID`) or `both`
//! (J2534 `CAN_ID_BOTH`, unverified on a cable).
use crate::port::{Op2Port, SerialOp2Port};
use crate::proto::{self, status, Msg, CAN_29BIT_ID, PROTO_CAN};
use crate::{Session, Settings, CMD_TIMEOUT_MS, SETUP_TIMEOUT_MS};
use opentune_core::monitor::MonitorClock;
use opentune_core::{CanMonitor, Error, ListenOnly, MonitorOpts, Result, TimedFrame};
use std::time::{Duration, Instant};

/// J2534 `CAN_ID_BOTH` connect flag: receive 11- and 29-bit ids on one
/// channel (SAE J2534-1). Not exercised on a cable.
pub const CAN_ID_BOTH: u32 = 0x0000_0800;

/// Receive-only capture on the OP2's raw CAN channel.
#[derive(Default)]
pub struct Op2Monitor {
    injected: Option<Box<dyn Op2Port>>,
    session: Option<Session>,
    clock: Option<MonitorClock>,
    describe: String,
}

impl Op2Monitor {
    /// Use `port` instead of opening the serial device at the next `start`.
    pub fn inject_port(&mut self, port: Box<dyn Op2Port>) {
        self.injected = Some(port);
    }

    /// Connect flags for the `can_ids` option.
    fn id_flags(opts: &MonitorOpts) -> Result<u32> {
        match opts.option("can_ids").map(str::trim) {
            None | Some("11") => Ok(0),
            Some("29") => Ok(CAN_29BIT_ID),
            Some("both") => Ok(CAN_ID_BOTH),
            Some(other) => Err(Error::Config(format!(
                "openport2 capture: can_ids '{other}' (expected 11, 29 or both)"
            ))),
        }
    }

    fn session(&mut self) -> Result<&mut Session> {
        self.session.as_mut().ok_or(Error::NotConnected)
    }
}

impl CanMonitor for Op2Monitor {
    fn start(&mut self, opts: &MonitorOpts) -> Result<()> {
        if opts.option("listen_only") == Some("require") {
            return Err(Error::Unsupported(
                "the OpenPort 2.0 has no listen-only mode (it acknowledges frames on the bus); \
                 listen_only=require refused before opening anything"
                    .into(),
            ));
        }
        let bitrate = opts.resolved_bitrate();
        if !crate::BITRATES.contains(&bitrate) {
            return Err(Error::Config(format!(
                "openport2 capture: CAN bit rate {bitrate} not supported (one of {:?}); a \
                 wrong rate on a live bus can hang the cable",
                crate::BITRATES
            )));
        }
        let flags = Self::id_flags(opts)?;
        let _ = self.stop();
        let (port, name): (Box<dyn Op2Port>, String) = match self.injected.take() {
            Some(p) => (p, "injected".into()),
            None => {
                let path = opts.option("port").unwrap_or("/dev/ttyACM0").to_string();
                (Box::new(SerialOp2Port::open(&path)?), path)
            }
        };
        let settings = Settings {
            bitrate,
            tx_timeout_ms: 1,
            extended: flags == CAN_29BIT_ID,
        };
        let mut s = Session::new(port, settings);
        s.receive_only = true;
        s.port.write_all(b"\r\n\r\n")?;
        let firmware = s.identify(1500)?;
        let setup = (|| -> Result<()> {
            s.command("atz", &[], SETUP_TIMEOUT_MS)?;
            s.command("ata", &[], CMD_TIMEOUT_MS)?;
            s.command(
                &proto::open_channel(PROTO_CAN, flags, bitrate),
                &[],
                SETUP_TIMEOUT_MS,
            )?;
            // Pass-all: mask 0 and pattern 0 match every id.
            let (body, payload) = proto::filter(
                PROTO_CAN,
                proto::filter::PASS,
                flags & CAN_29BIT_ID,
                &[0; 4],
                &[0; 4],
                None,
            )
            .ok_or_else(|| Error::Protocol("invalid filter".into()))?;
            s.command(&body, &payload, CMD_TIMEOUT_MS).map(|_| ())
        })();
        if let Err(e) = setup {
            let _ = s.command("ata", &[], CMD_TIMEOUT_MS);
            return Err(e);
        }
        s.rx.clear();
        self.describe = format!(
            "OpenPort 2.0 {firmware} on {name} @ {} kbit/s, receive-only (pass-all filter)",
            bitrate / 1000
        );
        self.clock = Some(MonitorClock::start());
        self.session = Some(s);
        Ok(())
    }

    fn next_frame(&mut self, timeout_ms: u64) -> Result<Option<TimedFrame>> {
        let clock = self.clock.ok_or(Error::NotConnected)?;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let s = self.session()?;
        // Frames queued while a setup command waited (none after start).
        if let Some((id, data)) = s.rx.pop_front() {
            return Ok(Some(TimedFrame::classic(
                clock.now_us(),
                id,
                id > 0x7FF,
                data,
            )));
        }
        loop {
            let Some(m) = s.read_msg(deadline)? else {
                return Ok(None);
            };
            let f = match m {
                Msg::Frame(f) => f,
                Msg::Reply(_) => {
                    s.stats.stale_replies += 1;
                    continue;
                }
            };
            let st = f.status;
            if f.proto != PROTO_CAN {
                s.stats.foreign_frames += 1;
                continue;
            }
            if st & (status::TX_INDICATION | status::LOOPBACK) != 0
                || (st & status::START != 0 && st & status::END == 0)
            {
                s.stats.indications += 1;
                continue;
            }
            match f.can() {
                Some(c) if c.data.len() <= 8 => {
                    let id = c.id & 0x1FFF_FFFF;
                    let ext = st & status::ID_29BIT != 0 || id > 0x7FF;
                    return Ok(Some(TimedFrame::classic(clock.now_us(), id, ext, c.data)));
                }
                _ => s.stats.foreign_frames += 1,
            }
        }
    }

    fn stop(&mut self) -> Result<()> {
        if let Some(mut s) = self.session.take() {
            s.close();
        }
        self.clock = None;
        Ok(())
    }

    fn wall_base_us(&self) -> u64 {
        self.clock.map(|c| c.wall_base_us()).unwrap_or(0)
    }

    fn listen_only(&self) -> ListenOnly {
        ListenOnly::NotGuaranteed
    }

    fn describe(&self) -> String {
        self.describe.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{Chunking, FakeConfig, FakeOp2, FakePort};

    /// The session under a monitor refuses every bus-writing verb before a
    /// byte reaches the cable, even if code inside the crate tried.
    #[test]
    fn a_receive_only_session_refuses_bus_writing_commands_unsent() {
        let (dev, h) = FakeOp2::new(FakeConfig::default(), Box::new(|_: &[u8]| Vec::new()));
        let settings = Settings {
            bitrate: 500_000,
            tx_timeout_ms: 100,
            extended: false,
        };
        let mut s = Session::new(Box::new(FakePort::new(dev, Chunking::Whole)), settings);
        s.receive_only = true;
        s.command("ata", &[], 1000).unwrap();
        for body in [
            "att5 8 0 100000",
            "atm5 100000 0 0 8",
            "aty4 4 0",
            "atw4 1",
            "atv 12 12000",
            "atx 1",
            "atp 5 0",
        ] {
            let e = s.command(body, &[0; 8], 100).unwrap_err();
            assert!(matches!(e, Error::Unsupported(_)), "{body}: {e}");
        }
        assert!(s.transmit(0x7E0, &[1, 2]).is_err());
        assert_eq!(
            h.log().lines,
            vec!["ata 1"],
            "only the harmless command went out"
        );
    }
}
