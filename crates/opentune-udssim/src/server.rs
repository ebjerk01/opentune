//! The in-memory simulated ECU: an independent ISO 14229 (UDS) **server**.
//!
//! [`UdsServer::handle`] takes one raw UDS request and returns one raw UDS
//! response (positive, or a `7F <sid> <nrc>` negative response), implementing the
//! server side of every service OpenTune's [`UdsClient`] speaks. It owns the
//! mutable ECU state a conformance flow drives through: the active session, the
//! SecurityAccess lock + seed + attempt counter, a DID store, routine results, a
//! DTC store, and the download/upload transfer state with its block-sequence
//! counter.
//!
//! The service semantics were reimplemented from the ISO 14229-1 service
//! catalogue and cross-checked against two independent MIT references,
//! `pylessard/python-udsoncan` and `driftregion/iso14229`. No code was copied
//! from either; this is a clean-room peer for **offline conformance only** and
//! never touches hardware.
//!
//! [`UdsClient`]: opentune_core::protocol::UdsClient

use crate::security::{SecurityChecker, XorSecurity};
use std::collections::BTreeMap;

/// UDS service ids this server answers.
mod sid {
    pub const SESSION_CONTROL: u8 = 0x10;
    pub const ECU_RESET: u8 = 0x11;
    pub const CLEAR_DTC: u8 = 0x14;
    pub const READ_DTC_INFORMATION: u8 = 0x19;
    pub const READ_DATA_BY_ID: u8 = 0x22;
    pub const SECURITY_ACCESS: u8 = 0x27;
    pub const WRITE_DATA_BY_ID: u8 = 0x2E;
    pub const IO_CONTROL: u8 = 0x2F;
    pub const ROUTINE_CONTROL: u8 = 0x31;
    pub const REQUEST_DOWNLOAD: u8 = 0x34;
    pub const REQUEST_UPLOAD: u8 = 0x35;
    pub const TRANSFER_DATA: u8 = 0x36;
    pub const TRANSFER_EXIT: u8 = 0x37;
    pub const TESTER_PRESENT: u8 = 0x3E;
}

/// Negative-response codes used by the server (ISO 14229-1 Annex A).
pub mod nrc {
    /// serviceNotSupported.
    pub const SERVICE_NOT_SUPPORTED: u8 = 0x11;
    /// subFunctionNotSupported.
    pub const SUB_FUNCTION_NOT_SUPPORTED: u8 = 0x12;
    /// incorrectMessageLengthOrInvalidFormat.
    pub const INCORRECT_LENGTH: u8 = 0x13;
    /// conditionsNotCorrect.
    pub const CONDITIONS_NOT_CORRECT: u8 = 0x22;
    /// requestSequenceError.
    pub const REQUEST_SEQUENCE_ERROR: u8 = 0x24;
    /// requestOutOfRange.
    pub const REQUEST_OUT_OF_RANGE: u8 = 0x31;
    /// securityAccessDenied.
    pub const SECURITY_ACCESS_DENIED: u8 = 0x33;
    /// invalidKey.
    pub const INVALID_KEY: u8 = 0x35;
    /// exceededNumberOfAttempts.
    pub const EXCEEDED_ATTEMPTS: u8 = 0x36;
    /// uploadDownloadNotAccepted.
    pub const UPLOAD_DOWNLOAD_NOT_ACCEPTED: u8 = 0x70;
    /// wrongBlockSequenceCounter.
    pub const WRONG_BLOCK_SEQUENCE_COUNTER: u8 = 0x73;
}

/// A stored diagnostic trouble code and its status byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DtcRecord {
    /// The 3-byte DTC (high byte first).
    pub dtc: [u8; 3],
    /// The DTC status mask byte (ISO 14229-1 §D.2).
    pub status: u8,
}

impl DtcRecord {
    /// Build a DTC record from its three bytes and status.
    pub fn new(dtc: [u8; 3], status: u8) -> Self {
        Self { dtc, status }
    }
}

/// One InputOutputControlByIdentifier (0x2F) channel: the ECU's own default
/// state and the state currently driven on the output.
#[derive(Debug, Clone, PartialEq, Eq)]
struct IoChannel {
    /// The state the ECU itself would drive (restored by returnControlToECU).
    default: Vec<u8>,
    /// The state currently on the output.
    current: Vec<u8>,
    /// Whether the tester currently holds control of the output.
    overridden: bool,
}

