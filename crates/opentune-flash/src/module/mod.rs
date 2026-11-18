//! **UDS module flashing** — programming a control module over ISO 14229 from
//! a decoded flash container, behind the flash-safety policy.
//!
//! [`flash_module`] runs, over any [`DiagTransport`]:
//!
//! 1. **Validate** (no device access): every selected block is plain, addressed
//!    and inside its erase region; writable regions, checksum coverage and the
//!    target identity are checked against the [`FlashSafetyPolicy`].
//! 2. **Identify** (`22` on the identity DIDs) and match the container's
//!    target identity — a mismatch is refused unless forced.
//! 3. **Voltage gate** (the policy's, fail-closed under production).
//! 4. **Backup**: extended session, then `23` ReadMemoryByAddress of every
//!    erase region. If the ECU refuses reads, no backup is possible: that is
//!    recorded and needs an explicit override (`allow_no_backup`).
//! 5. **Programming session + security** through the caller's [`SeedKey`] hook
//!    — a locked ECU without a hook is refused *before* anything is erased.
//! 6. **Per erase region**: `31 01 FF00` erase → per block `34` (plain,
//!    dataFormatIdentifier 0x00) → `36` chunks sized from the ECU's
//!    `maxNumberOfBlockLength` → `37` → read-back verify (when readable) →
//!    checksum routine (`31 01 0202`, configurable).
//! 7. **checkProgrammingDependencies** (`31 01 FF01`) and **ECUReset** (`11`).
//!
//! A state file ([`ModuleFlashOptions::state_path`]) records per-block progress,
//! the bound ECU, the image hash, the original backup (sidecar) and a
//! transcript. An interrupted run restarts from the erase region that failed
//! (it is erased again); finished regions are re-verified, not trusted blindly.
//!
//! [`plan_module_flash`] lists every request the run would send, without
//! sending anything.
//!
//! **Validation state: synthetic-tested** (the `opentune-udssim` flashable ECU).
//! Nothing here has programmed a real module yet. A real module needs a proper
//! CAN adapter (SocketCAN / gs_usb / J2534) — not an ELM327, whose
//! single-frame / buffer limits make block transfers unreliable.
use crate::hash::sha256_hex;
use crate::policy::{FlashSafetyPolicy, PolicyViolation, TargetIdentity};
use opentune_core::protocol::seedkey::SeedKey;
use opentune_core::protocol::uds::{encode_address_and_length, seed_is_unlocked};
use opentune_core::protocol::{DiagTransport, UdsClient};
use opentune_core::{EcuId, Error, Interface, MemoryRegion, Result};
use std::path::PathBuf;

pub mod image;
pub mod plan;
pub mod state;

pub use image::{BlockEncoding, ModuleBlock, ModuleImage};
pub use plan::{describe_request, plan_module_flash, ModulePlan, PlanStep};
pub use state::{BlockState, BlockStatus, ModuleState};

use image::span;
use state::BlockState as StBlock;

/// The checksum routine's option record layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckRecord {
    /// No record: the ECU checks the last download on its own.
    Empty,
    /// The block's CRC-16/CCITT-FALSE, big-endian.
    Crc16,
    /// `<ALFID> <address> <size> <crc16>` of the block.
    AddressLengthCrc16,
}

/// The identification DIDs read to match the ECU (ISO 14229-1 Annex C).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityDids {
    /// Part / hardware numbers (default F187 spare part, F191 hardware).
    pub part: Vec<u16>,
    /// Software numbers (default F188 software number, F189 software version).
    pub software: Vec<u16>,
}

impl Default for IdentityDids {
    fn default() -> Self {
        Self {
            part: vec![0xF187, 0xF191],
            software: vec![0xF188, 0xF189],
        }
    }
}

/// The ECU's programming procedure: sessions, security level, routine ids and
/// record layouts. The defaults follow ISO 14229-1 where it defines a value
/// (sessions, 0xFF00 / 0xFF01) and common practice elsewhere (0x0202).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleProtocol {
    /// Session entered before the programming session (and for the backup
    /// read). `None` goes straight to programming.
    pub extended_session: Option<u8>,
    /// The programming session.
    pub programming_session: u8,
    /// Request-seed (odd) SecurityAccess level for programming.
    pub security_level: u8,
    /// eraseMemory routine id (record `<ALFID> <address> <size>`).
    pub erase_routine: u16,
    /// Checksum routine id run after each block, if any.
    pub check_routine: Option<u16>,
    /// The checksum routine's record layout.
    pub check_record: CheckRecord,
    /// The first routine-status byte that means "correct".
    pub check_ok_status: u8,
    /// checkProgrammingDependencies routine id, if any.
    pub dependency_routine: Option<u16>,
    /// addressAndLengthFormatIdentifier for erase / download / reads.
    pub alfid: u8,
    /// ReadMemoryByAddress chunk for backup and read-back verify.
    pub read_chunk: u32,
    /// Cap on TransferData payload below the ECU's `maxNumberOfBlockLength - 2`.
    pub max_transfer_payload: Option<usize>,
    /// ECUReset type sent at the end (`None` = no reset).
    pub reset_type: Option<u8>,
    /// Identification DIDs.
    pub identity: IdentityDids,
}

