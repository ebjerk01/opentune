//! Flash (safety-gated, file target only) and the backup vault every
//! destructive write archives to first.

use crate::{common::parse_u32, drivers::open_file, state::OpGuard};
use opentune_core::{EcuId, MemoryRegion};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The flash vault beside `path`: `<dir of path>/opentune-vault`. Map
/// write-back, checksum fix and the (file) flash all archive there.
fn vault_dir_for(path: &str) -> PathBuf {
    Path::new(path)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("opentune-vault")
}

/// Archive `data` (the whole current contents of `path`) into the vault beside
/// `path` as a `.bin` + `.meta` pair. Callers abort their write if this fails,
/// so a file is never changed without a backup on disk.
pub(crate) fn vault_backup(path: &str, ecu: &EcuId, data: &[u8]) -> Result<PathBuf, String> {
    let dir = vault_dir_for(path);
    let whole = MemoryRegion::new(0, data.len() as u32);
    opentune_flash::vault::save_backup(&dir, ecu, whole, data).map_err(|e| {
        format!(
            "could not write the safety backup to {} — aborting before any edit: {e}",
            dir.display()
        )
    })
}

/// A vault label for a file: `"<what> <file name>"`.
pub(crate) fn file_label(what: &str, path: &str) -> EcuId {
    let name = Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    EcuId {
        ecu_name: Some(format!("{what} {name}")),
        ..Default::default()
    }
}

/// Run the safety-gated write sequence into an image **file** (file target
/// only in this build: real-ECU flashing is not validated, so there is no
/// driver choice). The whole target file is archived to the vault beside it
/// before anything is written (same vault as map write-back); the battery gate
/// is skipped because a file target has no battery to measure.
#[tauri::command]
pub fn flash(
    path: String,
    addr: String,
    data_path: String,
    guard: tauri::State<OpGuard>,
) -> Result<Value, String> {
    // Hold the long-operation slot for the whole flash so a software update
    // cannot replace the binary mid-write (freed automatically on return/error).
    let _op = guard.begin("flash")?;
    flash_file(&path, &addr, &data_path)
}

/// [`flash`] without the Tauri state wrapper.
fn flash_file(path: &str, addr: &str, data_path: &str) -> Result<Value, String> {
    let addr = parse_u32(addr)?;
    let bytes = std::fs::read(data_path).map_err(|e| e.to_string())?;
    if bytes.is_empty() {
        return Err("the data file is empty — nothing to write".into());
    }
    let region = MemoryRegion::new(addr, bytes.len() as u32);
    // BACKUP FIRST, to disk: abort before opening the target if this fails.
    let original = std::fs::read(path).map_err(|e| e.to_string())?;
    let end = addr as u64 + bytes.len() as u64;
    if end > original.len() as u64 {
        return Err(format!(
            "write past end of image: 0x{addr:08X} + {} bytes ends at 0x{end:08X}, the image is 0x{:08X} bytes",
            bytes.len(),
            original.len()
        ));
    }
    let backup = vault_backup(path, &file_label("before flash", path), &original)?;
    let mut drv = open_file(path)?;
    let safety = opentune_flash::SafetyConfig {
        // A file has no battery: the voltage gate is skipped (0 disables it).
        min_millivolts: 0,
        ..Default::default()
    };
    let rep = opentune_flash::flash_calibration(drv.as_mut(), region, &bytes, &safety, &mut |_| {})
        .map_err(|e| format!("{e} — the original is backed up at {}", backup.display()))?;
    Ok(json!({
        "target": "file",
        "bytes_written": rep.bytes_written,
        "voltage_gate": "skipped — file target, no battery to measure",
        "backup": backup.display().to_string(),
        "backup_len": original.len(),
        "verified": rep.verified,
        "region": format!("0x{:08X} +{} (0x{:08X})", region.address, region.length, region.length),
    }))
}

/// List the vault directory `dir` (see [`opentune_hub::vault::list`]).
#[tauri::command]
pub fn vault_list(dir: String) -> Result<Vec<Value>, String> {
    opentune_hub::vault::list(&dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testutil::{ramp, vault_bins, TmpDir};

    #[test]
    fn flash_file_archives_target_and_skips_voltage_gate() {
        let dir = TmpDir::new("flash");
        let img = dir.join("target.bin");
        let original = ramp(256);
        std::fs::write(&img, &original).unwrap();
        let data = dir.join("data.dat");
        std::fs::write(&data, [0xA5u8; 16]).unwrap();

        let r =
            flash_file(&img.to_string_lossy(), "0x10", &data.to_string_lossy()).expect("flash ok");
        assert_eq!(r["verified"], true);
        assert_eq!(r["bytes_written"], 16);
        assert!(r["voltage_gate"].as_str().unwrap().contains("skipped"));
        let now = std::fs::read(&img).unwrap();
        assert_eq!(&now[0x10..0x20], &[0xA5u8; 16]);
        assert_eq!(&now[..0x10], &original[..0x10]);
        let backup = PathBuf::from(r["backup"].as_str().unwrap());
        assert_eq!(
            std::fs::read(backup).unwrap(),
            original,
            "pre-write image on disk"
        );
    }

    #[test]
    fn flash_file_past_end_is_refused_before_any_backup_or_write() {
        let dir = TmpDir::new("flash-end");
        let img = dir.join("target.bin");
        std::fs::write(&img, ramp(64)).unwrap();
        let data = dir.join("data.dat");
        std::fs::write(&data, [0u8; 32]).unwrap();
        let e = flash_file(&img.to_string_lossy(), "0x30", &data.to_string_lossy()).unwrap_err();
        assert!(e.contains("past end"), "{e}");
        assert_eq!(std::fs::read(&img).unwrap(), ramp(64));
        assert!(vault_bins(&dir).is_empty());
    }
}
