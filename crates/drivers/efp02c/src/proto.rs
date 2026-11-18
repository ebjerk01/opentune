//! EFP02C USB record codec (pure functions, no I/O).
//!
//! Every OUT transfer is one or more 16-byte records; byte 0 is the record
//! type. Every IN transfer is `N` 16-byte records, normally followed by a
//! 3-byte trailer `[flag, 0, seq]`.
//!
//! **Provenance / confidence.** Everything here was implemented from captures
//! of the project owner's own EFP02C (USB `2323:0001`) talking to a bench ECU,
//! driven from Linux without any vendor software. Each item is tagged:
//!
//! * *confirmed* — sent or decoded exactly like this in the captures, with the
//!   expected effect on the bus;
//! * *inferred* — a reading of the captured bytes that fits every captured
//!   example, but whose meaning was not tested separately.
//!
//! The whole driver stays UNVERIFIED until the layout is re-confirmed from a
//! black-box capture of the vendor software.

/// USB vendor id of the EFP02C (*confirmed*).
pub const USB_VID: u16 = 0x2323;
/// USB product id of the EFP02C (*confirmed*).
pub const USB_PID: u16 = 0x0001;
/// The single vendor-specific interface (*confirmed*).
pub const USB_INTERFACE: u8 = 0;
/// Bulk OUT endpoint (*confirmed*).
pub const EP_OUT: u8 = 0x02;
/// Bulk IN endpoint (*confirmed*).
pub const EP_IN: u8 = 0x82;
/// Record size; also the endpoints' max packet size (*confirmed*).
pub const RECORD_LEN: usize = 16;
/// IN transfer trailer size (*confirmed*).
pub const TRAILER_LEN: usize = 3;

/// Record types.
pub mod rec {
    /// OUT: transmit one CAN frame (*confirmed*).
    pub const TX: u8 = 0x01;
    /// IN: one received CAN frame (*confirmed*).
    pub const RX: u8 = 0x03;
    /// OUT: control command; byte 1 selects it (*confirmed*).
    pub const CONTROL: u8 = 0x04;
    /// OUT: flush / commit; byte 1 selects it (*confirmed* as sent).
    pub const FLUSH: u8 = 0x06;
    /// OUT: request the identification block (*confirmed*).
    pub const IDENTIFY: u8 = 0x09;
    /// IN: response record (identification chunk, termination state)
    /// (*confirmed*).
    pub const RESPONSE: u8 = 0x0A;
    /// OUT: read poll — the box only sends IN data in reply to one
    /// (*confirmed*).
    pub const POLL: u8 = 0x0B;
}

/// `04 xx` control sub-commands (*confirmed* as sent; effect noted).
pub mod ctl {
    /// CAN bit-timing / acceptance configuration.
    pub const CONFIG: u8 = 0x01;
    /// Enable the CAN channel.
    pub const ENABLE: u8 = 0x02;
    /// Reset the box.
    pub const RESET: u8 = 0x05;
    /// Set bus termination (byte 2: `01` = on).
    pub const TERMINATION: u8 = 0x09;
    /// Read the termination state back (answered with a `0A` record).
    pub const TERMINATION_READ: u8 = 0x0A;
}

/// The one confirmed CAN bitrate.
pub const CONFIRMED_BITRATE: u32 = 1_000_000;

/// Bytes 2..=6 of the `04 01` config record for 1 Mbit/s, copied from the
/// working capture. The split into prescaler / segment fields is not known, so
/// no other bitrate is derived from it.
const CONFIG_1MBIT: [u8; 5] = [0x00, 0x01, 0x01, 0x14, 0x00];

/// Build an all-zero record of `kind`.
fn record(kind: u8) -> [u8; RECORD_LEN] {
    let mut r = [0u8; RECORD_LEN];
    r[0] = kind;
    r
}

/// `04 05`: reset (*confirmed*).
pub fn reset() -> [u8; RECORD_LEN] {
    control(ctl::RESET)
}