/// Tunable ECU policy.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Sessions the ECU accepts in DiagnosticSessionControl (0x10).
    pub supported_sessions: Vec<u8>,
    /// `maxNumberOfBlockLength` reported by request(Download|Upload).
    pub max_block_len: u32,
    /// How many SecurityAccess key attempts before the ECU answers
    /// `exceededNumberOfAttempts` (0x36).
    pub max_sec_attempts: u32,
    /// The seed the ECU issues for a (locked) SecurityAccess level.
    pub seed: Vec<u8>,
    /// Require an unlocked SecurityAccess before WriteDataByIdentifier (0x2E).
    pub require_security_for_write: bool,
    /// Require an unlocked SecurityAccess before request(Download|Upload).
    pub require_security_for_transfer: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            supported_sessions: vec![0x01, 0x02, 0x03],
            max_block_len: 0x0402,
            max_sec_attempts: 3,
            seed: vec![0x11, 0x22, 0x33, 0x44],
            require_security_for_write: false,
            require_security_for_transfer: false,
        }
    }
}

/// Active download/upload state (ISO 14229-1 §14).
#[derive(Debug, Clone)]
struct TransferState {
    is_download: bool,
    /// The next expected `blockSequenceCounter` (starts at 0x01, wraps 0xFF→0x00).
    next_seq: u8,
    /// Bytes received so far (download).
    received: Vec<u8>,
    /// Bytes left to hand out (upload), consumed per transferData.
    upload_remaining: Vec<u8>,
}

/// The simulated ECU.
pub struct UdsServer {
    /// Tunable policy.
    pub config: ServerConfig,
    dids: BTreeMap<u16, Vec<u8>>,
    routines: BTreeMap<u16, Vec<u8>>,
    dtcs: Vec<DtcRecord>,
    io: BTreeMap<u16, IoChannel>,
    session: u8,
    security_unlocked: bool,
    /// The (request-seed odd level, seed) last issued, awaiting a key.
    current_seed: Option<(u8, Vec<u8>)>,
    sec_attempts_left: u32,
    checker: Box<dyn SecurityChecker + Send>,
    transfer: Option<TransferState>,
}

impl Default for UdsServer {
    fn default() -> Self {
        Self::new()
    }
}

impl UdsServer {
    /// A fresh ECU in the default session with the default policy and an
    /// `seed XOR 0xA5` security relationship.
    pub fn new() -> Self {
        Self::with_config(ServerConfig::default())
    }

    /// A fresh ECU with an explicit config (default XOR security checker).
    pub fn with_config(config: ServerConfig) -> Self {
        let max = config.max_sec_attempts;
        Self {
            config,
            dids: BTreeMap::new(),
            routines: BTreeMap::new(),
            dtcs: Vec::new(),
            io: BTreeMap::new(),
            session: 0x01,
            security_unlocked: false,
            current_seed: None,
            sec_attempts_left: max,
            checker: Box::new(XorSecurity::new(0xA5)),
            transfer: None,
        }
    }

    /// Install the seed→key relationship the ECU enforces for 0x27. The checker
    /// is `Send` so a simulated ECU can live behind a shared `Mutex` (the
    /// virtual-car driver keeps one vehicle across driver instances).
    pub fn set_security_checker(&mut self, checker: Box<dyn SecurityChecker + Send>) {
        self.checker = checker;
    }

    /// Seed the DID store: `did` reads back `value`.
    pub fn set_did(&mut self, did: u16, value: impl Into<Vec<u8>>) {
        self.dids.insert(did, value.into());
    }

    /// The current value stored at `did`, if any (for assertions after a write).
    pub fn did(&self, did: u16) -> Option<&[u8]> {
        self.dids.get(&did).map(|v| v.as_slice())
    }

    /// Register a routine: RoutineControl on `routine` returns `status_record`.
    pub fn set_routine(&mut self, routine: u16, status_record: impl Into<Vec<u8>>) {
        self.routines.insert(routine, status_record.into());
    }

    /// Append a DTC to the store read by ReadDTCInformation (0x19).
    pub fn add_dtc(&mut self, dtc: DtcRecord) {
        self.dtcs.push(dtc);
    }

