//! A simulated SSM2 module, for testing the [`ssm`](crate::protocol::ssm)
//! transport, the logging engine and the CLI without a car. Synthetic: the
//! ROM ID, capability bitmap and RAM addresses are made up, and the behaviour
//! follows the public protocol description, not any particular ECU.
//!
//! [`SsmSim`] is both ends of the wire at once:
//!
//! * as a [`KLineRaw`] it is the bus behind a K-line adapter: a written packet
//!   comes back as its echo (with [`SsmSimConfig::echo`]) followed by the
//!   module's reply packet. A packet with a bad checksum, or for another
//!   address, gets no reply, as on a real bus;
//! * as a [`DiagTransport`] it is SSM over CAN: payload in, payload out.
//!
//! [`SsmSimConfig::animate`] makes a few RAM addresses ([`demo`]) change on
//! every read, so a log shows moving values. [`SsmFaults`] injects a wrong
//! echo, a corrupted reply checksum, a missing reply or a negative response.
//!
//! Commands: `BF` (init), `A8` (single addresses), `A0` (block) and `B8`
//! (write, stored in RAM and recorded in [`SsmSim::writes`]). Anything else is
//! ignored on the K-line and answered `7F <cmd> 11` over CAN.
use crate::protocol::diag::DiagTransport;
use crate::protocol::kline::KLineRaw;
use crate::protocol::ssm::{
    build_packet, parse_packet, CMD_INIT, CMD_READ_ADDRESSES, CMD_READ_BLOCK, CMD_WRITE_ADDRESS,
    ECU, TESTER,
};
use crate::{Error, Result};
use std::collections::{BTreeMap, VecDeque};

/// The synthetic module [`SsmSimConfig::demo`] describes.
pub mod demo {
    /// SSM ID (synthetic).
    pub const SSM_ID: [u8; 3] = [0xA0, 0x5E, 0x01];
    /// ROM ID (synthetic; `5A5A000001`).
    pub const ROM_ID: [u8; 5] = [0x5A, 0x5A, 0x00, 0x00, 0x01];
    /// Coolant temperature, raw `°C + 40` (capability byte 8, bit 7).
    pub const COOLANT: u32 = 0x00_1000;
    /// Engine speed, raw `rpm × 4`, big-endian over this and the next address
    /// (capability byte 8, bit 6).
    pub const RPM: u32 = 0x00_1001;
    /// Vehicle speed, km/h (capability byte 8, bit 5).
    pub const SPEED: u32 = 0x00_1003;
    /// Switch byte; bit 3 toggles on every read (capability byte 9, bit 3).
    pub const SWITCHES: u32 = 0x00_1010;
    /// A ROM-specific value: a big-endian IEEE-754 `f32` over 4 addresses.
    pub const ROM_FLOAT: u32 = 0x00_FF80;
    /// Capability bytes: byte 8 = bits 7..5 (coolant, rpm, speed), byte 9 =
    /// bit 3 (switches); bit 0 of byte 8 is clear (an unsupported channel).
    pub const CAPABILITIES: [u8; 4] = [0b1110_0000, 0b0000_1000, 0x00, 0x00];
}

/// What the simulated module is.
#[derive(Debug, Clone)]
pub struct SsmSimConfig {
    /// The module's address ([`ECU`] or [`TCU`](crate::protocol::ssm::TCU)).
    pub address: u8,
    /// SSM ID in the init reply.
    pub ssm_id: [u8; 3],
    /// ROM ID in the init reply.
    pub rom_id: [u8; 5],
    /// Capability bytes in the init reply (init-data index 8 onwards).
    pub capabilities: Vec<u8>,
    /// Return the tester's own bytes before the reply (a plain KKL cable).
    pub echo: bool,
    /// RAM contents; unset addresses read as `00`.
    pub ram: BTreeMap<u32, u8>,
    /// Move the [`demo`] addresses on every read.
    pub animate: bool,
}

impl SsmSimConfig {
    /// The synthetic [`demo`] module at the engine address, echoing.
    pub fn demo() -> Self {
        Self {
            address: ECU,
            ssm_id: demo::SSM_ID,
            rom_id: demo::ROM_ID,
            capabilities: demo::CAPABILITIES.to_vec(),
            echo: true,
            ram: BTreeMap::new(),
            animate: true,
        }
    }
}

/// Faults to inject, each counting down per affected exchange.
#[derive(Debug, Clone, Default)]
pub struct SsmFaults {
    /// Corrupt one byte of the next `n` echoes.
    pub bad_echoes: u32,
    /// Corrupt the checksum of the next `n` replies.
    pub bad_checksums: u32,
    /// Do not answer the next `n` requests.
    pub silent: u32,
    /// Answer every request for this command with `7F <cmd> <code>`.
    pub nrc: Option<(u8, u8)>,
}

