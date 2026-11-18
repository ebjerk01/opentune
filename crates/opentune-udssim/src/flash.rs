//! A **flashable ECU** model for the simulator: programming session with
//! preconditions, SecurityAccess at a programming level, the ISO 14229
//! eraseMemory / checkProgrammingDependencies routines plus a configurable
//! checksum routine, RequestDownload / TransferData / RequestTransferExit into a
//! persistent flash memory, `responsePending` on slow erase, and fault
//! injection (power loss mid-transfer, a lost TransferData response, erase
//! refused, corrupted write, failing checksum / dependency routines).
//!
//! Enable it on a server with [`UdsServer::enable_flash`](crate::UdsServer::enable_flash).
//! Without it the server behaves exactly as before (the generic download /
//! upload model).
//!
//! # Wire conventions (ISO 14229-1, clean-room)
//!
//! - `10 <programming>`: refused with `conditionsNotCorrect` (0x22) unless the
//!   ECU is already in the extended session (when
//!   [`FlashConfig::require_extended_session`]) and the programming
//!   preconditions hold ([`FlashConfig::preconditions_ok`]).
//! - Erase, download and the check routines need the programming session
//!   (`0x22` / `serviceNotSupportedInActiveSession` 0x7F otherwise) and the
//!   programming security level unlocked (`securityAccessDenied` 0x33).
//! - `31 01 FF00 <ALFID> <address> <size>` — eraseMemory (ISO 14229-1 Annex F
//!   routine id 0xFF00). Status record `00` = erased. Answered after
//!   [`FlashConfig::erase_pending`] `responsePending` (0x78) frames.
//! - `34 <DFI> <ALFID> <address> <size>` — RequestDownload. The DFI must be in
//!   [`FlashConfig::accepted_data_formats`] (`requestOutOfRange` 0x31) and the
//!   range must lie in memory erased since its last programming
//!   (`uploadDownloadNotAccepted` 0x70). Answer `74 20 <maxNumberOfBlockLength>`;
//!   per ISO the block length counts the SID and the block counter, so each
//!   TransferData carries at most `maxNumberOfBlockLength - 2` data bytes.
//! - `36 <counter> <data>` — TransferData. The counter starts at 0x01 and wraps
//!   0xFF→0x00. Repeating the **previous** accepted counter is a retransmit
//!   (ISO 14229-1 §14.4): answered positively and **not** written again. Any
//!   other mismatch is `wrongBlockSequenceCounter` (0x73). More data than
//!   announced is `transferDataSuspended` (0x71).
//! - `37` — RequestTransferExit; `requestSequenceError` (0x24) until every
//!   announced byte arrived.
//! - `31 01 <check> [<ALFID> <address> <size>] [<crc16>]` — checksum routine
//!   (default id 0x0202, configurable). The record is the simulator's own
//!   convention: an explicit range plus CRC-16/CCITT-FALSE, just a CRC (over the
//!   last download), or nothing (the last download is checked against its own
//!   CRC, i.e. only completeness). Status `00` = correct, `01` = wrong.
//! - `31 01 FF01` — checkProgrammingDependencies (ISO 14229-1 Annex F 0xFF01):
//!   status `00` when no download is incomplete and every completed download
//!   passed the checksum routine, else `01`.
//! - `23` — ReadMemoryByAddress of flash memory, when
//!   [`FlashConfig::readable`] (else `requestOutOfRange`).
//! - `11` — ECUReset: volatile state is lost, the flash memory is kept.
//!
//! The memory persists across resets and power losses, so a read-back after a
//! flash (or after an interruption) sees exactly what was programmed.

/// Routine identifiers used by the flash model.
pub mod routine {
    /// ISO 14229-1 Annex F `eraseMemory`.
    pub const ERASE_MEMORY: u16 = 0xFF00;
    /// ISO 14229-1 Annex F `checkProgrammingDependencies`.
    pub const CHECK_PROGRAMMING_DEPENDENCIES: u16 = 0xFF01;
    /// The default checksum routine id (vehicle-manufacturer range; widely used
    /// for "check memory" — configurable per ECU).
    pub const CHECK_MEMORY: u16 = 0x0202;
}

