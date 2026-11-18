//! The CRC32 "msEnvelope_1.0" framing used by Speeduino (new comms), rusEFI
//! and MS3 firmware:
//!
//! ```text
//! request:  [len: u16 BE] [payload: len bytes]            [crc32(payload): u32 BE]
//! response: [len: u16 BE] [status: u8] [data: len-1 bytes] [crc32(status+data): u32 BE]
//! ```
//!
//! Status codes below 0x80 are success; 0x80 and above are errors.
use crate::crc::crc32;
use crate::port::{recv_exact, Port};
use crate::ProtoError;
use std::time::Duration;

pub const STATUS_OK: u8 = 0x00;
pub const STATUS_REALTIME_OK: u8 = 0x01;
pub const STATUS_PAGE_OK: u8 = 0x02;
pub const STATUS_CONFIG_ERROR: u8 = 0x03;
pub const STATUS_BURN_OK: u8 = 0x04;
pub const STATUS_UNDERRUN: u8 = 0x80;
pub const STATUS_OVERRUN: u8 = 0x81;
pub const STATUS_CRC_FAILURE: u8 = 0x82;
pub const STATUS_UNRECOGNIZED: u8 = 0x83;
pub const STATUS_OUT_OF_RANGE: u8 = 0x84;
pub const STATUS_BUSY: u8 = 0x85;
pub const STATUS_FRAMING_ERROR: u8 = 0x8D;

/// Human name of a status byte.
pub fn status_name(code: u8) -> &'static str {
    match code {
        STATUS_OK => "OK",
        STATUS_REALTIME_OK => "realtime OK",
        STATUS_PAGE_OK => "page OK",
        STATUS_CONFIG_ERROR => "configuration error",
        STATUS_BURN_OK => "burn OK",
        STATUS_UNDERRUN => "underrun / timeout (request incomplete)",
        STATUS_OVERRUN => "overrun",
        STATUS_CRC_FAILURE => "CRC failure (request corrupted)",
        STATUS_UNRECOGNIZED => "unrecognized command",
        STATUS_OUT_OF_RANGE => "out of range",
        STATUS_BUSY => "busy",
        STATUS_FRAMING_ERROR => "framing error",
        c if c >= 0x80 => "error",
        _ => "success",
    }
}

/// Wrap a payload for sending.
pub fn wrap(payload: &[u8]) -> Result<Vec<u8>, ProtoError> {
    let len = u16::try_from(payload.len()).map_err(|_| {
        ProtoError::Frame(format!("payload of {} bytes is too long", payload.len()))
    })?;
    if len == 0 {
        return Err(ProtoError::Frame("empty payload".into()));
    }
    let mut out = Vec::with_capacity(payload.len() + 6);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(payload);
    out.extend_from_slice(&crc32(payload).to_be_bytes());
    Ok(out)
}

/// Split a complete wire frame into its payload, checking length and CRC.
pub fn unwrap(frame: &[u8]) -> Result<Vec<u8>, ProtoError> {
    if frame.len() < 2 {
        return Err(ProtoError::Frame("shorter than the length header".into()));
    }
    let len = u16::from_be_bytes([frame[0], frame[1]]) as usize;
    if frame.len() != len + 6 {
        return Err(ProtoError::Frame(format!(
            "length header says {len}, frame carries {}",
            frame.len().saturating_sub(6)
        )));
    }
    let payload = &frame[2..2 + len];
    let want = u32::from_be_bytes(frame[2 + len..].try_into().expect("4 bytes"));
    let got = crc32(payload);
    if want != got {
        return Err(ProtoError::Crc {
            expected: want,
            actual: got,
        });
    }
    Ok(payload.to_vec())
}

/// Read one response frame; returns its payload (status byte first).
pub fn read_frame(
    port: &mut dyn Port,
    timeout: Duration,
    what: &str,
) -> Result<Vec<u8>, ProtoError> {
    let mut hdr = [0u8; 2];
    let n = recv_exact(port, &mut hdr, timeout)?;
    if n < 2 {
        return Err(ProtoError::Timeout {
            what: what.into(),
            got: n,
            expected: 2,
        });
    }
    let len = u16::from_be_bytes(hdr) as usize;
    if len == 0 {
        return Err(ProtoError::Frame("zero-length response".into()));
    }
    let mut rest = vec![0u8; len + 4];
    let got = recv_exact(port, &mut rest, timeout)?;
    if got < rest.len() {
        return Err(ProtoError::Timeout {
            what: what.into(),
            got: got + 2,
            expected: len + 6,
        });
    }
    let mut frame = hdr.to_vec();
    frame.extend_from_slice(&rest);
    unwrap(&frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_unwrap() {
        let f = wrap(b"Q").unwrap();
        assert_eq!(&f[..3], &[0, 1, b'Q']);
        assert_eq!(&f[3..], &crc32(b"Q").to_be_bytes());
        assert_eq!(unwrap(&f).unwrap(), b"Q");
        let mut bad = f.clone();
        bad[3] ^= 1;
        assert!(matches!(unwrap(&bad), Err(ProtoError::Crc { .. })));
        assert!(matches!(unwrap(&f[..5]), Err(ProtoError::Frame(_))));
        assert!(wrap(&[]).is_err());
        assert_eq!(status_name(0x82), "CRC failure (request corrupted)");
    }
}