    /// The DTC store (in insertion order), for assertions after a clear.
    pub fn dtcs(&self) -> &[DtcRecord] {
        &self.dtcs
    }

    /// Empty the DTC store (what ClearDiagnosticInformation `14 FF FF FF` does).
    pub fn clear_dtcs(&mut self) {
        self.dtcs.clear();
    }

    /// Register an InputOutputControlByIdentifier (0x2F) channel whose ECU
    /// default (and initial) output state is `default_state`.
    pub fn set_io(&mut self, did: u16, default_state: impl Into<Vec<u8>>) {
        let d = default_state.into();
        self.io.insert(
            did,
            IoChannel {
                current: d.clone(),
                default: d,
                overridden: false,
            },
        );
    }

    /// The state currently driven on IO channel `did`, if registered.
    pub fn io_state(&self, did: u16) -> Option<&[u8]> {
        self.io.get(&did).map(|c| c.current.as_slice())
    }

    /// Whether the tester currently holds control of IO channel `did` (a
    /// shortTermAdjustment / freeze / reset without a returnControlToECU yet).
    pub fn io_overridden(&self, did: u16) -> bool {
        self.io.get(&did).is_some_and(|c| c.overridden)
    }

    /// The active diagnostic session.
    pub fn session(&self) -> u8 {
        self.session
    }

    /// Whether SecurityAccess is currently unlocked.
    pub fn is_unlocked(&self) -> bool {
        self.security_unlocked
    }

    /// Bytes accumulated by the active (or just-finished) download transfer.
    pub fn downloaded(&self) -> Option<&[u8]> {
        self.transfer.as_ref().map(|t| t.received.as_slice())
    }

    /// Handle one UDS request, returning the raw UDS response bytes.
    pub fn handle(&mut self, req: &[u8]) -> Vec<u8> {
        let Some(&service) = req.first() else {
            // An empty request cannot carry a service id; answer generalReject
            // against SID 0 so the frame is still a well-formed negative response.
            return neg(0x00, 0x10);
        };
        match service {
            sid::SESSION_CONTROL => self.session_control(req),
            sid::ECU_RESET => self.ecu_reset(req),
            sid::CLEAR_DTC => self.clear_dtc(req),
            sid::READ_DTC_INFORMATION => self.read_dtc(req),
            sid::READ_DATA_BY_ID => self.read_data_by_id(req),
            sid::SECURITY_ACCESS => self.security_access(req),
            sid::WRITE_DATA_BY_ID => self.write_data_by_id(req),
            sid::IO_CONTROL => self.io_control(req),
            sid::ROUTINE_CONTROL => self.routine_control(req),
            sid::REQUEST_DOWNLOAD => self.request_transfer(req, true),
            sid::REQUEST_UPLOAD => self.request_transfer(req, false),
            sid::TRANSFER_DATA => self.transfer_data(req),
            sid::TRANSFER_EXIT => self.transfer_exit(req),
            sid::TESTER_PRESENT => self.tester_present(req),
            other => neg(other, nrc::SERVICE_NOT_SUPPORTED),
        }
    }

    fn session_control(&mut self, req: &[u8]) -> Vec<u8> {
        if req.len() != 2 {
            return neg(sid::SESSION_CONTROL, nrc::INCORRECT_LENGTH);
        }
        let session = req[1] & 0x7F;
        if !self.config.supported_sessions.contains(&session) {
            return neg(sid::SESSION_CONTROL, nrc::SUB_FUNCTION_NOT_SUPPORTED);
        }
        self.session = session;
        // Changing session relocks security (ISO 14229-1 §9.4.1).
        self.security_unlocked = false;
        self.current_seed = None;
        // 50 <session> P2(2) P2*(2): the 2013+ timing record (50 ms / 5000 ms).
        vec![0x50, session, 0x00, 0x32, 0x01, 0xF4]
    }

    fn ecu_reset(&mut self, req: &[u8]) -> Vec<u8> {
        if req.len() != 2 {
            return neg(sid::ECU_RESET, nrc::INCORRECT_LENGTH);
        }
        let kind = req[1] & 0x7F;
        // A reset returns the ECU to default session, locked.
        self.session = 0x01;
        self.security_unlocked = false;
        self.current_seed = None;
        self.transfer = None;
        // 51 <resetType> [powerDownTime]. Add the powerDownTime byte only for
        // enableRapidPowerShutDown (0x04).
        if kind == 0x04 {
            vec![0x51, kind, 0xFF]
        } else {
            vec![0x51, kind]
        }
    }