/// CRC-16/CCITT-FALSE (poly 0x1021, init 0xFFFF, no reflection, no final XOR)
/// — the simulator's checksum-routine CRC.
pub fn crc16_ccitt_false(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in data {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// One flash region of the simulated ECU: its base address and initial
/// contents (the length of `data` is the region size).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlashRegion {
    /// First address of the region.
    pub address: u32,
    /// Initial contents ("stock" software).
    pub data: Vec<u8>,
}

impl FlashRegion {
    /// A region at `address` holding `data`.
    pub fn new(address: u32, data: impl Into<Vec<u8>>) -> Self {
        Self {
            address,
            data: data.into(),
        }
    }

    fn end(&self) -> u64 {
        u64::from(self.address) + self.data.len() as u64
    }
}

/// Faults the flash model can inject. One-shot faults fire once and clear
/// themselves; the boolean / NRC ones stay in force until changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FlashFaults {
    /// Lose power right after this many TransferData blocks have been written
    /// (counted over the model's lifetime): that block **is** written, its
    /// response is never sent, and the ECU reboots (default session, locked,
    /// transfer aborted; the interrupted range stays invalid). One-shot.
    pub power_loss_after_blocks: Option<usize>,
    /// Write the Nth TransferData block (1-based, lifetime count) but lose its
    /// response once — the tester must retransmit with the same counter.
    /// One-shot.
    pub drop_transfer_response_at: Option<usize>,
    /// Refuse every eraseMemory with this NRC (e.g. 0x22).
    pub refuse_erase: Option<u8>,
    /// Flip the first byte of the Nth TransferData block (1-based, lifetime
    /// count) as it is stored, so the checksum routine and a read-back fail.
    /// One-shot.
    pub corrupt_block_at: Option<usize>,
    /// The checksum routine reports failure regardless of the memory.
    pub fail_check: bool,
    /// checkProgrammingDependencies reports failure regardless of state.
    pub fail_dependencies: bool,
}

/// Configuration of the flashable ECU.
#[derive(Debug, Clone)]
pub struct FlashConfig {
    /// The flash memory: regions with their initial contents.
    pub regions: Vec<FlashRegion>,
    /// The programming session id (0x02).
    pub programming_session: u8,
    /// Only enter the programming session from the extended session (0x03).
    pub require_extended_session: bool,
    /// Programming preconditions (engine off, speed 0, …) hold. `false`
    /// refuses `10 <programming>` with conditionsNotCorrect.
    pub preconditions_ok: bool,
    /// The request-seed (odd) SecurityAccess level that unlocks programming.
    pub security_level: u8,
    /// eraseMemory routine id.
    pub erase_routine: u16,
    /// Checksum routine id.
    pub check_routine: u16,
    /// checkProgrammingDependencies routine id.
    pub dependency_routine: u16,
    /// `responsePending` (0x78) frames before an erase result.
    pub erase_pending: usize,
    /// `responsePending` frames before a checksum-routine result.
    pub check_pending: usize,
    /// `maxNumberOfBlockLength` reported by RequestDownload (includes SID and
    /// counter byte).
    pub max_block_len: u32,
    /// dataFormatIdentifiers RequestDownload accepts (0x00 = plain).
    pub accepted_data_formats: Vec<u8>,
    /// Allow ReadMemoryByAddress (0x23) of flash memory.
    pub readable: bool,
    /// The value an erased byte reads as.
    pub erased_value: u8,
    /// Injected faults.
    pub faults: FlashFaults,
}

