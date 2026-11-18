//! UDS (ISO 14229) service client over a [`DiagTransport`].
//!
//! Each method sends a request and returns the response payload *after* the
//! positive-response service id. Negative responses become [`Error`]s.
//!
//! ## Timing model (ISO 14229-2 §7)
//!
//! The client carries an explicit P2 / P2\* / total-request budget rather than a
//! single opaque timeout:
//!
//! - **P2_client** ([`UdsClient::timeout_ms`]) — how long to wait for the ECU's
//!   first (or only) response to a request.
//! - **P2\*_client** ([`UdsClient::p2_star_ms`]) — the longer per-response budget
//!   that applies once the ECU has answered NRC 0x78 (`responsePending`); used
//!   for the long-running programming services.
//! - **Total request budget** ([`UdsClient::total_timeout_ms`]) — an absolute
//!   monotonic cap on one service exchange. A chatty ECU (or a transport looping
//!   on 0x78) can extend each individual response under P2\*, but the per-request
//!   wait is clamped so interleaved traffic can never push a single exchange past
//!   this ceiling.
//!
//! The client issues every request **exactly once** and never auto-retries, so a
//! write (download / transferData) is never silently duplicated. The NRC 0x78
//! receive loop belongs to the [`DiagTransport`] implementation (which re-reads
//! without re-sending); if a 0x78 reaches this layer the client surfaces it
//! rather than re-sending the request.
use crate::protocol::diag::{check_response, DiagTransport};
use crate::{Error, Result};
use std::time::{Duration, Instant};

/// Default **P2_client** (ISO 14229-2): the window for the ECU's first response.
pub const P2_CLIENT_MS: u64 = 2000;

/// Default **P2\*_client** (ISO 14229-2): the extended per-response window that
/// applies after the ECU answers NRC 0x78 (`responsePending`). Used for the
/// long-running programming services (routineControl / request(Download|Upload) /
/// transferData / transferExit).
pub const P2_STAR_CLIENT_MS: u64 = 5000;

/// Default absolute cap on one UDS service exchange. No single request waits
/// longer than this regardless of how many `responsePending` replies arrive, so
/// a misbehaving ECU cannot reset the wait indefinitely.
pub const TOTAL_REQUEST_TIMEOUT_MS: u64 = 60_000;

pub mod sid {
    pub const SESSION_CONTROL: u8 = 0x10;
    pub const ECU_RESET: u8 = 0x11;
    pub const SECURITY_ACCESS: u8 = 0x27;
    pub const READ_DATA_BY_ID: u8 = 0x22;
    pub const READ_MEMORY_BY_ADDRESS: u8 = 0x23;
    pub const WRITE_DATA_BY_ID: u8 = 0x2E;
    pub const ROUTINE_CONTROL: u8 = 0x31;
    pub const REQUEST_DOWNLOAD: u8 = 0x34;
    pub const REQUEST_UPLOAD: u8 = 0x35;
    pub const TRANSFER_DATA: u8 = 0x36;
    pub const TRANSFER_EXIT: u8 = 0x37;
    pub const TESTER_PRESENT: u8 = 0x3E;
}

/// A UDS (ISO 14229) service client bound to a [`DiagTransport`]. See the module
/// docs for the P2 / P2\* / total-request timing model.
pub struct UdsClient<'a> {
    /// The request/response transport (ISO-TP, K-line, DoIP, …).
    pub transport: &'a mut dyn DiagTransport,
    /// **P2_client**: the budget for a quick request's response. Named
    /// `timeout_ms` for backward compatibility; it is the P2 base timeout.
    pub timeout_ms: u64,
    /// **P2\*_client**: the extended per-response budget used by the programming
    /// services, which legitimately answer NRC 0x78 (`responsePending`) and take
    /// longer to complete.
    pub p2_star_ms: u64,
    /// Absolute cap on one service exchange (see [`TOTAL_REQUEST_TIMEOUT_MS`]).
    pub total_timeout_ms: u64,
}

impl<'a> UdsClient<'a> {
    /// Build a client with the default P2 / P2\* / total-request budgets.
    pub fn new(transport: &'a mut dyn DiagTransport) -> Self {
        Self {
            transport,
            timeout_ms: P2_CLIENT_MS,
            p2_star_ms: P2_STAR_CLIENT_MS,
            total_timeout_ms: TOTAL_REQUEST_TIMEOUT_MS,
        }
    }

