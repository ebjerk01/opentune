//! **Subaru Select Monitor 2 (SSM2)**: packet framing, the init/capability
//! reply, address reads and a transport over a raw K-line.
//!
//! Written from the public protocol description (see `docs/PRIOR-ART.md`:
//! RomRaider and FreeSSM document it). No code is taken from either project.
//!
//! ## Packets (K-line, ISO 9141 physical layer, 4800 baud 8N1)
//!
//! ```text
//! 80 <dst> <src> <len> <cmd> <data…> <checksum>
//! ```
//!
//! * `80` starts every packet; `<len>` counts `<cmd>` plus `<data…>`.
//! * Addresses: tester [`TESTER`] `F0`, engine ECU [`ECU`] `10`, transmission
//!   [`TCU`] `18`.
//! * `<checksum>` is the 8-bit sum of every byte before it ([`checksum`]).
//! * There is no wake-up: the tester just sends. K-line is half duplex, so a
//!   plain cable hears its own packet first ([`strip_echo`]).
//!
//! A reply's command byte is the request's plus `0x40`, as in KWP2000/UDS, so
//! [`check_response`](crate::protocol::diag::check_response) classifies it:
//!
//! | Request | Reply | Meaning |
//! |---|---|---|
//! | `BF` | `FF <SSM ID ×3> <ROM ID ×5> <capability bytes…>` | init / identify |
//! | `A8 <pad> <addr ×3>…` | `E8 <byte per address>` | read single addresses |
//! | `A0 <pad> <addr ×3> <count-1>` | `E0 <count bytes>` | read a memory block |
//! | `B8 <addr ×3> <value>` | `F8 <value>` | **write** one address |
//!
//! [`SsmClient`] refuses `B8` until [`SsmClient::arm_writes`] is called;
//! logging never arms it.
//!
//! ## SSM over CAN
//!
//! Newer cars carry the same payload (`<cmd> <data…>`, no `80` header, length
//! or checksum) over ISO 15765 on `7E0`/`7E8`. Any ISO-TP
//! [`DiagTransport`] already moves payloads, so [`SsmClient`] runs on it
//! unchanged; only the K-line needs [`SsmKLine`].
//!
//! ## Capabilities
//!
//! The init reply's data (after `FF`) is the 3-byte SSM ID, the 5-byte ROM ID
//! and then capability bytes, one bit per standard parameter the ECU
//! supports. [`SsmInit::supports`] indexes that data the way RomRaider's
//! `logger.xml` `ecubyteindex`/`ecubit` do: index 0 is the first SSM ID byte,
//! so the first capability byte is index 8.
use crate::protocol::diag::{check_response, DiagTransport};
use crate::protocol::kline::{read_exact, KLineRaw};
use crate::{Error, Result};
use std::time::{Duration, Instant};

/// First byte of every K-line SSM packet.
pub const HEADER: u8 = 0x80;
/// The tester's address.
pub const TESTER: u8 = 0xF0;
/// The engine ECU's address.
pub const ECU: u8 = 0x10;
/// The transmission control unit's address.
pub const TCU: u8 = 0x18;
/// K-line baud rate (8N1).
pub const BAUD: u32 = 4800;

/// Init / identify (`BF`), answered by `FF`.
pub const CMD_INIT: u8 = 0xBF;
/// Read single addresses (`A8`), answered by `E8`.
pub const CMD_READ_ADDRESSES: u8 = 0xA8;
/// Read a memory block (`A0`), answered by `E0`.
pub const CMD_READ_BLOCK: u8 = 0xA0;
/// Write one address (`B8`), answered by `F8`. Gated: see [`SsmClient`].
pub const CMD_WRITE_ADDRESS: u8 = 0xB8;