impl Default for ModuleProtocol {
    fn default() -> Self {
        Self {
            extended_session: Some(0x03),
            programming_session: 0x02,
            security_level: 0x11,
            erase_routine: 0xFF00,
            check_routine: Some(0x0202),
            check_record: CheckRecord::AddressLengthCrc16,
            check_ok_status: 0x00,
            dependency_routine: Some(0xFF01),
            alfid: 0x44,
            read_chunk: 0x80,
            max_transfer_payload: None,
            reset_type: Some(0x01),
            identity: IdentityDids::default(),
        }
    }
}

/// What to flash and under which rules.
#[derive(Debug, Clone)]
pub struct ModuleFlashOptions {
    /// The safety policy (use [`FlashSafetyPolicy::production`] for a real
    /// module).
    pub policy: FlashSafetyPolicy,
    /// The ECU's programming procedure.
    pub protocol: ModuleProtocol,
    /// Only these block indices (into the image), in image order. `None` =
    /// all.
    pub blocks: Option<Vec<usize>>,
    /// Flash even if the connected ECU does not match (or does not report)
    /// the container's identity. Logged.
    pub force_identity: bool,
    /// Proceed when the ECU refuses reads, so no backup is possible. Logged.
    pub allow_no_backup: bool,
    /// Regions it is legal to write (each erase region must lie inside one).
    /// Empty = the container's declared erase regions.
    pub writable_regions: Vec<MemoryRegion>,
    /// The resumable state file. `None` = no resume (nothing on disk).
    pub state_path: Option<PathBuf>,
}

impl ModuleFlashOptions {
    /// Options with the default protocol and no filters.
    pub fn new(policy: FlashSafetyPolicy) -> Self {
        Self {
            policy,
            protocol: ModuleProtocol::default(),
            blocks: None,
            force_identity: false,
            allow_no_backup: false,
            writable_regions: Vec::new(),
            state_path: None,
        }
    }
}

/// The battery voltage reading for the gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Voltage {
    /// Measured millivolts, if the adapter could read it.
    pub millivolts: Option<u32>,
    /// Whether the adapter advertises voltage measurement.
    pub capable: bool,
}

/// Step of a module flash, for progress and the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModulePhase {
    /// Structural checks.
    Validate,
    /// Reading the identity DIDs.
    Identify,
    /// Voltage gate.
    Voltage,
    /// Reading the original.
    Backup,
    /// Session changes.
    Session,
    /// SecurityAccess.
    Security,
    /// eraseMemory.
    Erase,
    /// RequestDownload + TransferData.
    Download,
    /// RequestTransferExit.
    TransferExit,
    /// Read-back verify.
    Verify,
    /// Checksum routine.
    Check,
    /// checkProgrammingDependencies.
    Dependencies,
    /// ECUReset.
    Reset,
    /// Finished.
    Done,
}

/// One progress report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleProgress {
    /// The step.
    pub phase: ModulePhase,
    /// The block, for per-block steps.
    pub block: Option<usize>,
    /// Bytes done (download / backup / verify), else 0.
    pub done: u64,
    /// Bytes total, else 0.
    pub total: u64,
    /// What is happening.
    pub message: String,
}

/// The identity the ECU reported: `(DID, value)` pairs, in read order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectedIdentity {
    /// Part / hardware DID values.
    pub part: Vec<(u16, String)>,
    /// Software DID values.
    pub software: Vec<(u16, String)>,
}

impl ConnectedIdentity {
    /// Whether nothing could be read.
    pub fn is_empty(&self) -> bool {
        self.part.is_empty() && self.software.is_empty()
    }

    /// A stable fingerprint for binding the resume state.
    pub fn fingerprint(&self) -> String {
        self.part
            .iter()
            .chain(&self.software)
            .map(|(d, v)| format!("{d:04X}={v}"))
            .collect::<Vec<_>>()
            .join("|")
    }

    /// Whether `target` matches any reported part or software value.
    pub fn matches(&self, target: &TargetIdentity) -> bool {
        let parts = self.part.iter().map(|(_, v)| EcuId {
            hw_number: Some(v.clone()),
            ..Default::default()
        });
        let sws = self.software.iter().map(|(_, v)| EcuId {
            sw_number: Some(v.clone()),
            ..Default::default()
        });
        parts.chain(sws).any(|id| target.matches(&id))
    }
}

/// How a block's write was verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyMethod {
    /// Read back with ReadMemoryByAddress and compared byte for byte (and the
    /// checksum routine passed, when configured).
    ReadBack,
    /// The ECU's checksum routine passed (memory not readable).
    EcuRoutine,
    /// Not verified (no read access and no checksum routine).
    None,
}

