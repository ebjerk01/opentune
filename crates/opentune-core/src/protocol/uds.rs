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

    /// `DiagnosticSessionControl` (0x10). ISO 14229 §9.2: the positive response
    /// echoes the requested session byte (then, for 2013+ ECUs, the P2/P2* server
    /// timing). The echo is verified — a mismatch is a confused ECU, which
    /// `python-udsoncan` surfaces as an `UnexpectedResponseException`.
    pub fn session(&mut self, session: u8) -> Result<Vec<u8>> {
        let resp = self.call(&[sid::SESSION_CONTROL, session])?;
        check_subfunction_echo(sid::SESSION_CONTROL, session, &resp)?;
        Ok(resp)
    }

    /// `ECUReset` (0x11). ISO 14229 §9.3: the positive response echoes the reset
    /// type (and, for `enableRapidPowerShutDown`, a powerDownTime byte). The
    /// reset-type echo is verified.
    pub fn ecu_reset(&mut self, kind: u8) -> Result<Vec<u8>> {
        let resp = self.call(&[sid::ECU_RESET, kind])?;
        check_subfunction_echo(sid::ECU_RESET, kind, &resp)?;
        Ok(resp)
    }

    pub fn tester_present(&mut self) -> Result<()> {
        self.call(&[sid::TESTER_PRESENT, 0x00]).map(|_| ())
    }

    /// `ReadDataByIdentifier` (0x22) for a single DID. ISO 14229 §10.2: the
    /// positive response echoes the 2-byte DID before the record. The echo is
    /// verified against the request (udsoncan raises on a DID mismatch) so a
    /// mis-routed answer is not silently returned as the wrong parameter's data.
    pub fn read_data_by_id(&mut self, did: u16) -> Result<Vec<u8>> {
        let resp = self.call(&[sid::READ_DATA_BY_ID, (did >> 8) as u8, did as u8])?;
        // resp echoes the 2-byte DID, then the data.
        if resp.len() < 2 {
            return Err(Error::Protocol("short RDBI response".into()));
        }
        let echoed = u16::from_be_bytes([resp[0], resp[1]]);
        if echoed != did {
            return Err(Error::Protocol(format!(
                "RDBI echoed DID 0x{echoed:04X} != requested 0x{did:04X}"
            )));
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
        // The response echoes the requested security-access level (ISO 14229
        // §10.4); verify it so a reply to a different level is not mistaken for
        // this level's seed. udsoncan normalises odd=RequestSeed / even=SendKey
        // and checks this echo the same way.
        check_subfunction_echo(sid::SECURITY_ACCESS, level, &resp)?;
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
            Ok(resp) => {
                // The positive response echoes the (even) send-key level.
                check_subfunction_echo(sid::SECURITY_ACCESS, level, &resp)?;
                Ok(())
            }
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

/// Verify a sub-function service's positive response echoes the requested
/// sub-function (ISO 14229 §8.6.3). The server clears bit 7 (the
/// suppressPositiveResponse bit) in the echo, so compare the low 7 bits. `resp`
/// is the response body *after* the service id, whose first byte is the echo.
fn check_subfunction_echo(service: u8, requested: u8, resp: &[u8]) -> Result<()> {
    match resp.first() {
        Some(&echo) if echo & 0x7F == requested & 0x7F => Ok(()),
        Some(&echo) => Err(Error::Protocol(format!(
            "service 0x{service:02X}: echoed sub-function 0x{echo:02X} != requested 0x{requested:02X}"
        ))),
        None => Err(Error::Protocol(format!(
            "service 0x{service:02X}: response missing sub-function echo"
        ))),
    }
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
    use crate::protocol::diag::DiagTransport;
    use std::collections::VecDeque;

    /// A scripted request/response transport for conformance tests: returns
    /// queued responses in order. Mirrors the one in the integration tests but
    /// stays local so these unit tests need no external harness.
    struct Script {
        responses: VecDeque<Vec<u8>>,
    }
    impl Script {
        fn new(r: Vec<Vec<u8>>) -> Self {
            Self {
                responses: VecDeque::from(r),
            }
        }
    }
    impl DiagTransport for Script {
        fn request(&mut self, _req: &[u8], timeout_ms: u64) -> Result<Vec<u8>> {
            self.responses.pop_front().ok_or(Error::Timeout(timeout_ms))
        }
    }

    #[test]
    fn rdbi_accepts_matching_did_echo() {
        // 62 F1 90 'V' 'I' 'N' -> echoed DID F190 matches.
        let mut t = Script::new(vec![vec![0x62, 0xF1, 0x90, b'V', b'I', b'N']]);
        let mut uds = UdsClient::new(&mut t);
        assert_eq!(uds.read_data_by_id(0xF190).unwrap(), b"VIN");
    }

    #[test]
    fn rdbi_rejects_mismatched_did_echo() {
        // ECU echoes DID F18C though F190 was asked: must not be returned as data.
        let mut t = Script::new(vec![vec![0x62, 0xF1, 0x8C, 0x00]]);
        let mut uds = UdsClient::new(&mut t);
        assert!(matches!(
            uds.read_data_by_id(0xF190),
            Err(Error::Protocol(_))
        ));
    }

    #[test]
    fn session_verifies_subfunction_echo() {
        // 50 03 00 32 01 F4 -> echoes extendedDiagnosticSession (03) + P2 timing.
        let mut t = Script::new(vec![vec![0x50, 0x03, 0x00, 0x32, 0x01, 0xF4]]);
        let mut uds = UdsClient::new(&mut t);
        assert!(uds.session(0x03).is_ok());

        // Wrong session echo (01) to a request for 03 is rejected.
        let mut t2 = Script::new(vec![vec![0x50, 0x01]]);
        let mut uds2 = UdsClient::new(&mut t2);
        assert!(matches!(uds2.session(0x03), Err(Error::Protocol(_))));
    }

    #[test]
    fn request_seed_verifies_level_echo() {
        // 67 01 11 22 33 44 -> level 01 echoed, seed follows.
        let mut t = Script::new(vec![vec![0x67, 0x01, 0x11, 0x22, 0x33, 0x44]]);
        let mut uds = UdsClient::new(&mut t);
        assert_eq!(
            uds.request_seed(0x01).unwrap(),
            vec![0x11, 0x22, 0x33, 0x44]
        );

        // A reply echoing a different level (03) to a request for 01 is rejected.
        let mut t2 = Script::new(vec![vec![0x67, 0x03, 0xAA]]);
        let mut uds2 = UdsClient::new(&mut t2);
        assert!(matches!(uds2.request_seed(0x01), Err(Error::Protocol(_))));
    }

    #[test]
    fn subfunction_echo_ignores_suppress_bit() {
        // An ECU may clear only bit 7; the low 7 bits must still match.
        assert!(check_subfunction_echo(0x10, 0x03, &[0x03]).is_ok());
        assert!(check_subfunction_echo(0x10, 0x03, &[0x04]).is_err());
        assert!(check_subfunction_echo(0x10, 0x03, &[]).is_err());
    }

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