/// Highest address a 3-byte SSM address can carry.
pub const MAX_ADDRESS: u32 = 0x00FF_FFFF;
/// Most `<cmd> <data…>` bytes one packet can carry (the length byte).
pub const MAX_PAYLOAD: usize = 255;
/// Most addresses one `A8` request can carry (`2 + 3n ≤ 255`).
pub const MAX_ADDRESSES_PER_READ: usize = 84;
/// Most bytes one `A0` block read returns (`1 + n ≤ 255`).
pub const MAX_BLOCK: usize = 254;
/// Default limit on a whole request or reply packet, in bytes. Conservative:
/// it keeps every `A8` (24 addresses) inside the OpenTune K-line bridge's
/// 80-byte buffers. Raise it with [`SsmClient::with_max_packet_len`].
pub const DEFAULT_MAX_PACKET_LEN: usize = 80;

/// Header (4) and checksum (1) bytes around a payload.
const FRAME_OVERHEAD: usize = 5;

/// The reply command for a request command.
pub const fn reply_cmd(cmd: u8) -> u8 {
    cmd.wrapping_add(0x40)
}

/// 8-bit sum of `bytes` (the SSM checksum of everything before it).
pub fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |a, &b| a.wrapping_add(b))
}

/// Build `80 <dst> <src> <len> <payload…> <checksum>`.
///
/// `payload` is the command byte and its data: 1 to [`MAX_PAYLOAD`] bytes.
pub fn build_packet(dst: u8, src: u8, payload: &[u8]) -> Result<Vec<u8>> {
    if payload.is_empty() || payload.len() > MAX_PAYLOAD {
        return Err(Error::Protocol(format!(
            "SSM payload must be 1..={MAX_PAYLOAD} bytes, got {}",
            payload.len()
        )));
    }
    let mut p = Vec::with_capacity(payload.len() + FRAME_OVERHEAD);
    p.extend_from_slice(&[HEADER, dst, src, payload.len() as u8]);
    p.extend_from_slice(payload);
    p.push(checksum(&p));
    Ok(p)
}

/// One parsed SSM packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    /// Destination address.
    pub dst: u8,
    /// Source address.
    pub src: u8,
    /// The command byte and its data.
    pub payload: Vec<u8>,
}

/// Parse exactly one packet, verifying the header byte, the length and the
/// checksum.
pub fn parse_packet(bytes: &[u8]) -> Result<Packet> {
    if bytes.len() < FRAME_OVERHEAD + 1 {
        return Err(Error::Protocol(format!(
            "SSM packet too short ({} bytes): {bytes:02X?}",
            bytes.len()
        )));
    }
    if bytes[0] != HEADER {
        return Err(Error::Protocol(format!(
            "SSM packet must start with 0x80, got 0x{:02X}",
            bytes[0]
        )));
    }
    let len = usize::from(bytes[3]);
    if len == 0 || bytes.len() != len + FRAME_OVERHEAD {
        return Err(Error::Protocol(format!(
            "SSM length byte says {len} payload bytes, packet has {}",
            bytes.len().saturating_sub(FRAME_OVERHEAD)
        )));
    }
    let (body, cs) = bytes.split_at(bytes.len() - 1);
    let want = checksum(body);
    if cs[0] != want {
        return Err(Error::Protocol(format!(
            "SSM checksum mismatch: packet carries 0x{:02X}, bytes sum to 0x{want:02X}",
            cs[0]
        )));
    }
    Ok(Packet {
        dst: bytes[1],
        src: bytes[2],
        payload: body[4..].to_vec(),
    })
}

/// Remove the half-duplex echo of `sent` from the front of `rx`, returning
/// what follows. The echo must match byte for byte: a mismatch means a bus
/// collision or a wiring fault, and the reply cannot be trusted.
pub fn strip_echo<'a>(sent: &[u8], rx: &'a [u8]) -> Result<&'a [u8]> {
    if rx.len() < sent.len() {
        return Err(Error::Protocol(format!(
            "K-line echo truncated: sent {} bytes, heard {}",
            sent.len(),
            rx.len()
        )));
    }
    let (echo, rest) = rx.split_at(sent.len());
    if echo != sent {
        return Err(Error::Protocol(format!(
            "K-line echo mismatch: sent {sent:02X?}, heard {echo:02X?}"
        )));
    }
    Ok(rest)
}