/// Per-block outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockReport {
    /// Index into the image.
    pub index: usize,
    /// Block id.
    pub id: String,
    /// The programmed span.
    pub region: MemoryRegion,
    /// Finished by an earlier run and confirmed on the device (not rewritten).
    pub reused: bool,
    /// How it was verified.
    pub verify: VerifyMethod,
}

/// The original contents of one erase region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionBackup {
    /// The erase region.
    pub region: MemoryRegion,
    /// Its bytes before the flash.
    pub data: Vec<u8>,
}

/// Outcome of a module flash.
#[derive(Debug, Clone)]
pub struct ModuleFlashReport {
    /// What the ECU reported.
    pub identity: ConnectedIdentity,
    /// The identity check was overridden.
    pub identity_forced: bool,
    /// The voltage at the gate.
    pub battery_millivolts: Option<u32>,
    /// The originals of every erase region (empty when no backup was possible).
    pub backup: Vec<RegionBackup>,
    /// Why there is no backup, when there is none.
    pub backup_note: Option<String>,
    /// This run continued an interrupted one.
    pub resumed: bool,
    /// Per-block outcomes, in image order.
    pub blocks: Vec<BlockReport>,
    /// checkProgrammingDependencies result, if run.
    pub dependencies_ok: Option<bool>,
    /// ECUReset was sent.
    pub reset_sent: bool,
    /// The transcript.
    pub log: Vec<String>,
}

/// Blocks sharing one erase region, programmed together.
#[derive(Debug, Clone)]
pub(crate) struct Group {
    pub erase: MemoryRegion,
    /// Indices into the image's blocks.
    pub blocks: Vec<usize>,
}

/// The selected block indices, in image order.
pub(crate) fn selection(image: &ModuleImage, opts: &ModuleFlashOptions) -> Result<Vec<usize>> {
    let sel: Vec<usize> = match &opts.blocks {
        None => (0..image.blocks.len()).collect(),
        Some(want) => {
            for &i in want {
                if i >= image.blocks.len() {
                    return Err(Error::Config(format!(
                        "block {i} does not exist (the image has {} block(s))",
                        image.blocks.len()
                    )));
                }
            }
            (0..image.blocks.len())
                .filter(|i| want.contains(i))
                .collect()
        }
    };
    if sel.is_empty() {
        return Err(Error::Config("no blocks selected to flash".into()));
    }
    Ok(sel)
}

/// Group the selection by erase region (first appearance order). Partially
/// overlapping erase regions are refused: erasing one would wipe part of the
/// other's blocks.
pub(crate) fn groups(image: &ModuleImage, sel: &[usize]) -> Result<Vec<Group>> {
    let mut out: Vec<Group> = Vec::new();
    for &i in sel {
        let e = image.blocks[i].erase;
        if let Some(g) = out.iter_mut().find(|g| g.erase == e) {
            g.blocks.push(i);
            continue;
        }
        let (s, t) = span(e);
        if let Some(g) = out.iter().find(|g| {
            let (gs, gt) = span(g.erase);
            s < gt && gs < t
        }) {
            return Err(Error::Safety(format!(
                "erase regions 0x{:08X} +0x{:X} and 0x{:08X} +0x{:X} overlap — refusing",
                e.address, e.length, g.erase.address, g.erase.length
            )));
        }
        out.push(Group {
            erase: e,
            blocks: vec![i],
        });
    }
    Ok(out)
}

/// Every check that needs no device: block shape, writable regions, checksum
/// coverage, a target identity to match. Shared by the run and the plan.
pub(crate) fn validate(
    image: &ModuleImage,
    sel: &[usize],
    opts: &ModuleFlashOptions,
) -> Result<Vec<Group>> {
    let mut problems = Vec::new();
    for &i in sel {
        let b = &image.blocks[i];
        if let Some(why) = b.refusal() {
            problems.push(format!("block {i} ({}): {why}", b.id));
        }
    }
    if !problems.is_empty() {
        return Err(Error::Safety(format!(
            "refusing to flash: {}",
            problems.join("; ")
        )));
    }
    let policy = &opts.policy;
    let groups = groups(image, sel)?;
    if !opts.writable_regions.is_empty() {
        for g in &groups {
            let (s, t) = span(g.erase);
            let inside = opts.writable_regions.iter().any(|w| {
                let (ws, wt) = span(*w);
                s >= ws && t <= wt
            });
            if !inside {
                return Err(PolicyViolation::RegionNotWritable { region: g.erase }.into());
            }
        }
    } else if policy.require_writable_region {
        if let Some(&i) = sel.iter().find(|&&i| !image.blocks[i].erase_declared) {
            return Err(Error::Safety(format!(
                "{} (block {i} has no erase region from the container or layout) — declare \
                 writable regions",
                PolicyViolation::NoWritableRegionsDeclared
            )));
        }
    }
    if policy.require_checksum_coverage && opts.protocol.check_routine.is_none() {
        if let Some(&i) = sel
            .iter()
            .find(|&&i| image.blocks[i].container_crc_ok != Some(true))
        {
            return Err(Error::Safety(format!(
                "block {i} has no integrity check: the container carries no CRC for it and no \
                 ECU checksum routine is configured — refusing"
            )));
        }
    }
    if policy.require_identity_match && image.identity.is_unspecified() && !opts.force_identity {
        return Err(Error::Safety(format!(
            "{} — supply the expected part/software number, or force",
            PolicyViolation::TargetIdentityUnspecified
        )));
    }
    Ok(groups)
}

