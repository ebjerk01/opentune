//! Image, Identify and Analyze-bin panels: file size + hex preview, identity
//! matching against the ECU definitions and the vehicle catalog, and the offline
//! image analysis (no hardware).

use crate::{
    checksum::Checksums,
    drivers::open_file,
    state::{load_cat, load_defs, AppData},
};
use opentune_core::{
    catalog::{self, CatalogEntry},
    ecudef::{self, EcuDef},
    EcuId,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

#[tauri::command]
pub fn image_info(path: String) -> Result<Value, String> {
    let meta = std::fs::metadata(&path).map_err(|e| e.to_string())?;
    let name = Path::new(&path)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    Ok(json!({ "path": path, "name": name, "size": meta.len() }))
}

#[tauri::command]
pub fn hex_preview(path: String, offset: u64, rows: usize) -> Result<Value, String> {
    let total = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let data = std::fs::read(&path).map_err(|e| e.to_string())?;
    let start = offset.min(total) as usize;
    let want = rows.max(1) * 16;
    let end = (start + want).min(data.len());
    let mut lines = Vec::new();
    let mut a = start;
    while a < end {
        let row = &data[a..(a + 16).min(end)];
        let hex: Vec<String> = row.iter().map(|b| format!("{:02X}", b)).collect();
        let ascii: String = row
            .iter()
            .map(|&b| {
                if (0x20..0x7f).contains(&b) {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        lines.push(json!({ "addr": format!("0x{:08X}", a), "hex": hex, "ascii": ascii }));
        a += 16;
    }
    Ok(json!({ "offset": start, "total": total, "lines": lines }))
}

#[derive(Serialize)]
struct DefMatch {
    id: String,
    name: String,
    maker: String,
    family: String,
    protocol: String,
    checksum: Option<String>,
    seedkey: Option<String>,
}

#[derive(Serialize)]
struct CatMatch {
    make: String,
    models: Vec<String>,
    years: Option<String>,
    family: Option<String>,
    source: Option<String>,
}

fn def_match(d: &EcuDef) -> DefMatch {
    DefMatch {
        id: d.id.clone(),
        name: d.name.clone(),
        maker: d.maker.clone(),
        family: d.family.clone(),
        protocol: format!("{:?}", d.protocol),
        checksum: d.checksum.clone(),
        seedkey: d.seedkey.clone(),
    }
}

fn cat_matches(entries: &[CatalogEntry], id: &EcuId) -> Vec<CatMatch> {
    catalog::match_catalog(entries, id)
        .into_iter()
        .take(16)
        .map(|c| CatMatch {
            make: c.make.clone(),
            models: c.models.clone(),
            years: c.years.clone(),
            family: c.family.clone(),
            source: c.source.clone(),
        })
        .collect()
}

/// The offline file driver has no software number to report, so it puts the
/// image size (`"<n> bytes"`) in `sw_number`. That is not an identity: drop it
/// so the UI shows the SW # as unknown and the size only as the size.
fn drop_file_size_sw(id: &mut EcuId, size: u64) {
    if id.sw_number.as_deref() == Some(format!("{size} bytes").as_str()) {
        id.sw_number = None;
    }
}

#[tauri::command]
pub fn identify(path: String, state: tauri::State<AppData>) -> Result<Value, String> {
    let mut drv = open_file(&path)?;
    let mut id: EcuId = drv.read_ecu_id().map_err(|e| e.to_string())?;
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    drop_file_size_sw(&mut id, size);

    let defs = load_defs(&state.ecudefs);
    let matched = ecudef::match_ecu(&defs, &id).map(def_match);
    let entries = load_cat(&state.catalog);
    let catalog = cat_matches(&entries, &id);

    Ok(json!({
        "ecu_name": id.ecu_name,
        "hw_number": id.hw_number,
        "sw_number": id.sw_number,
        "vin": id.vin,
        "size": size,
        "matched": matched,
        "catalog": catalog,
    }))
}

/// Offline identity matching: the mock driver reports only a name + size, so this
/// lets the operator type a real HW / SW number to exercise ecudef + catalog
/// matching against the bundled databases.
#[tauri::command]
pub fn match_identity(
    hw: String,
    sw: String,
    state: tauri::State<AppData>,
) -> Result<Value, String> {
    let id = EcuId {
        hw_number: (!hw.trim().is_empty()).then(|| hw.trim().to_string()),
        sw_number: (!sw.trim().is_empty()).then(|| sw.trim().to_string()),
        ..Default::default()
    };
    let defs = load_defs(&state.ecudefs);
    let matched = ecudef::match_ecu(&defs, &id).map(def_match);
    let entries = load_cat(&state.catalog);
    let catalog = cat_matches(&entries, &id);
    Ok(json!({ "matched": matched, "catalog": catalog }))
}

/// Map a detected structural family name to the checksum algorithm id that
/// verifies its protected blocks.
fn family_to_algo(family: &str) -> Option<&'static str> {
    match family {
        "EDC16" => Some("edc16"),
        "EDC17" => Some("edc17"),
        "PCR2.1" => Some("pcr21"),
        "EDC15" => Some("edc15"),
        // Recognised by signature (no structural family): ME7.x has a validated
        // checksum engine that refuses images whose tables it cannot locate.
        "ME7" => Some("me7"),
        _ => None,
    }
}

/// Serialize a parsed part number into a readable breakdown (shared by Analyze
/// and the part-decoder utility panel).
pub(crate) fn part_breakdown(pn: &opentune_vin::PartNumber) -> Value {
    json!({
        "normalized": pn.normalized,
        "kind": format!("{:?}", pn.kind),
        "supplier": pn.supplier,
        "vag_type_group": pn.vag_type_group,
        "vag_main_group": pn.vag_main_group,
        "vag_subgroup": pn.vag_subgroup,
        "vag_version": pn.vag_version,
        "bosch_family": pn.bosch_family,
        "bosch_consecutive": pn.bosch_consecutive,
        "flags": pn.flags,
    })
}

/// Fully-offline analysis of a raw flash image: structural ECU family, identity
/// strings, catalog/definition match, checksum blocks and any known maps —
/// everything determinable with NO hardware. This is the honest fix for the old
/// `identify` gap (the mock driver reports almost nothing on a raw `.bin`).
///
/// `ecudefs_dir` / `catalog_dir` override the bundled data directories; both
/// fall back to the resolved app data. Confidence is graded by opentune-scan,
/// which never assigns a car it cannot justify from a DB/VIN/part match.
#[tauri::command]
pub fn analyze_image(
    path: String,
    ecudefs_dir: Option<String>,
    catalog_dir: Option<String>,
    state: tauri::State<AppData>,
    checksums: tauri::State<Checksums>,
) -> Result<Value, String> {
    let ecudefs_dir = ecudefs_dir
        .map(PathBuf::from)
        .or_else(|| state.ecudefs.clone());
    let catalog_dir = catalog_dir
        .map(PathBuf::from)
        .or_else(|| state.catalog.clone());
    let ecudefs = load_defs(&ecudefs_dir);
    let catalog = load_cat(&catalog_dir);
    let ctx = opentune_scan::ScanContext {
        catalog: catalog.to_vec(),
        ecudefs: ecudefs.to_vec(),
    };

    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    let p = Path::new(&path);
    let root = p.parent().unwrap_or_else(|| Path::new("."));
    let rec = opentune_scan::scan_file(&ctx, root, p, &bytes);

    // Checksum blocks for the detected family (best effort).
    let algo_id = family_to_algo(rec.ecu_family);
    let checksum_blocks: Vec<Value> = match algo_id.and_then(|id| checksums.get(id)) {
        Some(a) => a
            .verify(&bytes)
            .map(|blocks| {
                blocks
                    .into_iter()
                    .map(|b| {
                        json!({
                            "name": b.name,
                            "ok": b.ok,
                            "region": format!("0x{:08X} +{}", b.region.address, b.region.length),
                            "stored": format!("0x{:08X}", b.stored),
                            "computed": format!("0x{:08X}", b.computed),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        None => Vec::new(),
    };

    // Known maps from a matched definition.
    let maps: Vec<Value> = rec
        .ecudef_match
        .as_ref()
        .and_then(|hit| ecudefs.iter().find(|d| d.id == hit.id))
        .map(|def| {
            def.maps
                .iter()
                .map(|m| {
                    json!({
                        "name": m.name,
                        "address": format!("0x{:08X}", m.address),
                        "rows": m.rows,
                        "cols": m.cols,
                        "data_type": format!("{:?}", m.data_type),
                        "unit": m.unit,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    // Identity-string breakdowns.
    let best_part = rec
        .part_numbers
        .first()
        .or_else(|| rec.bosch_numbers.first())
        .or_else(|| rec.mercedes_numbers.first());
    let part_decoded = best_part.map(|p| part_breakdown(&opentune_vin::parse_part_number(p)));

    Ok(json!({
        "path": path,
        "size": bytes.len(),
        "size_class": rec.size_class,
        "family": rec.ecu_family,
        "family_how": rec.family_how,
        "supplier": rec.ecu_supplier,
        "variant": rec.ecu_variant,
        "family_confidence": rec.family_confidence,
        "recognition": rec.recognition,
        "identity": rec.identity,
        "part_numbers": rec.part_numbers,
        "bosch_numbers": rec.bosch_numbers,
        "sw_numbers": rec.sw_numbers,
        "mercedes_numbers": rec.mercedes_numbers,
        "epk_hits": rec.epk_hits,
        "vin": rec.vin,
        "vin_decoded": rec.vin_decoded,
        "part_decoded": part_decoded,
        "catalog_match": rec.catalog_match,
        "ecudef_match": rec.ecudef_match,
        "assignment": rec.assigned_core,
        "confidence": rec.confidence_core.as_str(),
        "checksum_algo": algo_id,
        "checksum_blocks": checksum_blocks,
        "maps": maps,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_algo_mapping() {
        assert_eq!(family_to_algo("EDC16"), Some("edc16"));
        assert_eq!(family_to_algo("EDC17"), Some("edc17"));
        assert_eq!(family_to_algo("PCR2.1"), Some("pcr21"));
        assert_eq!(family_to_algo("EDC15"), Some("edc15"));
        assert_eq!(family_to_algo("ME7"), Some("me7"));
        assert_eq!(family_to_algo("Unknown"), None);
    }

    #[test]
    fn identify_drops_the_file_size_posing_as_sw_number() {
        let mut id = EcuId {
            sw_number: Some("2048 bytes".into()),
            ..Default::default()
        };
        drop_file_size_sw(&mut id, 2048);
        assert!(id.sw_number.is_none());
        let mut real = EcuId {
            sw_number: Some("1037370634".into()),
            ..Default::default()
        };
        drop_file_size_sw(&mut real, 2048);
        assert_eq!(real.sw_number.as_deref(), Some("1037370634"));
    }
}