fn push_address(out: &mut Vec<u8>, address: u32) -> Result<()> {
    if address > MAX_ADDRESS {
        return Err(Error::Protocol(format!(
            "SSM address 0x{address:X} does not fit 3 bytes"
        )));
    }
    out.extend_from_slice(&address.to_be_bytes()[1..]);
    Ok(())
}

/// `A8 00 <addr ×3>…`: read one byte at each address.
pub fn read_addresses_payload(addresses: &[u32]) -> Result<Vec<u8>> {
    if addresses.is_empty() || addresses.len() > MAX_ADDRESSES_PER_READ {
        return Err(Error::Protocol(format!(
            "an A8 read carries 1..={MAX_ADDRESSES_PER_READ} addresses, got {}",
            addresses.len()
        )));
    }
    let mut p = vec![CMD_READ_ADDRESSES, 0x00];
    for &a in addresses {
        push_address(&mut p, a)?;
    }
    Ok(p)
}

/// `A0 00 <addr ×3> <count-1>`: read `count` consecutive bytes.
pub fn read_block_payload(address: u32, count: usize) -> Result<Vec<u8>> {
    if count == 0 || count > MAX_BLOCK {
        return Err(Error::Protocol(format!(
            "an A0 read returns 1..={MAX_BLOCK} bytes, asked for {count}"
        )));
    }
    let mut p = vec![CMD_READ_BLOCK, 0x00];
    push_address(&mut p, address)?;
    p.push((count - 1) as u8);
    Ok(p)
}

/// `B8 <addr ×3> <value>`: write one byte. Only [`SsmClient::write_address`]
/// (behind its gate) sends it.
pub fn write_address_payload(address: u32, value: u8) -> Result<Vec<u8>> {
    let mut p = vec![CMD_WRITE_ADDRESS];
    push_address(&mut p, address)?;
    p.push(value);
    Ok(p)
}

/// Most addresses one `A8` can carry when neither the request packet
/// (`7 + 3n` bytes) nor the reply packet (`6 + n` bytes) may exceed
/// `max_packet_len`. 0 when not even one address fits.
pub fn addresses_per_read(max_packet_len: usize) -> usize {
    let by_request = max_packet_len.saturating_sub(FRAME_OVERHEAD + 2) / 3;
    let by_reply = max_packet_len.saturating_sub(FRAME_OVERHEAD + 1);
    by_request.min(by_reply).min(MAX_ADDRESSES_PER_READ)
}

/// The decoded init (`BF` → `FF`) reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SsmInit {
    /// SSM protocol/ECU id (3 bytes).
    pub ssm_id: [u8; 3],
    /// ROM (calibration) id (5 bytes); per-ROM parameters key on it.
    pub rom_id: [u8; 5],
    /// Capability bytes after the ROM ID.
    pub capabilities: Vec<u8>,
    /// The whole reply data after `FF`: what
    /// [`supports`](Self::supports) indexes.
    pub data: Vec<u8>,
}

impl SsmInit {
    /// Decode an init reply payload (`FF` + data).
    pub fn from_reply(resp: &[u8]) -> Result<Self> {
        check_response(CMD_INIT, resp)?;
        let data = &resp[1..];
        if data.len() < 8 {
            return Err(Error::Protocol(format!(
                "SSM init reply too short for SSM ID + ROM ID: {resp:02X?}"
            )));
        }
        let mut ssm_id = [0u8; 3];
        ssm_id.copy_from_slice(&data[..3]);
        let mut rom_id = [0u8; 5];
        rom_id.copy_from_slice(&data[3..8]);
        Ok(Self {
            ssm_id,
            rom_id,
            capabilities: data[8..].to_vec(),
            data: data.to_vec(),
        })
    }

    /// The ROM ID as upper-case hex (`logger.xml` `<ecu id>` form).
    pub fn rom_id_hex(&self) -> String {
        hex(&self.rom_id)
    }