/// Hash binding a state file to the exact selected blocks.
pub(crate) fn image_hash(image: &ModuleImage, sel: &[usize]) -> String {
    let mut buf = Vec::new();
    for &i in sel {
        let b = &image.blocks[i];
        buf.extend_from_slice(&(i as u32).to_be_bytes());
        buf.extend_from_slice(&b.address.to_be_bytes());
        buf.extend_from_slice(&b.erase.address.to_be_bytes());
        buf.extend_from_slice(&b.erase.length.to_be_bytes());
        buf.extend_from_slice(&(b.data.len() as u32).to_be_bytes());
        buf.extend_from_slice(&b.data);
    }
    sha256_hex(&buf)
}

/// The CRC-16/CCITT-FALSE the checksum routine record carries.
pub fn block_crc16(data: &[u8]) -> u16 {
    opentune_container::vbf::crc16_ccitt_false(data)
}

/// The checksum routine's option record for `block`.
pub(crate) fn check_record(p: &ModuleProtocol, block: &ModuleBlock) -> Result<Vec<u8>> {
    let crc = block_crc16(&block.data).to_be_bytes();
    Ok(match p.check_record {
        CheckRecord::Empty => Vec::new(),
        CheckRecord::Crc16 => crc.to_vec(),
        CheckRecord::AddressLengthCrc16 => {
            let mut r = vec![p.alfid];
            r.extend(encode_address_and_length(
                p.alfid,
                block.address,
                block.data.len() as u32,
            )?);
            r.extend_from_slice(&crc);
            r
        }
    })
}

/// The eraseMemory option record for `region`.
pub(crate) fn erase_record(p: &ModuleProtocol, region: MemoryRegion) -> Result<Vec<u8>> {
    let mut r = vec![p.alfid];
    r.extend(encode_address_and_length(
        p.alfid,
        region.address,
        region.length,
    )?);
    Ok(r)
}

/// `(address, size)` chunks of `region` for ReadMemoryByAddress.
pub(crate) fn read_chunks(region: MemoryRegion, chunk: u32) -> Vec<(u32, u32)> {
    let chunk = chunk.max(1);
    let mut out = Vec::new();
    let mut off = 0u32;
    while off < region.length {
        let n = chunk.min(region.length - off);
        out.push((region.address + off, n));
        off += n;
    }
    out
}

/// TransferData payload per request from the ECU's `maxNumberOfBlockLength`
/// (which counts the SID and the block counter).
pub(crate) fn payload_len(p: &ModuleProtocol, max_block_len: usize) -> usize {
    let n = max_block_len.saturating_sub(2).max(1);
    match p.max_transfer_payload {
        Some(cap) => n.min(cap.max(1)),
        None => n,
    }
}

fn text_of(v: &[u8]) -> String {
    let t: &[u8] = {
        let end = v
            .iter()
            .rposition(|&b| b != 0 && b != b' ')
            .map_or(0, |p| p + 1);
        &v[..end]
    };
    if !t.is_empty() && t.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
        String::from_utf8_lossy(t).into_owned()
    } else {
        v.iter().map(|b| format!("{b:02X}")).collect()
    }
}

/// The transcript and state persistence of one run.
struct Recorder<'p> {
    log: Vec<String>,
    state: Option<ModuleState>,
    path: Option<PathBuf>,
    progress: &'p mut dyn FnMut(&ModuleProgress),
}

impl Recorder<'_> {
    fn note(&mut self, phase: ModulePhase, block: Option<usize>, msg: impl Into<String>) {
        let msg = msg.into();
        (self.progress)(&ModuleProgress {
            phase,
            block,
            done: 0,
            total: 0,
            message: msg.clone(),
        });
        if let Some(st) = self.state.as_mut() {
            st.event(msg.clone());
        }
        self.log.push(msg);
    }

    fn bytes(&mut self, phase: ModulePhase, block: Option<usize>, done: u64, total: u64) {
        (self.progress)(&ModuleProgress {
            phase,
            block,
            done,
            total,
            message: String::new(),
        });
    }

    fn save(&self) -> Result<()> {
        if let (Some(st), Some(p)) = (&self.state, &self.path) {
            st.save(p)?;
        }
        Ok(())
    }

    fn set_status(&mut self, index: usize, status: BlockStatus, err: Option<String>) {
        if let Some(b) = self
            .state
            .as_mut()
            .and_then(|s| s.blocks.iter_mut().find(|b| b.index == index))
        {
            if status == BlockStatus::Started {
                b.attempts += 1;
            }
            b.status = status;
            b.last_error = err;
        }
    }
}

