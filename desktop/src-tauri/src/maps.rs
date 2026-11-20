//! Maps panel: definition and map lists, read-only map decode, and the map
//! editor's inverse scaling and byte-level write-back (vault backup + checksum fix).

use crate::{
    checksum::{fix_checksums, Checksums},
    common::write_error,
    flash::vault_backup,
    state::{load_defs, AppData, OpGuard},
};
use opentune_core::{
    ecudef::{EcuDef, MapDef},
    EcuId,
};
// The pure image <-> value codecs live in `opentune_hub::maps`, shared with
// other front ends.
use opentune_hub::maps::encode_elem;
pub(crate) use opentune_hub::maps::decode_map_value;
use serde::Serialize;
use serde_json::{json, Value};
use std::path::Path;

#[derive(Serialize)]
pub(crate) struct DefInfo {
    id: String,
    name: String,
    maker: String,
    family: String,
    protocol: String,
    flash_kib: u32,
    checksum: Option<String>,
    seedkey: Option<String>,
    maps: usize,
    notes: String,
}

// Async (off the UI thread): the first call may parse every definition.
#[tauri::command]
pub async fn list_ecudefs(state: tauri::State<'_, AppData>) -> Result<Vec<DefInfo>, String> {
    Ok(load_defs(&state.ecudefs)
        .iter()
        .map(|d| DefInfo {
            protocol: format!("{:?}", d.protocol),
            flash_kib: d.flash_size / 1024,
            maps: d.maps.len(),
            notes: d.notes.clone(),
            checksum: d.checksum.clone(),
            seedkey: d.seedkey.clone(),
            maker: d.maker.clone(),
            family: d.family.clone(),
            name: d.name.clone(),
            id: d.id.clone(),
        })
        .collect())
}

#[tauri::command]
pub async fn list_maps(
    ecudef_id: String,
    state: tauri::State<'_, AppData>,
) -> Result<Vec<Value>, String> {
    let defs = load_defs(&state.ecudefs);
    let def = defs
        .iter()
        .find(|d| d.id == ecudef_id)
        .ok_or("ecudef not found")?;
    Ok(def
        .maps
        .iter()
        .map(|m| {
            json!({
                "name": m.name,
                "address": format!("0x{:08X}", m.address),
                "rows": m.rows,
                "cols": m.cols,
                "data_type": format!("{:?}", m.data_type),
                "endian": format!("{:?}", m.endian),
                "unit": m.unit,
                "factor": m.scaling.factor,
                "offset": m.scaling.offset,
                "min": m.min,
                "max": m.max,
                "byte_len": m.byte_len(),
                "notes": m.notes,
            })
        })
        .collect())
}

/// Decode a defined `MapDef` out of the opened image into a physical-value grid,
/// honouring the data type, endianness and linear scaling. Read-only: this is the
/// public edition's map viewer, so it only reads — it never writes the image.
#[tauri::command]
pub fn decode_map(
    path: String,
    ecudef_id: String,
    map_name: String,
    state: tauri::State<AppData>,
) -> Result<Value, String> {
    let defs = load_defs(&state.ecudefs);
    let def = defs
        .iter()
        .find(|d| d.id == ecudef_id)
        .ok_or("ecudef not found")?;
    let map: &MapDef = def
        .maps
        .iter()
        .find(|m| m.name == map_name)
        .ok_or("map not found")?;
    let img = std::fs::read(&path).map_err(|e| e.to_string())?;
    decode_map_value(&img, map, Some(def.flash_size / 1024))
}

/// Decode an **ad-hoc** `MapDef` (imported from an XDF/A2L or promoted from a
/// findmaps region) out of the opened image, without it belonging to any bundled
/// definition. Returns the same viewer/editor payload as [`decode_map`], plus a
/// `mapdef` field echoing the exact MapDef so the frontend can hand it back to
/// [`mapdef_write`] for a byte-level write. This closes the import/promote → edit
/// loop the CLI importers could not reach.
#[tauri::command]
pub fn decode_mapdef(path: String, map: MapDef) -> Result<Value, String> {
    let img = std::fs::read(&path).map_err(|e| e.to_string())?;
    let mut v = decode_map_value(&img, &map, None)?;
    v["mapdef"] = serde_json::to_value(&map).map_err(|e| e.to_string())?;
    Ok(v)
}

