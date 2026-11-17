//! Checksum panel: algorithm lookup (an embedder's resolvers first, then the
//! public `opentune-checksum` set), verify, and fix with a vault backup.

use crate::{
    common::write_error,
    flash::{file_label, vault_backup},
};
use serde::Serialize;
use serde_json::{json, Value};
use std::path::Path;

#[derive(Serialize)]
pub(crate) struct Block {
    name: String,
    ok: bool,
    region: String,
    stored: String,
    computed: String,
}

/// Looks up an extra checksum algorithm by id (see [`DesktopConfig::checksums`]).
pub type ChecksumResolver = fn(&str) -> Option<Box<dyn opentune_checksum::ChecksumAlgo>>;

/// Checksum algorithm lookup (managed state): the embedder's resolvers first,
/// in order, then the public `opentune-checksum` set.
#[derive(Clone, Default)]
pub struct Checksums(pub(crate) Vec<ChecksumResolver>);

impl Checksums {
    /// Resolve `algo` to an implementation, if any resolver knows it.
    pub fn get(&self, algo: &str) -> Option<Box<dyn opentune_checksum::ChecksumAlgo>> {
        self.0
            .iter()
            .find_map(|r| r(algo))
            .or_else(|| opentune_checksum::get(algo))
    }

    pub(crate) fn require(
        &self,
        algo: &str,
    ) -> Result<Box<dyn opentune_checksum::ChecksumAlgo>, String> {
        self.get(algo)
            .ok_or_else(|| format!("unknown algo '{algo}'"))
    }
}

/// One entry of the Checksum panel's algorithm list.
#[derive(Serialize)]
pub(crate) struct AlgoRow {
    id: String,
    name: String,
    needs_reference: bool,
    notes: String,
}

/// The checksum algorithms the core library knows, for the Checksum panel's
/// selector. Plugins that register extra resolvers add their own options from
/// their UI overlay.
#[tauri::command]
pub fn checksum_algos() -> Vec<AlgoRow> {
    opentune_checksum::algorithms()
        .into_iter()
        .map(|a| AlgoRow {
            id: a.id.into(),
            name: a.name.into(),
            needs_reference: a.needs_reference,
            notes: a.notes.into(),
        })
        .collect()
}

#[tauri::command]
pub fn checksum_verify(
    path: String,
    algo: String,
    checksums: tauri::State<'_, Checksums>,
) -> Result<Vec<Block>, String> {
    let a = checksums.require(&algo)?;
    let img = std::fs::read(&path).map_err(|e| e.to_string())?;
    Ok(a.verify(&img)
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|b| Block {
            name: b.name,
            ok: b.ok,
            region: format!("0x{:08X} +{}", b.region.address, b.region.length),
            stored: format!("0x{:08X}", b.stored),
            computed: format!("0x{:08X}", b.computed),
        })
        .collect())
}

/// Recompute and write correct checksums. Writes to `out_path` when given (the
/// UI's default: a new `<name>.fixed.bin`), otherwise in place over `path`.
/// Whenever an existing file is about to be overwritten — the opened image, or
/// an existing Save-As target — its current contents are first archived to the
/// vault beside it (same vault as map write-back); the result's `backup` names
/// that file (`null` when nothing existing was overwritten).
#[tauri::command]
pub fn checksum_fix(
    path: String,
    algo: String,
    out_path: Option<String>,
    checksums: tauri::State<'_, Checksums>,
) -> Result<Value, String> {
    fix_checksums(path, &algo, out_path, &checksums, true)
}

