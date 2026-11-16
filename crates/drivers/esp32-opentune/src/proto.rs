//! USB-CDC wire protocol for the OpenTune ESP32-S3 interface.
//!
//! Frame (both directions), matching `hardware/firmware/esp32-s3`:
//!
//! ```text
//! +------+-----+--------+--------+--------------+------+
//! | 0xB5 | CMD | LEN_HI | LEN_LO | PAYLOAD[LEN] | CRC8 |
//! +------+-----+--------+--------+--------------+------+
//! ```
//!
//! * `CMD` — command id (host→device), or that id with [`REPLY_BIT`] set in a
//!   device reply.
//! * `LEN` — payload length, big-endian u16 (≤ [`MAX_PAYLOAD`]).
//! * `CRC8` — CRC-8 (poly `0x07`, init `0x00`) over `CMD`, `LEN_HI`, `LEN_LO`
//!   and the payload (the `0xB5` sync is excluded), byte-identical with the
//!   firmware's `ot_crc8`.
//!
//! A reply payload is `status (1) || body`, where `status == 0x00` means OK.

use opentune_core::{Error, Result};

pub const SYNC: u8 = 0xB5;
pub const REPLY_BIT: u8 = 0x80;
pub const MAX_PAYLOAD: usize = 4096;
pub const PROTO_VERSION: u8 = 0x01;

/// Command ids (host → device).
pub mod cmd {
    pub const PING: u8 = 0x01;
    pub const CAN_SET_BITRATE: u8 = 0x02;
    pub const CAN_START: u8 = 0x03;
    pub const CAN_STOP: u8 = 0x04;
    pub const CAN_SEND: u8 = 0x05;
    pub const CAN_RECV: u8 = 0x06;
    pub const ISOTP_CONFIG: u8 = 0x07;
    pub const ISOTP_REQUEST: u8 = 0x08;
    pub const KLINE_SET_BAUD: u8 = 0x09;
    pub const KLINE_INIT_FAST: u8 = 0x0A;
    pub const KLINE_INIT_5BAUD: u8 = 0x0B;
    pub const KLINE_TRANSCEIVE: u8 = 0x0C;
    pub const BATTERY: u8 = 0x0D;
    pub const RESET: u8 = 0x0E;
    /// Unsolicited device event, seen on the wire as `0x6E | REPLY_BIT` (0xEE).
    pub const EVENT: u8 = 0x6E;
}

/// Reply status codes (first byte of a reply payload).
pub mod status {
    pub const OK: u8 = 0x00;
    pub const ERR_BAD_CMD: u8 = 0x01;
    pub const ERR_BAD_LEN: u8 = 0x02;
    pub const ERR_CRC: u8 = 0x03;
    pub const ERR_CAN_DOWN: u8 = 0x04;
    pub const ERR_CAN_TX: u8 = 0x05;
    pub const ERR_TIMEOUT: u8 = 0x06;
    pub const ERR_ISOTP: u8 = 0x07;
    pub const ERR_KLINE_INIT: u8 = 0x08;
    pub const ERR_KLINE_NODATA: u8 = 0x09;
    pub const ERR_BUSY: u8 = 0x0A;
    pub const ERR_UNSUPPORTED: u8 = 0x0B;
    pub const ERR_OVERFLOW: u8 = 0x0C;
}

/// Capability bits reported by [`cmd::PING`].
pub mod caps {
    pub const CAN: u8 = 0x01;
    pub const ISOTP: u8 = 0x02;
    pub const KLINE: u8 = 0x04;
    pub const BATTERY: u8 = 0x08;
}