/// `04 09 01`: bus termination on (*confirmed*: without it the owner's bench
/// received nothing). The "off" value is not confirmed and is not offered.
pub fn termination_on() -> [u8; RECORD_LEN] {
    let mut r = control(ctl::TERMINATION);
    r[2] = 0x01;
    r
}

/// `04 0A`: ask for the termination state (*confirmed* as sent; answered with a
/// `0A` record whose byte 1 was `01` after [`termination_on`] — *inferred* to
/// be the state).
pub fn termination_read() -> [u8; RECORD_LEN] {
    control(ctl::TERMINATION_READ)
}

/// `04 02`: enable the CAN channel (*confirmed*).
pub fn enable() -> [u8; RECORD_LEN] {
    control(ctl::ENABLE)
}

/// A `04 <sub>` control record.
pub fn control(sub: u8) -> [u8; RECORD_LEN] {
    let mut r = record(rec::CONTROL);
    r[1] = sub;
    r
}

/// `06 <sub>`: `06 02` is the flush sent after reset; `06 00` follows the
/// config record and the identification read (*confirmed* as sent).
pub fn flush(sub: u8) -> [u8; RECORD_LEN] {
    let mut r = record(rec::FLUSH);
    r[1] = sub;
    r
}

/// `09`: request the identification block (*confirmed*).
pub fn identify() -> [u8; RECORD_LEN] {
    record(rec::IDENTIFY)
}

/// The 1 Mbit/s accept-all configuration, as one OUT transfer: the `04 01`
/// config record followed by `06 00` (*confirmed*).
///
/// Layout: `04 01 | 00 01 01 14 00 | id32 | mask32 | 00`. The captured,
/// working values are id `00000000`, mask `FFFFFFFF` (every frame received).
/// Byte 15 must be `00` — with `01` the box receives but neither transmits nor
/// ACKs (*confirmed*). Only this exact record is produced: other bitrates and
/// hardware filters are unconfirmed, so filtering is done on the host.
pub fn config_1mbit_accept_all() -> [u8; 2 * RECORD_LEN] {
    let mut out = [0u8; 2 * RECORD_LEN];
    let c = &mut out[..RECORD_LEN];
    c[0] = rec::CONTROL;
    c[1] = ctl::CONFIG;
    c[2..7].copy_from_slice(&CONFIG_1MBIT);
    // id32 (bytes 7..=10) stays 0; mask32 (bytes 11..=14) = all ones.
    c[11..15].copy_from_slice(&[0xFF; 4]);
    // byte 15 = 0 (see above).
    out[RECORD_LEN..].copy_from_slice(&flush(0x00));
    out
}

/// Poll parameters (`0B` record).
///
/// `0B 00 | wait16 LE | max | 00 00 | seq | 00…`. Captured requests:
/// `0b 00 c8 00 04 00 00 <seq>` (identification), `0b 00 c8 00 01 00 00 <seq>`
/// (single response) and `0b 00 32 00 20 00 00 01` (CAN receive).
///
/// * `seq` (byte 7) — echoed in the IN trailer (*confirmed*).
/// * `max_records` (byte 4) — the most records one reply carries (*inferred*:
///   a reply never exceeded it and a full reply ended with trailer flag 0).
/// * `wait` (bytes 2..=3, little-endian) — how long the box waits for data
///   (*inferred*: `0xC8` = 200 gave empty replies after about 100 ms, so the
///   unit may be 0.5 ms; only two values were ever sent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Poll {
    pub wait: u16,
    pub max_records: u8,
}

impl Poll {
    /// The CAN receive poll from the capture (`wait 0x32`, up to 32 records).
    pub const CAN: Poll = Poll {
        wait: 0x0032,
        max_records: 0x20,
    };
    /// The identification poll from the capture (`wait 0xC8`, up to 4).
    pub const IDENTIFY: Poll = Poll {
        wait: 0x00C8,
        max_records: 0x04,
    };
    /// The single-response poll from the capture (`wait 0xC8`, 1 record).
    pub const ONE: Poll = Poll {
        wait: 0x00C8,
        max_records: 0x01,
    };

