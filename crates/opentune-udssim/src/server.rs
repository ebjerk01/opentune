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

use crate::flash::{Ctx as FlashCtx, FlashConfig, FlashEcu, Outcome as FlashOutcome};
use crate::security::{SecurityChecker, XorSecurity};
use std::collections::BTreeMap;

/// UDS service ids this server answers.
mod sid {
    pub const SESSION_CONTROL: u8 = 0x10;
    pub const ECU_RESET: u8 = 0x11;
    pub const CLEAR_DTC: u8 = 0x14;
    pub const READ_DTC_INFORMATION: u8 = 0x19;
    pub const READ_DATA_BY_ID: u8 = 0x22;
    pub const READ_MEMORY_BY_ADDRESS: u8 = 0x23;
    pub const SECURITY_ACCESS: u8 = 0x27;
    pub const DYNAMICALLY_DEFINE_DID: u8 = 0x2C;
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
    /// DynamicallyDefineDataIdentifier (0x2C) policy.
    pub dddid: DddidConfig,
}

/// Policy for DynamicallyDefineDataIdentifier (0x2C, ISO 14229-1 §10.6).
#[derive(Debug, Clone)]
pub struct DddidConfig {
    /// Whether the ECU implements 0x2C at all (`false` → serviceNotSupported).
    pub supported: bool,
    /// Sessions in which 0x2C is accepted; any other session answers
    /// conditionsNotCorrect (0x22).
    pub allowed_sessions: Vec<u8>,
    /// Require an unlocked SecurityAccess before a define (0x01 / 0x02);
    /// answered with securityAccessDenied (0x33).
    pub require_security: bool,
    /// Largest record (bytes) one dynamic DID may hold; a define that would
    /// grow a record past it is requestOutOfRange (0x31).
    pub max_record_len: usize,
    /// How many dynamic DIDs may be defined at once (requestOutOfRange past it).
    pub max_defined: usize,
}

impl Default for DddidConfig {
    fn default() -> Self {
        Self {
            supported: true,
            allowed_sessions: vec![0x01, 0x02, 0x03],
            require_security: false,
            max_record_len: 64,
            max_defined: 16,
        }
    }
}

/// The ISO 14229-1 range reserved for dynamically defined data identifiers.
pub const DYNAMIC_DID_RANGE: std::ops::RangeInclusive<u16> = 0xF200..=0xF3FF;

/// One source element of a dynamically defined data identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DddidSource {
    /// `memorySize` bytes of `source`'s record, starting at the 1-based
    /// `position` (defineByIdentifier).
    Did { source: u16, position: u8, size: u8 },
    /// `size` bytes of simulated memory at `address` (defineByMemoryAddress).
    Memory { address: u64, size: usize },
}

impl DddidSource {
    fn len(&self) -> usize {
        match self {
            DddidSource::Did { size, .. } => usize::from(*size),
            DddidSource::Memory { size, .. } => *size,
        }
    }
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
            dddid: DddidConfig::default(),
        }
    }
}

