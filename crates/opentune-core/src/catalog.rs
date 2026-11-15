//! Vehicle → ECU catalog: a broad, lightweight identification database (e.g.
//! harvested from open datasets like comma.ai's opendbc/openpilot). It maps a
//! reported ECU identity (firmware/part numbers) to a car + ECU family, even
//! for ECUs OpenTune doesn't yet fully define. Detailed support still lives in
//! `ecudefs/`; this is the wide net for `identify`.
use crate::types::EcuId;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// One catalog record: an ECU variant seen in one or more vehicles.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub make: String,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub years: Option<String>,
    /// Which ECU (e.g. "engine", "transmission").
    #[serde(default)]
    pub ecu: Option<String>,
    /// Diagnostic addresses the ECU answers on (e.g. "0x7E0").
    #[serde(default)]
    pub addresses: Vec<String>,
    /// Firmware/version strings this ECU reports.
    #[serde(default)]
    pub fw_versions: Vec<String>,
    /// Hardware/part numbers.
    #[serde(default)]
    pub part_numbers: Vec<String>,
    /// ECU family (e.g. "EDC17", "PCR2.1"), when known.
    #[serde(default)]
    pub family: Option<String>,
    /// If a detailed `ecudefs/` definition exists, its id.
    #[serde(default)]
    pub ecudef_id: Option<String>,
    /// Where this record came from (e.g. "opendbc").
    #[serde(default)]
    pub source: Option<String>,
}

fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_uppercase()
}

impl CatalogEntry {
    /// True if `id`'s reported HW/SW number matches this entry.
    pub fn matches_id(&self, id: &EcuId) -> bool {
        let mut keys: Vec<String> = Vec::new();
        keys.extend(self.fw_versions.iter().cloned());
        keys.extend(self.part_numbers.iter().cloned());
        let keys: Vec<String> = keys
            .iter()
            .map(|k| norm(k))
            .filter(|k| !k.is_empty())
            .collect();
        for v in [&id.hw_number, &id.sw_number].into_iter().flatten() {
            let v = norm(v);
            if keys
                .iter()
                .any(|k| v.starts_with(k) || v.contains(k) || k.contains(&v))
            {
                return true;
            }
        }
        false
    }
}

/// Load every `*.json` catalog file in a directory. Each file is either a
/// single `CatalogEntry` or an array of them.
pub fn load_catalog(dir: &Path) -> crate::Result<Vec<CatalogEntry>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let txt = std::fs::read_to_string(&path)?;
        if let Ok(arr) = serde_json::from_str::<Vec<CatalogEntry>>(&txt) {
            out.extend(arr);
        } else {
            let one: CatalogEntry =
                serde_json::from_str(&txt).map_err(|e| crate::Error::Config(e.to_string()))?;
            out.push(one);
        }
    }
    Ok(out)
}

/// All catalog entries matching a reported ECU id.
pub fn match_catalog<'a>(entries: &'a [CatalogEntry], id: &EcuId) -> Vec<&'a CatalogEntry> {
    entries.iter().filter(|e| e.matches_id(id)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_by_part_number() {
        let e = CatalogEntry {
            make: "Volkswagen".into(),
            part_numbers: vec!["03L906".into()],
            ..Default::default()
        };
        let id = EcuId {
            hw_number: Some("03L906023E".into()),
            ..Default::default()
        };
        assert!(e.matches_id(&id));
    }

    #[test]
    fn no_false_match() {
        let e = CatalogEntry {
            make: "Toyota".into(),
            part_numbers: vec!["896631".into()],
            ..Default::default()
        };
        let id = EcuId {
            hw_number: Some("03G906016".into()),
            ..Default::default()
        };
        assert!(!e.matches_id(&id));
    }
}