    /// The SSM ID as upper-case hex.
    pub fn ssm_id_hex(&self) -> String {
        hex(&self.ssm_id)
    }

    /// Whether capability bit `bit` of init-data byte `byte_index` is set
    /// (RomRaider `ecubyteindex`/`ecubit`; the first capability byte is index
    /// 8). An index past the reply is unsupported.
    pub fn supports(&self, byte_index: usize, bit: u8) -> bool {
        bit < 8
            && self
                .data
                .get(byte_index)
                .is_some_and(|b| b & (1 << bit) != 0)
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

/// SSM2 over a raw K-line: frames each payload, checks the echo (when the
/// adapter does not strip it), reads one reply packet by its length byte and
/// verifies its checksum and addresses.
///
/// It implements [`DiagTransport`] at the payload level (`A8 …` in,
/// `E8 …` out), the same shape an ISO-TP transport has for SSM over CAN, so
/// [`SsmClient`] and the logging engine run on either.
pub struct SsmKLine<'a> {
    link: &'a mut dyn KLineRaw,
    dst: u8,
}

impl<'a> SsmKLine<'a> {
    /// Talk to `dst` ([`ECU`] or [`TCU`]) over `link` without touching the
    /// link's settings.
    pub fn new(link: &'a mut dyn KLineRaw, dst: u8) -> Self {
        Self { link, dst }
    }

    /// [`new`](Self::new) after setting the link to [`BAUD`] and dropping
    /// stale input.
    pub fn open(link: &'a mut dyn KLineRaw, dst: u8) -> Result<Self> {
        link.set_baud(BAUD)?;
        link.clear_input()?;
        Ok(Self::new(link, dst))
    }

    /// The module this transport addresses.
    pub fn target(&self) -> u8 {
        self.dst
    }
}

impl DiagTransport for SsmKLine<'_> {
    fn request(&mut self, payload: &[u8], timeout_ms: u64) -> Result<Vec<u8>> {
        let packet = build_packet(self.dst, TESTER, payload)?;
        if let Some(max) = self.link.max_write() {
            if packet.len() > max {
                return Err(Error::Config(format!(
                    "SSM packet of {} bytes exceeds the adapter's {max}-byte limit",
                    packet.len()
                )));
            }
        }
        self.link.clear_input()?;
        self.link.write(&packet)?;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);

        if self.link.echoes() {
            let mut echo = vec![0u8; packet.len()];
            read_exact(self.link, &mut echo, deadline, timeout_ms)?;
            strip_echo(&packet, &echo)?;
        }

        let mut head = [0u8; 4];
        read_exact(self.link, &mut head, deadline, timeout_ms)?;
        if head[0] != HEADER {
            return Err(Error::Protocol(format!(
                "SSM reply must start with 0x80, got {head:02X?}"
            )));
        }
        let mut reply = head.to_vec();
        reply.resize(4 + usize::from(head[3]) + 1, 0);
        read_exact(self.link, &mut reply[4..], deadline, timeout_ms)?;
        let p = parse_packet(&reply)?;
        if p.dst != TESTER || p.src != self.dst {
            return Err(Error::Protocol(format!(
                "SSM reply addressed 0x{:02X}→0x{:02X}, expected 0x{:02X}→0x{TESTER:02X}",
                p.src, p.dst, self.dst
            )));
        }
        Ok(p.payload)
    }
}

/// SSM2 service client over any payload-level [`DiagTransport`]: an
/// [`SsmKLine`] or an ISO-TP transport for SSM over CAN.
pub struct SsmClient<'a> {
    t: &'a mut dyn DiagTransport,
    timeout_ms: u64,
    max_packet_len: usize,
    writes_armed: bool,
}

impl<'a> SsmClient<'a> {
    /// A client issuing every request with `timeout_ms`, batching under
    /// [`DEFAULT_MAX_PACKET_LEN`]. Writes start refused.
    pub fn new(t: &'a mut dyn DiagTransport, timeout_ms: u64) -> Self {
        Self {
            t,
            timeout_ms,
            max_packet_len: DEFAULT_MAX_PACKET_LEN,
            writes_armed: false,
        }
    }