    /// The `0B` record for this poll with sequence number `seq`.
    pub fn record(self, seq: u8) -> [u8; RECORD_LEN] {
        let mut r = record(rec::POLL);
        r[2..4].copy_from_slice(&self.wait.to_le_bytes());
        r[4] = self.max_records;
        r[7] = seq;
        r
    }
}

/// Why a frame cannot be sent through the EFP02C.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxError {
    /// Only 11-bit identifiers are confirmed.
    ExtendedId(u32),
    /// Classic CAN carries at most 8 data bytes.
    TooLong(usize),
}

impl std::fmt::Display for TxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TxError::ExtendedId(id) => write!(
                f,
                "CAN id 0x{id:X} is not an 11-bit id; only 11-bit standard ids are confirmed \
                 for the EFP02C"
            ),
            TxError::TooLong(n) => write!(f, "{n} data bytes; a classic CAN frame carries 0-8"),
        }
    }
}

/// `01` transmit record for an 11-bit data frame (*confirmed*).
///
/// `01 | ID10..3 | ID2..0<<5 | DLC | 00 00 | DLC | data[8] | 00 00`, unused
/// data bytes zero. Captured: `01 7f 64 00 00 04 ff ff ff ff 00…` put id
/// `0x3FB` with 4 bytes `ff ff ff ff` on the bus. The DLC is carried in byte
/// 2's low nibble *and* in byte 5 (the captured, working form); bit 4 of byte 2
/// would be the RTR flag in this register layout but is never set here.
pub fn tx(id: u32, data: &[u8]) -> Result<[u8; RECORD_LEN], TxError> {
    if id > 0x7FF {
        return Err(TxError::ExtendedId(id));
    }
    if data.len() > 8 {
        return Err(TxError::TooLong(data.len()));
    }
    let dlc = data.len() as u8;
    let mut r = record(rec::TX);
    r[1] = (id >> 3) as u8;
    r[2] = (((id & 0x7) as u8) << 5) | dlc;
    r[5] = dlc;
    r[6..6 + data.len()].copy_from_slice(data);
    Ok(r)
}

/// One received CAN frame (`03` record).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RxFrame {
    /// 11-bit identifier.
    pub id: u32,
    /// Payload, `DLC` bytes.
    pub data: Vec<u8>,
    /// Bytes 3..=4, big-endian, uninterpreted. Called a timestamp by analogy
    /// with other adapters, but the captured values cycle through a short
    /// fixed sequence rather than counting, so it is **not** used as one.
    pub aux: u16,
    /// Byte 15, uninterpreted (`00`, `01` and `04` seen).
    pub status: u8,
}

/// Decode a `03` record (*confirmed* for 11-bit data frames).
///
/// `03 | ID10..3 | ID2..0<<5 | aux16 | DLC | data[8] | 00 | status`. In every
/// captured RX record the low 5 bits of byte 2 were zero (the DLC is in byte 5
/// only); bytes past the DLC are stale buffer contents and are dropped.
/// Returns `None` for another record type or a DLC above 8.
pub fn decode_rx(r: &[u8]) -> Option<RxFrame> {
    if r.len() < RECORD_LEN || r[0] != rec::RX {
        return None;
    }
    let dlc = r[5] as usize;
    if dlc > 8 {
        return None;
    }
    let id = ((r[1] as u32) << 3) | ((r[2] as u32) >> 5);
    Some(RxFrame {
        id,
        data: r[6..6 + dlc].to_vec(),
        aux: u16::from_be_bytes([r[3], r[4]]),
        status: r[15],
    })
}

