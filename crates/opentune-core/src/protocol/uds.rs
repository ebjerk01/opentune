//! UDS (ISO 14229) service client over a [`DiagTransport`].
//!
//! Each method sends a request and returns the response payload *after* the
//! positive-response service id. Negative responses become [`Error`]s.
use crate::protocol::diag::{check_response, DiagTransport};
use crate::{Error, Result};

pub mod sid {
    pub const SESSION_CONTROL: u8 = 0x10;
    pub const ECU_RESET: u8 = 0x11;
    pub const SECURITY_ACCESS: u8 = 0x27;
    pub const READ_DATA_BY_ID: u8 = 0x22;
    pub const WRITE_DATA_BY_ID: u8 = 0x2E;
    pub const ROUTINE_CONTROL: u8 = 0x31;
    pub const REQUEST_DOWNLOAD: u8 = 0x34;
    pub const REQUEST_UPLOAD: u8 = 0x35;
    pub const TRANSFER_DATA: u8 = 0x36;
    pub const TRANSFER_EXIT: u8 = 0x37;
    pub const TESTER_PRESENT: u8 = 0x3E;
}

pub struct UdsClient<'a> {
    pub transport: &'a mut dyn DiagTransport,
    pub timeout_ms: u64,
}

impl<'a> UdsClient<'a> {
    pub fn new(transport: &'a mut dyn DiagTransport) -> Self {
        Self {
            transport,
            timeout_ms: 2000,
        }
    }

    fn call(&mut self, req: &[u8]) -> Result<Vec<u8>> {
        let resp = self.transport.request(req, self.timeout_ms)?;
        check_response(req[0], &resp)?;
        Ok(resp[1..].to_vec())
    }

    pub fn session(&mut self, session: u8) -> Result<Vec<u8>> {
        self.call(&[sid::SESSION_CONTROL, session])
    }

    pub fn ecu_reset(&mut self, kind: u8) -> Result<Vec<u8>> {
        self.call(&[sid::ECU_RESET, kind])
    }

    pub fn tester_present(&mut self) -> Result<()> {
        self.call(&[sid::TESTER_PRESENT, 0x00]).map(|_| ())
    }

    pub fn read_data_by_id(&mut self, did: u16) -> Result<Vec<u8>> {
        let resp = self.call(&[sid::READ_DATA_BY_ID, (did >> 8) as u8, did as u8])?;
        // resp echoes the 2-byte DID, then the data.
        if resp.len() < 2 {
            return Err(Error::Protocol("short RDBI response".into()));
        }
        Ok(resp[2..].to_vec())
    }

    /// Request a seed at an odd access level (e.g. 0x11).
    ///
    /// A seed of all-zero bytes is the ISO 14229 convention for "already
    /// unlocked" — call [`seed_is_unlocked`] on the result to detect it and skip
    /// the key exchange. Per §9.4.5 the ECU may answer 0x37
    /// (`requiredTimeDelayNotExpired`) if a prior attempt failed and the delay
    /// timer has not elapsed; that surfaces as [`Error::NegativeResponse`] so the
    /// caller can back off.
    pub fn request_seed(&mut self, level: u8) -> Result<Vec<u8>> {
        let resp = self.call(&[sid::SECURITY_ACCESS, level])?;
        if resp.is_empty() {
            return Err(Error::Protocol("empty seed".into()));
        }
        Ok(resp[1..].to_vec()) // drop the echoed level
    }

    /// Send a key at an even access level (seed level + 1).
    ///
    /// The ECU enforces an attempt counter: once it is exhausted it answers 0x36
    /// (`exceededNumberOfAttempts`) and starts the delay timer. Both an invalid
    /// key (0x35) and an exhausted counter (0x36) map to [`Error::SecurityDenied`]
    /// so the caller treats them as a failed unlock; 0x37 stays a raw negative
    /// response so a retry can honor the delay.
    pub fn send_key(&mut self, level: u8, key: &[u8]) -> Result<()> {
        let mut req = vec![sid::SECURITY_ACCESS, level];
        req.extend_from_slice(key);
        match self.call(&req) {
            Ok(_) => Ok(()),
            Err(Error::NegativeResponse {
                code: 0x35 | 0x36, ..
            }) => Err(Error::SecurityDenied),
            Err(e) => Err(e),
        }
    }