    fn tester_present(&mut self, req: &[u8]) -> Vec<u8> {
        if req.len() != 2 {
            return neg(sid::TESTER_PRESENT, nrc::INCORRECT_LENGTH);
        }
        // 7E <subFunction>.
        vec![0x7E, req[1] & 0x7F]
    }

    fn read_data_by_id(&mut self, req: &[u8]) -> Vec<u8> {
        if req.len() != 3 {
            return neg(sid::READ_DATA_BY_ID, nrc::INCORRECT_LENGTH);
        }
        let did = u16::from_be_bytes([req[1], req[2]]);
        match self.dids.get(&did) {
            Some(value) => {
                let mut out = vec![0x62, req[1], req[2]];
                out.extend_from_slice(value);
                out
            }
            None => neg(sid::READ_DATA_BY_ID, nrc::REQUEST_OUT_OF_RANGE),
        }
    }

    fn write_data_by_id(&mut self, req: &[u8]) -> Vec<u8> {
        if req.len() < 3 {
            return neg(sid::WRITE_DATA_BY_ID, nrc::INCORRECT_LENGTH);
        }
        if self.config.require_security_for_write && !self.security_unlocked {
            return neg(sid::WRITE_DATA_BY_ID, nrc::SECURITY_ACCESS_DENIED);
        }
        let did = u16::from_be_bytes([req[1], req[2]]);
        self.dids.insert(did, req[3..].to_vec());
        // 6E <did hi> <did lo>.
        vec![0x6E, req[1], req[2]]
    }

    fn clear_dtc(&mut self, req: &[u8]) -> Vec<u8> {
        // 14 <groupOfDTC: 3 bytes>.
        if req.len() != 4 {
            return neg(sid::CLEAR_DTC, nrc::INCORRECT_LENGTH);
        }
        let group = [req[1], req[2], req[3]];
        if group == [0xFF, 0xFF, 0xFF] {
            // All groups.
            self.dtcs.clear();
        } else {
            // A single DTC: it must be one the ECU knows.
            let before = self.dtcs.len();
            self.dtcs.retain(|d| d.dtc != group);
            if self.dtcs.len() == before {
                return neg(sid::CLEAR_DTC, nrc::REQUEST_OUT_OF_RANGE);
            }
        }
        vec![0x54]
    }

    fn io_control(&mut self, req: &[u8]) -> Vec<u8> {
        // 2F <did hi> <did lo> <inputOutputControlParameter> [controlState…].
        if req.len() < 4 {
            return neg(sid::IO_CONTROL, nrc::INCORRECT_LENGTH);
        }
        let did = u16::from_be_bytes([req[1], req[2]]);
        let option = req[3];
        let Some(ch) = self.io.get_mut(&did) else {
            return neg(sid::IO_CONTROL, nrc::REQUEST_OUT_OF_RANGE);
        };
        match option {
            // returnControlToECU.
            0x00 => {
                ch.current = ch.default.clone();
                ch.overridden = false;
            }
            // resetToDefault.
            0x01 => {
                ch.current = ch.default.clone();
                ch.overridden = true;
            }
            // freezeCurrentState.
            0x02 => ch.overridden = true,
            // shortTermAdjustment: the new state must be supplied.
            0x03 => {
                if req.len() == 4 {
                    return neg(sid::IO_CONTROL, nrc::INCORRECT_LENGTH);
                }
                ch.current = req[4..].to_vec();
                ch.overridden = true;
            }
            _ => return neg(sid::IO_CONTROL, nrc::REQUEST_OUT_OF_RANGE),
        }
        // 6F <did hi> <did lo> <parameter> <controlStatusRecord = current state>.
        let mut out = vec![0x6F, req[1], req[2], option];
        out.extend_from_slice(&ch.current);
        out
    }

