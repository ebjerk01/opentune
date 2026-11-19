//! OpenPort 2.0 wire codec: the ASCII command lines the host sends and a
//! parser for the device's mixed ASCII-reply / binary-frame stream.
//!
//! Built from public protocol documentation only (facts, no code copied):
//! bisak/openport-j2534 `docs/PROTOCOL.md` (GPL-3.0; cable-verified, with
//! traces of the vendor DLL run as a black box), Mackanized/op2j2534
//! `PROTOCOL.md` and roffe/libj2534_openport2 `RE_NOTES.md` (both Apache-2.0),
//! plus the SAE J2534-1 (04.04) constants (protocol ids, flags, filter types,
//! return codes). Where the sources disagree, the choice and the reason are
//! noted at the item; `docs/DRIVERS.md` (§3l) lists every point a hardware
//! owner must confirm.
//!
//! Framing summary:
//! - Host → device: `at<verb>[<proto>] <args…> <seq>\r\n`, then any binary
//!   payload **in the same write**. The protocol digit is glued to the verb
//!   (`ato5`, `att5`); `atr`/`ati`/`ata`/`atz` take a space. A bare `\r` does
//!   not end a line, so every line ends in `\r\n`.
//! - Device → host: ASCII replies `ar<verb> <fields…> <seq>\r\n` (the trailing
//!   number echoes the command's sequence number; `ari` has none), interleaved
//!   with binary records `'a' 'r' <proto digit> <len> <status> <body…>` where
//!   `len` counts every byte after it (status + body).

/// J2534 protocol id of the K-line ISO 9141 channel.
pub const PROTO_ISO9141: u8 = 3;
/// J2534 protocol id of the K-line ISO 14230 channel.
pub const PROTO_ISO14230: u8 = 4;
/// J2534 protocol id of the raw-CAN channel (the one this driver opens).
pub const PROTO_CAN: u8 = 5;
/// J2534 protocol id of the ISO 15765 (on-cable ISO-TP) channel.
pub const PROTO_ISO15765: u8 = 6;

/// J2534 `CAN_29BIT_ID`: connect flag and TxFlags bit for 29-bit identifiers.
pub const CAN_29BIT_ID: u32 = 0x0000_0100;
/// J2534 `ISO15765_FRAME_PAD` TxFlags bit (pad CAN frames to 8 bytes).
pub const ISO15765_FRAME_PAD: u32 = 0x0000_0040;

/// Analog input carrying the vehicle battery (OBD pin 16), read with `atr`.
pub const PIN_VBAT: u8 = 16;

/// J2534 filter types (the `<type>` field of `atf`).
pub mod filter {
    pub const PASS: u32 = 1;
    pub const BLOCK: u32 = 2;
    pub const FLOW_CONTROL: u32 = 3;
}

/// Bits of the status byte of a binary frame record (bisak PROTOCOL.md §7.2).
pub mod status {
    /// A segmented ISO 15765 message begins (indication, no data).
    pub const START: u8 = 0x80;
    /// This record completes a message.
    pub const END: u8 = 0x40;
    /// An echo of our own transmit (`LOOPBACK` on).
    pub const LOOPBACK: u8 = 0x20;
    /// Transmit indication: the CAN id of a frame we sent, no data.
    pub const TX_INDICATION: u8 = 0x10;
    /// Meaning unknown; the vendor DLL is reported to deliver nothing for it.
    pub const UNKNOWN_08: u8 = 0x08;
    /// ISO 15765 extended addressing (not seen on a cable).
    pub const EXT_ADDR: u8 = 0x04;
    /// The CAN id is 29-bit.
    pub const ID_29BIT: u8 = 0x02;
}

/// Longest ASCII reply line accepted before the parser gives up on it and
/// resynchronises (real replies are well under 64 bytes).
const MAX_LINE: usize = 256;