/// IN transfer trailer `[flag, 0, seq]`.
///
/// * `seq` echoes byte 7 of the poll (*confirmed*, though one captured reply
///   repeated the previous seq — mismatches are tolerated, not fatal).
/// * `flag` (*inferred*): `1` when the poll ended because the wait ran out,
///   `0` when it ended at `max_records`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trailer {
    pub flag: u8,
    pub seq: u8,
}

/// One parsed IN transfer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InTransfer {
    /// Whole 16-byte records, in order.
    pub records: Vec<[u8; RECORD_LEN]>,
    /// The trailer, when the transfer ended with one. Some captured transfers
    /// carried records only.
    pub trailer: Option<Trailer>,
    /// Bytes that were neither a whole record nor a trailer (ignored).
    pub junk: usize,
}

/// Split an IN transfer into records and the optional 3-byte trailer.
pub fn parse_in(buf: &[u8]) -> InTransfer {
    let tail = buf.len() % RECORD_LEN;
    let body = buf.len() - tail;
    let records = buf[..body]
        .chunks_exact(RECORD_LEN)
        .map(|c| {
            let mut r = [0u8; RECORD_LEN];
            r.copy_from_slice(c);
            r
        })
        .collect();
    let (trailer, junk) = if tail == TRAILER_LEN {
        let t = &buf[body..];
        (
            Some(Trailer {
                flag: t[0],
                seq: t[2],
            }),
            0,
        )
    } else {
        (None, tail)
    };
    InTransfer {
        records,
        trailer,
        junk,
    }
}

/// Length of the identification block (*confirmed*).
pub const IDENT_LEN: usize = 32;
/// Data bytes per identification chunk.
pub const IDENT_CHUNK: usize = 8;
/// Chunks per identification block.
pub const IDENT_CHUNKS: usize = IDENT_LEN / IDENT_CHUNK;

/// An identification chunk from a `0A` response record (*confirmed*):
/// `0A 00 xx 00 00 00 00 <index> | data[8]`.
pub fn decode_ident_chunk(r: &[u8]) -> Option<(usize, [u8; IDENT_CHUNK])> {
    if r.len() < RECORD_LEN || r[0] != rec::RESPONSE || r[1] != 0 {
        return None;
    }
    let idx = r[7] as usize;
    if idx >= IDENT_CHUNKS {
        return None;
    }
    let mut d = [0u8; IDENT_CHUNK];
    d.copy_from_slice(&r[8..16]);
    Some((idx, d))
}

/// The 32-byte identification block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentBlock(pub [u8; IDENT_LEN]);

impl IdentBlock {
    /// The last byte equals the low byte of the sum of the first 31
    /// (*confirmed* on the captured block).
    pub fn checksum_ok(&self) -> bool {
        let sum = self.0[..IDENT_LEN - 1]
            .iter()
            .fold(0u8, |a, &b| a.wrapping_add(b));
        sum == self.0[IDENT_LEN - 1]
    }

    /// The longest run of printable ASCII (at least 3 characters), e.g. the
    /// model name. Field boundaries are not otherwise decoded, and serial
    /// numbers are deliberately not extracted.
    pub fn text(&self) -> Option<String> {
        self.0[..IDENT_LEN - 1]
            .split(|b| !(0x20..0x7F).contains(b))
            .filter(|s| s.len() >= 3)
            .max_by_key(|s| s.len())
            .map(|s| String::from_utf8_lossy(s).into_owned())
    }
}