/// The simulated module plus the bus it sits on.
#[derive(Debug)]
pub struct SsmSim {
    /// Its configuration.
    pub cfg: SsmSimConfig,
    /// Faults still to inject.
    pub faults: SsmFaults,
    /// Every request payload it received (valid packets only on K-line).
    pub requests: Vec<Vec<u8>>,
    /// Every `B8` write it accepted, in order.
    pub writes: Vec<(u32, u8)>,
    /// The last baud the tester set.
    pub baud: u32,
    rx: VecDeque<u8>,
    reads: u64,
}

impl SsmSim {
    /// A module with `cfg` and no faults.
    pub fn new(cfg: SsmSimConfig) -> Self {
        let mut sim = Self {
            cfg,
            faults: SsmFaults::default(),
            requests: Vec::new(),
            writes: Vec::new(),
            baud: 0,
            rx: VecDeque::new(),
            reads: 0,
        };
        if sim.cfg.animate {
            sim.animate();
        }
        sim
    }

    /// The [`demo`] module.
    pub fn demo() -> Self {
        Self::new(SsmSimConfig::demo())
    }

    /// Read requests (`A8`/`A0`) answered so far.
    pub fn reads(&self) -> u64 {
        self.reads
    }

    fn peek(&self, address: u32) -> u8 {
        self.cfg.ram.get(&address).copied().unwrap_or(0)
    }

    /// Set the [`demo`] addresses for the current read count.
    fn animate(&mut self) {
        let n = self.reads;
        let ram = &mut self.cfg.ram;
        ram.insert(demo::COOLANT, (40 + 70 + n % 20) as u8);
        let rpm4 = ((800 + (n * 120) % 4000) * 4) as u16;
        ram.insert(demo::RPM, (rpm4 >> 8) as u8);
        ram.insert(demo::RPM + 1, rpm4 as u8);
        ram.insert(demo::SPEED, (n % 140) as u8);
        ram.insert(demo::SWITCHES, if n % 2 == 1 { 0b0000_1000 } else { 0 });
        let f = -0.35f32 * (n % 5) as f32;
        for (i, b) in f.to_be_bytes().into_iter().enumerate() {
            ram.insert(demo::ROM_FLOAT + i as u32, b);
        }
    }

    fn on_read(&mut self) {
        self.reads += 1;
        if self.cfg.animate {
            self.animate();
        }
    }

    /// The module's answer to one request payload, `None` for silence.
    /// `can` selects the CAN behaviour for an unknown command (an NRC rather
    /// than silence).
    fn handle(&mut self, req: &[u8], can: bool) -> Option<Vec<u8>> {
        self.requests.push(req.to_vec());
        let cmd = *req.first()?;
        if self.faults.silent > 0 {
            self.faults.silent -= 1;
            return None;
        }
        if let Some((c, code)) = self.faults.nrc {
            if c == cmd {
                return Some(vec![0x7F, cmd, code]);
            }
        }
        let addr = |b: &[u8]| (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let reply = match cmd {
            CMD_INIT if req.len() == 1 => {
                let mut r = vec![0xFF];
                r.extend_from_slice(&self.cfg.ssm_id);
                r.extend_from_slice(&self.cfg.rom_id);
                r.extend_from_slice(&self.cfg.capabilities);
                Some(r)
            }
            CMD_READ_ADDRESSES if req.len() >= 5 && (req.len() - 2) % 3 == 0 && req[1] == 0 => {
                self.on_read();
                let mut r = vec![0xE8];
                r.extend(req[2..].chunks(3).map(|c| self.peek(addr(c))));
                Some(r)
            }
            CMD_READ_BLOCK if req.len() == 6 && req[1] == 0 => {
                self.on_read();
                let start = addr(&req[2..5]);
                let mut r = vec![0xE0];
                r.extend((0..=u32::from(req[5])).map(|i| self.peek(start + i)));
                Some(r)
            }
            CMD_WRITE_ADDRESS if req.len() == 5 => {
                let a = addr(&req[1..4]);
                self.cfg.ram.insert(a, req[4]);
                self.writes.push((a, req[4]));
                Some(vec![0xF8, req[4]])
            }
            _ => None,
        };
        match reply {
            Some(r) => Some(r),
            None if can => Some(vec![0x7F, cmd, 0x11]),
            None => None,
        }
    }
}

impl KLineRaw for SsmSim {
    fn set_baud(&mut self, baud: u32) -> Result<()> {
        self.baud = baud;
        Ok(())
    }