/// Name of a J2534-1 return code as the device reports it in `are <code>`
/// (decimal). Codes 119/120 are the Tactrix OEM voltage codes.
pub fn j2534_error_name(code: u32) -> &'static str {
    match code {
        0x00 => "STATUS_NOERROR",
        0x01 => "ERR_NOT_SUPPORTED",
        0x02 => "ERR_INVALID_CHANNEL_ID",
        0x03 => "ERR_INVALID_PROTOCOL_ID",
        0x04 => "ERR_NULL_PARAMETER",
        0x05 => "ERR_INVALID_IOCTL_VALUE",
        0x06 => "ERR_INVALID_FLAGS",
        0x07 => "ERR_FAILED",
        0x08 => "ERR_DEVICE_NOT_CONNECTED",
        0x09 => "ERR_TIMEOUT",
        0x0A => "ERR_INVALID_MSG",
        0x0B => "ERR_INVALID_TIME_INTERVAL",
        0x0C => "ERR_EXCEEDED_LIMIT",
        0x0D => "ERR_INVALID_MSG_ID",
        0x0E => "ERR_DEVICE_IN_USE",
        0x0F => "ERR_INVALID_IOCTL_ID",
        0x10 => "ERR_BUFFER_EMPTY",
        0x11 => "ERR_BUFFER_FULL",
        0x12 => "ERR_BUFFER_OVERFLOW",
        0x13 => "ERR_PIN_INVALID",
        0x14 => "ERR_CHANNEL_IN_USE",
        0x15 => "ERR_MSG_PROTOCOL_ID",
        0x16 => "ERR_INVALID_FILTER_ID",
        0x17 => "ERR_NO_FLOW_CONTROL",
        0x18 => "ERR_NOT_UNIQUE",
        0x19 => "ERR_INVALID_BAUDRATE",
        0x1A => "ERR_INVALID_DEVICE_ID",
        119 => "ERR_OEM_VOLTAGE_TOO_HIGH",
        120 => "ERR_OEM_VOLTAGE_TOO_LOW",
        _ => "unknown error",
    }
}

/// One host → device write: `<body> <seq>\r\n` followed by `payload`.
/// The vendor DLL sends line and payload in one write; a payload that arrives
/// separately (~15 ms later) is rejected and desynchronises the parser.
pub fn numbered(body: &str, seq: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = format!("{body} {seq}\r\n").into_bytes();
    out.extend_from_slice(payload);
    out
}

/// `ato<proto> <flags> <baud> 0`: open a channel. Flags come **before** the
/// baud rate (all three sources' code and the J2534 `PassThruConnect` order;
/// Mackanized's PROTOCOL.md table shows the opposite but its code does not).
/// The fourth field is ignored by the firmware (bisak §4) and sent as 0.
pub fn open_channel(proto: u8, flags: u32, baud: u32) -> String {
    format!("ato{proto} {flags} {baud} 0")
}

/// `atc<proto>`: close a channel (addressed by protocol id, not a handle).
pub fn close_channel(proto: u8) -> String {
    format!("atc{proto}")
}

/// `att<proto> <len> <txflags> <budget_us>` + `<id BE32><data>`: transmit one
/// message. `len` counts the 4-byte id plus the data; the budget is how long
/// (µs) the cable may try to get the frame on the bus and must be non-zero (a
/// zero budget answers `ERR_TIMEOUT` without sending).
pub fn transmit(
    proto: u8,
    id: u32,
    data: &[u8],
    txflags: u32,
    budget_us: u32,
) -> (String, Vec<u8>) {
    let mut payload = Vec::with_capacity(4 + data.len());
    payload.extend_from_slice(&id.to_be_bytes());
    payload.extend_from_slice(data);
    (
        format!("att{proto} {} {txflags} {budget_us}", payload.len()),
        payload,
    )
}

/// `atf<proto> <type> <txflags> <len>` + mask + pattern (+ flow control):
/// install a message filter. `len` is the length of **one** of the messages
/// (they must be equal); the reply is `arf<proto> <filter_id> <seq>`.
///
/// Sources disagree on the 2nd/3rd fields: Mackanized/roffe read them as
/// `<mask_len> <pattern_len>` and send `4 4`, bisak reads `<txflags> <len>`
/// and shows the vendor DLL sending `atf6 3 64 4` + 12 bytes (TxFlags 0x40,
/// three 4-byte messages), which only the second reading explains. This
/// follows bisak; `4 4` also parses as "TxFlags 4, length 4" under it, which
/// is why both projects work on hardware.
pub fn filter(
    proto: u8,
    filter_type: u32,
    txflags: u32,
    mask: &[u8],
    pattern: &[u8],
    flow_control: Option<&[u8]>,
) -> Option<(String, Vec<u8>)> {
    let len = mask.len();
    if pattern.len() != len || flow_control.is_some_and(|f| f.len() != len) {
        return None;
    }
    if (filter_type == filter::FLOW_CONTROL) != flow_control.is_some() {
        return None;
    }
    let mut payload = Vec::with_capacity(len * 3);
    payload.extend_from_slice(mask);
    payload.extend_from_slice(pattern);
    if let Some(fc) = flow_control {
        payload.extend_from_slice(fc);
    }
    Some((format!("atf{proto} {filter_type} {txflags} {len}"), payload))
}