/// Write an edited calibration map back into a flash image.
///
/// Safety is the whole point of this command, in order:
/// 1. Refuse on a dimension mismatch (the grid must be exactly `rows × cols`).
/// 2. Refuse on a zero scaling factor (physical→raw would divide by zero).
/// 3. Refuse if the map region runs past the end of the image.
/// 4. Take a **full-image backup into the flash vault FIRST** — if that fails,
///    abort before touching anything, so an edit can never happen un-backed-up.
/// 5. Apply inverse scaling (`raw = (phys − offset) / factor`), round + clamp to
///    the data type's range, and write ONLY the map's byte region (type+endian).
/// 6. Optionally run a checksum fix (reusing [`checksum_fix`] with the def's algo).
///
/// `out_path` writes a Save-As copy instead of editing `path` in place (the
/// read-only / permission fallback). On a write error the message names the
/// safety backup so the original is always recoverable.
/// Shared write-back core for both the definition-backed [`map_write`] and the
/// ad-hoc [`mapdef_write`]. Carries the full safety sequence (dimension / zero-
/// factor / bounds guards, backup-first, byte-level write, optional checksum
/// fix). `ecu` labels the vault backup; `algo` is the checksum id to run after a
/// successful write (or `None` to skip even when `fix_checksum` is set, which is
/// reported honestly).
#[allow(clippy::too_many_arguments)]
fn write_map_core(
    path: &str,
    map: &MapDef,
    new_cells: &[Vec<f64>],
    out_path: Option<String>,
    fix_checksum: bool,
    algo: Option<String>,
    ecu: EcuId,
    flash_kib: Option<u32>,
    checksums: &Checksums,
) -> Result<Value, String> {
    let rows = map.rows.max(1) as usize;
    let cols = map.cols.max(1) as usize;

    // (1) Dimension guard — reject before reading anything.
    if new_cells.len() != rows || new_cells.iter().any(|r| r.len() != cols) {
        let got_cols = new_cells.first().map(|r| r.len()).unwrap_or(0);
        return Err(format!(
            "dimension mismatch: map '{}' is {rows}×{cols} but the edit is {}×{got_cols} — refusing to write",
            map.name,
            new_cells.len(),
        ));
    }
    // (2) Zero scaling factor guard — cannot invert physical→raw.
    if map.scaling.factor == 0.0 {
        return Err(format!(
            "map '{}' has a zero scaling factor — physical→raw is undefined, refusing to write",
            map.name
        ));
    }

    let mut img = std::fs::read(path).map_err(|e| e.to_string())?;
    let w = map.data_type.byte_len() as usize;
    let need = map.address as usize + rows * cols * w;
    // (3) Bounds guard — never write past the end of the image.
    if need > img.len() {
        let hint = match flash_kib {
            Some(kib) => format!(" — open a full dump (its flash is {kib} KiB)"),
            None => " — open a full dump this map fits in".to_string(),
        };
        return Err(format!(
            "map needs 0x{:08X} bytes but the image is only 0x{:08X}{hint}",
            need,
            img.len(),
        ));
    }

    // (4) BACKUP FIRST: archive the whole original image into the flash vault,
    // beside the file being edited. If this fails we abort before any edit.
    let backup = vault_backup(path, &ecu, &img)?;

    // (5) Apply the edit: inverse scaling, clamp, write only the map's bytes.
    let mut cells_changed = 0usize;
    // Dimensions were checked above: new_cells is exactly rows × cols.
    for (r, row) in new_cells.iter().enumerate() {
        for (c, &cell) in row.iter().enumerate() {
            let off = map.address as usize + (r * cols + c) * w;
            let bytes = encode_elem(cell, map.data_type, map.endian, map.scaling);
            if img[off..off + w] != bytes[..] {
                cells_changed += 1;
            }
            img[off..off + w].copy_from_slice(&bytes);
        }
    }

    let target = out_path.unwrap_or_else(|| path.to_string());
    std::fs::write(&target, &img).map_err(|e| {
        format!(
            "{} — your original is safe: a full backup was saved to {}",
            write_error(&target, &e),
            backup.display()
        )
    })?;

    // (6) Optional checksum fix, reusing the existing command with the map's algo.
    let mut checksum = Value::Null;
    if fix_checksum {
        checksum = match &algo {
            Some(a) => match fix_checksums(target.clone(), a, None, checksums, false) {
                Ok(v) => v,
                Err(e) => json!({ "algo": a, "error": e }),
            },
            None => {
                json!({ "error": "no checksum algorithm is set for this map — pick one on the Checksum panel and re-run it manually" })
            }
        };
    }

    Ok(json!({
        "out_path": target,
        "backup": backup.display().to_string(),
        "in_place": target == path,
        "cells_total": rows * cols,
        "cells_changed": cells_changed,
        "checksum_algo": algo,
        "checksum": checksum,
    }))
}

