//! The flash orchestrator: the safety-enforcing sequence around any write.
//!
//! No driver writes to an ECU directly from the app. Writes go through this
//! crate, which enforces the safety sequence centrally so no driver can skip it.
//!
//! There are two entry layers:
//!
//! - [`flash_calibration`] / [`flash_calibration_resumable_logged`] take the
//!   legacy, flexible [`SafetyConfig`]. They are preserved for the offline mock
//!   pipeline and existing callers. Their voltage gate only fails closed for
//!   voltage-capable interfaces and they do no target-level checks.
//! - [`flash_calibration_checked`] / [`flash_calibration_resumable_checked`] take
//!   an explicit [`FlashSafetyPolicy`] and a [`FlashTarget`]. With
//!   [`FlashSafetyPolicy::production`] they **fail closed**: a real write is
//!   refused unless voltage is confirmed, a verified backup exists, the region is
//!   declared writable, the connected ECU identity matches the payload's intended
//!   target, the payload carries a checksum attestation bound to its bytes, and
//!   the post-write read-back matches. See [`policy`] for the full contract and
//!   its documented limitations.
//!
//! A third entry point programs a **control module over UDS** from a decoded
//! flash container: [`module::flash_module`] (erase → RequestDownload →
//! TransferData → RequestTransferExit → checksum routine per block, behind the
//! same [`FlashSafetyPolicy`], resumable per erase region) and its dry run
//! [`module::plan_module_flash`].
pub mod hash;
pub mod module;
pub mod policy;
pub mod resume;
pub mod transcript;
pub mod vault;

pub use module::{
    flash_module, flash_module_on, plan_module_flash, ModuleFlashOptions, ModuleFlashReport,
    ModuleImage, ModuleProtocol,
};
pub use policy::{
    ChecksumAttestation, FlashSafetyPolicy, FlashTarget, PolicyViolation, ProgrammingProcedure,
    TargetIdentity, VoltagePolicy, WriteMode,
};
pub use resume::{
    flash_calibration_resumable_checked, flash_calibration_resumable_logged, DEFAULT_CHUNK_SIZE,
};
pub use transcript::{FlashEvent, Transcript, TranscriptEntry};

use opentune_core::{EcuId, Error, Interface, MemoryRegion, Phase, Progress, Result};
use std::path::Path;

/// Safety thresholds and switches for a flash operation.
#[derive(Debug, Clone)]
pub struct SafetyConfig {
    /// Refuse to flash below this battery voltage (mV). 0 disables the gate.
    pub min_millivolts: u32,
    /// Require a successful backup read before writing.
    pub require_backup: bool,
    /// Read back and compare after writing.
    pub verify_after_write: bool,
    /// Chunk size (bytes) for the resumable flash path. 0 means
    /// [`DEFAULT_CHUNK_SIZE`]. Ignored by [`flash_calibration`].
    pub chunk_size: usize,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            min_millivolts: 12_000,
            require_backup: true,
            verify_after_write: true,
            chunk_size: DEFAULT_CHUNK_SIZE,
        }
    }
}

/// Outcome of a flash operation.
#[derive(Debug, Clone)]
pub struct FlashReport {
    pub region: MemoryRegion,
    pub battery_millivolts: Option<u32>,
    pub backup: Vec<u8>,
    pub bytes_written: usize,
    pub verified: bool,
}

/// Read a region into a backup buffer (always do this before writing).
pub fn backup(
    iface: &mut dyn Interface,
    region: MemoryRegion,
    progress: &mut dyn FnMut(Progress),
) -> Result<Vec<u8>> {
    iface.read_memory(region, progress)
}

/// Write `new_data` to `region` with the legacy [`SafetyConfig`] enforcement.
///
/// This is the historical, deliberately flexible path: its voltage gate only
/// fails closed for interfaces that advertise [`battery_voltage`], and it does no
/// target-level checks (identity, writable region, checksum). It is preserved for
/// the mock pipeline and existing callers. **For a real write to a controller use
/// [`flash_calibration_checked`] with a [`FlashSafetyPolicy::production`] and a
/// [`FlashTarget`].**
pub fn flash_calibration(
    iface: &mut dyn Interface,
    region: MemoryRegion,
    new_data: &[u8],
    safety: &SafetyConfig,
    progress: &mut dyn FnMut(Progress),
) -> Result<FlashReport> {
    let policy = FlashSafetyPolicy::from_legacy(safety);
    flash_once(iface, region, new_data, &policy, None, progress)
}