/// [`checksum_fix`] without the Tauri state wrapper (shared with map write-back,
/// which has already archived the original and passes `archive = false`).
pub(crate) fn fix_checksums(
    path: String,
    algo: &str,
    out_path: Option<String>,
    checksums: &Checksums,
    archive: bool,
) -> Result<Value, String> {
    let a = checksums.require(algo)?;
    let mut img = std::fs::read(&path).map_err(|e| e.to_string())?;
    let fixes = a.correct(&mut img).map_err(|e| e.to_string())?;
    let target = out_path
        .filter(|p| !p.trim().is_empty())
        .unwrap_or_else(|| path.clone());
    let mut backup = None;
    if !fixes.is_empty() || target != path {
        if archive && Path::new(&target).exists() {
            let old = std::fs::read(&target).map_err(|e| e.to_string())?;
            backup = Some(vault_backup(
                &target,
                &file_label("before checksum fix", &target),
                &old,
            )?);
        }
        std::fs::write(&target, &img).map_err(|e| {
            let mut m = write_error(&target, &e);
            if let Some(b) = &backup {
                m.push_str(&format!(" — the original is backed up at {}", b.display()));
            }
            m
        })?;
    }
    Ok(json!({
        "in_place": target == path,
        "backup": backup.map(|b| b.display().to_string()),
        "out_path": target,
        "fixes": fixes
            .into_iter()
            .map(|f| format!("{} @0x{:08X}: 0x{:08X} -> 0x{:08X}", f.name, f.offset, f.old, f.new))
            .collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testutil::{ramp, vault_bins, TmpDir};
    use std::path::PathBuf;

    #[test]
    fn checksum_algos_lists_every_core_algorithm() {
        let ids: Vec<String> = checksum_algos().into_iter().map(|a| a.id).collect();
        for id in ["edc16", "edc17", "pcr21", "edc15c6", "me7", "crc32-tail"] {
            assert!(ids.iter().any(|x| x == id), "{id} missing from {ids:?}");
        }
        // Every listed id resolves through the same path the panel uses.
        let cs = Checksums(Vec::new());
        assert!(ids.iter().all(|id| cs.get(id).is_some()));
    }

    #[test]
    fn checksum_fix_in_place_archives_the_original_first() {
        let dir = TmpDir::new("csfix-inplace");
        let img = dir.join("stock.bin");
        let original = ramp(1024);
        std::fs::write(&img, &original).unwrap();
        let p = img.to_string_lossy().into_owned();

        let r = fix_checksums(p.clone(), "crc32-tail", None, &Checksums::default(), true)
            .expect("fix ok");
        assert!(!r["fixes"].as_array().unwrap().is_empty(), "{r}");
        assert_eq!(r["in_place"], true);
        let backup = PathBuf::from(r["backup"].as_str().expect("backup path reported"));
        assert!(backup.starts_with(dir.join("opentune-vault")));
        assert_eq!(std::fs::read(&backup).unwrap(), original);
        assert_ne!(std::fs::read(&img).unwrap(), original, "file was fixed");
    }

    #[test]
    fn checksum_fix_save_as_leaves_original_and_needs_no_backup() {
        let dir = TmpDir::new("csfix-saveas");
        let img = dir.join("stock.bin");
        let original = ramp(1024);
        std::fs::write(&img, &original).unwrap();
        let out = dir.join("stock.fixed.bin");

        let r = fix_checksums(
            img.to_string_lossy().into_owned(),
            "crc32-tail",
            Some(out.to_string_lossy().into_owned()),
            &Checksums::default(),
            true,
        )
        .unwrap();
        assert_eq!(r["in_place"], false);
        assert!(r["backup"].is_null(), "new file: nothing overwritten");
        assert_eq!(std::fs::read(&img).unwrap(), original);
        assert!(out.exists());
        assert!(vault_bins(&dir).is_empty());

        // Overwriting an existing Save-As target archives what was there.
        std::fs::write(&out, b"old contents").unwrap();
        let r2 = fix_checksums(
            img.to_string_lossy().into_owned(),
            "crc32-tail",
            Some(out.to_string_lossy().into_owned()),
            &Checksums::default(),
            true,
        )
        .unwrap();
        let b = PathBuf::from(r2["backup"].as_str().unwrap());
        assert_eq!(std::fs::read(b).unwrap(), b"old contents");
    }
}
