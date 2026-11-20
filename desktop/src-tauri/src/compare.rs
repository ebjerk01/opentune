//! Image diffs: the findmaps region diff and the map-by-map compare of two
//! images against one definition. The diff and compare themselves live in
//! [`opentune_hub::compare`] (shared with other front ends); these are the
//! Tauri commands around them.

use crate::state::{load_defs, AppData};
pub use opentune_hub::compare::RegionHit;
use serde_json::Value;

#[tauri::command]
pub fn findmaps(
    stock: String,
    tuned: String,
    gap: usize,
    min_len: usize,
) -> Result<Vec<RegionHit>, String> {
    let a = std::fs::read(&stock).map_err(|e| e.to_string())?;
    let b = std::fs::read(&tuned).map_err(|e| e.to_string())?;
    Ok(opentune_hub::compare::findmaps(&a, &b, gap, min_len))
}

/// Compare two images map-by-map through an ECU definition, mirroring the CLI
/// `compare` command. Returns, per `MapDef`: changed-cell count and Δmax / Δavg
/// in physical units (or an `out_of_range` flag). When `map_name` is given, that
/// one map also carries the full before/after physical grids + axes so the UI can
/// draw a before→after view. See [`opentune_hub::compare::compare_maps`].
#[tauri::command]
pub fn compare_maps(
    def_id: String,
    stock: String,
    tuned: String,
    map_name: Option<String>,
    state: tauri::State<AppData>,
) -> Result<Value, String> {
    let defs = load_defs(&state.ecudefs);
    let def = defs
        .iter()
        .find(|d| d.id == def_id)
        .ok_or("ecudef not found")?;
    if def.maps.is_empty() {
        return Err(format!("definition '{def_id}' has no maps to compare"));
    }
    let a = std::fs::read(&stock).map_err(|e| e.to_string())?;
    let b = std::fs::read(&tuned).map_err(|e| e.to_string())?;
    Ok(opentune_hub::compare::compare_maps(
        def,
        &a,
        &b,
        map_name.as_deref(),
    ))
}