impl Default for FlashConfig {
    fn default() -> Self {
        Self {
            regions: vec![FlashRegion::new(0x0001_0000, vec![0x00; 0x1000])],
            programming_session: 0x02,
            require_extended_session: true,
            preconditions_ok: true,
            security_level: 0x11,
            erase_routine: routine::ERASE_MEMORY,
            check_routine: routine::CHECK_MEMORY,
            dependency_routine: routine::CHECK_PROGRAMMING_DEPENDENCIES,
            erase_pending: 0,
            check_pending: 0,
            max_block_len: 0x0102,
            accepted_data_formats: vec![0x00],
            readable: true,
            erased_value: 0xFF,
            faults: FlashFaults::default(),
        }
    }
}

/// Counters for assertions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FlashStats {
    /// Successful eraseMemory routines.
    pub erases: usize,
    /// Accepted RequestDownloads.
    pub downloads: usize,
    /// TransferData blocks written to memory (retransmits excluded).
    pub blocks_written: usize,
    /// Retransmitted TransferData blocks acknowledged without rewriting.
    pub duplicate_blocks: usize,
    /// Completed RequestTransferExits.
    pub transfer_exits: usize,
    /// Checksum routines that passed / failed.
    pub checks_ok: usize,
    /// Checksum routines that failed.
    pub checks_failed: usize,
    /// Injected power losses.
    pub power_losses: usize,
    /// ECU resets (0x11) seen.
    pub resets: usize,
}

#[derive(Debug, Clone)]
struct Transfer {
    address: u32,
    size: u32,
    cursor: u32,
    next_seq: u8,
    last_seq: Option<u8>,
}

/// Range `[start, end)`.
type Span = (u64, u64);

/// The flashable-ECU state. Lives inside a [`UdsServer`](crate::UdsServer).
#[derive(Debug, Clone)]
pub struct FlashEcu {
    /// The configuration (faults may be changed between requests).
    pub config: FlashConfig,
    memory: Vec<FlashRegion>,
    /// Erased and not yet (re)programmed.
    erased: Vec<Span>,
    /// Downloads started but never completed (invalid until re-erased).
    incomplete: Vec<Span>,
    /// Downloads completed but not yet confirmed by the checksum routine.
    unchecked: Vec<Span>,
    last_download: Option<Span>,
    transfer: Option<Transfer>,
    pending: usize,
    stats: FlashStats,
}

/// What the flash model decided about one request.
pub(crate) enum Outcome {
    /// Not a flash request: the generic server handles it.
    Pass,
    /// Answer with these bytes.
    Reply(Vec<u8>),
    /// Send nothing (lost response).
    Silent,
    /// Send nothing and reboot the ECU (power loss).
    PowerLoss,
}

/// The server state the flash model needs to decide.
pub(crate) struct Ctx {
    pub session: u8,
    pub unlocked_level: Option<u8>,
    pub generic_transfer_active: bool,
}

const SID_SESSION: u8 = 0x10;
const SID_RESET: u8 = 0x11;
const SID_READ_MEM: u8 = 0x23;
const SID_ROUTINE: u8 = 0x31;
const SID_DOWNLOAD: u8 = 0x34;
const SID_TRANSFER: u8 = 0x36;
const SID_EXIT: u8 = 0x37;

const NRC_INCORRECT_LENGTH: u8 = 0x13;
const NRC_CONDITIONS: u8 = 0x22;
const NRC_SEQUENCE: u8 = 0x24;
const NRC_OUT_OF_RANGE: u8 = 0x31;
const NRC_SECURITY: u8 = 0x33;
const NRC_NOT_ACCEPTED: u8 = 0x70;
const NRC_SUSPENDED: u8 = 0x71;
const NRC_WRONG_COUNTER: u8 = 0x73;
const NRC_NOT_IN_SESSION: u8 = 0x7F;
const NRC_SUBFUNCTION: u8 = 0x12;

fn neg(sid: u8, nrc: u8) -> Outcome {
    Outcome::Reply(vec![0x7F, sid, nrc])
}

fn be(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0u64, |v, b| (v << 8) | u64::from(*b))
}