/// Flash `image` through a registry driver: reads its voltage, then runs
/// [`flash_module`] over its [`Interface::diag`] transport.
pub fn flash_module_on(
    iface: &mut dyn Interface,
    image: &ModuleImage,
    seed_key: Option<&dyn SeedKey>,
    opts: &ModuleFlashOptions,
    progress: &mut dyn FnMut(&ModuleProgress),
) -> Result<ModuleFlashReport> {
    let voltage = Voltage {
        millivolts: iface.battery_millivolts()?,
        capable: iface.capabilities().battery_voltage,
    };
    let diag = iface.diag().ok_or_else(|| {
        Error::Unsupported(
            "this driver offers no diagnostic transport, so it cannot flash a UDS module".into(),
        )
    })?;
    flash_module(diag, image, voltage, seed_key, opts, progress)
}

/// Flash `image` over `diag`. See the module docs for the sequence.
pub fn flash_module(
    diag: &mut dyn DiagTransport,
    image: &ModuleImage,
    voltage: Voltage,
    seed_key: Option<&dyn SeedKey>,
    opts: &ModuleFlashOptions,
    progress: &mut dyn FnMut(&ModuleProgress),
) -> Result<ModuleFlashReport> {
    let p = &opts.protocol;
    let policy = &opts.policy;
    let mut rec = Recorder {
        log: Vec::new(),
        state: None,
        path: opts.state_path.clone(),
        progress,
    };

    // 1. Structural checks.
    let sel = selection(image, opts)?;
    let groups = validate(image, &sel, opts)?;
    let total: u64 = sel.iter().map(|&i| image.blocks[i].data.len() as u64).sum();
    rec.note(
        ModulePhase::Validate,
        None,
        format!(
            "image ({}) : {} block(s), {} erase region(s), {total} bytes",
            image.source,
            sel.len(),
            groups.len()
        ),
    );

    let mut uds = UdsClient::new(diag);

    // 2. Identify.
    let mut identity = ConnectedIdentity::default();
    for (dids, out) in [
        (&p.identity.part, &mut identity.part),
        (&p.identity.software, &mut identity.software),
    ] {
        for &did in dids {
            if let Ok(v) = uds.read_data_by_id(did) {
                if !v.is_empty() {
                    out.push((did, text_of(&v)));
                }
            }
        }
    }
    rec.note(
        ModulePhase::Identify,
        None,
        format!("connected ECU: {}", identity.fingerprint()),
    );
    let mut identity_forced = false;
    if policy.require_identity_match || !image.identity.is_unspecified() {
        let ok = !image.identity.is_unspecified() && identity.matches(&image.identity);
        if !ok {
            let why = if identity.is_empty() {
                PolicyViolation::IdentityUnreadable
            } else if image.identity.is_unspecified() {
                PolicyViolation::TargetIdentityUnspecified
            } else {
                PolicyViolation::IdentityMismatch {
                    expected: image.identity.label.clone().unwrap_or_else(|| {
                        format!(
                            "{:?}/{:?}",
                            image.identity.part_numbers, image.identity.sw_numbers
                        )
                    }),
                    connected: identity.fingerprint(),
                }
            };
            if !opts.force_identity {
                return Err(why.into());
            }
            identity_forced = true;
            rec.note(
                ModulePhase::Identify,
                None,
                format!("IDENTITY CHECK OVERRIDDEN (forced): {why}"),
            );
        }
    }

    // Resume binding.
    let hash = image_hash(image, &sel);
    let prior = match &opts.state_path {
        Some(path) => ModuleState::load(path)?,
        None => None,
    };
    let resumed = prior.is_some();
    if let Some(st) = &prior {
        if st.image_sha256 != hash {
            return Err(Error::Safety(
                "the resume state is for a different image or block selection — refusing to \
                 resume (remove the state file to start over)"
                    .into(),
            ));
        }
        if st.ecu_fingerprint != identity.fingerprint() {
            return Err(PolicyViolation::CheckpointIdentityMismatch {
                bound: st.ecu_fingerprint.clone(),
                connected: identity.fingerprint(),
            }
            .into());
        }
    }

    // 3. Voltage gate.
    policy.check_voltage(voltage.millivolts, voltage.capable)?;
    rec.note(
        ModulePhase::Voltage,
        None,
        match voltage.millivolts {
            Some(mv) => format!("battery {mv} mV"),
            None => "battery voltage not readable".into(),
        },
    );

    // 4. Backup (or the one saved by the interrupted run).
    if let Some(s) = p.extended_session {
        uds.session(s)?;
        rec.note(ModulePhase::Session, None, format!("session 0x{s:02X}"));
    }
    let mut backup: Vec<RegionBackup> = Vec::new();
    let mut backup_note = None;
    let readable;
    if let Some(st) = &prior {
        readable = st.backup_saved;
        if st.backup_saved {
            let path = opts.state_path.as_ref().expect("state path");
            let bytes = st.load_backup(path)?;
            let mut off = 0usize;
            for g in &groups {
                let n = g.erase.length as usize;
                let data = bytes.get(off..off + n).ok_or_else(|| {
                    Error::Safety("the original backup is shorter than recorded".into())
                })?;
                backup.push(RegionBackup {
                    region: g.erase,
                    data: data.to_vec(),
                });
                off += n;
            }
        } else {
            backup_note = st.backup_note.clone();
        }
        rec.state = prior.clone();
        rec.note(
            ModulePhase::Backup,
            None,
            "resuming an interrupted flash: original backup taken from the state file",
        );
    } else {
        let backup_total: u64 = groups.iter().map(|g| u64::from(g.erase.length)).sum();
        let mut done = 0u64;
        let mut refused = None;
        'regions: for g in &groups {
            let mut data = Vec::with_capacity(g.erase.length as usize);
            for (addr, n) in read_chunks(g.erase, p.read_chunk) {
                match uds.read_memory_by_address(p.alfid, addr, n) {
                    Ok(d) if d.len() == n as usize => data.extend_from_slice(&d),
                    Ok(d) => {
                        return Err(Error::Safety(format!(
                            "backup read at 0x{addr:08X} returned {} of {n} bytes — aborting \
                             before any write",
                            d.len()
                        )))
                    }
                    Err(Error::NegativeResponse { code, name, .. }) if done == 0 => {
                        refused = Some(format!(
                            "the ECU refused ReadMemoryByAddress (NRC 0x{code:02X} {name})"
                        ));
                        break 'regions;
                    }
                    Err(e) => return Err(e),
                }
                done += u64::from(n);
                rec.bytes(ModulePhase::Backup, None, done, backup_total);
            }
            backup.push(RegionBackup {
                region: g.erase,
                data,
            });
        }
        readable = refused.is_none();
        if let Some(why) = refused {
            backup.clear();
            if policy.require_verified_backup && !opts.allow_no_backup {
                return Err(Error::Safety(format!(
                    "{why}: no backup of the original is possible — refusing to erase \
                     (override explicitly to flash without a backup)"
                )));
            }
            rec.note(
                ModulePhase::Backup,
                None,
                format!("NO BACKUP (override): {why}"),
            );
            backup_note = Some(why);
        } else {
            rec.note(
                ModulePhase::Backup,
                None,
                format!("backup of {backup_total} bytes read"),
            );
        }
        let mut st = ModuleState {
            version: state::STATE_VERSION,
            image_sha256: hash.clone(),
            ecu_fingerprint: identity.fingerprint(),
            backup_saved: false,
            backup_note: backup_note.clone(),
            backup_sha256: None,
            blocks: sel
                .iter()
                .map(|&i| {
                    let b = &image.blocks[i];
                    StBlock {
                        index: i,
                        id: b.id.clone(),
                        address: b.address,
                        len: b.data.len() as u32,
                        data_sha256: sha256_hex(&b.data),
                        status: BlockStatus::Pending,
                        attempts: 0,
                        last_error: None,
                    }
                })
                .collect(),
            events: Vec::new(),
        };
        for l in &rec.log {
            st.event(l.clone());
        }
        if let Some(path) = &opts.state_path {
            if !backup.is_empty() {
                let all: Vec<u8> = backup.iter().flat_map(|b| b.data.clone()).collect();
                st.save_backup(path, &all)?;
            }
        }
        rec.state = opts.state_path.as_ref().map(|_| st);
        rec.save()?;
    }
    if !readable && p.check_routine.is_none() && policy.require_readback_verify {
        return Err(Error::Safety(
            "the ECU allows no read-back and no checksum routine is configured, so the write \
             cannot be verified — refusing"
                .into(),
        ));
    }

    // 5. Programming session + security.
    uds.session(p.programming_session)?;
    rec.note(
        ModulePhase::Session,
        None,
        format!("programming session 0x{:02X}", p.programming_session),
    );
    let seed = uds.request_seed(p.security_level)?;
    if seed_is_unlocked(&seed) {
        rec.note(ModulePhase::Security, None, "already unlocked");
    } else {
        let Some(sk) = seed_key else {
            return Err(Error::Safety(format!(
                "security access level 0x{:02X} is required to program, but no seed-key \
                 handler was supplied — refusing (nothing was erased)",
                p.security_level
            )));
        };
        let key = sk.compute(&seed, p.security_level)?;
        uds.send_key(p.security_level.wrapping_add(1), &key)?;
        rec.note(
            ModulePhase::Security,
            None,
            format!(
                "unlocked level 0x{:02X} via seed-key '{}'",
                p.security_level,
                sk.id()
            ),
        );
    }

    // 6. Program, one erase region at a time.
    let mut reports: Vec<BlockReport> = Vec::new();
    let mut written = 0u64;
    for g in &groups {
        let status_of = |rec: &Recorder, i: usize| {
            rec.state
                .as_ref()
                .and_then(|s| s.blocks.iter().find(|b| b.index == i))
                .map(|b| b.status)
        };
        // A region whose blocks all finished earlier: confirm on the device.
        if resumed
            && g.blocks
                .iter()
                .all(|&i| status_of(&rec, i) == Some(BlockStatus::Done))
        {
            let mut confirmed = Vec::new();
            for &i in &g.blocks {
                let b = &image.blocks[i];
                let how = if readable {
                    match read_back(&mut uds, p, b) {
                        Ok(true) => Some(VerifyMethod::ReadBack),
                        Ok(false) => None,
                        Err(e) => return Err(e),
                    }
                } else if let Some(rid) = p.check_routine {
                    let st = uds.routine_control(0x01, rid, &check_record(p, b)?)?;
                    (st.first() == Some(&p.check_ok_status)).then_some(VerifyMethod::EcuRoutine)
                } else {
                    Some(VerifyMethod::None)
                };
                match how {
                    Some(v) => confirmed.push((i, v)),
                    None => break,
                }
            }
            if confirmed.len() == g.blocks.len() {
                for (i, v) in confirmed {
                    let b = &image.blocks[i];
                    written += b.data.len() as u64;
                    reports.push(BlockReport {
                        index: i,
                        id: b.id.clone(),
                        region: b.region(),
                        reused: true,
                        verify: v,
                    });
                }
                rec.note(
                    ModulePhase::Verify,
                    None,
                    format!(
                        "erase region 0x{:08X}: finished earlier, confirmed on the device",
                        g.erase.address
                    ),
                );
                continue;
            }
            rec.note(
                ModulePhase::Verify,
                None,
                format!(
                    "erase region 0x{:08X}: recorded as done but the device disagrees — \
                     reprogramming it",
                    g.erase.address
                ),
            );
        }
        let first = g.blocks[0];
        for &i in &g.blocks {
            rec.set_status(i, BlockStatus::Started, None);
        }
        rec.save()?;
        let res = program_group(
            &mut uds,
            p,
            image,
            g,
            readable,
            &mut rec,
            &mut written,
            total,
        );
        match res {
            Ok(mut r) => reports.append(&mut r),
            Err((i, e)) => {
                let i = i.unwrap_or(first);
                rec.set_status(i, BlockStatus::Failed, Some(e.to_string()));
                rec.note(
                    ModulePhase::Done,
                    Some(i),
                    format!("ABORTED at block {i}: {e}"),
                );
                rec.save()?;
                return Err(e);
            }
        }
    }

    // 7. Dependencies, reset.
    let mut dependencies_ok = None;
    if let Some(rid) = p.dependency_routine {
        let st = uds.routine_control(0x01, rid, &[])?;
        let ok = st.first() == Some(&p.check_ok_status);
        dependencies_ok = Some(ok);
        if !ok {
            rec.note(
                ModulePhase::Dependencies,
                None,
                format!("checkProgrammingDependencies FAILED (status {st:02X?}) — not resetting"),
            );
            rec.save()?;
            return Err(Error::Verify(format!(
                "the ECU's checkProgrammingDependencies routine 0x{rid:04X} reported failure \
                 (status {st:02X?}); the ECU was not reset"
            )));
        }
        rec.note(
            ModulePhase::Dependencies,
            None,
            "programming dependencies ok",
        );
    }
    let mut reset_sent = false;
    if let Some(kind) = p.reset_type {
        match uds.ecu_reset(kind) {
            Ok(_) => rec.note(ModulePhase::Reset, None, format!("ECU reset 0x{kind:02X}")),
            // Some ECUs reset before answering.
            Err(Error::Timeout(_)) => rec.note(
                ModulePhase::Reset,
                None,
                format!("ECU reset 0x{kind:02X} sent (no response)"),
            ),
            Err(e) => return Err(e),
        }
        reset_sent = true;
    }
    rec.note(ModulePhase::Done, None, "module flash complete");
    if let Some(path) = &opts.state_path {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(state::backup_path(path));
    }
    reports.sort_by_key(|r| r.index);
    Ok(ModuleFlashReport {
        identity,
        identity_forced,
        battery_millivolts: voltage.millivolts,
        backup,
        backup_note,
        resumed,
        blocks: reports,
        dependencies_ok,
        reset_sent,
        log: rec.log,
    })
}