    fn security_access(&mut self, req: &[u8]) -> Vec<u8> {
        if req.len() < 2 {
            return neg(sid::SECURITY_ACCESS, nrc::INCORRECT_LENGTH);
        }
        let level = req[1];
        let is_request_seed = level & 0x01 == 0x01;
        if is_request_seed {
            // requestSeed (odd level).
            if req.len() != 2 {
                return neg(sid::SECURITY_ACCESS, nrc::INCORRECT_LENGTH);
            }
            if self.security_unlocked {
                // Already unlocked: the ISO convention is an all-zero seed.
                let mut out = vec![0x67, level];
                out.extend(std::iter::repeat(0u8).take(self.config.seed.len()));
                return out;
            }
            self.current_seed = Some((level, self.config.seed.clone()));
            let mut out = vec![0x67, level];
            out.extend_from_slice(&self.config.seed);
            out
        } else {
            // sendKey (even level): it pairs with request-seed level `level - 1`.
            if req.len() < 3 {
                return neg(sid::SECURITY_ACCESS, nrc::INCORRECT_LENGTH);
            }
            let request_level = level.wrapping_sub(1);
            let Some((seed_level, seed)) = self.current_seed.clone() else {
                // No seed was requested first.
                return neg(sid::SECURITY_ACCESS, nrc::REQUEST_SEQUENCE_ERROR);
            };
            if seed_level != request_level {
                return neg(sid::SECURITY_ACCESS, nrc::REQUEST_SEQUENCE_ERROR);
            }
            let key = &req[2..];
            let expected = self.checker.expected_key(&seed, request_level);
            if key == expected.as_slice() {
                self.security_unlocked = true;
                self.current_seed = None;
                self.sec_attempts_left = self.config.max_sec_attempts;
                // 67 <level>.
                vec![0x67, level]
            } else {
                self.sec_attempts_left = self.sec_attempts_left.saturating_sub(1);
                if self.sec_attempts_left == 0 {
                    self.current_seed = None;
                    neg(sid::SECURITY_ACCESS, nrc::EXCEEDED_ATTEMPTS)
                } else {
                    neg(sid::SECURITY_ACCESS, nrc::INVALID_KEY)
                }
            }
        }
    }

    fn routine_control(&mut self, req: &[u8]) -> Vec<u8> {
        if req.len() < 4 {
            return neg(sid::ROUTINE_CONTROL, nrc::INCORRECT_LENGTH);
        }
        let kind = req[1] & 0x7F;
        // Only start/stop/requestResults (0x01/0x02/0x03) are defined.
        if !(0x01..=0x03).contains(&kind) {
            return neg(sid::ROUTINE_CONTROL, nrc::SUB_FUNCTION_NOT_SUPPORTED);
        }
        let routine = u16::from_be_bytes([req[2], req[3]]);
        let status = self.routines.get(&routine).cloned().unwrap_or_default();
        if !self.routines.contains_key(&routine) {
            return neg(sid::ROUTINE_CONTROL, nrc::REQUEST_OUT_OF_RANGE);
        }
        // 71 <type> <rid hi> <rid lo> [status record].
        let mut out = vec![0x71, kind, req[2], req[3]];
        out.extend_from_slice(&status);
        out
    }

    fn request_transfer(&mut self, req: &[u8], is_download: bool) -> Vec<u8> {
        let service = if is_download {
            sid::REQUEST_DOWNLOAD
        } else {
            sid::REQUEST_UPLOAD
        };
        // 34/35 <dataFormatId> <addrAndLenFormatId> <addr…> <size…>.
        if req.len() < 3 {
            return neg(service, nrc::INCORRECT_LENGTH);
        }
        if self.config.require_security_for_transfer && !self.security_unlocked {
            return neg(service, nrc::SECURITY_ACCESS_DENIED);
        }
        let alfi = req[2];
        let addr_bytes = (alfi & 0x0F) as usize;
        let size_bytes = (alfi >> 4) as usize;
        if addr_bytes == 0 || size_bytes == 0 || req.len() != 3 + addr_bytes + size_bytes {
            return neg(service, nrc::INCORRECT_LENGTH);
        }
        let size = be_to_usize(&req[3 + addr_bytes..3 + addr_bytes + size_bytes]);
        if size == 0 {
            return neg(service, nrc::UPLOAD_DOWNLOAD_NOT_ACCEPTED);
        }
        let upload_remaining = if is_download {
            Vec::new()
        } else {
            // Hand out a deterministic ramp for upload reads.
            (0..size).map(|i| (i & 0xFF) as u8).collect()
        };
        self.transfer = Some(TransferState {
            is_download,
            next_seq: 0x01,
            received: Vec::new(),
            upload_remaining,
        });
        // 74/75 <lengthFormatId> <maxNumberOfBlockLength…>. Report 2 length bytes.
        let positive = service + 0x40;
        let mbl = self.config.max_block_len;
        vec![positive, 0x20, (mbl >> 8) as u8, mbl as u8]
    }