    fn clear_input(&mut self) -> Result<()> {
        self.rx.clear();
        Ok(())
    }

    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        if self.cfg.echo {
            let mut echo = bytes.to_vec();
            if self.faults.bad_echoes > 0 && !echo.is_empty() {
                self.faults.bad_echoes -= 1;
                let mid = echo.len() / 2;
                echo[mid] ^= 0x01;
            }
            self.rx.extend(echo);
        }
        // A corrupt packet, or one for another module, is ignored.
        let Ok(p) = parse_packet(bytes) else {
            return Ok(());
        };
        if p.dst != self.cfg.address || p.src != TESTER {
            return Ok(());
        }
        if let Some(reply) = self.handle(&p.payload, false) {
            let mut pkt = build_packet(TESTER, self.cfg.address, &reply)?;
            if self.faults.bad_checksums > 0 {
                self.faults.bad_checksums -= 1;
                let last = pkt.len() - 1;
                pkt[last] ^= 0x5A;
            }
            self.rx.extend(pkt);
        }
        Ok(())
    }

    fn read(&mut self, buf: &mut [u8], timeout_ms: u64) -> Result<usize> {
        if self.rx.is_empty() {
            return Err(Error::Timeout(timeout_ms));
        }
        let n = buf.len().min(self.rx.len());
        for (slot, b) in buf.iter_mut().zip(self.rx.drain(..n)) {
            *slot = b;
        }
        Ok(n)
    }

    fn echoes(&self) -> bool {
        self.cfg.echo
    }
}

impl DiagTransport for SsmSim {
    fn request(&mut self, req: &[u8], timeout_ms: u64) -> Result<Vec<u8>> {
        self.handle(req, true).ok_or(Error::Timeout(timeout_ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ssm::{SsmClient, SsmKLine, BAUD, TCU};

    #[test]
    fn kline_init_and_reads_with_echo() {
        let mut sim = SsmSim::demo();
        let mut kl = SsmKLine::open(&mut sim, ECU).unwrap();
        let mut c = SsmClient::new(&mut kl, 100);
        let init = c.init().unwrap();
        assert_eq!(init.rom_id_hex(), "5A5A000001");
        assert!(init.supports(8, 7) && !init.supports(8, 0));
        let a = c
            .read_addresses(&[demo::COOLANT, demo::RPM, demo::RPM + 1])
            .unwrap();
        assert_eq!(a.len(), 3);
        let b = c.read_addresses(&[demo::COOLANT]).unwrap();
        assert_ne!(a[0], b[0], "the demo values move between reads");
        let blk = c.read_block(demo::ROM_FLOAT, 4).unwrap();
        assert_eq!(blk.len(), 4);
        assert_eq!(sim.baud, BAUD);
    }

    #[test]
    fn writes_are_refused_until_armed() {
        let mut sim = SsmSim::demo();
        {
            let mut c = SsmClient::new(&mut sim, 100);
            let e = c.write_address(0x00_2000, 0x42).unwrap_err();
            assert!(matches!(e, Error::Safety(_)), "{e}");
        }
        assert!(sim.requests.is_empty(), "a refused write sends nothing");
        let mut c = SsmClient::new(&mut sim, 100);
        c.arm_writes();
        assert_eq!(c.write_address(0x00_2000, 0x42).unwrap(), 0x42);
        assert_eq!(sim.writes, vec![(0x00_2000, 0x42)]);
    }

    #[test]
    fn other_addresses_and_bad_packets_get_no_reply() {
        let mut sim = SsmSim::demo();
        {
            let mut kl = SsmKLine::new(&mut sim, TCU);
            let e = SsmClient::new(&mut kl, 50).init().unwrap_err();
            assert!(matches!(e, Error::Timeout(_)), "{e}");
        }
        let mut bad = build_packet(ECU, TESTER, &[CMD_INIT]).unwrap();
        bad[5] ^= 1;
        sim.cfg.echo = false;
        KLineRaw::write(&mut sim, &bad).unwrap();
        let mut buf = [0u8; 8];
        assert!(KLineRaw::read(&mut sim, &mut buf, 10).is_err());
    }

    #[test]
    fn can_mode_answers_payloads_and_unknown_commands_with_an_nrc() {
        let mut sim = SsmSim::demo();
        let mut c = SsmClient::new(&mut sim, 100);
        assert_eq!(c.init().unwrap().rom_id, demo::ROM_ID);
        let e = DiagTransport::request(&mut sim, &[0x12], 10).unwrap();
        assert_eq!(e, vec![0x7F, 0x12, 0x11]);
    }
}