/// Active download/upload state (ISO 14229-1 §14).
#[derive(Debug, Clone)]
struct TransferState {
    is_download: bool,
    /// Start address of the transfer (downloads are written here).
    address: u64,
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
    /// Readable simulated memory regions: base address → bytes.
    memory: BTreeMap<u64, Vec<u8>>,
    /// Dynamically defined DIDs (0x2C): DID → ordered source elements.
    dynamic: BTreeMap<u16, Vec<DddidSource>>,
    /// The request-seed (odd) level of the current unlock, if any.
    unlocked_level: Option<u8>,
    /// The optional flashable-ECU model (see [`crate::flash`]).
    flash: Option<FlashEcu>,
    /// `responsePending` frames to interpose before the last reply.
    pending: usize,
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
            memory: BTreeMap::new(),
            dynamic: BTreeMap::new(),
            unlocked_level: None,
            flash: None,
            pending: 0,
        }
    }

    /// Turn this ECU into a flashable one (see [`crate::flash`]): programming
    /// session, erase / check routines, download into a persistent flash
    /// memory. Replaces any previous flash model.
    pub fn enable_flash(&mut self, config: FlashConfig) {
        if !self
            .config
            .supported_sessions
            .contains(&config.programming_session)
        {
            self.config
                .supported_sessions
                .push(config.programming_session);
        }
        self.flash = Some(FlashEcu::new(config));
    }

    /// The flash model, if enabled (memory, stats, validity).
    pub fn flash(&self) -> Option<&FlashEcu> {
        self.flash.as_ref()
    }

    /// The flash model, mutably (e.g. to arm a fault mid-test).
    pub fn flash_mut(&mut self) -> Option<&mut FlashEcu> {
        self.flash.as_mut()
    }

    /// The request-seed (odd) level SecurityAccess is unlocked at, if any.
    pub fn unlocked_level(&self) -> Option<u8> {
        if self.security_unlocked {
            self.unlocked_level
        } else {
            None
        }
    }

    /// How many `responsePending` (0x78) frames the ECU sends before the reply
    /// to the last request (a slow erase). Reading clears it. A transport
    /// interposes them before the reply; see [`crate::SimTransport`].
    pub fn take_pending(&mut self) -> usize {
        std::mem::take(&mut self.pending)
    }

    /// Reboot (power cycle): default session, locked, transfers aborted.
    /// Flash memory persists.
    pub fn power_cycle(&mut self) {
        self.session = 0x01;
        self.security_unlocked = false;
        self.unlocked_level = None;
        self.current_seed = None;
        self.transfer = None;
        self.dynamic.clear();
        if let Some(f) = self.flash.as_mut() {
            f.on_reboot();
        }
    }

    /// Handle one UDS request; `None` when the ECU sends no response at all
    /// (an injected lost response or power loss of the flash model). Without
    /// a flash model this is always `Some(`[`handle`](Self::handle)`)`.
    pub fn respond(&mut self, req: &[u8]) -> Option<Vec<u8>> {
        self.pending = 0;
        if let Some(flash) = self.flash.as_mut() {
            let ctx = FlashCtx {
                session: self.session,
                unlocked_level: if self.security_unlocked {
                    self.unlocked_level
                } else {
                    None
                },
                generic_transfer_active: self.transfer.is_some(),
            };
            match flash.handle(req, ctx) {
                FlashOutcome::Pass => {}
                FlashOutcome::Reply(r) => {
                    self.pending = flash.take_pending();
                    return Some(r);
                }
                FlashOutcome::Silent => return None,
                FlashOutcome::PowerLoss => {
                    self.power_cycle();
                    return None;
                }
            }
        }
        Some(self.dispatch(req))
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

    /// Map a readable memory region: `bytes` live at `base` onward, for
    /// ReadMemoryByAddress (0x23) and defineByMemoryAddress (0x2C 02). A read
    /// must fall entirely inside one region.
    pub fn set_memory(&mut self, base: u64, bytes: impl Into<Vec<u8>>) {
        self.memory.insert(base, bytes.into());
    }

    /// Overwrite bytes inside an already-mapped region (e.g. to make a logged
    /// value change between samples). Returns `false` if the span is unmapped.
    pub fn write_memory(&mut self, address: u64, bytes: &[u8]) -> bool {
        let Some((base, region)) = self.memory.range_mut(..=address).next_back() else {
            return false;
        };
        let off = (address - base) as usize;
        match region.get_mut(off..off + bytes.len()) {
            Some(dst) => {
                dst.copy_from_slice(bytes);
                true
            }
            None => false,
        }
    }

    /// The dynamically defined DIDs currently in force (ascending).
    pub fn dynamic_dids(&self) -> Vec<u16> {
        self.dynamic.keys().copied().collect()
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

    /// Handle one UDS request, returning the raw UDS response bytes. A
    /// suppressed response (flash-model fault, see [`respond`](Self::respond))
    /// comes back empty.
    pub fn handle(&mut self, req: &[u8]) -> Vec<u8> {
        self.respond(req).unwrap_or_default()
    }

    fn dispatch(&mut self, req: &[u8]) -> Vec<u8> {
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
            sid::READ_MEMORY_BY_ADDRESS => self.read_memory_by_address(req),
            sid::DYNAMICALLY_DEFINE_DID => self.dynamically_define(req),
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
        self.unlocked_level = None;
        self.current_seed = None;
        // Dynamically defined DIDs do not survive a session transition. ISO
        // 14229-1 requires at least the return to the default session to clear
        // them; this server is strict and clears on every 0x10, so a client
        // must be ready to re-define after any session change.
        self.dynamic.clear();
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
        self.unlocked_level = None;
        self.current_seed = None;
        self.transfer = None;
        self.dynamic.clear();
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
        // A dynamically defined DID is assembled at read time from the current
        // values of its sources, in definition order.
        if let Some(sources) = self.dynamic.get(&did) {
            let mut out = vec![0x62, req[1], req[2]];
            for src in sources {
                match self.read_source(src) {
                    Some(bytes) => out.extend_from_slice(&bytes),
                    // A source vanished since the define (e.g. its DID was
                    // removed): the record can no longer be built.
                    None => return neg(sid::READ_DATA_BY_ID, nrc::CONDITIONS_NOT_CORRECT),
                }
            }
            return out;
        }
        match self.dids.get(&did) {
            Some(value) => {
                let mut out = vec![0x62, req[1], req[2]];
                out.extend_from_slice(value);
                out
            }
            None => neg(sid::READ_DATA_BY_ID, nrc::REQUEST_OUT_OF_RANGE),
        }
    }

    /// What an upload of `size` bytes at `address` hands out: the flash
    /// model's memory (when it maps the span and allows reads), else the
    /// mapped memory, else the historical deterministic ramp (byte `i` =
    /// `i & 0xFF`) for unmapped addresses.
    fn upload_source(&self, address: u64, size: usize) -> Vec<u8> {
        let from_flash = self.flash.as_ref().and_then(|f| {
            let addr = u32::try_from(address).ok()?;
            f.config.readable.then(|| f.read(addr, size)).flatten()
        });
        from_flash
            .or_else(|| self.read_memory(address, size))
            .unwrap_or_else(|| (0..size).map(|i| (i & 0xFF) as u8).collect())
    }

    /// `size` bytes of mapped memory at `address`, if the span lies wholly
    /// inside one region.
    fn read_memory(&self, address: u64, size: usize) -> Option<Vec<u8>> {
        let (base, region) = self.memory.range(..=address).next_back()?;
        let off = usize::try_from(address - base).ok()?;
        region.get(off..off.checked_add(size)?).map(<[u8]>::to_vec)
    }

    /// The current bytes of one dynamic-DID source element.
    fn read_source(&self, src: &DddidSource) -> Option<Vec<u8>> {
        match *src {
            DddidSource::Did {
                source,
                position,
                size,
            } => {
                let rec = self.dids.get(&source)?;
                let start = usize::from(position).checked_sub(1)?;
                rec.get(start..start + usize::from(size))
                    .map(<[u8]>::to_vec)
            }
            DddidSource::Memory { address, size } => self.read_memory(address, size),
        }
    }

    /// Split `addressAndLengthFormatIdentifier` into (address bytes, size
    /// bytes), rejecting the zero/oversized widths this server cannot hold.
    fn alfid_widths(alfid: u8) -> Option<(usize, usize)> {
        let addr = usize::from(alfid & 0x0F);
        let size = usize::from(alfid >> 4);
        ((1..=8).contains(&addr) && (1..=4).contains(&size)).then_some((addr, size))
    }

    fn read_memory_by_address(&mut self, req: &[u8]) -> Vec<u8> {
        // 23 <ALFID> <memoryAddress…> <memorySize…>.
        if req.len() < 2 {
            return neg(sid::READ_MEMORY_BY_ADDRESS, nrc::INCORRECT_LENGTH);
        }
        let Some((addr_len, size_len)) = Self::alfid_widths(req[1]) else {
            return neg(sid::READ_MEMORY_BY_ADDRESS, nrc::REQUEST_OUT_OF_RANGE);
        };
        if req.len() != 2 + addr_len + size_len {
            return neg(sid::READ_MEMORY_BY_ADDRESS, nrc::INCORRECT_LENGTH);
        }
        let address = be_to_u64(&req[2..2 + addr_len]);
        let size = be_to_usize(&req[2 + addr_len..]);
        let data = if size == 0 {
            None
        } else {
            self.read_memory(address, size)
        };
        match data {
            Some(bytes) => {
                let mut out = vec![0x63];
                out.extend_from_slice(&bytes);
                out
            }
            None => neg(sid::READ_MEMORY_BY_ADDRESS, nrc::REQUEST_OUT_OF_RANGE),
        }
    }

    /// DynamicallyDefineDataIdentifier (0x2C, ISO 14229-1 §10.6).
    ///
    /// * `2C 01 <DDDID:2> {<sourceDID:2> <position:1> <memorySize:1>}…` —
    ///   defineByIdentifier (`position` is 1-based in the source record).
    /// * `2C 02 <DDDID:2> <ALFID> {<memoryAddress> <memorySize>}…` —
    ///   defineByMemoryAddress.
    /// * `2C 03 [<DDDID:2>]` — clear one dynamic DID, or all of them.
    ///
    /// A define **appends** to an existing definition of the same DDDID, so a
    /// record can mix identifier and memory sources across several requests.
    /// A request with any invalid element is rejected whole: the definition is
    /// left exactly as it was.
    fn dynamically_define(&mut self, req: &[u8]) -> Vec<u8> {
        const SID: u8 = sid::DYNAMICALLY_DEFINE_DID;
        let cfg = self.config.dddid.clone();
        if !cfg.supported {
            return neg(SID, nrc::SERVICE_NOT_SUPPORTED);
        }
        if req.len() < 2 {
            return neg(SID, nrc::INCORRECT_LENGTH);
        }
        let subfn = req[1] & 0x7F;
        if !(0x01..=0x03).contains(&subfn) {
            return neg(SID, nrc::SUB_FUNCTION_NOT_SUPPORTED);
        }
        // Minimum length per sub-function: a define carries the DDDID plus at
        // least one element; a clear carries nothing or exactly the DDDID.
        let len_ok = match subfn {
            0x01 => req.len() >= 8 && (req.len() - 4) % 4 == 0,
            0x02 => req.len() >= 5,
            _ => req.len() == 2 || req.len() == 4,
        };
        if !len_ok {
            return neg(SID, nrc::INCORRECT_LENGTH);
        }
        if !cfg.allowed_sessions.contains(&self.session) {
            return neg(SID, nrc::CONDITIONS_NOT_CORRECT);
        }

        if subfn == 0x03 {
            if req.len() == 2 {
                self.dynamic.clear();
                return vec![0x6C, 0x03];
            }
            let dddid = u16::from_be_bytes([req[2], req[3]]);
            if !DYNAMIC_DID_RANGE.contains(&dddid) {
                return neg(SID, nrc::REQUEST_OUT_OF_RANGE);
            }
            // Clearing a DDDID that is not defined is not an error.
            self.dynamic.remove(&dddid);
            return vec![0x6C, 0x03, req[2], req[3]];
        }

        let dddid = u16::from_be_bytes([req[2], req[3]]);
        if !DYNAMIC_DID_RANGE.contains(&dddid) {
            return neg(SID, nrc::REQUEST_OUT_OF_RANGE);
        }
        if cfg.require_security && !self.security_unlocked {
            return neg(SID, nrc::SECURITY_ACCESS_DENIED);
        }

        let mut elements = Vec::new();
        if subfn == 0x01 {
            for e in req[4..].chunks_exact(4) {
                let source = u16::from_be_bytes([e[0], e[1]]);
                let (position, size) = (e[2], e[3]);
                // The source must be a known (static) DID — a dynamic DID
                // cannot nest — and the slice must lie inside its record.
                let fits = self.dids.get(&source).is_some_and(|rec| {
                    position >= 1
                        && size >= 1
                        && usize::from(position) - 1 + usize::from(size) <= rec.len()
                });
                if !fits {
                    return neg(SID, nrc::REQUEST_OUT_OF_RANGE);
                }
                elements.push(DddidSource::Did {
                    source,
                    position,
                    size,
                });
            }
        } else {
            let Some((addr_len, size_len)) = Self::alfid_widths(req[4]) else {
                return neg(SID, nrc::REQUEST_OUT_OF_RANGE);
            };
            let body = &req[5..];
            let entry = addr_len + size_len;
            if body.is_empty() || body.len() % entry != 0 {
                return neg(SID, nrc::INCORRECT_LENGTH);
            }
            for e in body.chunks_exact(entry) {
                let address = be_to_u64(&e[..addr_len]);
                let size = be_to_usize(&e[addr_len..]);
                if size == 0 || self.read_memory(address, size).is_none() {
                    return neg(SID, nrc::REQUEST_OUT_OF_RANGE);
                }
                elements.push(DddidSource::Memory { address, size });
            }
        }

        let existing = self.dynamic.get(&dddid);
        if existing.is_none() && self.dynamic.len() >= cfg.max_defined {
            return neg(SID, nrc::REQUEST_OUT_OF_RANGE);
        }
        let old_len: usize = existing.map_or(0, |v| v.iter().map(DddidSource::len).sum());
        let add_len: usize = elements.iter().map(DddidSource::len).sum();
        if old_len + add_len > cfg.max_record_len {
            return neg(SID, nrc::REQUEST_OUT_OF_RANGE);
        }
        self.dynamic.entry(dddid).or_default().extend(elements);
        // 6C <subFunction> <DDDID:2>.
        vec![0x6C, subfn, req[2], req[3]]
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
                self.unlocked_level = Some(request_level);
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
        let address = be_to_u64(&req[3..3 + addr_bytes]);
        let size = be_to_usize(&req[3 + addr_bytes..3 + addr_bytes + size_bytes]);
        if size == 0 {
            return neg(service, nrc::UPLOAD_DOWNLOAD_NOT_ACCEPTED);
        }
        let upload_remaining = if is_download {
            // Downloads persist into the memory map, so a later upload /
            // ReadMemoryByAddress reads them back. An unmapped target gets a
            // fresh (erased, 0xFF) region.
            if self.read_memory(address, size).is_none() {
                self.memory.insert(address, vec![0xFF; size]);
            }
            Vec::new()
        } else {
            self.upload_source(address, size)
        };
        self.transfer = Some(TransferState {
            is_download,
            address,
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
            let at = state.address + state.received.len() as u64;
            state.received.extend_from_slice(&req[2..]);
            let data = req[2..].to_vec();
            self.write_memory(at, &data);
            let Some(state) = self.transfer.as_mut() else {
                return neg(sid::TRANSFER_DATA, nrc::REQUEST_SEQUENCE_ERROR);
            };
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

/// Big-endian byte slice (≤ 8 bytes) → u64.
fn be_to_u64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0u64, |v, b| (v << 8) | u64::from(*b))
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
            address: 0,
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

    // ---- 0x23 ReadMemoryByAddress / 0x2C DynamicallyDefineDataIdentifier ----

    /// A server with two source DIDs and one mapped memory region.
    fn dddid_server() -> UdsServer {
        let mut s = UdsServer::new();
        s.set_did(0x1000, vec![0xAA, 0xBB, 0xCC, 0xDD]);
        s.set_did(0x2000, vec![0x01, 0x02]);
        s.set_memory(0x0010_0000, vec![0x10, 0x11, 0x12, 0x13, 0x14, 0x15]);
        s
    }

    #[test]
    fn read_memory_by_address_reads_mapped_bytes() {
        let mut s = dddid_server();
        // ALFID 0x14: 4-byte address, 1-byte size.
        assert_eq!(
            s.handle(&[0x23, 0x14, 0x00, 0x10, 0x00, 0x02, 0x03]),
            vec![0x63, 0x12, 0x13, 0x14]
        );
        // Span crossing the end of the region.
        assert_eq!(
            s.handle(&[0x23, 0x14, 0x00, 0x10, 0x00, 0x05, 0x02]),
            vec![0x7F, 0x23, nrc::REQUEST_OUT_OF_RANGE]
        );
        // Wrong length for the ALFID.
        assert_eq!(
            s.handle(&[0x23, 0x14, 0x00, 0x10, 0x00]),
            vec![0x7F, 0x23, nrc::INCORRECT_LENGTH]
        );
        // Invalid ALFID (zero-width address).
        assert_eq!(
            s.handle(&[0x23, 0x10, 0x01]),
            vec![0x7F, 0x23, nrc::REQUEST_OUT_OF_RANGE]
        );
    }

    #[test]
    fn dddid_define_by_identifier_then_read() {
        let mut s = dddid_server();
        // F200 = 0x1000[pos 2, 2 bytes] ++ 0x2000[pos 1, 2 bytes].
        assert_eq!(
            s.handle(&[0x2C, 0x01, 0xF2, 0x00, 0x10, 0x00, 0x02, 0x02, 0x20, 0x00, 0x01, 0x02]),
            vec![0x6C, 0x01, 0xF2, 0x00]
        );
        assert_eq!(
            s.handle(&[0x22, 0xF2, 0x00]),
            vec![0x62, 0xF2, 0x00, 0xBB, 0xCC, 0x01, 0x02]
        );
        // The record is assembled at read time: a changed source shows up.
        s.set_did(0x2000, vec![0x07, 0x08]);
        assert_eq!(
            s.handle(&[0x22, 0xF2, 0x00]),
            vec![0x62, 0xF2, 0x00, 0xBB, 0xCC, 0x07, 0x08]
        );
    }

    #[test]
    fn dddid_define_by_memory_appends_to_identifier_sources() {
        let mut s = dddid_server();
        s.handle(&[0x2C, 0x01, 0xF2, 0x01, 0x20, 0x00, 0x01, 0x01]);
        // Two memory elements, ALFID 0x14.
        assert_eq!(
            s.handle(&[
                0x2C, 0x02, 0xF2, 0x01, 0x14, 0x00, 0x10, 0x00, 0x00, 0x02, 0x00, 0x10, 0x00, 0x04,
                0x01,
            ]),
            vec![0x6C, 0x02, 0xF2, 0x01]
        );
        assert_eq!(
            s.handle(&[0x22, 0xF2, 0x01]),
            vec![0x62, 0xF2, 0x01, 0x01, 0x10, 0x11, 0x14]
        );
        assert!(s.write_memory(0x0010_0001, &[0x99]));
        assert_eq!(
            s.handle(&[0x22, 0xF2, 0x01]),
            vec![0x62, 0xF2, 0x01, 0x01, 0x10, 0x99, 0x14]
        );
    }

    #[test]
    fn dddid_clear_one_and_all() {
        let mut s = dddid_server();
        s.handle(&[0x2C, 0x01, 0xF2, 0x00, 0x20, 0x00, 0x01, 0x01]);
        s.handle(&[0x2C, 0x01, 0xF2, 0x10, 0x20, 0x00, 0x01, 0x01]);
        assert_eq!(s.dynamic_dids(), vec![0xF200, 0xF210]);
        assert_eq!(
            s.handle(&[0x2C, 0x03, 0xF2, 0x00]),
            vec![0x6C, 0x03, 0xF2, 0x00]
        );
        assert_eq!(s.dynamic_dids(), vec![0xF210]);
        // A cleared DDDID reads as unknown.
        assert_eq!(
            s.handle(&[0x22, 0xF2, 0x00]),
            vec![0x7F, 0x22, nrc::REQUEST_OUT_OF_RANGE]
        );
        // Clearing an undefined DDDID in range is fine.
        assert_eq!(
            s.handle(&[0x2C, 0x03, 0xF3, 0xFF]),
            vec![0x6C, 0x03, 0xF3, 0xFF]
        );
        // Clear all.
        assert_eq!(s.handle(&[0x2C, 0x03]), vec![0x6C, 0x03]);
        assert!(s.dynamic_dids().is_empty());
    }

    #[test]
    fn dddid_length_and_subfunction_nrcs() {
        let mut s = dddid_server();
        assert_eq!(s.handle(&[0x2C]), vec![0x7F, 0x2C, nrc::INCORRECT_LENGTH]);
        assert_eq!(
            s.handle(&[0x2C, 0x04, 0xF2, 0x00]),
            vec![0x7F, 0x2C, nrc::SUB_FUNCTION_NOT_SUPPORTED]
        );
        // defineByIdentifier with a truncated element.
        assert_eq!(
            s.handle(&[0x2C, 0x01, 0xF2, 0x00, 0x20, 0x00, 0x01]),
            vec![0x7F, 0x2C, nrc::INCORRECT_LENGTH]
        );
        // defineByMemoryAddress with no element after the ALFID.
        assert_eq!(
            s.handle(&[0x2C, 0x02, 0xF2, 0x00, 0x14]),
            vec![0x7F, 0x2C, nrc::INCORRECT_LENGTH]
        );
        // ... and with a partial element.
        assert_eq!(
            s.handle(&[0x2C, 0x02, 0xF2, 0x00, 0x14, 0x00, 0x10, 0x00]),
            vec![0x7F, 0x2C, nrc::INCORRECT_LENGTH]
        );
        // Clear with a 1-byte DDDID.
        assert_eq!(
            s.handle(&[0x2C, 0x03, 0xF2]),
            vec![0x7F, 0x2C, nrc::INCORRECT_LENGTH]
        );
    }

    #[test]
    fn dddid_out_of_range_nrcs_leave_definition_untouched() {
        let mut s = dddid_server();
        let oor = vec![0x7F, 0x2C, nrc::REQUEST_OUT_OF_RANGE];
        // DDDID outside 0xF200..=0xF3FF.
        assert_eq!(
            s.handle(&[0x2C, 0x01, 0x10, 0x00, 0x20, 0x00, 0x01, 0x01]),
            oor
        );
        assert_eq!(
            s.handle(&[0x2C, 0x01, 0xF4, 0x00, 0x20, 0x00, 0x01, 0x01]),
            oor
        );
        assert_eq!(s.handle(&[0x2C, 0x03, 0xF1, 0xFF]), oor);
        // Unknown source DID.
        assert_eq!(
            s.handle(&[0x2C, 0x01, 0xF2, 0x00, 0x30, 0x00, 0x01, 0x01]),
            oor
        );
        // Position 0 (positions are 1-based) and a slice past the record end.
        assert_eq!(
            s.handle(&[0x2C, 0x01, 0xF2, 0x00, 0x20, 0x00, 0x00, 0x01]),
            oor
        );
        assert_eq!(
            s.handle(&[0x2C, 0x01, 0xF2, 0x00, 0x20, 0x00, 0x02, 0x02]),
            oor
        );
        // Unmapped memory and an invalid ALFID.
        assert_eq!(
            s.handle(&[0x2C, 0x02, 0xF2, 0x00, 0x14, 0x00, 0x20, 0x00, 0x00, 0x01]),
            oor
        );
        assert_eq!(
            s.handle(&[0x2C, 0x02, 0xF2, 0x00, 0x04, 0x00, 0x10, 0x00, 0x00]),
            oor
        );
        // A valid define, then one whose *second* element is bad: rejected
        // whole, the first definition stays as it was.
        s.handle(&[0x2C, 0x01, 0xF2, 0x00, 0x20, 0x00, 0x01, 0x02]);
        assert_eq!(
            s.handle(&[0x2C, 0x01, 0xF2, 0x00, 0x10, 0x00, 0x01, 0x01, 0x30, 0x00, 0x01, 0x01]),
            oor
        );
        assert_eq!(
            s.handle(&[0x22, 0xF2, 0x00]),
            vec![0x62, 0xF2, 0x00, 0x01, 0x02]
        );
    }

    #[test]
    fn dddid_record_and_count_limits_are_out_of_range() {
        let mut s = UdsServer::with_config(ServerConfig {
            dddid: DddidConfig {
                max_record_len: 4,
                max_defined: 1,
                ..DddidConfig::default()
            },
            ..ServerConfig::default()
        });
        s.set_did(0x1000, vec![0xAA, 0xBB, 0xCC, 0xDD]);
        assert_eq!(
            s.handle(&[0x2C, 0x01, 0xF2, 0x00, 0x10, 0x00, 0x01, 0x03]),
            vec![0x6C, 0x01, 0xF2, 0x00]
        );
        // Appending 2 more bytes would make the record 5 > 4.
        assert_eq!(
            s.handle(&[0x2C, 0x01, 0xF2, 0x00, 0x10, 0x00, 0x01, 0x02]),
            vec![0x7F, 0x2C, nrc::REQUEST_OUT_OF_RANGE]
        );
        // A second DDDID exceeds max_defined.
        assert_eq!(
            s.handle(&[0x2C, 0x01, 0xF2, 0x01, 0x10, 0x00, 0x01, 0x01]),
            vec![0x7F, 0x2C, nrc::REQUEST_OUT_OF_RANGE]
        );
    }

    #[test]
    fn dddid_conditions_security_and_support_nrcs() {
        let define = [0x2C, 0x01, 0xF2, 0x00, 0x20, 0x00, 0x01, 0x01];
        // Only the extended session allows 0x2C.
        let mut s = UdsServer::with_config(ServerConfig {
            dddid: DddidConfig {
                allowed_sessions: vec![0x03],
                ..DddidConfig::default()
            },
            ..ServerConfig::default()
        });
        s.set_did(0x2000, vec![0x01]);
        assert_eq!(
            s.handle(&define),
            vec![0x7F, 0x2C, nrc::CONDITIONS_NOT_CORRECT]
        );
        s.handle(&[0x10, 0x03]);
        assert_eq!(s.handle(&define), vec![0x6C, 0x01, 0xF2, 0x00]);

        // Security required for a define.
        let mut s = UdsServer::with_config(ServerConfig {
            dddid: DddidConfig {
                require_security: true,
                ..DddidConfig::default()
            },
            ..ServerConfig::default()
        });
        s.set_did(0x2000, vec![0x01]);
        assert_eq!(
            s.handle(&define),
            vec![0x7F, 0x2C, nrc::SECURITY_ACCESS_DENIED]
        );
        s.security_unlocked = true;
        assert_eq!(s.handle(&define), vec![0x6C, 0x01, 0xF2, 0x00]);

        // An ECU without 0x2C.
        let mut s = UdsServer::with_config(ServerConfig {
            dddid: DddidConfig {
                supported: false,
                ..DddidConfig::default()
            },
            ..ServerConfig::default()
        });
        assert_eq!(
            s.handle(&define),
            vec![0x7F, 0x2C, nrc::SERVICE_NOT_SUPPORTED]
        );
    }

    #[test]
    fn dddid_cleared_by_session_change_and_reset() {
        let mut s = dddid_server();
        let define = [0x2C, 0x01, 0xF2, 0x00, 0x20, 0x00, 0x01, 0x01];
        s.handle(&define);
        assert_eq!(s.dynamic_dids(), vec![0xF200]);
        s.handle(&[0x10, 0x03]);
        assert!(s.dynamic_dids().is_empty(), "session change clears DDDIDs");
        assert_eq!(
            s.handle(&[0x22, 0xF2, 0x00]),
            vec![0x7F, 0x22, nrc::REQUEST_OUT_OF_RANGE]
        );
        s.handle(&define);
        s.handle(&[0x11, 0x01]);
        assert!(s.dynamic_dids().is_empty(), "ECU reset clears DDDIDs");
    }

    #[test]
    fn dddid_read_fails_when_a_source_disappears() {
        let mut s = dddid_server();
        s.handle(&[0x2C, 0x01, 0xF2, 0x00, 0x10, 0x00, 0x03, 0x02]);
        // The source record shrank below the defined slice.
        s.set_did(0x1000, vec![0xAA]);
        assert_eq!(
            s.handle(&[0x22, 0xF2, 0x00]),
            vec![0x7F, 0x22, nrc::CONDITIONS_NOT_CORRECT]
        );
    }
}
