//! ECU definitions: data describing a known ECU (protocol, memory map, which
//! checksum and seed/key handlers apply). Loaded from JSON in `ecudefs/`.
use crate::types::{DiagLayer, MemoryRegion, Protocol};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EcuDef {
    pub id: String,
    pub name: String,
    pub maker: String,
    pub family: String,
    #[serde(default)]
    pub notes: String,
    pub protocol: Protocol,
    pub diag_layer: DiagLayer,
    pub bitrate: u32,
    #[serde(default)]
    pub tx_id: u32,
    #[serde(default)]
    pub rx_id: u32,
    #[serde(default)]
    pub flash_size: u32,
    #[serde(default)]
    pub regions: Vec<NamedRegion>,
    /// Checksum algorithm id handled by `opentune-checksum` (e.g. "edc16").
    #[serde(default)]
    pub checksum: Option<String>,
    /// Seed/key algorithm id handled by a seed/key plugin.
    #[serde(default)]
    pub seedkey: Option<String>,
    #[serde(default)]
    pub full_read: FullRead,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamedRegion {
    pub name: String,
    pub region: MemoryRegion,
    #[serde(default)]
    pub writable: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FullRead {
    #[default]
    Obd,
    BenchBoot,
}

impl EcuDef {
    pub fn from_json(s: &str) -> crate::Result<Self> {
        serde_json::from_str(s).map_err(|e| crate::Error::Config(e.to_string()))
    }
}

/// Load every `*.json` ECU definition in a directory.
pub fn load_dir(dir: &Path) -> crate::Result<Vec<EcuDef>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|s| s.to_str()) == Some("json") {
            out.push(EcuDef::from_json(&std::fs::read_to_string(&path)?)?);
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}