/// Parse `<ALFID> <address> <size>` at the start of `rec`; returns the span and
/// the bytes consumed.
fn parse_range(rec: &[u8]) -> Option<(Span, usize)> {
    let alfid = *rec.first()?;
    let a = usize::from(alfid & 0x0F);
    let s = usize::from(alfid >> 4);
    if !(1..=4).contains(&a) || !(1..=4).contains(&s) || rec.len() < 1 + a + s {
        return None;
    }
    let addr = be(&rec[1..1 + a]);
    let size = be(&rec[1 + a..1 + a + s]);
    Some(((addr, addr + size), 1 + a + s))
}

fn contains(outer: Span, inner: Span) -> bool {
    inner.0 >= outer.0 && inner.1 <= outer.1
}

fn overlaps(a: Span, b: Span) -> bool {
    a.0 < b.1 && b.0 < a.1
}

/// `spans` minus `cut`.
fn subtract(spans: &mut Vec<Span>, cut: Span) {
    let mut out = Vec::with_capacity(spans.len() + 1);
    for &s in spans.iter() {
        if !overlaps(s, cut) {
            out.push(s);
            continue;
        }
        if s.0 < cut.0 {
            out.push((s.0, cut.0));
        }
        if cut.1 < s.1 {
            out.push((cut.1, s.1));
        }
    }
    *spans = out;
}

impl FlashEcu {
    /// A flashable ECU in its initial state.
    pub fn new(config: FlashConfig) -> Self {
        Self {
            memory: config.regions.clone(),
            config,
            erased: Vec::new(),
            incomplete: Vec::new(),
            unchecked: Vec::new(),
            last_download: None,
            transfer: None,
            pending: 0,
            stats: FlashStats::default(),
        }
    }

    /// `len` bytes of flash memory at `address`, if the span is inside one
    /// region.
    pub fn read(&self, address: u32, len: usize) -> Option<Vec<u8>> {
        let span = (u64::from(address), u64::from(address) + len as u64);
        let r = self
            .memory
            .iter()
            .find(|r| contains((u64::from(r.address), r.end()), span))?;
        let off = (span.0 - u64::from(r.address)) as usize;
        Some(r.data[off..off + len].to_vec())
    }

    /// Counters for assertions.
    pub fn stats(&self) -> &FlashStats {
        &self.stats
    }

    /// Whether the programmed application is consistent: no interrupted
    /// download and every completed download passed the checksum routine.
    pub fn is_valid(&self) -> bool {
        self.incomplete.is_empty() && self.unchecked.is_empty()
    }

    /// Whether a download is in progress.
    pub fn transfer_active(&self) -> bool {
        self.transfer.is_some()
    }

    /// Take (and clear) the number of `responsePending` frames to interpose
    /// before the last reply.
    pub(crate) fn take_pending(&mut self) -> usize {
        std::mem::take(&mut self.pending)
    }

    /// Volatile state lost on a reset / reboot. Memory and validity persist.
    pub(crate) fn on_reboot(&mut self) {
        if let Some(t) = self.transfer.take() {
            // The interrupted range stays invalid until erased again.
            let span = (
                u64::from(t.address),
                u64::from(t.address) + u64::from(t.size),
            );
            if !self.incomplete.contains(&span) {
                self.incomplete.push(span);
            }
        }
    }

    fn region_of(&self, span: Span) -> Option<usize> {
        self.memory
            .iter()
            .position(|r| contains((u64::from(r.address), r.end()), span))
    }

    fn write(&mut self, address: u64, data: &[u8]) {
        let span = (address, address + data.len() as u64);
        if let Some(i) = self.region_of(span) {
            let r = &mut self.memory[i];
            let off = (address - u64::from(r.address)) as usize;
            r.data[off..off + data.len()].copy_from_slice(data);
        }
    }

    fn crc_of(&self, span: Span) -> Option<u16> {
        let len = usize::try_from(span.1 - span.0).ok()?;
        let addr = u32::try_from(span.0).ok()?;
        self.read(addr, len).map(|d| crc16_ccitt_false(&d))
    }