/// CRC-8 (poly `0x07`, init `0x00`, no reflection). Matches the firmware.
pub fn crc8(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &b in data {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x07
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// Encode a request frame: `SYNC, CMD, LEN(2 BE), PAYLOAD, CRC8`.
pub fn encode_frame(cmd: u8, payload: &[u8]) -> Vec<u8> {
    debug_assert!(payload.len() <= MAX_PAYLOAD);
    let len = payload.len() as u16;
    let mut body = Vec::with_capacity(3 + payload.len());
    body.push(cmd);
    body.push((len >> 8) as u8);
    body.push((len & 0xFF) as u8);
    body.extend_from_slice(payload);
    let crc = crc8(&body);

    let mut frame = Vec::with_capacity(1 + body.len() + 1);
    frame.push(SYNC);
    frame.extend_from_slice(&body);
    frame.push(crc);
    frame
}

/// A decoded frame: the (reply-masked) command id and its payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub cmd: u8,
    pub payload: Vec<u8>,
}

impl Frame {
    /// True if this is a device→host reply (reply bit set) rather than an event.
    pub fn is_reply(&self) -> bool {
        self.cmd & REPLY_BIT != 0 && (self.cmd & !REPLY_BIT) != cmd::EVENT
    }
    /// The underlying command id with the reply bit stripped.
    pub fn base_cmd(&self) -> u8 {
        self.cmd & !REPLY_BIT
    }
}

/// Parse one whole frame buffer (`SYNC .. CRC`), validating sync and CRC.
pub fn parse_frame(buf: &[u8]) -> Result<Frame> {
    if buf.len() < 5 {
        return Err(Error::Protocol("frame too short".into()));
    }
    if buf[0] != SYNC {
        return Err(Error::Protocol("bad frame sync".into()));
    }
    let len = ((buf[1 + 1] as usize) << 8) | buf[1 + 2] as usize; // LEN after CMD
    let cmd = buf[1];
    let expected_total = 1 + 3 + len + 1;
    if buf.len() != expected_total {
        return Err(Error::Protocol(format!(
            "frame length mismatch: payload {len}, buffer {}",
            buf.len()
        )));
    }
    let body = &buf[1..1 + 3 + len]; // CMD, LEN_HI, LEN_LO, payload
    let crc = buf[expected_total - 1];
    if crc8(body) != crc {
        return Err(Error::Protocol("frame CRC mismatch".into()));
    }
    Ok(Frame {
        cmd,
        payload: buf[4..4 + len].to_vec(),
    })
}

/// Split a reply payload into `(status, body)`.
pub fn split_status(payload: &[u8]) -> Result<(u8, &[u8])> {
    match payload.split_first() {
        Some((s, rest)) => Ok((*s, rest)),
        None => Err(Error::Protocol("empty reply payload".into())),
    }
}

/// Map a non-OK device status byte to a human error.
pub fn status_error(status: u8) -> Error {
    let name = match status {
        status::ERR_BAD_CMD => "bad command",
        status::ERR_BAD_LEN => "bad length",
        status::ERR_CRC => "CRC error",
        status::ERR_CAN_DOWN => "CAN not started",
        status::ERR_CAN_TX => "CAN transmit failed",
        status::ERR_TIMEOUT => "device timeout",
        status::ERR_ISOTP => "ISO-TP error",
        status::ERR_KLINE_INIT => "K-line init failed",
        status::ERR_KLINE_NODATA => "no K-line response",
        status::ERR_BUSY => "device busy",
        status::ERR_UNSUPPORTED => "unsupported",
        status::ERR_OVERFLOW => "buffer overflow",
        _ => "unknown device error",
    };
    if status == status::ERR_TIMEOUT || status == status::ERR_KLINE_NODATA {
        return Error::Timeout(0);
    }
    Error::Protocol(format!("device: {name} (0x{status:02X})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc8_known_vectors() {
        // CRC-8/poly-0x07, init 0x00.
        assert_eq!(crc8(&[]), 0x00);
        assert_eq!(crc8(&[0x00]), 0x00);
        // "123456789" check value for CRC-8 (SMBus, poly 0x07) is 0xF4.
        assert_eq!(crc8(b"123456789"), 0xF4);
    }

    #[test]
    fn encode_round_trips_through_parse() {
        let payload = [0x10u8, 0x20, 0x30, 0x40];
        let frame = encode_frame(cmd::ISOTP_REQUEST, &payload);
        assert_eq!(frame[0], SYNC);
        let parsed = parse_frame(&frame).unwrap();
        assert_eq!(parsed.cmd, cmd::ISOTP_REQUEST);
        assert_eq!(parsed.payload, payload);
    }

    #[test]
    fn encode_empty_payload() {
        let frame = encode_frame(cmd::PING, &[]);
        // SYNC, CMD, LEN_HI=0, LEN_LO=0, CRC
        assert_eq!(frame.len(), 5);
        let parsed = parse_frame(&frame).unwrap();
        assert_eq!(parsed.cmd, cmd::PING);
        assert!(parsed.payload.is_empty());
    }

    #[test]
    fn parse_detects_crc_error() {
        let mut frame = encode_frame(cmd::CAN_SEND, &[1, 2, 3]);
        let last = frame.len() - 1;
        frame[last] ^= 0xFF; // corrupt CRC
        assert!(matches!(parse_frame(&frame), Err(Error::Protocol(_))));
    }

    #[test]
    fn parse_detects_bad_sync() {
        let mut frame = encode_frame(cmd::CAN_SEND, &[1, 2, 3]);
        frame[0] = 0x00;
        assert!(matches!(parse_frame(&frame), Err(Error::Protocol(_))));
    }

    #[test]
    fn parse_detects_length_mismatch() {
        let mut frame = encode_frame(cmd::CAN_SEND, &[1, 2, 3]);
        frame.push(0x00); // trailing junk -> length mismatch
        assert!(matches!(parse_frame(&frame), Err(Error::Protocol(_))));
    }

    #[test]
    fn reply_bit_detection() {
        let f = Frame {
            cmd: cmd::PING | REPLY_BIT,
            payload: vec![status::OK],
        };
        assert!(f.is_reply());
        assert_eq!(f.base_cmd(), cmd::PING);

        let event = Frame {
            cmd: cmd::EVENT | REPLY_BIT, // 0xEE on the wire
            payload: vec![],
        };
        assert!(!event.is_reply());
    }

    #[test]
    fn split_status_works() {
        let (s, body) = split_status(&[status::OK, 1, 2, 3]).unwrap();
        assert_eq!(s, status::OK);
        assert_eq!(body, &[1, 2, 3]);
        assert!(split_status(&[]).is_err());
    }

    #[test]
    fn status_error_maps_timeout() {
        assert!(matches!(
            status_error(status::ERR_TIMEOUT),
            Error::Timeout(_)
        ));
        assert!(matches!(
            status_error(status::ERR_CAN_TX),
            Error::Protocol(_)
        ));
    }

    #[test]
    fn large_payload_length_encodes_big_endian() {
        let payload = vec![0xAB; 300];
        let frame = encode_frame(cmd::ISOTP_REQUEST, &payload);
        assert_eq!(frame[2], (300u16 >> 8) as u8); // LEN_HI = 1
        assert_eq!(frame[3], (300u16 & 0xFF) as u8); // LEN_LO = 44
        let parsed = parse_frame(&frame).unwrap();
        assert_eq!(parsed.payload.len(), 300);
    }
}
