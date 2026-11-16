//! Resumable / recoverable flashing.
//!
//! The write is split into fixed-size chunks. After every chunk is written and
//! read-back verified, progress is checkpointed to a JSON sidecar next to the
//! session (`state_path`). A later run loads that checkpoint, skips the chunks
//! already written+verified, and continues — so an interrupted flash is
//! recoverable instead of fatal.
//!
//! Safety is unchanged from [`crate::flash_calibration`]: voltage gate, then a
//! mandatory pre-flash backup of the *original*, then chunked write, each chunk
//! verified. Two extra guarantees are specific to resume:
//!
//! 1. The checkpoint stores a SHA-256 of the intended payload. A resume whose
//!    data hash differs is refused — a checkpoint can never be used to apply a
//!    *different* image to the region it was created for.
//! 2. The original backup is persisted to a sidecar on the first run and reused
//!    on resume; it is never re-read from a region that is already partially
//!    written, so the true original is never lost.
use crate::hash::{crc32, sha256_hex};
use crate::transcript::{FlashEvent, Transcript};
use crate::{backup, FlashReport, SafetyConfig};
use opentune_core::{Error, Interface, MemoryRegion, Phase, Progress, Result};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const CKPT_VERSION: u32 = 1;
/// Chunk size used when [`SafetyConfig::chunk_size`] is 0.
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
    /// SHA-256 (hex) of the intended payload — the resume-safety anchor.
    data_sha256: String,
    require_backup: bool,
    backup_file: Option<String>,
    backup_crc32: Option<u32>,
    backup_len: Option<u32>,
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
fn effective_chunk_size(safety: &SafetyConfig, total: u32) -> u32 {
    if total == 0 {
        return 1;
    }
    let cs = if safety.chunk_size == 0 {
        DEFAULT_CHUNK_SIZE
    } else {
        safety.chunk_size
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
fn save_checkpoint(path: &Path, ck: &Checkpoint) -> Result<()> {
    let json = serde_json::to_vec_pretty(ck)
        .map_err(|e| Error::Config(format!("serialize checkpoint: {e}")))?;
    let tmp = sibling(path, ".tmp");
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Verify a stored checkpoint describes *this* job. Returns a refusal for any
/// mismatch, most importantly a payload-hash mismatch (wrong image).
fn validate_checkpoint(
    ck: &Checkpoint,
    region: MemoryRegion,
    chunk_size: u32,
    want_sha: &str,
) -> Result<()> {
    if ck.version != CKPT_VERSION {
        return Err(Error::Safety(format!(
            "checkpoint version {} != {CKPT_VERSION}",
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
    Ok(())
}

/// Resumable flash that records a [`Transcript`]. See [`crate::flash_calibration_resumable`].
///
/// The caller owns `transcript` and persists it (e.g. [`Transcript::write_next_to`]).
pub fn flash_calibration_resumable_logged(
    iface: &mut dyn Interface,
    region: MemoryRegion,
    data: &[u8],
    safety: &SafetyConfig,
    state_path: &Path,
    transcript: &mut Transcript,
    progress: &mut dyn FnMut(Progress),
) -> Result<FlashReport> {
    let result = run(
        iface, region, data, safety, state_path, transcript, progress,
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
fn run(
    iface: &mut dyn Interface,
    region: MemoryRegion,
    data: &[u8],
    safety: &SafetyConfig,
    state_path: &Path,
    transcript: &mut Transcript,
    progress: &mut dyn FnMut(Progress),
) -> Result<FlashReport> {
    if data.len() != region.length as usize {
        return Err(Error::Safety(format!(
            "data length {} != region length {}",
            data.len(),
            region.length
        )));
    }

    let total = region.length;
    let chunk_size = effective_chunk_size(safety, total);
    let want_sha = sha256_hex(data);

    // Load + validate any existing checkpoint (refuses a wrong payload).
    let existing = load_checkpoint(state_path)?;
    if let Some(ck) = &existing {
        validate_checkpoint(ck, region, chunk_size, &want_sha)?;
    }
    let resumed = existing.is_some();

    // Chunk plan: reuse the checkpoint's verified flags when resuming.
    let mut chunks = match &existing {
        Some(ck) => ck.chunks.clone(),
        None => plan_chunks(total, chunk_size),
    };
    let any_written = chunks.iter().any(|c| c.verified);

    // Session start.
    let ecu_name = iface
        .read_ecu_id()
        .ok()
        .and_then(|id| id.ecu_name.or(id.sw_number).or(id.hw_number).or(id.vin));
    transcript.record(FlashEvent::SessionStart {
        ecu: ecu_name,
        address: region.address,
        length: total,
        resumed,
    });

    // 1. Voltage gate (every run).
    let mv = iface.battery_millivolts()?;
    transcript.record(FlashEvent::Voltage { millivolts: mv });
    if safety.min_millivolts > 0 {
        match mv {
            Some(v) if v < safety.min_millivolts => {
                return Err(Error::Safety(format!(
                    "battery {v} mV below minimum {} mV",
                    safety.min_millivolts
                )));
            }
            None if iface.capabilities().battery_voltage => {
                return Err(Error::Safety("battery voltage unreadable".into()));
            }
            _ => {}
        }
    }

    // 2. Mandatory pre-flash backup of the ORIGINAL.
    let backup_path = sibling(state_path, ".backup");
    let (backup_buf, backup_meta) = if safety.require_backup {
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
                    return Err(Error::Safety("backup read short — aborting".into()));
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

    // Persist a baseline checkpoint (records backup metadata) before writing.
    let mut ckpt = Checkpoint {
        version: CKPT_VERSION,
        address: region.address,
        total,
        chunk_size,
        data_sha256: want_sha,
        require_backup: safety.require_backup,
        backup_file: backup_meta.as_ref().map(|p| p.display().to_string()),
        backup_crc32: backup_meta.as_ref().map(|_| crc32(&backup_buf)),
        backup_len: backup_meta.as_ref().map(|_| backup_buf.len() as u32),
        updated_unix: now_unix(),
        chunks: chunks.clone(),
    };
    save_checkpoint(state_path, &ckpt)?;

    // 3. Write + verify each remaining chunk, checkpointing after each.
    let chunks_total = chunks.len();
    let mut writes = 0usize;
    let mut done: u64 = chunks
        .iter()
        .filter(|c| c.verified)
        .map(|c| c.len as u64)
        .sum();

    for i in 0..chunks.len() {
        if chunks[i].verified {
            continue; // already written+verified in a prior run → skip.
        }
        let off = chunks[i].offset;
        let len = chunks[i].len;
        let addr = region
            .address
            .checked_add(off)
            .ok_or_else(|| Error::Safety("chunk address overflow".into()))?;
        let cr = MemoryRegion::new(addr, len);
        let want = &data[off as usize..(off + len) as usize];

        // Read-back first: robust to a chunk written but not yet checkpointed
        // (re-verify, don't blindly re-write).
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

    // 4. Optional final whole-region verify (defense in depth).
    if safety.verify_after_write {
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