#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub fn map_write(
    path: String,
    ecudef_id: String,
    map_name: String,
    new_cells: Vec<Vec<f64>>,
    out_path: Option<String>,
    fix_checksum: Option<bool>,
    state: tauri::State<AppData>,
    guard: tauri::State<OpGuard>,
    checksums: tauri::State<Checksums>,
) -> Result<Value, String> {
    // A map write-back takes a full-image backup and rewrites the image; hold the
    // long-operation slot so a software update cannot land in the middle of it.
    let _op = guard.begin("map write-back")?;
    let defs = load_defs(&state.ecudefs);
    let def = defs
        .iter()
        .find(|d| d.id == ecudef_id)
        .ok_or("ecudef not found")?;
    let map: &MapDef = def
        .maps
        .iter()
        .find(|m| m.name == map_name)
        .ok_or("map not found")?;

    let ecu = EcuId {
        ecu_name: Some(format!("{} [{}]", def.name, def.id)),
        hw_number: def.identify.part_numbers.first().cloned(),
        sw_number: def.identify.sw_numbers.first().cloned(),
        ..Default::default()
    };
    let mut v = write_map_core(
        &path,
        map,
        &new_cells,
        out_path,
        fix_checksum.unwrap_or(false),
        def.checksum.clone(),
        ecu,
        Some(def.flash_size / 1024),
        &checksums,
    )?;
    v["checksum_source"] = json!(format!("definition {}", def.name));
    Ok(v)
}

/// Pick the checksum family for an ad-hoc map write-back: the matched ECU
/// definition's `checksum` first, then the caller's (Checksum panel) choice only
/// when no definition names one. Returns `(algo, source)`; `source` says which
/// one was used so the UI can show it.
fn adhoc_checksum_algo(
    matched: Option<&EcuDef>,
    panel: Option<String>,
) -> (Option<String>, String) {
    if let Some(d) = matched {
        if let Some(a) = d.checksum.as_ref().filter(|a| !a.trim().is_empty()) {
            return (Some(a.clone()), format!("matched definition {}", d.name));
        }
    }
    match panel.filter(|a| !a.trim().is_empty()) {
        Some(a) => (Some(a), "Checksum panel selection".into()),
        None => (None, "none known".into()),
    }
}

/// Write-back for an **ad-hoc** `MapDef` (imported/promoted, not in any bundled
/// definition). Same full safety sequence as [`map_write`]. The checksum family
/// comes from the ECU definition the image matches (structural/identity scan,
/// as in Analyze bin); the caller's `checksum_algo` (the Checksum panel) is only
/// a fallback when no definition with a checksum matches. The result's
/// `checksum_source` says which was used.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn mapdef_write(
    path: String,
    map: MapDef,
    new_cells: Vec<Vec<f64>>,
    out_path: Option<String>,
    fix_checksum: Option<bool>,
    checksum_algo: Option<String>,
    state: tauri::State<AppData>,
    guard: tauri::State<OpGuard>,
    checksums: tauri::State<Checksums>,
) -> Result<Value, String> {
    // Same long-operation guard as the definition-backed write path.
    let _op = guard.begin("map write-back")?;
    let ecu = EcuId {
        ecu_name: Some(format!("ad-hoc map '{}'", map.name)),
        ..Default::default()
    };
    let defs = load_defs(&state.ecudefs);
    let matched = if fix_checksum.unwrap_or(false) && !defs.is_empty() {
        let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
        let p = Path::new(&path);
        let ctx = opentune_scan::ScanContext {
            catalog: Vec::new(),
            ecudefs: defs.to_vec(),
        };
        let rec = opentune_scan::scan_file(&ctx, p.parent().unwrap_or(Path::new(".")), p, &bytes);
        rec.ecudef_match
            .and_then(|hit| defs.iter().find(|d| d.id == hit.id).cloned())
    } else {
        None
    };
    let (algo, source) = adhoc_checksum_algo(matched.as_ref(), checksum_algo);
    let mut v = write_map_core(
        &path,
        &map,
        &new_cells,
        out_path,
        fix_checksum.unwrap_or(false),
        algo,
        ecu,
        None,
        &checksums,
    )?;
    v["checksum_source"] = json!(source);
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def_with_checksum(cs: Option<&str>) -> EcuDef {
        serde_json::from_value(json!({
            "id": "t", "name": "Test ECU", "maker": "x", "family": "EDC16",
            "protocol": "iso_tp", "diag_layer": "uds", "bitrate": 500000,
            "checksum": cs,
        }))
        .unwrap()
    }

    #[test]
    fn adhoc_checksum_prefers_matched_definition_over_panel() {
        let d = def_with_checksum(Some("edc16"));
        let (a, src) = adhoc_checksum_algo(Some(&d), Some("crc32-tail".into()));
        assert_eq!(a.as_deref(), Some("edc16"));
        assert!(src.contains("matched definition Test ECU"), "{src}");

        // A matched def without a checksum, or no match: the panel is the fallback.
        let none = def_with_checksum(None);
        let (a, src) = adhoc_checksum_algo(Some(&none), Some("crc32-tail".into()));
        assert_eq!(a.as_deref(), Some("crc32-tail"));
        assert_eq!(src, "Checksum panel selection");
        let (a, _) = adhoc_checksum_algo(None, Some("pcr21".into()));
        assert_eq!(a.as_deref(), Some("pcr21"));
        let (a, src) = adhoc_checksum_algo(None, Some("  ".into()));
        assert!(a.is_none());
        assert_eq!(src, "none known");
    }
}