/// Read `b` back and compare.
fn read_back(uds: &mut UdsClient<'_>, p: &ModuleProtocol, b: &ModuleBlock) -> Result<bool> {
    let mut got = Vec::with_capacity(b.data.len());
    for (addr, n) in read_chunks(b.region(), p.read_chunk) {
        got.extend(uds.read_memory_by_address(p.alfid, addr, n)?);
    }
    Ok(got == b.data)
}

/// TransferData with one ISO-defined retransmit: if the response is lost, the
/// same counter is sent again, which the ECU acknowledges without rewriting.
fn transfer(
    uds: &mut UdsClient<'_>,
    seq: u8,
    chunk: &[u8],
    rec: &mut Recorder<'_>,
    index: usize,
) -> Result<()> {
    match uds.transfer_data(seq, chunk) {
        Ok(_) => Ok(()),
        Err(Error::Timeout(ms)) => {
            rec.note(
                ModulePhase::Download,
                Some(index),
                format!("no response to block counter 0x{seq:02X} after {ms} ms — retransmitting"),
            );
            uds.transfer_data(seq, chunk).map(|_| ())
        }
        Err(e) => Err(e),
    }
}

type GroupError = (Option<usize>, Error);

/// Erase one region and program its blocks. On error, returns the failing
/// block index (if inside a block).
#[allow(clippy::too_many_arguments)]
fn program_group(
    uds: &mut UdsClient<'_>,
    p: &ModuleProtocol,
    image: &ModuleImage,
    g: &Group,
    readable: bool,
    rec: &mut Recorder<'_>,
    written: &mut u64,
    total: u64,
) -> std::result::Result<Vec<BlockReport>, GroupError> {
    let e = g.erase;
    let rec_err = |e: Error| (None, e);
    let erase = erase_record(p, e).map_err(rec_err)?;
    let st = uds
        .routine_control(0x01, p.erase_routine, &erase)
        .map_err(rec_err)?;
    if let Some(&s) = st.first() {
        if s != 0x00 {
            return Err((
                None,
                Error::Verify(format!(
                    "erase of 0x{:08X} +0x{:X} reported status 0x{s:02X}",
                    e.address, e.length
                )),
            ));
        }
    }
    rec.note(
        ModulePhase::Erase,
        Some(g.blocks[0]),
        format!("erased 0x{:08X} +0x{:X}", e.address, e.length),
    );
    let mut out = Vec::new();
    for &i in &g.blocks {
        let b = &image.blocks[i];
        let at = |e: Error| (Some(i), e);
        let mbl = uds
            .request_download_with_format(0x00, p.alfid, b.address, b.data.len() as u32)
            .map_err(at)?;
        let n = payload_len(p, mbl);
        rec.note(
            ModulePhase::Download,
            Some(i),
            format!(
                "block {i}: download 0x{:08X} +0x{:X}, maxNumberOfBlockLength {mbl} → {n} bytes per transfer",
                b.address,
                b.data.len()
            ),
        );
        let mut seq: u8 = 0x01;
        for chunk in b.data.chunks(n) {
            transfer(uds, seq, chunk, rec, i).map_err(at)?;
            seq = seq.wrapping_add(1);
            *written += chunk.len() as u64;
            rec.bytes(ModulePhase::Download, Some(i), *written, total);
        }
        uds.transfer_exit().map_err(at)?;
        rec.note(
            ModulePhase::TransferExit,
            Some(i),
            format!("block {i}: transfer exit"),
        );
        let mut verify = VerifyMethod::None;
        if readable {
            if !read_back(uds, p, b).map_err(at)? {
                return Err((
                    Some(i),
                    Error::Verify(format!(
                        "block {i}: read-back does not match the written data — stopping \
                         before the checksum routine and reset"
                    )),
                ));
            }
            verify = VerifyMethod::ReadBack;
            rec.note(
                ModulePhase::Verify,
                Some(i),
                format!("block {i}: read-back ok"),
            );
        }
        if let Some(rid) = p.check_routine {
            let st = uds
                .routine_control(0x01, rid, &check_record(p, b).map_err(at)?)
                .map_err(at)?;
            if st.first() != Some(&p.check_ok_status) {
                return Err((
                    Some(i),
                    Error::Verify(format!(
                        "block {i}: the ECU's checksum routine 0x{rid:04X} reported failure \
                         (status {st:02X?}) — stopping before reset"
                    )),
                ));
            }
            if verify == VerifyMethod::None {
                verify = VerifyMethod::EcuRoutine;
            }
            rec.note(
                ModulePhase::Check,
                Some(i),
                format!("block {i}: checksum routine ok"),
            );
        }
        rec.set_status(i, BlockStatus::Done, None);
        rec.save().map_err(at)?;
        out.push(BlockReport {
            index: i,
            id: b.id.clone(),
            region: b.region(),
            reused: false,
            verify,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_counts_sid_and_counter() {
        let p = ModuleProtocol::default();
        assert_eq!(payload_len(&p, 0x102), 0x100);
        assert_eq!(payload_len(&p, 1), 1);
        let capped = ModuleProtocol {
            max_transfer_payload: Some(64),
            ..ModuleProtocol::default()
        };
        assert_eq!(payload_len(&capped, 0x102), 64);
    }

    #[test]
    fn text_of_trims_and_falls_back_to_hex() {
        assert_eq!(text_of(b"1K0907115 \0"), "1K0907115");
        assert_eq!(text_of(&[0x01, 0x02]), "0102");
    }

    #[test]
    fn read_chunks_cover_region() {
        let c = read_chunks(MemoryRegion::new(0x100, 0x130), 0x80);
        assert_eq!(c, vec![(0x100, 0x80), (0x180, 0x80), (0x200, 0x30)]);
    }
}