    fn transfer_data(&mut self, req: &[u8]) -> Vec<u8> {
        if req.len() < 2 {
            return neg(sid::TRANSFER_DATA, nrc::INCORRECT_LENGTH);
        }
        let seq = req[1];
        let Some(state) = self.transfer.as_mut() else {
            return neg(sid::TRANSFER_DATA, nrc::REQUEST_SEQUENCE_ERROR);
        };
        if seq != state.next_seq {
            // Re-sending the previous (already-accepted) counter is the defined
            // retransmit; any other mismatch is wrongBlockSequenceCounter.
            return neg(sid::TRANSFER_DATA, nrc::WRONG_BLOCK_SEQUENCE_COUNTER);
        }
        if state.is_download {
            state.received.extend_from_slice(&req[2..]);
            state.next_seq = state.next_seq.wrapping_add(1);
            // 76 <blockSequenceCounter>.
            vec![0x76, seq]
        } else {
            // Upload: return the next chunk (bounded by max_block_len) as the
            // transferResponseParameterRecord.
            let chunk_len = (self.config.max_block_len as usize).min(state.upload_remaining.len());
            let chunk: Vec<u8> = state.upload_remaining.drain(..chunk_len).collect();
            state.next_seq = state.next_seq.wrapping_add(1);
            let mut out = vec![0x76, seq];
            out.extend_from_slice(&chunk);
            out
        }
    }

    fn transfer_exit(&mut self, _req: &[u8]) -> Vec<u8> {
        if self.transfer.is_none() {
            return neg(sid::TRANSFER_EXIT, nrc::REQUEST_SEQUENCE_ERROR);
        }
        // Keep the received buffer for inspection but end the active transfer by
        // leaving the state in place; a fresh request(Download|Upload) replaces it.
        // 77 [transferResponseParameterRecord].
        vec![0x77]
    }

    fn read_dtc(&mut self, req: &[u8]) -> Vec<u8> {
        if req.len() < 2 {
            return neg(sid::READ_DTC_INFORMATION, nrc::INCORRECT_LENGTH);
        }
        let subfn = req[1];
        // availabilityMask is the OR of every stored status byte.
        let availability = self.dtcs.iter().fold(0u8, |m, d| m | d.status);
        match subfn {
            // reportNumberOfDTCByStatusMask.
            0x01 => {
                if req.len() != 3 {
                    return neg(sid::READ_DTC_INFORMATION, nrc::INCORRECT_LENGTH);
                }
                let mask = req[2];
                let count = self.dtcs.iter().filter(|d| d.status & mask != 0).count() as u16;
                // 59 01 <availability> <DTCFormatIdentifier=01> <count hi> <count lo>.
                vec![
                    0x59,
                    0x01,
                    availability,
                    0x01,
                    (count >> 8) as u8,
                    count as u8,
                ]
            }
            // reportDTCByStatusMask.
            0x02 => {
                if req.len() != 3 {
                    return neg(sid::READ_DTC_INFORMATION, nrc::INCORRECT_LENGTH);
                }
                let mask = req[2];
                // 59 02 <availability> [DTC(3) status(1)]…
                let mut out = vec![0x59, 0x02, availability];
                for d in self.dtcs.iter().filter(|d| d.status & mask != 0) {
                    out.extend_from_slice(&d.dtc);
                    out.push(d.status);
                }
                out
            }
            _ => neg(sid::READ_DTC_INFORMATION, nrc::SUB_FUNCTION_NOT_SUPPORTED),
        }
    }
}

/// Build the negative response `7F <sid> <nrc>`.
fn neg(sid: u8, code: u8) -> Vec<u8> {
    vec![0x7F, sid, code]
}

