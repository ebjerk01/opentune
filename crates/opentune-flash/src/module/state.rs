//! The module flasher's on-disk state: which blocks are done, the ECU it is
//! bound to, the image it is for, where the original backup lives, and a
//! running event transcript. Written atomically before every step that changes
//! the ECU, so an interrupted flash restarts from the block that failed.
use crate::hash::sha256_hex;
use opentune_core::{Error, Result};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) const STATE_VERSION: u32 = 1;

/// Progress of one block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockStatus {
    /// Not touched yet.
    Pending,
    /// Erase was requested: the block's contents are undefined until it is
    /// erased and written again.
    Started,
    /// Written, verified and accepted by the ECU's checksum routine.
    Done,
    /// A step failed; the block is re-erased and rewritten on resume.
    Failed,
}

/// One block's record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockState {
    /// Index into the image's blocks.
    pub index: usize,
    /// Block id.
    pub id: String,
    /// Target address.
    pub address: u32,
    /// Length.
    pub len: u32,
    /// SHA-256 of the block data.
    pub data_sha256: String,
    /// Progress.
    pub status: BlockStatus,
    /// Write attempts so far.
    pub attempts: u32,
    /// The last error, if the block failed.
    #[serde(default)]
    pub last_error: Option<String>,
}

/// One transcript line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateEvent {
    /// Seconds since the Unix epoch.
    pub t_unix: u64,
    /// What happened.
    pub message: String,
}

/// The state file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleState {
    /// Format version.
    pub version: u32,
    /// Hash binding the state to the exact image (selected blocks).
    pub image_sha256: String,
    /// Fingerprint of the connected ECU on the first run.
    pub ecu_fingerprint: String,
    /// Whether the original was backed up (the sidecar holds it).
    pub backup_saved: bool,
    /// Why no backup exists, when it does not (override recorded).
    #[serde(default)]
    pub backup_note: Option<String>,
    /// SHA-256 of the backup sidecar.
    #[serde(default)]
    pub backup_sha256: Option<String>,
    /// Per-block progress.
    pub blocks: Vec<BlockState>,
    /// The transcript.
    #[serde(default)]
    pub events: Vec<StateEvent>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `path` with `suffix` appended to its file name.
pub fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut s: OsString = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// The backup sidecar next to a state file.
pub fn backup_path(state: &Path) -> PathBuf {
    sibling(state, ".backup.bin")
}

impl ModuleState {
    /// Append a transcript event.
    pub fn event(&mut self, message: impl Into<String>) {
        self.events.push(StateEvent {
            t_unix: now(),
            message: message.into(),
        });
    }

    /// Load a state file; `None` if absent. A corrupt file is an error.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let st: ModuleState = serde_json::from_slice(&bytes).map_err(|e| {
                    Error::Safety(format!(
                        "corrupt module-flash state {}: {e} — refusing to guess progress",
                        path.display()
                    ))
                })?;
                if st.version != STATE_VERSION {
                    return Err(Error::Safety(format!(
                        "module-flash state version {} != {STATE_VERSION} — refusing to resume",
                        st.version
                    )));
                }
                Ok(Some(st))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Write atomically (temp file + rename).
    pub fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_vec_pretty(self)
            .map_err(|e| Error::Config(format!("serialize module-flash state: {e}")))?;
        let tmp = sibling(path, ".tmp");
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Write the original backup sidecar and record its hash.
    pub fn save_backup(&mut self, state_path: &Path, bytes: &[u8]) -> Result<()> {
        let p = backup_path(state_path);
        let tmp = sibling(&p, ".tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &p)?;
        self.backup_saved = true;
        self.backup_sha256 = Some(sha256_hex(bytes));
        Ok(())
    }

    /// Read the backup sidecar back, checking its hash.
    pub fn load_backup(&self, state_path: &Path) -> Result<Vec<u8>> {
        let p = backup_path(state_path);
        let bytes = std::fs::read(&p).map_err(|e| {
            Error::Safety(format!(
                "the original backup {} is missing ({e}) — refusing to resume without it",
                p.display()
            ))
        })?;
        if self.backup_sha256.as_deref() != Some(sha256_hex(&bytes).as_str()) {
            return Err(Error::Safety(format!(
                "the original backup {} does not match its recorded hash — refusing to resume",
                p.display()
            )));
        }
        Ok(bytes)
    }
}
