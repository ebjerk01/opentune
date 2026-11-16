//! Resumable / recoverable flashing.
//!
//! The write is split into fixed-size chunks. After every chunk is written and
//! read-back verified, progress is checkpointed to a JSON sidecar next to the
//! session (`state_path`). A later run loads that checkpoint and continues — so
//! an interrupted flash is recoverable instead of fatal.
//!
//! # Recovery safety
//!
//! Resuming onto an ECU is where a toolkit bricks a controller, so recovery here
//! is evidence-based, not bookkeeping-based:
//!
//! 1. **Wrong image.** The checkpoint stores a SHA-256 of the intended payload. A
//!    resume whose data hash differs is refused — a checkpoint can never apply a
//!    *different* image to the region it was created for.
//! 2. **Wrong controller.** The checkpoint binds the ECU definition id, the
//!    *connected ECU's identity fingerprint* (part/software/VIN) and the
//!    programming procedure observed on the first run. On resume under a policy
//!    that requires identity, the connected ECU must match that binding, or the
//!    resume is refused. If identity cannot be read after an interruption, the
//!    resume is refused and a fresh verified read is required — a checkpoint is
//!    never applied to an unidentified controller.
//! 3. **Erase / reset blanking.** Recorded progress is never trusted by index
//!    alone. On a resumed run *every* chunk — including ones a prior run marked
//!    verified — is read back from the device first and only skipped if the
//!    device currently holds the intended bytes. If an erase or ECU reset blanked
//!    already-written data, the read-back no longer matches and the chunk is
//!    rewritten. This is what makes resume correct on an ECU that erases: generic
//!    "skip the first N chunks" resumption is *not* proof of recovery, and is not
//!    done here. See [`ProgrammingProcedure`].
//! 4. **Lost original.** The original backup is persisted to a sidecar on the
//!    first run and reused on resume; it is never re-read from a region that is
//!    already partially written, so the true original is never lost.
//!
//! Safety is otherwise the same as [`crate::flash_calibration`]: the configured
//! voltage gate, a mandatory pre-flash backup of the *original*, chunked write,
//! each chunk verified. The policy-checked entry point
//! [`flash_calibration_resumable_checked`] adds the production fail-closed gates
//! (identity, writable region, checksum, fail-closed voltage) from
//! [`crate::policy`].
use crate::hash::{crc32, sha256_hex};
use crate::policy::{
    FlashSafetyPolicy, FlashTarget, PolicyViolation, ProgrammingProcedure, TargetIdentity,
};
use crate::transcript::{FlashEvent, Transcript};
use crate::{backup, FlashReport, SafetyConfig};
use opentune_core::{EcuId, Error, Interface, MemoryRegion, Phase, Progress, Result};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const CKPT_VERSION: u32 = 2;
/// Chunk size used when the policy chunk size is 0.
pub const DEFAULT_CHUNK_SIZE: usize = 4096;

/// Per-chunk checkpoint record.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChunkState {
    index: u32,
    offset: u32,
    len: u32,
    verified: bool,
}

/// On-disk checkpoint. Written atomically after every verified chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Checkpoint {
    version: u32,
    address: u32,
    total: u32,
    chunk_size: u32,
    /// SHA-256 (hex) of the intended payload — the wrong-image anchor.
    data_sha256: String,
    require_backup: bool,
    backup_file: Option<String>,
    backup_crc32: Option<u32>,
    backup_len: Option<u32>,
    /// ECU definition id this flash targets (v2).
    #[serde(default)]
    ecu_def_id: Option<String>,
    /// Fingerprint of the *connected* ECU observed on the first run (v2). The
    /// wrong-controller anchor: a resume must see the same fingerprint.
    #[serde(default)]
    ecu_identity: Option<String>,
    /// Programming procedure in force on the first run (v2).
    #[serde(default)]
    procedure: Option<ProgrammingProcedure>,
    updated_unix: u64,
    chunks: Vec<ChunkState>,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Append a suffix to a path's file name (keeps the full original name).
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut s: OsString = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Effective chunk size: clamp to `[1, total]`, and treat 0 as the default.
fn effective_chunk_size(requested: usize, total: u32) -> u32 {
    if total == 0 {
        return 1;
    }
    let cs = if requested == 0 {
        DEFAULT_CHUNK_SIZE
    } else {
        requested
    };
    (cs as u64).clamp(1, total as u64) as u32
}

fn plan_chunks(total: u32, chunk_size: u32) -> Vec<ChunkState> {
    let mut chunks = Vec::new();
    let mut off = 0u32;
    let mut index = 0u32;
    while off < total {
        let len = chunk_size.min(total - off);
        chunks.push(ChunkState {
            index,
            offset: off,
            len,
            verified: false,
        });
        off += len;
        index += 1;
    }
    chunks
}