    /// Decide one request. `Pass` leaves it to the generic server.
    pub(crate) fn handle(&mut self, req: &[u8], ctx: Ctx) -> Outcome {
        let Some(&sid) = req.first() else {
            return Outcome::Pass;
        };
        match sid {
            SID_SESSION => self.session(req, &ctx),
            SID_RESET => {
                self.stats.resets += 1;
                self.on_reboot();
                Outcome::Pass
            }
            SID_READ_MEM => self.read_memory(req),
            SID_ROUTINE => self.routine(req, &ctx),
            SID_DOWNLOAD => self.download(req, &ctx),
            SID_TRANSFER | SID_EXIT if self.transfer.is_none() && ctx.generic_transfer_active => {
                // A generic upload (0x35) is running: not ours.
                Outcome::Pass
            }
            SID_TRANSFER => self.transfer_data(req, &ctx),
            SID_EXIT => self.transfer_exit(&ctx),
            _ => Outcome::Pass,
        }
    }

    fn session(&mut self, req: &[u8], ctx: &Ctx) -> Outcome {
        let Some(&sub) = req.get(1) else {
            return Outcome::Pass;
        };
        let target = sub & 0x7F;
        if target != ctx.session && req.len() == 2 {
            // A session transition aborts an open download (the range stays
            // invalid until it is erased again).
            self.on_reboot();
        }
        if target != self.config.programming_session || ctx.session == target {
            return Outcome::Pass;
        }
        if self.config.require_extended_session && ctx.session != 0x03 {
            return neg(SID_SESSION, NRC_CONDITIONS);
        }
        if !self.config.preconditions_ok {
            return neg(SID_SESSION, NRC_CONDITIONS);
        }
        Outcome::Pass
    }

    fn read_memory(&mut self, req: &[u8]) -> Outcome {
        let Some((span, used)) = req.get(1..).and_then(parse_range) else {
            return Outcome::Pass;
        };
        if req.len() != 1 + used || span.1 == span.0 {
            return Outcome::Pass;
        }
        if self.region_of(span).is_none() {
            return Outcome::Pass;
        }
        if !self.config.readable {
            return neg(SID_READ_MEM, NRC_OUT_OF_RANGE);
        }
        let data = self
            .read(span.0 as u32, (span.1 - span.0) as usize)
            .unwrap_or_default();
        let mut out = vec![0x63];
        out.extend_from_slice(&data);
        Outcome::Reply(out)
    }

    /// Programming session + security gates shared by the flash services.
    fn gate(&self, sid: u8, ctx: &Ctx, session_nrc: u8) -> Option<Outcome> {
        if ctx.session != self.config.programming_session {
            return Some(neg(sid, session_nrc));
        }
        if ctx.unlocked_level != Some(self.config.security_level) {
            return Some(neg(sid, NRC_SECURITY));
        }
        None
    }