/// Byte 1 of the `0A` record that answers [`termination_read`]: `01` was seen
/// after [`termination_on`] (*inferred* to be the state).
pub fn decode_termination(r: &[u8]) -> Option<bool> {
    (r.len() >= RECORD_LEN && r[0] == rec::RESPONSE).then(|| r[1] != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace()
            .map(|h| u8::from_str_radix(h, 16).unwrap())
            .collect()
    }

    #[test]
    fn init_records_match_the_capture() {
        assert_eq!(
            reset().to_vec(),
            hex("04 05 00 00 00 00 00 00 00 00 00 00 00 00 00 00")
        );
        assert_eq!(
            termination_on().to_vec(),
            hex("04 09 01 00 00 00 00 00 00 00 00 00 00 00 00 00")
        );
        assert_eq!(
            termination_read().to_vec(),
            hex("04 0a 00 00 00 00 00 00 00 00 00 00 00 00 00 00")
        );
        assert_eq!(
            config_1mbit_accept_all().to_vec(),
            hex("04 01 00 01 01 14 00 00 00 00 00 ff ff ff ff 00 \
                 06 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00")
        );
        assert_eq!(
            enable().to_vec(),
            hex("04 02 00 00 00 00 00 00 00 00 00 00 00 00 00 00")
        );
        assert_eq!(
            flush(2).to_vec(),
            hex("06 02 00 00 00 00 00 00 00 00 00 00 00 00 00 00")
        );
    }

    #[test]
    fn config_byte_15_is_zero() {
        // With byte 15 = 1 the box receives but neither transmits nor ACKs.
        assert_eq!(config_1mbit_accept_all()[15], 0);
    }

    #[test]
    fn poll_records_match_the_capture() {
        assert_eq!(
            Poll::CAN.record(1).to_vec(),
            hex("0b 00 32 00 20 00 00 01 00 00 00 00 00 00 00 00")
        );
        assert_eq!(
            Poll::IDENTIFY.record(4).to_vec(),
            hex("0b 00 c8 00 04 00 00 04 00 00 00 00 00 00 00 00")
        );
        assert_eq!(
            Poll::ONE.record(5).to_vec(),
            hex("0b 00 c8 00 01 00 00 05 00 00 00 00 00 00 00 00")
        );
    }

    #[test]
    fn tx_record_matches_the_capture() {
        // Captured: id 0x3FB, 4 bytes ff ff ff ff.
        assert_eq!(
            tx(0x3FB, &[0xFF; 4]).unwrap().to_vec(),
            hex("01 7f 64 00 00 04 ff ff ff ff 00 00 00 00 00 00")
        );
    }

    #[test]
    fn tx_refuses_unconfirmed_frames() {
        assert_eq!(
            tx(0x18DA_10F1, &[1]).unwrap_err(),
            TxError::ExtendedId(0x18DA_10F1)
        );
        assert_eq!(tx(0x800, &[]).unwrap_err(), TxError::ExtendedId(0x800));
        assert_eq!(tx(0x7E0, &[0; 9]).unwrap_err(), TxError::TooLong(9));
        // Boundaries that are fine.
        assert_eq!(tx(0x7FF, &[]).unwrap()[1..3], [0xFF, 0xE0]);
        assert_eq!(tx(0x000, &[0; 8]).unwrap()[2], 0x08);
    }

    #[test]
    fn tx_rx_id_layout_round_trips() {
        for id in [0x000, 0x001, 0x123, 0x3FB, 0x4BB, 0x7A0, 0x7E0, 0x7FF] {
            let mut r = tx(id, &[0xAA, 0xBB]).unwrap();
            // Re-shape the TX record into an RX record (DLC only in byte 5).
            r[0] = rec::RX;
            r[2] &= 0xE0;
            let f = decode_rx(&r).unwrap();
            assert_eq!(f.id, id);
            assert_eq!(f.data, vec![0xAA, 0xBB]);
        }
    }

    #[test]
    fn rx_record_decodes_the_captured_reply() {
        // Captured 0x7A0 reply: DLC 4, data 00 00 60 00; bytes after the DLC
        // are stale and must be dropped.
        let r = hex("03 f4 00 18 b7 04 00 00 60 00 4f 5f 7f 1a 00 01");
        let f = decode_rx(&r).unwrap();
        assert_eq!(f.id, 0x7A0);
        assert_eq!(f.data, vec![0x00, 0x00, 0x60, 0x00]);
        assert_eq!((f.aux, f.status), (0x18B7, 0x01));

        let r = hex("03 97 60 af 8c 08 00 27 00 27 00 53 00 00 00 00");
        let f = decode_rx(&r).unwrap();
        assert_eq!(f.id, 0x4BB);
        assert_eq!(f.data.len(), 8);
        let f = decode_rx(&hex("03 95 20 01 57 08 ff ff 00 02 00 04 00 00 00 01")).unwrap();
        assert_eq!(f.id, 0x4A9);
        let f = decode_rx(&hex("03 96 40 66 65 08 00 00 00 00 00 00 04 1a 00 00")).unwrap();
        assert_eq!(f.id, 0x4B2);
    }

    #[test]
    fn rx_rejects_other_records_and_bad_dlc() {
        assert!(decode_rx(&hex("0a 00 03 00 00 00 00 00 00 00 00 00 00 00 00 00")).is_none());
        assert!(decode_rx(&hex("03 f4 00 18 b7 09 00 00 00 00 00 00 00 00 00 00")).is_none());
        assert!(decode_rx(&[0x03; 8]).is_none());
    }

    #[test]
    fn in_transfer_split() {
        let mut buf = hex("03 f4 00 18 b7 04 00 00 60 00 4f 5f 7f 1a 00 01");
        buf.extend(hex("01 00 07"));
        let t = parse_in(&buf);
        assert_eq!(t.records.len(), 1);
        assert_eq!(t.trailer, Some(Trailer { flag: 1, seq: 7 }));
        assert_eq!(t.junk, 0);

        // Trailer only (an empty poll).
        let t = parse_in(&hex("01 00 03"));
        assert!(t.records.is_empty());
        assert_eq!(t.trailer, Some(Trailer { flag: 1, seq: 3 }));

        // Records only (seen in the capture), and a stray tail.
        let t = parse_in(&[0u8; 32]);
        assert_eq!((t.records.len(), t.trailer, t.junk), (2, None, 0));
        let t = parse_in(&[0u8; 21]);
        assert_eq!((t.records.len(), t.trailer, t.junk), (1, None, 5));
    }

    /// A synthetic identification block: zeros, a model string and a valid
    /// checksum (no real serial number).
    fn synthetic_block() -> [u8; IDENT_LEN] {
        let mut b = [0u8; IDENT_LEN];
        b[20..25].copy_from_slice(b"EFP02");
        b[31] = b[..31].iter().fold(0u8, |a, &x| a.wrapping_add(x));
        b
    }

    #[test]
    fn ident_chunks_and_checksum() {
        let block = synthetic_block();
        let mut got = [0u8; IDENT_LEN];
        for i in 0..IDENT_CHUNKS {
            let mut r = [0u8; RECORD_LEN];
            r[0] = rec::RESPONSE;
            r[2] = 0x03;
            r[7] = i as u8;
            r[8..].copy_from_slice(&block[i * 8..i * 8 + 8]);
            let (idx, d) = decode_ident_chunk(&r).unwrap();
            got[idx * 8..idx * 8 + 8].copy_from_slice(&d);
        }
        let b = IdentBlock(got);
        assert!(b.checksum_ok());
        assert_eq!(b.text().as_deref(), Some("EFP02"));
        let mut bad = b.clone();
        bad.0[3] ^= 1;
        assert!(!bad.checksum_ok());
        // Out-of-range chunk index / non-response record.
        let mut r = [0u8; RECORD_LEN];
        r[0] = rec::RESPONSE;
        r[7] = 4;
        assert!(decode_ident_chunk(&r).is_none());
        r[0] = rec::RX;
        r[7] = 0;
        assert!(decode_ident_chunk(&r).is_none());
    }

    #[test]
    fn termination_state() {
        let r = hex("0a 01 00 00 00 00 00 00 00 00 00 00 00 00 00 04");
        assert_eq!(decode_termination(&r), Some(true));
        assert_eq!(decode_termination(&[0x03; 16]), None);
    }
}