/// Load an existing checkpoint, or `None` if the file is absent.
/// A present-but-unparseable checkpoint is an error (never silently ignored).
fn load_checkpoint(path: &Path) -> Result<Option<Checkpoint>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let ck: Checkpoint = serde_json::from_slice(&bytes).map_err(|e| {
                Error::Safety(format!(
                    "corrupt flash checkpoint {}: {e} — refusing to guess progress",
                    path.display()
                ))
            })?;
            Ok(Some(ck))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Write the checkpoint atomically (temp file + rename on the same directory).
/// A failure here aborts the flash: progress that cannot be persisted must never
/// be relied on, so the caller sees the error rather than a silent write.
fn save_checkpoint(path: &Path, ck: &Checkpoint) -> Result<()> {
    let json = serde_json::to_vec_pretty(ck)
        .map_err(|e| Error::Config(format!("serialize checkpoint: {e}")))?;
    let tmp = sibling(path, ".tmp");
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Structural check that a stored checkpoint describes *this* job: version,
/// region, chunking, payload hash and (when both are known) the ECU definition.
/// Refuses on any mismatch. Identity/procedure binding is checked separately
/// against the live device in [`verify_resume_identity`].
fn validate_checkpoint(
    ck: &Checkpoint,
    region: MemoryRegion,
    chunk_size: u32,
    want_sha: &str,
    target: Option<&FlashTarget>,
) -> Result<()> {
    if ck.version != CKPT_VERSION {
        return Err(Error::Safety(format!(
            "checkpoint version {} != {CKPT_VERSION} — refusing to resume an incompatible checkpoint",
            ck.version
        )));
    }
    if ck.address != region.address {
        return Err(Error::Safety(format!(
            "checkpoint address 0x{:08X} != region 0x{:08X}",
            ck.address, region.address
        )));
    }
    if ck.total != region.length {
        return Err(Error::Safety(format!(
            "checkpoint length {} != region length {}",
            ck.total, region.length
        )));
    }
    if ck.chunk_size != chunk_size {
        return Err(Error::Safety(format!(
            "checkpoint chunk size {} != {chunk_size} — cannot resume with a different chunking",
            ck.chunk_size
        )));
    }
    if ck.data_sha256 != want_sha {
        return Err(Error::Safety(
            "checkpoint is for a different payload (data hash mismatch) — refusing to resume"
                .into(),
        ));
    }
    if let (Some(cid), Some(t)) = (&ck.ecu_def_id, target) {
        if cid != &t.ecu_def_id {
            return Err(Error::Safety(format!(
                "checkpoint ECU definition '{cid}' != target '{}' — refusing to resume",
                t.ecu_def_id
            )));
        }
    }
    Ok(())
}

/// Verify a resume is the SAME controller the checkpoint was created for, under a
/// policy that requires identity. Flexible/legacy flows (no identity requirement)
/// skip this. Defines the behaviour when identity cannot be recovered: refuse.
fn verify_resume_identity(
    ck: &Checkpoint,
    connected: Option<&EcuId>,
    policy: &FlashSafetyPolicy,
    target: Option<&FlashTarget>,
) -> Result<()> {
    if !policy.require_identity_match {
        return Ok(());
    }
    // A production resume needs a checkpoint that actually bound an identity.
    let Some(bound) = &ck.ecu_identity else {
        return Err(PolicyViolation::CheckpointIdentityMissing.into());
    };
    // Identity must be recoverable after the interruption, or we refuse.
    let Some(id) = connected else {
        return Err(PolicyViolation::ResumeIdentityUnrecoverable.into());
    };
    let now_fp = TargetIdentity::fingerprint_of(id);
    if &now_fp != bound {
        return Err(PolicyViolation::CheckpointIdentityMismatch {
            bound: bound.clone(),
            connected: now_fp,
        }
        .into());
    }
    // The programming procedure must not have changed under the resume.
    if let (Some(bp), Some(t)) = (&ck.procedure, target) {
        if bp != &t.procedure {
            return Err(PolicyViolation::CheckpointProcedureMismatch {
                bound: bp.name.clone(),
                current: t.procedure.name.clone(),
            }
            .into());
        }
    }
    Ok(())
}

/// Legacy, flexible resumable flash with a [`Transcript`]. See
/// [`crate::flash_calibration_resumable`]. Uses the historical [`SafetyConfig`]
/// behaviour (no target-level checks). For a real write use
/// [`flash_calibration_resumable_checked`].
pub fn flash_calibration_resumable_logged(
    iface: &mut dyn Interface,
    region: MemoryRegion,
    data: &[u8],
    safety: &SafetyConfig,
    state_path: &Path,
    transcript: &mut Transcript,
    progress: &mut dyn FnMut(Progress),
) -> Result<FlashReport> {
    let policy = FlashSafetyPolicy::from_legacy(safety);
    run_logged(
        iface, region, data, &policy, None, state_path, transcript, progress,
    )
}

/// Production, policy-checked resumable flash with a [`Transcript`].
///
/// Enforces the full [`FlashSafetyPolicy`] (fail-closed voltage, writable region,
/// identity, checksum) on every run and binds the recovery checkpoint to the
/// connected ECU identity and programming procedure, so a resume cannot be
/// applied to the wrong controller or variant.
#[allow(clippy::too_many_arguments)]
pub fn flash_calibration_resumable_checked(
    iface: &mut dyn Interface,
    region: MemoryRegion,
    data: &[u8],
    target: &FlashTarget,
    policy: &FlashSafetyPolicy,
    state_path: &Path,
    transcript: &mut Transcript,
    progress: &mut dyn FnMut(Progress),
) -> Result<FlashReport> {
    run_logged(
        iface,
        region,
        data,
        policy,
        Some(target),
        state_path,
        transcript,
        progress,
    )
}

/// Shared transcript-recording wrapper around [`run`].
#[allow(clippy::too_many_arguments)]
fn run_logged(
    iface: &mut dyn Interface,
    region: MemoryRegion,
    data: &[u8],
    policy: &FlashSafetyPolicy,
    target: Option<&FlashTarget>,
    state_path: &Path,
    transcript: &mut Transcript,
    progress: &mut dyn FnMut(Progress),
) -> Result<FlashReport> {
    let result = run(
        iface, region, data, policy, target, state_path, transcript, progress,
    );
    if let Err(e) = &result {
        transcript.record(FlashEvent::Error {
            message: e.to_string(),
        });
        transcript.record(FlashEvent::SessionEnd {
            ok: false,
            bytes_written: 0,
            chunks_total: 0,
            chunks_reused: 0,
        });
    }
    result
}

#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
fn run(
    iface: &mut dyn Interface,
    region: MemoryRegion,
    data: &[u8],
    policy: &FlashSafetyPolicy,
    target: Option<&FlashTarget>,
    state_path: &Path,
    transcript: &mut Transcript,
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

    let total = region.length;
    let chunk_size = effective_chunk_size(policy.chunk_size, total);
    let want_sha = sha256_hex(data);

    // Load + validate any existing checkpoint (refuses a wrong payload/region).
    let existing = load_checkpoint(state_path)?;
    if let Some(ck) = &existing {
        validate_checkpoint(ck, region, chunk_size, &want_sha, target)?;
    }
    let resumed = existing.is_some();

    // Chunk plan: reuse the checkpoint's recorded flags when resuming. On a
    // resumed run these flags are NOT trusted to skip writes — every chunk is
    // still read back below — but they drive the reuse accounting.
    let mut chunks = match &existing {
        Some(ck) => ck.chunks.clone(),
        None => plan_chunks(total, chunk_size),
    };
    let any_written = chunks.iter().any(|c| c.verified);

    // Read the connected identity once: used for the identity gate, the recovery
    // binding and the transcript label.
    let connected = iface.read_ecu_id().ok();

    // Identity gate against the intended target (fail-closed when required).
    policy.check_identity(connected.as_ref(), target)?;
    // Recovery identity binding: a resume must be the SAME controller.
    if let Some(ck) = &existing {
        verify_resume_identity(ck, connected.as_ref(), policy, target)?;
    }

    // Session start.
    let ecu_name = connected.as_ref().and_then(|id| {
        id.ecu_name
            .clone()
            .or_else(|| id.sw_number.clone())
            .or_else(|| id.hw_number.clone())
            .or_else(|| id.vin.clone())
    });
    transcript.record(FlashEvent::SessionStart {
        ecu: ecu_name,
        address: region.address,
        length: total,
        resumed,
    });

    // 1. Voltage gate (every run).
    let mv = iface.battery_millivolts()?;
    transcript.record(FlashEvent::Voltage { millivolts: mv });
    policy.check_voltage(mv, iface.capabilities().battery_voltage)?;

    // 2. Mandatory pre-flash backup of the ORIGINAL.
    let backup_path = sibling(state_path, ".backup");
    let (backup_buf, backup_meta) = if policy.require_verified_backup {
        match reuse_backup(&backup_path, existing.as_ref()) {
            Some(buf) => (buf, Some(backup_path.clone())),
            None => {
                // No valid saved original. Only safe to (re)read it if nothing
                // has been written to the region yet.
                if any_written {
                    return Err(Error::Safety(
                        "original backup missing or corrupt and the region is already \
                         partially written — refusing to resume without the original"
                            .into(),
                    ));
                }
                let b = backup(iface, region, progress)?;
                if b.len() != total as usize {
                    return Err(PolicyViolation::BackupShort {
                        got: b.len(),
                        want: total,
                    }
                    .into());
                }
                std::fs::write(&backup_path, &b)?;
                (b, Some(backup_path.clone()))
            }
        }
    } else {
        (Vec::new(), None)
    };
    if let Some(bp) = &backup_meta {
        transcript.record(FlashEvent::BackupSaved {
            path: bp.display().to_string(),
            crc32: crc32(&backup_buf),
            len: backup_buf.len(),
        });
    }

    // Persist a baseline checkpoint (records backup + identity + procedure)
    // before writing. A persistence failure here aborts before any write.
    let mut ckpt = Checkpoint {
        version: CKPT_VERSION,
        address: region.address,
        total,
        chunk_size,
        data_sha256: want_sha,
        require_backup: policy.require_verified_backup,
        backup_file: backup_meta.as_ref().map(|p| p.display().to_string()),
        backup_crc32: backup_meta.as_ref().map(|_| crc32(&backup_buf)),
        backup_len: backup_meta.as_ref().map(|_| backup_buf.len() as u32),
        ecu_def_id: target.map(|t| t.ecu_def_id.clone()),
        ecu_identity: connected.as_ref().map(TargetIdentity::fingerprint_of),
        procedure: target.map(|t| t.procedure.clone()),
        updated_unix: now_unix(),
        chunks: chunks.clone(),
    };
    save_checkpoint(state_path, &ckpt)?;

    // 3. Write + verify each chunk, checkpointing after each. Every chunk is
    //    read back FIRST (even ones a prior run marked verified): a chunk is
    //    reused only if the device currently holds the intended bytes, so an
    //    erase/reset that blanked earlier progress forces a rewrite instead of a
    //    silent skip.
    let chunks_total = chunks.len();
    let mut writes = 0usize;
    let mut done: u64 = 0;

    for i in 0..chunks.len() {
        let off = chunks[i].offset;
        let len = chunks[i].len;
        let addr = region
            .address
            .checked_add(off)
            .ok_or_else(|| Error::Safety("chunk address overflow".into()))?;
        let cr = MemoryRegion::new(addr, len);
        let want = &data[off as usize..(off + len) as usize];

        // Read-back first: reuse only if the device already holds these bytes.
        let current = iface.read_memory(cr, progress)?;
        let reused = current == want;
        if !reused {
            iface.write_memory(cr, want, progress)?;
            let readback = iface.read_memory(cr, progress)?;
            if readback != want {
                transcript.record(FlashEvent::ChunkVerify {
                    index: i,
                    ok: false,
                });
                return Err(Error::Verify(format!(
                    "chunk {i} @0x{addr:08X} read-back does not match written data"
                )));
            }
            writes += 1;
        }
        transcript.record(FlashEvent::ChunkWrite {
            index: i,
            offset: off,
            len: len as usize,
            reused,
        });
        transcript.record(FlashEvent::ChunkVerify { index: i, ok: true });

        chunks[i].verified = true;
        ckpt.chunks[i].verified = true;
        ckpt.updated_unix = now_unix();
        save_checkpoint(state_path, &ckpt)?;

        done += len as u64;
        progress(Progress {
            phase: Phase::Write,
            done,
            total: total as u64,
        });
    }

    // 4. Final whole-region verify (defense in depth).
    if policy.require_readback_verify {
        let readback = iface.read_memory(region, progress)?;
        let ok = readback == data;
        transcript.record(FlashEvent::FinalVerify { ok });
        if !ok {
            return Err(Error::Verify(
                "final read-back does not match written data".into(),
            ));
        }
        progress(Progress {
            phase: Phase::Verify,
            done: total as u64,
            total: total as u64,
        });
    }

    // 5. Success → remove the checkpoint (keep the backup sidecar).
    let _ = std::fs::remove_file(state_path);
    let chunks_reused = chunks_total - writes;
    transcript.record(FlashEvent::SessionEnd {
        ok: true,
        bytes_written: total as usize,
        chunks_total,
        chunks_reused,
    });

    Ok(FlashReport {
        region,
        battery_millivolts: mv,
        backup: backup_buf,
        bytes_written: total as usize,
        verified: true,
    })
}

/// Load a previously saved original backup if its sidecar matches the
/// checkpoint's recorded crc/len. Returns `None` if absent or mismatched.
fn reuse_backup(backup_path: &Path, existing: Option<&Checkpoint>) -> Option<Vec<u8>> {
    let ck = existing?;
    let want_crc = ck.backup_crc32?;
    let want_len = ck.backup_len? as usize;
    let buf = std::fs::read(backup_path).ok()?;
    if buf.len() == want_len && crc32(&buf) == want_crc {
        Some(buf)
    } else {
        None
    }
}