    fn routine(&mut self, req: &[u8], ctx: &Ctx) -> Outcome {
        if req.len() < 4 {
            return Outcome::Pass;
        }
        let rid = u16::from_be_bytes([req[2], req[3]]);
        let cfg = &self.config;
        if rid != cfg.erase_routine && rid != cfg.check_routine && rid != cfg.dependency_routine {
            return Outcome::Pass;
        }
        if req[1] & 0x7F != 0x01 {
            return neg(SID_ROUTINE, NRC_SUBFUNCTION);
        }
        if let Some(refusal) = self.gate(SID_ROUTINE, ctx, NRC_CONDITIONS) {
            return refusal;
        }
        if self.transfer.is_some() {
            return neg(SID_ROUTINE, NRC_SEQUENCE);
        }
        let rec = &req[4..];
        let reply = |status: u8| Outcome::Reply(vec![0x71, 0x01, req[2], req[3], status]);
        if rid == self.config.erase_routine {
            let Some((span, used)) = parse_range(rec) else {
                return neg(SID_ROUTINE, NRC_INCORRECT_LENGTH);
            };
            if used != rec.len() {
                return neg(SID_ROUTINE, NRC_INCORRECT_LENGTH);
            }
            let Some(i) = self.region_of(span).filter(|_| span.1 > span.0) else {
                return neg(SID_ROUTINE, NRC_OUT_OF_RANGE);
            };
            if let Some(nrc) = self.config.faults.refuse_erase {
                return neg(SID_ROUTINE, nrc);
            }
            let r = &mut self.memory[i];
            let off = (span.0 - u64::from(r.address)) as usize;
            let len = (span.1 - span.0) as usize;
            r.data[off..off + len].fill(self.config.erased_value);
            subtract(&mut self.erased, span);
            self.erased.push(span);
            for list in [&mut self.incomplete, &mut self.unchecked] {
                list.retain(|s| !overlaps(*s, span));
            }
            self.stats.erases += 1;
            self.pending = self.config.erase_pending;
            return reply(0x00);
        }
        if rid == self.config.check_routine {
            // [<ALFID> <addr> <size>] [<crc16>]
            let (span, crc) = match rec.len() {
                0 => match self.last_download {
                    Some(s) => (s, None),
                    None => return neg(SID_ROUTINE, NRC_SEQUENCE),
                },
                2 => match self.last_download {
                    Some(s) => (s, Some(u16::from_be_bytes([rec[0], rec[1]]))),
                    None => return neg(SID_ROUTINE, NRC_SEQUENCE),
                },
                _ => match parse_range(rec) {
                    Some((s, used)) if rec.len() == used + 2 => {
                        (s, Some(u16::from_be_bytes([rec[used], rec[used + 1]])))
                    }
                    Some((s, used)) if rec.len() == used => (s, None),
                    _ => return neg(SID_ROUTINE, NRC_INCORRECT_LENGTH),
                },
            };
            let Some(actual) = self.crc_of(span) else {
                return neg(SID_ROUTINE, NRC_OUT_OF_RANGE);
            };
            let complete = !self.incomplete.iter().any(|s| overlaps(*s, span));
            let crc_ok = match crc {
                Some(c) => c == actual,
                None => true,
            };
            let ok = complete && crc_ok && !self.config.faults.fail_check;
            if ok {
                self.unchecked.retain(|s| !contains(span, *s));
                self.stats.checks_ok += 1;
            } else {
                self.stats.checks_failed += 1;
            }
            self.pending = self.config.check_pending;
            return reply(if ok { 0x00 } else { 0x01 });
        }
        // checkProgrammingDependencies.
        let ok = self.is_valid() && !self.config.faults.fail_dependencies;
        reply(if ok { 0x00 } else { 0x01 })
    }

    fn download(&mut self, req: &[u8], ctx: &Ctx) -> Outcome {
        if req.len() < 3 {
            return neg(SID_DOWNLOAD, NRC_INCORRECT_LENGTH);
        }
        if let Some(refusal) = self.gate(SID_DOWNLOAD, ctx, NRC_NOT_IN_SESSION) {
            return refusal;
        }
        let Some((span, used)) = parse_range(&req[2..]) else {
            return neg(SID_DOWNLOAD, NRC_INCORRECT_LENGTH);
        };
        if req.len() != 2 + used {
            return neg(SID_DOWNLOAD, NRC_INCORRECT_LENGTH);
        }
        if self.transfer.is_some() {
            return neg(SID_DOWNLOAD, NRC_CONDITIONS);
        }
        if !self.config.accepted_data_formats.contains(&req[1]) {
            return neg(SID_DOWNLOAD, NRC_OUT_OF_RANGE);
        }
        if span.1 == span.0 || self.region_of(span).is_none() {
            return neg(SID_DOWNLOAD, NRC_OUT_OF_RANGE);
        }
        // Flash must be erased before it can be programmed.
        if !self.erased.iter().any(|e| contains(*e, span)) {
            return neg(SID_DOWNLOAD, NRC_NOT_ACCEPTED);
        }
        subtract(&mut self.erased, span);
        self.incomplete.push(span);
        self.transfer = Some(Transfer {
            address: span.0 as u32,
            size: (span.1 - span.0) as u32,
            cursor: 0,
            next_seq: 0x01,
            last_seq: None,
        });
        self.stats.downloads += 1;
        let mbl = self.config.max_block_len;
        Outcome::Reply(vec![0x74, 0x20, (mbl >> 8) as u8, mbl as u8])
    }