/// Big-endian byte slice → usize.
fn be_to_usize(bytes: &[u8]) -> usize {
    bytes.iter().fold(0usize, |v, b| (v << 8) | *b as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::XorSecurity;

    #[test]
    fn unknown_service_is_service_not_supported() {
        let mut s = UdsServer::new();
        assert_eq!(
            s.handle(&[0x99]),
            vec![0x7F, 0x99, nrc::SERVICE_NOT_SUPPORTED]
        );
    }

    #[test]
    fn session_change_relocks_security() {
        let mut s = UdsServer::new();
        s.security_unlocked = true;
        s.handle(&[sid::SESSION_CONTROL, 0x03]);
        assert!(!s.is_unlocked());
        assert_eq!(s.session(), 0x03);
    }

    #[test]
    fn unsupported_session_rejected() {
        let mut s = UdsServer::new();
        assert_eq!(
            s.handle(&[sid::SESSION_CONTROL, 0x7E]),
            vec![0x7F, sid::SESSION_CONTROL, nrc::SUB_FUNCTION_NOT_SUPPORTED]
        );
    }

    #[test]
    fn security_seed_then_key_unlocks() {
        let mut s = UdsServer::new();
        s.set_security_checker(Box::new(XorSecurity::new(0x5A)));
        let seed_resp = s.handle(&[sid::SECURITY_ACCESS, 0x01]);
        assert_eq!(seed_resp[0], 0x67);
        assert_eq!(seed_resp[1], 0x01);
        let seed = &seed_resp[2..];
        let key: Vec<u8> = seed.iter().map(|b| b ^ 0x5A).collect();
        let mut req = vec![sid::SECURITY_ACCESS, 0x02];
        req.extend_from_slice(&key);
        assert_eq!(s.handle(&req), vec![0x67, 0x02]);
        assert!(s.is_unlocked());
    }

    #[test]
    fn send_key_before_seed_is_sequence_error() {
        let mut s = UdsServer::new();
        assert_eq!(
            s.handle(&[sid::SECURITY_ACCESS, 0x02, 0x00]),
            vec![0x7F, sid::SECURITY_ACCESS, nrc::REQUEST_SEQUENCE_ERROR]
        );
    }

    #[test]
    fn attempt_counter_exhausts_to_0x36() {
        let mut s = UdsServer::with_config(ServerConfig {
            max_sec_attempts: 2,
            ..ServerConfig::default()
        });
        s.handle(&[sid::SECURITY_ACCESS, 0x01]);
        // First wrong key: invalidKey (0x35).
        assert_eq!(
            s.handle(&[sid::SECURITY_ACCESS, 0x02, 0x00]),
            vec![0x7F, sid::SECURITY_ACCESS, nrc::INVALID_KEY]
        );
        // Second wrong key exhausts the counter: exceededNumberOfAttempts (0x36).
        assert_eq!(
            s.handle(&[sid::SECURITY_ACCESS, 0x02, 0x00]),
            vec![0x7F, sid::SECURITY_ACCESS, nrc::EXCEEDED_ATTEMPTS]
        );
    }

    #[test]
    fn transfer_data_without_request_is_sequence_error() {
        let mut s = UdsServer::new();
        assert_eq!(
            s.handle(&[sid::TRANSFER_DATA, 0x01, 0xAA]),
            vec![0x7F, sid::TRANSFER_DATA, nrc::REQUEST_SEQUENCE_ERROR]
        );
    }

    #[test]
    fn transfer_data_wrong_counter_is_0x73() {
        let mut s = UdsServer::new();
        // requestDownload addr(4)+size(4), alfi 0x44.
        s.handle(&[0x34, 0x00, 0x44, 0, 0, 0x10, 0, 0, 0, 0, 4]);
        // Send counter 0x02 when 0x01 is expected.
        assert_eq!(
            s.handle(&[sid::TRANSFER_DATA, 0x02, 0xAA]),
            vec![0x7F, sid::TRANSFER_DATA, nrc::WRONG_BLOCK_SEQUENCE_COUNTER]
        );
    }

    #[test]
    fn transfer_data_counter_wraps_ff_to_00() {
        let mut s = UdsServer::new();
        s.transfer = Some(TransferState {
            is_download: true,
            next_seq: 0xFF,
            received: Vec::new(),
            upload_remaining: Vec::new(),
        });
        assert_eq!(
            s.handle(&[sid::TRANSFER_DATA, 0xFF, 0x01]),
            vec![0x76, 0xFF]
        );
        // Next expected counter wrapped to 0x00.
        assert_eq!(
            s.handle(&[sid::TRANSFER_DATA, 0x00, 0x02]),
            vec![0x76, 0x00]
        );
    }

    #[test]
    fn upload_hands_out_a_data_record() {
        let mut s = UdsServer::with_config(ServerConfig {
            max_block_len: 2,
            ..ServerConfig::default()
        });
        // requestUpload size 4.
        let r = s.handle(&[0x35, 0x00, 0x44, 0, 0, 0x10, 0, 0, 0, 0, 4]);
        assert_eq!(r[0], 0x75);
        // First transferData returns 2 bytes (max_block_len).
        let b1 = s.handle(&[sid::TRANSFER_DATA, 0x01]);
        assert_eq!(b1, vec![0x76, 0x01, 0x00, 0x01]);
    }

    #[test]
    fn read_dtc_counts_and_lists_by_mask() {
        let mut s = UdsServer::new();
        s.add_dtc(DtcRecord::new([0x11, 0x22, 0x33], 0x24));
        s.add_dtc(DtcRecord::new([0x44, 0x55, 0x66], 0x01));
        // reportNumberOfDTCByStatusMask with mask that matches only the first.
        assert_eq!(
            s.handle(&[0x19, 0x01, 0x20]),
            vec![0x59, 0x01, 0x25, 0x01, 0x00, 0x01]
        );
        // Unknown sub-function.
        assert_eq!(
            s.handle(&[0x19, 0x7F]),
            vec![0x7F, 0x19, nrc::SUB_FUNCTION_NOT_SUPPORTED]
        );
    }

    #[test]
    fn clear_dtc_all_groups_and_single() {
        let mut s = UdsServer::new();
        s.add_dtc(DtcRecord::new([0x11, 0x22, 0x33], 0x24));
        s.add_dtc(DtcRecord::new([0x44, 0x55, 0x66], 0x01));
        // A DTC the ECU does not store.
        assert_eq!(
            s.handle(&[0x14, 0x01, 0x02, 0x03]),
            vec![0x7F, 0x14, nrc::REQUEST_OUT_OF_RANGE]
        );
        assert_eq!(s.handle(&[0x14, 0x11, 0x22, 0x33]), vec![0x54]);
        assert_eq!(s.dtcs().len(), 1);
        assert_eq!(s.handle(&[0x14, 0xFF, 0xFF, 0xFF]), vec![0x54]);
        assert!(s.dtcs().is_empty());
        assert_eq!(
            s.handle(&[0x14, 0xFF]),
            vec![0x7F, 0x14, nrc::INCORRECT_LENGTH]
        );
    }

    #[test]
    fn io_control_adjust_then_return_control() {
        let mut s = UdsServer::new();
        s.set_io(0x0501, vec![0x00]);
        assert_eq!(
            s.handle(&[0x2F, 0x05, 0x01, 0x03, 0x01]),
            vec![0x6F, 0x05, 0x01, 0x03, 0x01]
        );
        assert_eq!(s.io_state(0x0501), Some(&[0x01][..]));
        assert!(s.io_overridden(0x0501));
        assert_eq!(
            s.handle(&[0x2F, 0x05, 0x01, 0x00]),
            vec![0x6F, 0x05, 0x01, 0x00, 0x00]
        );
        assert!(!s.io_overridden(0x0501));
        // Unknown IO DID, missing state, unknown parameter.
        assert_eq!(
            s.handle(&[0x2F, 0x09, 0x99, 0x00]),
            vec![0x7F, 0x2F, nrc::REQUEST_OUT_OF_RANGE]
        );
        assert_eq!(
            s.handle(&[0x2F, 0x05, 0x01, 0x03]),
            vec![0x7F, 0x2F, nrc::INCORRECT_LENGTH]
        );
        assert_eq!(
            s.handle(&[0x2F, 0x05, 0x01, 0x07]),
            vec![0x7F, 0x2F, nrc::REQUEST_OUT_OF_RANGE]
        );
    }

    #[test]
    fn write_requires_security_when_configured() {
        let mut s = UdsServer::with_config(ServerConfig {
            require_security_for_write: true,
            ..ServerConfig::default()
        });
        assert_eq!(
            s.handle(&[0x2E, 0xF1, 0x90, 0xAA]),
            vec![0x7F, 0x2E, nrc::SECURITY_ACCESS_DENIED]
        );
    }
}