    /// Send a quick request under the **P2_client** budget.
    fn call(&mut self, req: &[u8]) -> Result<Vec<u8>> {
        self.call_within(req, self.timeout_ms)
    }

    /// Send a long-running programming request under the **P2\*_client** budget.
    /// These services may answer NRC 0x78 while the ECU works; the transport
    /// keeps receiving under this budget without re-sending the request.
    fn call_pending(&mut self, req: &[u8]) -> Result<Vec<u8>> {
        self.call_within(req, self.p2_star_ms)
    }

    /// Issue one request (exactly once) with a per-response budget clamped to the
    /// remaining total-request budget, then validate the response's service id.
    ///
    /// The absolute deadline is captured from a monotonic clock so that, however
    /// the transport loops on `responsePending`, a single exchange can never wait
    /// past [`UdsClient::total_timeout_ms`]. The request is never re-sent here.
    fn call_within(&mut self, req: &[u8], budget_ms: u64) -> Result<Vec<u8>> {
        let deadline = Instant::now() + Duration::from_millis(self.total_timeout_ms);
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(u64::MAX as u128) as u64;
        if remaining == 0 {
            return Err(Error::Timeout(self.total_timeout_ms));
        }
        let wait = budget_ms.clamp(1, remaining);
        let resp = self.transport.request(req, wait)?;
        // A 0x78 that reaches this layer means the transport did not resolve the
        // responsePending itself; surface it (do NOT re-send — that would
        // duplicate a write).
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
    ///
    /// Use [`send_key_detailed`](Self::send_key_detailed) to keep the ECU's
    /// exact NRC instead.
    pub fn send_key(&mut self, level: u8, key: &[u8]) -> Result<()> {
        match self.send_key_detailed(level, key) {
            Err(Error::NegativeResponse {
                code: 0x35 | 0x36, ..
            }) => Err(Error::SecurityDenied),
            other => other,
        }
    }

    /// [`send_key`](Self::send_key) without folding the refusal: an invalid
    /// key (0x35), exhausted attempts (0x36) and an unexpired delay (0x37) all
    /// come back as [`Error::NegativeResponse`] with the ECU's NRC. Classify
    /// one with [`KeyRejection::of`].
    pub fn send_key_detailed(&mut self, level: u8, key: &[u8]) -> Result<()> {
        let mut req = vec![sid::SECURITY_ACCESS, level];
        req.extend_from_slice(key);
        let resp = self.call(&req)?;
        // The positive response echoes the (even) send-key level.
        check_subfunction_echo(sid::SECURITY_ACCESS, level, &resp)
    }

    /// `RoutineControl` (0x31). ISO 14229 §9.9: the positive response echoes the
    /// `routineControlType` sub-function and the 2-byte `routineIdentifier`
    /// before any (optional) `routineStatusRecord`. Both echoes are verified and
    /// a response too short to carry them is rejected — the optional status data
    /// must not excuse a missing echo correlation, so a routine result is never
    /// mistaken for a different routine's. Returns the `routineStatusRecord`
    /// (which may be empty).
    pub fn routine_control(&mut self, kind: u8, routine: u16, data: &[u8]) -> Result<Vec<u8>> {
        let mut req = vec![
            sid::ROUTINE_CONTROL,
            kind,
            (routine >> 8) as u8,
            routine as u8,
        ];
        req.extend_from_slice(data);
        let resp = self.call_pending(&req)?;
        // resp is the body after the 0x71 SID: controlType + routineId(2) + status.
        if resp.len() < 3 {
            return Err(Error::Protocol(
                "truncated routineControl response: missing control-type / routine-id echo".into(),
            ));
        }
        // The server clears bit 7 (suppressPositiveResponse) in the echoed type.
        if resp[0] & 0x7F != kind & 0x7F {
            return Err(Error::Protocol(format!(
                "routineControl echoed control type 0x{:02X} != requested 0x{:02X}",
                resp[0], kind
            )));
        }
        let echoed = u16::from_be_bytes([resp[1], resp[2]]);
        if echoed != routine {
            return Err(Error::Protocol(format!(
                "routineControl echoed routine id 0x{echoed:04X} != requested 0x{routine:04X}"
            )));
        }
        Ok(resp[3..].to_vec())
    }

    /// `requestDownload` (0x34): returns the ECU's maximum block length. The
    /// 0x74 positive response's `lengthFormatIdentifier` + `maxNumberOfBlockLength`
    /// shape is validated and a truncated response is rejected.
    pub fn request_download(&mut self, data_format: u8, address: u32, size: u32) -> Result<usize> {
        self.request_download_with_format(data_format, 0x44, address, size)
    }

    /// `requestDownload` (0x34) with an explicit `addressAndLengthFormatIdentifier`
    /// (high nibble = memorySize bytes, low nibble = memoryAddress bytes). A
    /// compact format (e.g. `0x22`) keeps the request short enough for adapters
    /// that can only transmit a single ISO-TP frame. See
    /// [`encode_address_and_length`].
    pub fn request_download_with_format(
        &mut self,
        data_format: u8,
        alfid: u8,
        address: u32,
        size: u32,
    ) -> Result<usize> {
        let mut req = vec![sid::REQUEST_DOWNLOAD, data_format, alfid];
        req.extend(encode_address_and_length(alfid, address, size)?);
        let resp = self.call_pending(&req)?;
        parse_max_block_len(&resp)
    }

    /// `requestUpload` (0x35): returns the ECU's maximum block length. The 0x75
    /// positive response shape is validated like [`request_download`].
    pub fn request_upload(&mut self, data_format: u8, address: u32, size: u32) -> Result<usize> {
        self.request_upload_with_format(data_format, 0x44, address, size)
    }

    /// `requestUpload` (0x35) with an explicit `addressAndLengthFormatIdentifier`;
    /// see [`request_download_with_format`](Self::request_download_with_format).
    pub fn request_upload_with_format(
        &mut self,
        data_format: u8,
        alfid: u8,
        address: u32,
        size: u32,
    ) -> Result<usize> {
        let mut req = vec![sid::REQUEST_UPLOAD, data_format, alfid];
        req.extend(encode_address_and_length(alfid, address, size)?);
        let resp = self.call_pending(&req)?;
        parse_max_block_len(&resp)
    }

    /// `ReadMemoryByAddress` (0x23) of `size` bytes at `address`, encoded with
    /// `alfid` (see [`encode_address_and_length`]). Returns the data record after
    /// the 0x63 positive-response id. An empty record is rejected; a record
    /// longer than requested is truncated to `size` (some ECUs pad).
    pub fn read_memory_by_address(
        &mut self,
        alfid: u8,
        address: u32,
        size: u32,
    ) -> Result<Vec<u8>> {
        let mut req = vec![sid::READ_MEMORY_BY_ADDRESS, alfid];
        req.extend(encode_address_and_length(alfid, address, size)?);
        let mut data = self.call(&req)?;
        if data.is_empty() {
            return Err(Error::Protocol(format!(
                "ReadMemoryByAddress 0x{address:08X}: empty data record"
            )));
        }
        data.truncate(size as usize);
        Ok(data)
    }

    /// `transferData` (0x36) with a block sequence counter.
    ///
    /// Per ISO 14229 §14.3 the `blockSequenceCounter` starts at 0x01 for the
    /// first block after `requestDownload`/`requestUpload`, increments by one per
    /// block and wraps 0xFF→0x00. A mismatch is answered with NRC 0x73
    /// (`wrongBlockSequenceCounter`); re-sending the same counter is the defined
    /// way to retransmit a lost block. The caller owns the counter — see
    /// [`next_block_seq`] for the wrap.
    ///
    /// The echoed `blockSequenceCounter` in the 0x76 response is verified against
    /// the counter that was sent: a response with no counter byte, or one echoing
    /// a different counter, is rejected. Counter *arithmetic* on the caller's side
    /// is not acknowledgement — only this echo confirms the ECU accepted *this*
    /// block, including across the 0xFF→0x00 wrap. The full body
    /// (`[blockSequenceCounter, transferResponseParameterRecord…]`) is returned so
    /// upload callers can read the record that follows the counter.
    pub fn transfer_data(&mut self, seq: u8, block: &[u8]) -> Result<Vec<u8>> {
        let mut req = vec![sid::TRANSFER_DATA, seq];
        req.extend_from_slice(block);
        let resp = self.call_pending(&req)?;
        match resp.first() {
            None => Err(Error::Protocol(
                "transferData response missing blockSequenceCounter echo".into(),
            )),
            Some(&echo) if echo == seq => Ok(resp),
            Some(&echo) => Err(Error::Protocol(format!(
                "transferData echoed blockSequenceCounter 0x{echo:02X} != sent 0x{seq:02X}"
            ))),
        }
    }

    /// `RequestTransferExit` (0x37). Finalises a download/upload; the ECU may
    /// take time (checksum / erase), so it runs under the P2\* budget.
    pub fn transfer_exit(&mut self) -> Result<Vec<u8>> {
        self.call_pending(&[sid::TRANSFER_EXIT])
    }
}

/// Why an ECU refused a SecurityAccess key (ISO 14229-1 §10.4), from the NRC
/// kept by [`UdsClient::send_key_detailed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyRejection {
    /// NRC 0x35 `invalidKey`: wrong key, attempts remain.
    InvalidKey,
    /// NRC 0x36 `exceededNumberOfAttempts`: the delay timer has started.
    ExceededAttempts,
    /// NRC 0x37 `requiredTimeDelayNotExpired`: wait before trying again.
    DelayNotExpired,
}