    /// Batch reads so no request or reply packet exceeds `max` bytes.
    pub fn with_max_packet_len(mut self, max: usize) -> Self {
        self.max_packet_len = max;
        self
    }

    fn exchange(&mut self, req: &[u8]) -> Result<Vec<u8>> {
        let resp = self.t.request(req, self.timeout_ms)?;
        check_response(req[0], &resp)?;
        Ok(resp)
    }

    /// `BF`: identify the module and read its capability bitmap.
    pub fn init(&mut self) -> Result<SsmInit> {
        let resp = self.exchange(&[CMD_INIT])?;
        SsmInit::from_reply(&resp)
    }

    /// `A8`: one byte per address, in order, split into as many requests as
    /// the packet limit needs.
    pub fn read_addresses(&mut self, addresses: &[u32]) -> Result<Vec<u8>> {
        let per = addresses_per_read(self.max_packet_len);
        if per == 0 {
            return Err(Error::Config(format!(
                "a {}-byte packet limit cannot carry one SSM address",
                self.max_packet_len
            )));
        }
        let mut out = Vec::with_capacity(addresses.len());
        for batch in addresses.chunks(per) {
            let resp = self.exchange(&read_addresses_payload(batch)?)?;
            if resp.len() != 1 + batch.len() {
                return Err(Error::Protocol(format!(
                    "A8 of {} address(es) answered with {} byte(s)",
                    batch.len(),
                    resp.len() - 1
                )));
            }
            out.extend_from_slice(&resp[1..]);
        }
        Ok(out)
    }

    /// `A0`: `count` consecutive bytes from `address`, in as many requests as
    /// the packet limit needs.
    pub fn read_block(&mut self, address: u32, count: usize) -> Result<Vec<u8>> {
        let per = MAX_BLOCK.min(self.max_packet_len.saturating_sub(FRAME_OVERHEAD + 1));
        if per == 0 {
            return Err(Error::Config(format!(
                "a {}-byte packet limit cannot carry an A0 reply",
                self.max_packet_len
            )));
        }
        let mut out = Vec::with_capacity(count);
        while out.len() < count {
            let n = per.min(count - out.len());
            let at = address
                .checked_add(out.len() as u32)
                .filter(|&a| u64::from(a) + n as u64 - 1 <= u64::from(MAX_ADDRESS))
                .ok_or_else(|| Error::Protocol("A0 block runs past 0xFFFFFF".into()))?;
            let resp = self.exchange(&read_block_payload(at, n)?)?;
            if resp.len() != 1 + n {
                return Err(Error::Protocol(format!(
                    "A0 of {n} byte(s) answered with {}",
                    resp.len() - 1
                )));
            }
            out.extend_from_slice(&resp[1..]);
        }
        Ok(out)
    }

    /// Allow [`write_address`](Self::write_address). Writing ECU RAM changes
    /// how the engine runs; nothing in the logging path calls this.
    pub fn arm_writes(&mut self) {
        self.writes_armed = true;
    }