    pub fn routine_control(&mut self, kind: u8, routine: u16, data: &[u8]) -> Result<Vec<u8>> {
        let mut req = vec![
            sid::ROUTINE_CONTROL,
            kind,
            (routine >> 8) as u8,
            routine as u8,
        ];
        req.extend_from_slice(data);
        self.call(&req)
    }

    /// `requestDownload`: returns the ECU's maximum block length.
    pub fn request_download(&mut self, data_format: u8, address: u32, size: u32) -> Result<usize> {
        let mut req = vec![sid::REQUEST_DOWNLOAD, data_format, 0x44];
        req.extend_from_slice(&address.to_be_bytes());
        req.extend_from_slice(&size.to_be_bytes());
        let resp = self.call(&req)?;
        parse_max_block_len(&resp)
    }

    pub fn request_upload(&mut self, data_format: u8, address: u32, size: u32) -> Result<usize> {
        let mut req = vec![sid::REQUEST_UPLOAD, data_format, 0x44];
        req.extend_from_slice(&address.to_be_bytes());
        req.extend_from_slice(&size.to_be_bytes());
        let resp = self.call(&req)?;
        parse_max_block_len(&resp)
    }

    /// `transferData` with a block sequence counter.
    ///
    /// Per ISO 14229 §14.3 the `blockSequenceCounter` starts at 0x01 for the
    /// first block after `requestDownload`/`requestUpload`, increments by one per
    /// block and wraps 0xFF→0x00. A mismatch is answered with NRC 0x73
    /// (`wrongBlockSequenceCounter`); re-sending the same counter is the defined
    /// way to retransmit a lost block. The caller owns the counter — see
    /// [`next_block_seq`] for the wrap.
    pub fn transfer_data(&mut self, seq: u8, block: &[u8]) -> Result<Vec<u8>> {
        let mut req = vec![sid::TRANSFER_DATA, seq];
        req.extend_from_slice(block);
        self.call(&req)
    }

    pub fn transfer_exit(&mut self) -> Result<Vec<u8>> {
        self.call(&[sid::TRANSFER_EXIT])
    }
}

/// True if a security-access seed is all-zero, the ISO 14229 convention for an
/// ECU that is already unlocked at that level (skip the key exchange).
pub fn seed_is_unlocked(seed: &[u8]) -> bool {
    !seed.is_empty() && seed.iter().all(|&b| b == 0)
}

/// Next `transferData` block sequence counter, wrapping 0xFF→0x00.
pub fn next_block_seq(seq: u8) -> u8 {
    seq.wrapping_add(1)
}

fn parse_max_block_len(resp: &[u8]) -> Result<usize> {
    // lengthFormatIdentifier high nibble = number of maxNumberOfBlockLength bytes.
    if resp.is_empty() {
        return Err(Error::Protocol("short requestDownload response".into()));
    }
    let n = (resp[0] >> 4) as usize;
    if resp.len() < 1 + n || n == 0 {
        return Err(Error::Protocol("bad maxNumberOfBlockLength".into()));
    }
    let mut v = 0usize;
    for b in &resp[1..1 + n] {
        v = (v << 8) | *b as usize;
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlocked_seed_detection() {
        assert!(seed_is_unlocked(&[0, 0, 0, 0]));
        assert!(!seed_is_unlocked(&[0, 0, 1, 0]));
        assert!(!seed_is_unlocked(&[])); // no seed is not "unlocked"
    }

    #[test]
    fn block_seq_wraps() {
        assert_eq!(next_block_seq(0x01), 0x02);
        assert_eq!(next_block_seq(0xFF), 0x00);
    }

    #[test]
    fn max_block_len_parsed() {
        // lengthFormatIdentifier 0x20 -> 2 length bytes: 0x0F 0xFA = 4090.
        assert_eq!(parse_max_block_len(&[0x20, 0x0F, 0xFA]).unwrap(), 4090);
        assert!(parse_max_block_len(&[0x20, 0x0F]).is_err()); // short
        assert!(parse_max_block_len(&[0x00]).is_err()); // n == 0
    }
}