/// `atr <pin>`: read a pin voltage in millivolts (reply `arr <pin> <mV> <seq>`).
pub fn read_pin(pin: u8) -> String {
    format!("atr {pin}")
}

/// One message decoded from the device stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    Reply(Reply),
    Frame(RawFrame),
}

/// An ASCII reply line `ar<verb><rest>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// The reply letter: `o` ok, `e` error, `i` identify, `r` pin, `f` filter…
    pub verb: u8,
    /// Everything after the verb, trimmed (`5 0 8` for `arf5 0 8`).
    pub text: String,
    /// `text` split on whitespace.
    pub fields: Vec<String>,
}

impl Reply {
    /// The echoed sequence number: the last field, when it is a number.
    pub fn seq(&self) -> Option<u16> {
        self.fields.last().and_then(|s| s.parse().ok())
    }

    /// The J2534 code of an `are <code> … <seq>` reply.
    pub fn error_code(&self) -> Option<u32> {
        if self.verb != b'e' {
            return None;
        }
        self.fields.first().and_then(|s| s.parse().ok())
    }
}

/// A binary record `ar<proto digit><len><status><body>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawFrame {
    /// Channel / J2534 protocol id from the ASCII digit.
    pub proto: u8,
    pub status: u8,
    /// The bytes after the status byte.
    pub body: Vec<u8>,
}

/// A CAN-channel record decoded: `<timestamp BE32 µs><id BE32><data>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanRecord {
    pub status: u8,
    pub timestamp_us: u32,
    pub id: u32,
    pub data: Vec<u8>,
}

impl RawFrame {
    /// Decode the CAN / ISO 15765 layout (timestamp, id, data). `None` when the
    /// body is too short to hold the timestamp and id. K-line records use a
    /// different layout (bisak §7.9) and must not be decoded with this.
    pub fn can(&self) -> Option<CanRecord> {
        if self.body.len() < 8 {
            return None;
        }
        let b = &self.body;
        Some(CanRecord {
            status: self.status,
            timestamp_us: u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
            id: u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
            data: b[8..].to_vec(),
        })
    }
}

/// Incremental parser for the device → host stream. Feed it whatever the port
/// returns (split or merged reads) and pull complete messages out. Bytes that
/// cannot start a message are skipped until the next `ar` (resynchronisation),
/// and counted in [`garbage`](Self::garbage).
#[derive(Debug, Default)]
pub struct Parser {
    buf: Vec<u8>,
    /// Bytes discarded while resynchronising.
    pub garbage: u64,
}