impl KeyRejection {
    /// Classify a SecurityAccess error: a 0x27 negative response with NRC
    /// 0x35 / 0x36 / 0x37. Anything else (including the folded
    /// [`Error::SecurityDenied`] of [`UdsClient::send_key`]) is `None`.
    pub fn of(e: &Error) -> Option<Self> {
        match e {
            Error::NegativeResponse {
                service: sid::SECURITY_ACCESS,
                code,
                ..
            } => match code {
                0x35 => Some(Self::InvalidKey),
                0x36 => Some(Self::ExceededAttempts),
                0x37 => Some(Self::DelayNotExpired),
                _ => None,
            },
            _ => None,
        }
    }

    /// The NRC this rejection stands for.
    pub fn nrc(self) -> u8 {
        match self {
            Self::InvalidKey => 0x35,
            Self::ExceededAttempts => 0x36,
            Self::DelayNotExpired => 0x37,
        }
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

/// Encode `memoryAddress` + `memorySize` for an `addressAndLengthFormatIdentifier`
/// (ISO 14229-1 §7.5.4: high nibble = memorySize byte count, low nibble =
/// memoryAddress byte count, each 1..=4 here since both values are `u32`).
/// Big-endian. Rejects a zero/oversized nibble and a value that does not fit
/// its field, rather than silently truncating an address.
pub fn encode_address_and_length(alfid: u8, address: u32, size: u32) -> Result<Vec<u8>> {
    let size_len = (alfid >> 4) as usize;
    let addr_len = (alfid & 0x0F) as usize;
    if !(1..=4).contains(&size_len) || !(1..=4).contains(&addr_len) {
        return Err(Error::Config(format!(
            "addressAndLengthFormatIdentifier 0x{alfid:02X}: address and size must each be 1..=4 bytes"
        )));
    }
    let fits = |v: u32, n: usize| n >= 4 || u64::from(v) < (1u64 << (8 * n));
    if !fits(address, addr_len) {
        return Err(Error::Config(format!(
            "address 0x{address:08X} does not fit the {addr_len}-byte address field of format 0x{alfid:02X}"
        )));
    }
    if !fits(size, size_len) {
        return Err(Error::Config(format!(
            "size {size} does not fit the {size_len}-byte size field of format 0x{alfid:02X}"
        )));
    }
    let mut out = Vec::with_capacity(addr_len + size_len);
    out.extend_from_slice(&address.to_be_bytes()[4 - addr_len..]);
    out.extend_from_slice(&size.to_be_bytes()[4 - size_len..]);
    Ok(out)
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

/// Parse the `0x74`/`0x75` positive-response body (after the SID): a
/// `lengthFormatIdentifier` whose high nibble is the number of
/// `maxNumberOfBlockLength` bytes that follow, big-endian. Rejects an empty body,
/// a zero length-count, a truncated length field, and a zero block length (a
/// block size of 0 would make the following `transferData` loop meaningless).
fn parse_max_block_len(resp: &[u8]) -> Result<usize> {
    if resp.is_empty() {
        return Err(Error::Protocol(
            "truncated requestDownload/Upload response: no lengthFormatIdentifier".into(),
        ));
    }
    // lengthFormatIdentifier: high nibble = byte count, low nibble reserved (0).
    let n = (resp[0] >> 4) as usize;
    if n == 0 {
        return Err(Error::Protocol(
            "requestDownload/Upload lengthFormatIdentifier claims 0 length bytes".into(),
        ));
    }
    if resp.len() < 1 + n {
        return Err(Error::Protocol(format!(
            "truncated requestDownload/Upload response: need {} maxNumberOfBlockLength bytes, have {}",
            n,
            resp.len().saturating_sub(1)
        )));
    }
    let mut v = 0usize;
    for b in &resp[1..1 + n] {
        v = (v << 8) | *b as usize;
    }
    if v == 0 {
        return Err(Error::Protocol(
            "requestDownload/Upload maxNumberOfBlockLength is 0".into(),
        ));
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

    /// A transport that records every request it was handed and the timeout
    /// budget it was given, for the P2 / never-resend assertions.
    struct Recorder {
        responses: VecDeque<Vec<u8>>,
        reqs: Vec<Vec<u8>>,
        budgets: Vec<u64>,
    }
    impl Recorder {
        fn new(r: Vec<Vec<u8>>) -> Self {
            Self {
                responses: VecDeque::from(r),
                reqs: Vec::new(),
                budgets: Vec::new(),
            }
        }
    }
    impl DiagTransport for Recorder {
        fn request(&mut self, req: &[u8], timeout_ms: u64) -> Result<Vec<u8>> {
            self.reqs.push(req.to_vec());
            self.budgets.push(timeout_ms);
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
    fn send_key_detailed_keeps_the_nrc() {
        for (nrc, want) in [
            (0x35, KeyRejection::InvalidKey),
            (0x36, KeyRejection::ExceededAttempts),
            (0x37, KeyRejection::DelayNotExpired),
        ] {
            let mut t = Script::new(vec![vec![0x7F, 0x27, nrc]]);
            let e = UdsClient::new(&mut t)
                .send_key_detailed(0x02, &[1, 2])
                .unwrap_err();
            assert_eq!(KeyRejection::of(&e), Some(want));
            assert_eq!(want.nrc(), nrc);
        }
        // The folding send_key is unchanged: 0x35/0x36 → SecurityDenied,
        // 0x37 stays a raw negative response.
        for nrc in [0x35, 0x36] {
            let mut t = Script::new(vec![vec![0x7F, 0x27, nrc]]);
            assert!(matches!(
                UdsClient::new(&mut t).send_key(0x02, &[1]),
                Err(Error::SecurityDenied)
            ));
        }
        let mut t = Script::new(vec![vec![0x7F, 0x27, 0x37]]);
        let e = UdsClient::new(&mut t).send_key(0x02, &[1]).unwrap_err();
        assert_eq!(KeyRejection::of(&e), Some(KeyRejection::DelayNotExpired));
        assert_eq!(KeyRejection::of(&Error::SecurityDenied), None);
        // A positive answer still checks the level echo.
        let mut t = Script::new(vec![vec![0x67, 0x02]]);
        assert!(UdsClient::new(&mut t).send_key_detailed(0x02, &[1]).is_ok());
    }

    #[test]
    fn unlocked_seed_detection() {
        assert!(seed_is_unlocked(&[0, 0, 0, 0]));
        assert!(!seed_is_unlocked(&[0, 0, 1, 0]));
        assert!(!seed_is_unlocked(&[])); // no seed is not "unlocked"
    }

    #[test]
    fn address_and_length_encoding() {
        assert_eq!(
            encode_address_and_length(0x14, 0x0080_1000, 0x80).unwrap(),
            vec![0x00, 0x80, 0x10, 0x00, 0x80]
        );
        assert_eq!(
            encode_address_and_length(0x22, 0x1234, 0x0100).unwrap(),
            vec![0x12, 0x34, 0x01, 0x00]
        );
        assert_eq!(
            encode_address_and_length(0x13, 0x01_2345, 0xFF).unwrap(),
            vec![0x01, 0x23, 0x45, 0xFF]
        );
        assert_eq!(
            encode_address_and_length(0x44, 0xDEAD_BEEF, 1)
                .unwrap()
                .len(),
            8
        );
        // An address or size that does not fit its field is refused, not truncated.
        assert!(encode_address_and_length(0x22, 0x1_0000, 1).is_err());
        assert!(encode_address_and_length(0x14, 0, 0x100).is_err());
        assert!(encode_address_and_length(0x05, 0, 1).is_err());
        assert!(encode_address_and_length(0x10, 0, 1).is_err());
    }

    #[test]
    fn read_memory_by_address_request_and_record() {
        let mut t = Recorder::new(vec![vec![0x63, 0xAA, 0xBB, 0xCC]]);
        let mut uds = UdsClient::new(&mut t);
        assert_eq!(
            uds.read_memory_by_address(0x14, 0x0001_0000, 2).unwrap(),
            vec![0xAA, 0xBB]
        );
        assert_eq!(t.reqs[0], vec![0x23, 0x14, 0x00, 0x01, 0x00, 0x00, 0x02]);
        let mut e = Script::new(vec![vec![0x63]]);
        assert!(UdsClient::new(&mut e)
            .read_memory_by_address(0x14, 0, 1)
            .is_err());
    }

    #[test]
    fn compact_upload_format_is_encoded() {
        let mut t = Recorder::new(vec![vec![0x75, 0x20, 0x01, 0x02]]);
        let mut uds = UdsClient::new(&mut t);
        assert_eq!(
            uds.request_upload_with_format(0x00, 0x22, 0x8000, 0x0400)
                .unwrap(),
            0x0102
        );
        assert_eq!(t.reqs[0], vec![0x35, 0x00, 0x22, 0x80, 0x00, 0x04, 0x00]);
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
        assert!(parse_max_block_len(&[]).is_err()); // empty body
        assert!(parse_max_block_len(&[0x20, 0x00, 0x00]).is_err()); // block length 0
    }

    // --- routineControl (0x31) echo correlation ---------------------------

    #[test]
    fn routine_control_accepts_matching_echoes() {
        // 71 01 FF 00 <status...> for startRoutine(01) of routine 0xFF00.
        let mut t = Script::new(vec![vec![0x71, 0x01, 0xFF, 0x00, 0xAA, 0xBB]]);
        let mut uds = UdsClient::new(&mut t);
        assert_eq!(
            uds.routine_control(0x01, 0xFF00, &[]).unwrap(),
            vec![0xAA, 0xBB]
        );
    }

    #[test]
    fn routine_control_empty_status_is_ok() {
        // Minimal positive response: echoes only control type + routine id.
        let mut t = Script::new(vec![vec![0x71, 0x01, 0xFF, 0x00]]);
        let mut uds = UdsClient::new(&mut t);
        assert_eq!(
            uds.routine_control(0x01, 0xFF00, &[]).unwrap(),
            Vec::<u8>::new()
        );
    }

    #[test]
    fn routine_control_rejects_truncated_response() {
        // Missing the routine-id echo (only control type present): a trailing
        // status record is optional, but the echo correlation is not.
        for short in [vec![0x71], vec![0x71, 0x01], vec![0x71, 0x01, 0xFF]] {
            let mut t = Script::new(vec![short]);
            let mut uds = UdsClient::new(&mut t);
            assert!(matches!(
                uds.routine_control(0x01, 0xFF00, &[]),
                Err(Error::Protocol(_))
            ));
        }
    }

    #[test]
    fn routine_control_rejects_wrong_control_type_or_routine() {
        // Wrong control type echo (02 for a 01 request).
        let mut t = Script::new(vec![vec![0x71, 0x02, 0xFF, 0x00]]);
        let mut uds = UdsClient::new(&mut t);
        assert!(matches!(
            uds.routine_control(0x01, 0xFF00, &[]),
            Err(Error::Protocol(_))
        ));
        // Wrong routine id echo (FF01 for an FF00 request).
        let mut t2 = Script::new(vec![vec![0x71, 0x01, 0xFF, 0x01]]);
        let mut uds2 = UdsClient::new(&mut t2);
        assert!(matches!(
            uds2.routine_control(0x01, 0xFF00, &[]),
            Err(Error::Protocol(_))
        ));
    }

    #[test]
    fn routine_control_ignores_suppress_bit_on_type_echo() {
        // The ECU may clear bit 7 of the control-type echo; low 7 bits match.
        let mut t = Script::new(vec![vec![0x71, 0x01, 0xFF, 0x00]]);
        let mut uds = UdsClient::new(&mut t);
        // Request with the suppressPositiveResponse bit set (0x81); echo is 0x01.
        assert!(uds.routine_control(0x81, 0xFF00, &[]).is_ok());
    }

    // --- transferData (0x36) block-sequence-counter echo ------------------

    #[test]
    fn transfer_data_accepts_matching_counter() {
        // 76 05 <record> -> counter 0x05 echoed; full body returned.
        let mut t = Script::new(vec![vec![0x76, 0x05, 0xDE, 0xAD]]);
        let mut uds = UdsClient::new(&mut t);
        assert_eq!(
            uds.transfer_data(0x05, &[0x11]).unwrap(),
            vec![0x05, 0xDE, 0xAD]
        );
    }

    #[test]
    fn transfer_data_rejects_missing_counter() {
        // 76 with no blockSequenceCounter byte at all.
        let mut t = Script::new(vec![vec![0x76]]);
        let mut uds = UdsClient::new(&mut t);
        assert!(matches!(
            uds.transfer_data(0x01, &[0x11]),
            Err(Error::Protocol(_))
        ));
    }

    #[test]
    fn transfer_data_rejects_wrong_counter() {
        // Echoes 0x02 though 0x01 was sent: not an acknowledgement of this block.
        let mut t = Script::new(vec![vec![0x76, 0x02]]);
        let mut uds = UdsClient::new(&mut t);
        assert!(matches!(
            uds.transfer_data(0x01, &[0x11]),
            Err(Error::Protocol(_))
        ));
    }

    #[test]
    fn transfer_data_validates_ff_to_00_wraparound() {
        // The 256th block wraps the counter 0xFF -> 0x00; the echo must be 0x00.
        let seq = next_block_seq(0xFF);
        assert_eq!(seq, 0x00);
        let mut good = Script::new(vec![vec![0x76, 0x00]]);
        let mut uds = UdsClient::new(&mut good);
        assert!(uds.transfer_data(seq, &[0x11]).is_ok());
        // An ECU that echoes the pre-wrap 0xFF to a wrapped 0x00 request is wrong.
        let mut bad = Script::new(vec![vec![0x76, 0xFF]]);
        let mut uds2 = UdsClient::new(&mut bad);
        assert!(matches!(
            uds2.transfer_data(seq, &[0x11]),
            Err(Error::Protocol(_))
        ));
    }

    // --- P2 / P2* / total-request timing model ----------------------------

    #[test]
    fn programming_services_use_p2_star_budget() {
        // transferData runs under P2* (not the shorter P2) so a slow flash write
        // is not cut off early.
        let mut t = Recorder::new(vec![vec![0x76, 0x01]]);
        {
            let mut uds = UdsClient::new(&mut t);
            uds.transfer_data(0x01, &[0x00]).unwrap();
        }
        assert_eq!(t.budgets, vec![P2_STAR_CLIENT_MS]);

        // A quick service (RDBI) runs under the P2 base budget.
        let mut t2 = Recorder::new(vec![vec![0x62, 0xF1, 0x90, 0x00]]);
        {
            let mut uds = UdsClient::new(&mut t2);
            uds.read_data_by_id(0xF190).unwrap();
        }
        assert_eq!(t2.budgets, vec![P2_CLIENT_MS]);
    }

    #[test]
    fn per_request_wait_is_clamped_to_total_budget() {
        // With a small absolute cap, the per-request wait handed to the transport
        // is clamped below the (larger) P2* budget — traffic can't make one
        // request wait past the total ceiling.
        let mut t = Recorder::new(vec![vec![0x76, 0x01]]);
        {
            let mut uds = UdsClient::new(&mut t);
            uds.total_timeout_ms = 10; // << P2_STAR_CLIENT_MS
            uds.transfer_data(0x01, &[0x00]).unwrap();
        }
        assert_eq!(t.budgets.len(), 1);
        assert!(
            t.budgets[0] <= 10,
            "wait {} should be clamped to the 10 ms total budget",
            t.budgets[0]
        );
    }

    #[test]
    fn responsepending_at_client_is_not_resent() {
        // If a (non-looping) transport surfaces NRC 0x78, the client must NOT
        // re-issue the request — a write would be duplicated. Exactly one request
        // is sent and the 0x78 is surfaced as a negative response.
        let mut t = Recorder::new(vec![vec![0x7F, 0x36, 0x78]]);
        {
            let mut uds = UdsClient::new(&mut t);
            let r = uds.transfer_data(0x01, &[0x00]);
            assert!(matches!(r, Err(Error::NegativeResponse { code: 0x78, .. })));
        }
        assert_eq!(t.reqs.len(), 1, "the request must be sent exactly once");
    }
}