/// Write `data` to `region` under an explicit [`FlashSafetyPolicy`], validating
/// against the intended [`FlashTarget`].
///
/// With [`FlashSafetyPolicy::production`] this enforces, in order, before any
/// device write: payload length, region ⊆ a declared writable region, a payload
/// checksum attestation, the connected ECU identity, and a confirmed voltage
/// (fail-closed if unreadable); then a verified backup, the write, and a
/// post-write read-back verify. Any unmet criterion is a typed
/// [`PolicyViolation`] (surfaced as [`Error::Safety`]).
pub fn flash_calibration_checked(
    iface: &mut dyn Interface,
    region: MemoryRegion,
    data: &[u8],
    target: &FlashTarget,
    policy: &FlashSafetyPolicy,
    progress: &mut dyn FnMut(Progress),
) -> Result<FlashReport> {
    flash_once(iface, region, data, policy, Some(target), progress)
}

/// Read the connected ECU identity when the policy needs it. Returns `None` if
/// the policy does not require identity (so we do not touch the device
/// needlessly) or if the read failed (the caller's identity check turns a failed
/// read into a fail-closed refusal).
pub(crate) fn read_identity_if_needed(
    iface: &mut dyn Interface,
    policy: &FlashSafetyPolicy,
) -> Option<EcuId> {
    if policy.require_identity_match {
        iface.read_ecu_id().ok()
    } else {
        None
    }
}

/// The single safety-enforcing write sequence shared by the legacy and the
/// policy-checked entry points. Every gate runs before any byte is written.
fn flash_once(
    iface: &mut dyn Interface,
    region: MemoryRegion,
    data: &[u8],
    policy: &FlashSafetyPolicy,
    target: Option<&FlashTarget>,
    progress: &mut dyn FnMut(Progress),
) -> Result<FlashReport> {
    // 0. Structural gates (no device access).
    policy.check_target(target)?;
    if data.len() != region.length as usize {
        return Err(PolicyViolation::PayloadLengthMismatch {
            got: data.len(),
            want: region.length,
        }
        .into());
    }
    policy.check_region_writable(region, target)?;
    policy.check_checksum(data, target)?;

    // 1. Identity: the payload must be going to the controller it was built for.
    let connected = read_identity_if_needed(iface, policy);
    policy.check_identity(connected.as_ref(), target)?;

    // 2. Voltage gate (fail-closed under a production policy).
    let mv = iface.battery_millivolts()?;
    policy.check_voltage(mv, iface.capabilities().battery_voltage)?;

    // 3. Verified backup of the original.
    let backup_buf = if policy.require_verified_backup {
        let b = backup(iface, region, progress)?;
        if b.len() != region.length as usize {
            return Err(PolicyViolation::BackupShort {
                got: b.len(),
                want: region.length,
            }
            .into());
        }
        b
    } else {
        Vec::new()
    };

    // 4. Write.
    iface.write_memory(region, data, progress)?;

    // 5. Read-back verify.
    let mut verified = false;
    if policy.require_readback_verify {
        let readback = iface.read_memory(region, progress)?;
        if readback != data {
            return Err(Error::Verify(
                "read-back does not match written data".into(),
            ));
        }
        progress(Progress {
            phase: Phase::Verify,
            done: region.length as u64,
            total: region.length as u64,
        });
        verified = true;
    }

    Ok(FlashReport {
        region,
        battery_millivolts: mv,
        backup: backup_buf,
        bytes_written: data.len(),
        verified,
    })
}

/// Resumable, checkpointed flash of `data` to `region`.
///
/// Identical safety to [`flash_calibration`] (voltage gate → mandatory original
/// backup → write → verify), but the write is chunked and progress is
/// checkpointed to `state_path` after every verified chunk. If `state_path`
/// already holds a valid checkpoint *for this exact payload*, already
/// written+verified chunks are skipped and the flash continues; a checkpoint
/// for a different payload is refused. The checkpoint is removed on full
/// success. See [`SafetyConfig::chunk_size`].
///
/// This is the drop-in form without a transcript. For a session transcript, use
/// [`flash_calibration_resumable_logged`], which takes a [`Transcript`].
pub fn flash_calibration_resumable(
    iface: &mut dyn Interface,
    region: MemoryRegion,
    data: &[u8],
    safety: &SafetyConfig,
    state_path: &Path,
    progress: &mut dyn FnMut(Progress),
) -> Result<FlashReport> {
    let mut transcript = Transcript::new();
    flash_calibration_resumable_logged(
        iface,
        region,
        data,
        safety,
        state_path,
        &mut transcript,
        progress,
    )
}