    /// `B8`: write one byte and return the value the module echoes back.
    /// Refused (nothing sent) unless [`arm_writes`](Self::arm_writes) was
    /// called.
    pub fn write_address(&mut self, address: u32, value: u8) -> Result<u8> {
        if !self.writes_armed {
            return Err(Error::Safety(
                "SSM write (B8) refused: writes are not armed; logging never writes".into(),
            ));
        }
        let resp = self.exchange(&write_address_payload(address, value)?)?;
        resp.get(1).copied().ok_or_else(|| {
            Error::Protocol(format!(
                "B8 answered without the written value: {resp:02X?}"
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_roundtrip_and_checksum() {
        let p = build_packet(ECU, TESTER, &[CMD_INIT]).unwrap();
        assert_eq!(p, vec![0x80, 0x10, 0xF0, 0x01, 0xBF, 0x40]);
        let back = parse_packet(&p).unwrap();
        assert_eq!((back.dst, back.src), (ECU, TESTER));
        assert_eq!(back.payload, vec![CMD_INIT]);
    }

    #[test]
    fn parse_rejects_bad_checksum_header_and_length() {
        let mut p = build_packet(TESTER, ECU, &[0xE8, 0x12]).unwrap();
        let last = p.len() - 1;
        p[last] ^= 0xFF;
        assert!(parse_packet(&p)
            .unwrap_err()
            .to_string()
            .contains("checksum"));
        let mut q = build_packet(TESTER, ECU, &[0xE8, 0x12]).unwrap();
        q[0] = 0x81;
        assert!(parse_packet(&q).is_err());
        let r = build_packet(TESTER, ECU, &[0xE8, 0x12]).unwrap();
        assert!(parse_packet(&r[..r.len() - 1]).is_err());
        assert!(build_packet(ECU, TESTER, &[]).is_err());
        assert!(build_packet(ECU, TESTER, &[0u8; 256]).is_err());
    }

    #[test]
    fn echo_must_match() {
        let sent = [0x80, 0x10, 0xF0, 0x01, 0xBF, 0x40];
        let mut rx = sent.to_vec();
        rx.extend_from_slice(&[0x80, 0xF0]);
        assert_eq!(strip_echo(&sent, &rx).unwrap(), &[0x80, 0xF0]);
        rx[2] = 0x00;
        assert!(strip_echo(&sent, &rx)
            .unwrap_err()
            .to_string()
            .contains("mismatch"));
        assert!(strip_echo(&sent, &sent[..3]).is_err());
    }

    #[test]
    fn request_payloads() {
        assert_eq!(
            read_addresses_payload(&[0x000008, 0x12_3456]).unwrap(),
            vec![0xA8, 0x00, 0x00, 0x00, 0x08, 0x12, 0x34, 0x56]
        );
        assert!(read_addresses_payload(&[0x0100_0000]).is_err());
        assert!(read_addresses_payload(&[]).is_err());
        assert!(read_addresses_payload(&[0; 85]).is_err());
        assert_eq!(
            read_block_payload(0x00_2000, 16).unwrap(),
            vec![0xA0, 0x00, 0x00, 0x20, 0x00, 0x0F]
        );
        assert_eq!(
            write_address_payload(0x00_1234, 0x7F).unwrap(),
            vec![0xB8, 0x00, 0x12, 0x34, 0x7F]
        );
    }

    #[test]
    fn batch_size_follows_the_packet_limit() {
        assert_eq!(addresses_per_read(80), 24);
        assert_eq!(addresses_per_read(10), 1);
        assert_eq!(addresses_per_read(9), 0);
        assert_eq!(addresses_per_read(1000), MAX_ADDRESSES_PER_READ);
        // The largest A8 still fits the length byte.
        let p = read_addresses_payload(&[0; MAX_ADDRESSES_PER_READ]).unwrap();
        assert!(build_packet(ECU, TESTER, &p).is_ok());
    }

    #[test]
    fn init_decodes_ids_and_capabilities() {
        let mut r = vec![0xFF, 0xA1, 0xB2, 0xC3, 0x5A, 0x5A, 0x00, 0x00, 0x01];
        r.extend_from_slice(&[0b1000_0001, 0b0000_1000]);
        let init = SsmInit::from_reply(&r).unwrap();
        assert_eq!(init.ssm_id_hex(), "A1B2C3");
        assert_eq!(init.rom_id_hex(), "5A5A000001");
        assert_eq!(init.capabilities, vec![0x81, 0x08]);
        assert!(init.supports(8, 7) && init.supports(8, 0) && init.supports(9, 3));
        assert!(!init.supports(8, 6) && !init.supports(40, 0) && !init.supports(8, 8));
        assert!(SsmInit::from_reply(&[0xFF, 1, 2, 3]).is_err());
        assert!(SsmInit::from_reply(&[0x7F, 0xBF, 0x11]).is_err());
    }
}