impl Parser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Bytes buffered but not yet parsed.
    pub fn pending(&self) -> usize {
        self.buf.len()
    }

    pub fn clear(&mut self) {
        self.buf.clear();
    }

    fn skip(&mut self, n: usize) {
        let n = n.min(self.buf.len());
        self.buf.drain(..n);
        self.garbage += n as u64;
    }

    /// The next complete message, or `None` until more bytes arrive.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<Msg> {
        loop {
            // Align on the "ar" preamble.
            match self.buf.windows(2).position(|w| w == b"ar") {
                Some(0) => {}
                Some(i) => self.skip(i),
                None => {
                    // Keep a trailing 'a' that may be the start of "ar".
                    let keep = usize::from(self.buf.last() == Some(&b'a'));
                    let n = self.buf.len() - keep;
                    self.skip(n);
                    return None;
                }
            }
            if self.buf.len() < 3 {
                return None;
            }
            let kind = self.buf[2];
            if kind.is_ascii_digit() {
                if self.buf.len() < 4 {
                    return None;
                }
                let len = self.buf[3] as usize;
                if len == 0 {
                    // A record must at least carry its status byte.
                    self.skip(2);
                    continue;
                }
                if self.buf.len() < 4 + len {
                    return None;
                }
                let rec: Vec<u8> = self.buf.drain(..4 + len).collect();
                return Some(Msg::Frame(RawFrame {
                    proto: kind - b'0',
                    status: rec[4],
                    body: rec[5..].to_vec(),
                }));
            }
            if kind.is_ascii_lowercase() {
                let Some(nl) = self.buf[3..].iter().position(|&b| b == b'\n') else {
                    if self.buf.len() > MAX_LINE {
                        self.skip(2);
                        continue;
                    }
                    return None;
                };
                let line: Vec<u8> = self.buf.drain(..3 + nl + 1).collect();
                let rest = &line[3..3 + nl];
                let text = String::from_utf8_lossy(rest).trim().to_string();
                let fields = text.split_whitespace().map(str::to_string).collect();
                return Some(Msg::Reply(Reply {
                    verb: kind,
                    text,
                    fields,
                }));
            }
            // "ar" followed by something that is neither a digit nor a reply
            // letter: not a message start.
            self.skip(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(proto: u8, status: u8, ts: u32, id: u32, data: &[u8]) -> Vec<u8> {
        let mut body = vec![status];
        body.extend_from_slice(&ts.to_be_bytes());
        body.extend_from_slice(&id.to_be_bytes());
        body.extend_from_slice(data);
        let mut out = vec![b'a', b'r', b'0' + proto, body.len() as u8];
        out.extend(body);
        out
    }

    #[test]
    fn command_lines_match_the_documented_shapes() {
        assert_eq!(numbered("ata", 2, &[]), b"ata 2\r\n");
        assert_eq!(open_channel(5, 0, 500_000), "ato5 0 500000 0");
        assert_eq!(open_channel(5, CAN_29BIT_ID, 250_000), "ato5 256 250000 0");
        assert_eq!(close_channel(5), "atc5");
        assert_eq!(read_pin(PIN_VBAT), "atr 16");
        let (body, payload) = transmit(5, 0x7E0, &[0x02, 0x10, 0x03, 0xAA], 0, 100_000);
        assert_eq!(body, "att5 8 0 100000");
        assert_eq!(payload, vec![0, 0, 0x07, 0xE0, 0x02, 0x10, 0x03, 0xAA]);
        // One write: line, CRLF, then the binary payload.
        let w = numbered(&body, 7, &payload);
        assert!(w.starts_with(b"att5 8 0 100000 7\r\n"));
        assert_eq!(&w[w.len() - 8..], &payload[..]);
    }

    #[test]
    fn filter_lines_carry_txflags_and_one_message_length() {
        let (body, payload) = filter(
            5,
            filter::PASS,
            0,
            &0x7FFu32.to_be_bytes(),
            &0x7E8u32.to_be_bytes(),
            None,
        )
        .unwrap();
        assert_eq!(body, "atf5 1 0 4");
        assert_eq!(payload, vec![0, 0, 0x07, 0xFF, 0, 0, 0x07, 0xE8]);
        // The vendor DLL's ISO 15765 flow-control filter shape (bisak §4).
        let (body, payload) = filter(
            6,
            filter::FLOW_CONTROL,
            ISO15765_FRAME_PAD,
            &[0xFF; 4],
            &0x7E8u32.to_be_bytes(),
            Some(&0x7E0u32.to_be_bytes()),
        )
        .unwrap();
        assert_eq!(body, "atf6 3 64 4");
        assert_eq!(payload.len(), 12);
        // Mismatched lengths, or a flow-control message on the wrong type.
        assert!(filter(5, filter::PASS, 0, &[0; 4], &[0; 3], None).is_none());
        assert!(filter(5, filter::PASS, 0, &[0; 4], &[0; 4], Some(&[0; 4])).is_none());
        assert!(filter(6, filter::FLOW_CONTROL, 0, &[0; 4], &[0; 4], None).is_none());
    }

    #[test]
    fn parses_replies_and_their_sequence_numbers() {
        let mut p = Parser::new();
        p.push(b"aro 2\r\nare 9 3\r\narr 16 12034 4\r\narf5 0 8\r\nari main code version : 1.17.4877\r\n");
        let Some(Msg::Reply(r)) = p.next() else {
            panic!()
        };
        assert_eq!((r.verb, r.seq()), (b'o', Some(2)));
        let Some(Msg::Reply(r)) = p.next() else {
            panic!()
        };
        assert_eq!((r.verb, r.error_code(), r.seq()), (b'e', Some(9), Some(3)));
        let Some(Msg::Reply(r)) = p.next() else {
            panic!()
        };
        assert_eq!(r.fields, vec!["16", "12034", "4"]);
        let Some(Msg::Reply(r)) = p.next() else {
            panic!()
        };
        assert_eq!(
            (r.verb, r.fields.clone()),
            (b'f', vec!["5".into(), "0".into(), "8".into()])
        );
        let Some(Msg::Reply(r)) = p.next() else {
            panic!()
        };
        assert_eq!(r.verb, b'i');
        assert_eq!(r.text, "main code version : 1.17.4877");
        assert!(p.next().is_none());
        assert_eq!(p.garbage, 0);
    }

    #[test]
    fn frame_length_counts_status_timestamp_id_and_data() {
        // The real record from bisak §7.1: a transmit indication for 0x7E0.
        let raw = [
            0x61, 0x72, 0x36, 0x09, 0x10, 0x19, 0x50, 0xb8, 0x0c, 0x00, 0x00, 0x07, 0xe0,
        ];
        let mut p = Parser::new();
        p.push(&raw);
        let Some(Msg::Frame(f)) = p.next() else {
            panic!()
        };
        assert_eq!((f.proto, f.status), (6, status::TX_INDICATION));
        let c = f.can().unwrap();
        assert_eq!(
            (c.id, c.timestamp_us, c.data.len()),
            (0x7E0, 0x1950_b80c, 0)
        );
        // An 8-byte CAN frame: len = 1 + 4 + 4 + 8 = 17.
        let f8 = frame(5, 0, 7, 0x7E8, &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(f8[3], 17);
    }

    #[test]
    fn split_and_merged_reads_parse_identically() {
        let mut stream = Vec::new();
        stream.extend(b"aro 1\r\n");
        // Data bytes that spell "ar" and CR/LF must not mis-frame.
        stream.extend(frame(
            5,
            0,
            1,
            0x7E8,
            &[0x61, 0x72, 0x6f, 0x0d, 0x0a, 0x61, 0x72, 0x00],
        ));
        stream.extend(b"are 7 2\r\n");
        stream.extend(frame(
            5,
            status::ID_29BIT,
            2,
            0x18DA_F110,
            &[0x02, 0x50, 0x03],
        ));
        let mut whole = Parser::new();
        whole.push(&stream);
        let mut expected = Vec::new();
        while let Some(m) = whole.next() {
            expected.push(m);
        }
        assert_eq!(expected.len(), 4);
        for chunk in 1..stream.len() {
            let mut p = Parser::new();
            let mut got = Vec::new();
            for c in stream.chunks(chunk) {
                p.push(c);
                while let Some(m) = p.next() {
                    got.push(m);
                }
            }
            assert_eq!(got, expected, "chunk size {chunk}");
            assert_eq!(p.garbage, 0);
        }
    }

    #[test]
    fn resynchronises_after_garbage() {
        let mut p = Parser::new();
        p.push(b"\x00\xffjunk a");
        assert!(p.next().is_none());
        p.push(b"ro 5\r\n");
        let Some(Msg::Reply(r)) = p.next() else {
            panic!()
        };
        assert_eq!(r.seq(), Some(5));
        assert_eq!(p.garbage, 7);
        // "ar" + an invalid kind byte, then a zero-length record, then a reply.
        p.push(b"ar\x01ar5\x00aro 6\r\n");
        let Some(Msg::Reply(r)) = p.next() else {
            panic!()
        };
        assert_eq!(r.seq(), Some(6));
        // An endless line without LF is abandoned instead of buffering forever.
        let mut p = Parser::new();
        p.push(b"arz");
        p.push(&[b'x'; 300]);
        p.push(b"aro 9\r\n");
        let Some(Msg::Reply(r)) = p.next() else {
            panic!()
        };
        assert_eq!(r.seq(), Some(9));
    }

    #[test]
    fn error_names_follow_j2534() {
        assert_eq!(j2534_error_name(9), "ERR_TIMEOUT");
        assert_eq!(j2534_error_name(20), "ERR_CHANNEL_IN_USE");
        assert_eq!(j2534_error_name(25), "ERR_INVALID_BAUDRATE");
        assert_eq!(j2534_error_name(200), "unknown error");
    }
}