    fn transfer_data(&mut self, req: &[u8], ctx: &Ctx) -> Outcome {
        if ctx.session != self.config.programming_session {
            return neg(SID_TRANSFER, NRC_NOT_IN_SESSION);
        }
        if req.len() < 2 {
            return neg(SID_TRANSFER, NRC_INCORRECT_LENGTH);
        }
        let max_data = (self.config.max_block_len as usize).saturating_sub(2);
        let Some(t) = self.transfer.as_mut() else {
            return neg(SID_TRANSFER, NRC_SEQUENCE);
        };
        let seq = req[1];
        let data = &req[2..];
        if seq != t.next_seq {
            if Some(seq) == t.last_seq {
                // ISO 14229-1 §14.4: a repeat of the previous block is a
                // retransmit — acknowledge it, do not write it again.
                self.stats.duplicate_blocks += 1;
                return Outcome::Reply(vec![0x76, seq]);
            }
            return neg(SID_TRANSFER, NRC_WRONG_COUNTER);
        }
        if data.len() > max_data {
            return neg(SID_TRANSFER, NRC_INCORRECT_LENGTH);
        }
        if u64::from(t.cursor) + data.len() as u64 > u64::from(t.size) {
            return neg(SID_TRANSFER, NRC_SUSPENDED);
        }
        let at = u64::from(t.address) + u64::from(t.cursor);
        t.cursor += data.len() as u32;
        t.last_seq = Some(seq);
        t.next_seq = seq.wrapping_add(1);
        self.stats.blocks_written += 1;
        let n = self.stats.blocks_written;
        let mut stored = data.to_vec();
        if self.config.faults.corrupt_block_at == Some(n) {
            self.config.faults.corrupt_block_at = None;
            if let Some(b) = stored.first_mut() {
                *b ^= 0xFF;
            }
        }
        self.write(at, &stored);
        if self.config.faults.power_loss_after_blocks == Some(n) {
            self.config.faults.power_loss_after_blocks = None;
            self.stats.power_losses += 1;
            return Outcome::PowerLoss;
        }
        if self.config.faults.drop_transfer_response_at == Some(n) {
            self.config.faults.drop_transfer_response_at = None;
            return Outcome::Silent;
        }
        Outcome::Reply(vec![0x76, seq])
    }

    fn transfer_exit(&mut self, ctx: &Ctx) -> Outcome {
        if ctx.session != self.config.programming_session {
            return neg(SID_EXIT, NRC_NOT_IN_SESSION);
        }
        let Some(t) = self.transfer.as_ref() else {
            return neg(SID_EXIT, NRC_SEQUENCE);
        };
        if t.cursor != t.size {
            return neg(SID_EXIT, NRC_SEQUENCE);
        }
        let span = (
            u64::from(t.address),
            u64::from(t.address) + u64::from(t.size),
        );
        self.transfer = None;
        self.incomplete.retain(|s| *s != span);
        self.unchecked.push(span);
        self.last_download = Some(span);
        self.stats.transfer_exits += 1;
        Outcome::Reply(vec![0x77])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc16_ccitt_false_check_value() {
        // The catalogue check value for "123456789".
        assert_eq!(crc16_ccitt_false(b"123456789"), 0x29B1);
    }

    #[test]
    fn subtract_splits_spans() {
        let mut v = vec![(0, 100)];
        subtract(&mut v, (10, 20));
        assert_eq!(v, vec![(0, 10), (20, 100)]);
        subtract(&mut v, (0, 10));
        assert_eq!(v, vec![(20, 100)]);
    }
}
