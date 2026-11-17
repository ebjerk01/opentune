//! Image diffs: the findmaps region diff and the map-by-map compare of two
//! images against one definition.

use crate::{
    maps::{decode_axis, decode_cells},
    state::{load_defs, AppData},
};
use serde::Serialize;
use serde_json::{json, Value};

#[derive(Serialize)]
pub(crate) struct RegionHit {
    index: usize,
    start: String,
    end: String,
    len: usize,
    hint: String,
}

fn stride_hint(len: usize) -> String {
    if len >= 4 && len.is_multiple_of(2) {
        let cells = len / 2;
        for &w in &[16usize, 12, 10, 8] {
            if cells.is_multiple_of(w) {
                return format!("~{}x{} u16?", cells / w, w);
            }
        }
        return format!("~{cells} u16?");
    }
    String::new()
}

#[tauri::command]
pub fn findmaps(
    stock: String,
    tuned: String,
    gap: usize,
    min_len: usize,
) -> Result<Vec<RegionHit>, String> {
    let a = std::fs::read(&stock).map_err(|e| e.to_string())?;
    let b = std::fs::read(&tuned).map_err(|e| e.to_string())?;
    let n = a.len().min(b.len());
    let mut out = Vec::new();
    let mut i = 0;
    let mut idx = 0;
    while i < n {
        if a[i] != b[i] {
            let start = i;
            let mut end = i + 1;
            let mut since = 0usize;
            let mut j = i + 1;
            while j < n && since <= gap {
                if a[j] != b[j] {
                    end = j + 1;
                    since = 0;
                } else {
                    since += 1;
                }
                j += 1;
            }
            if end - start >= min_len {
                out.push(RegionHit {
                    index: idx,
                    start: format!("0x{start:08X}"),
                    end: format!("0x{end:08X}"),
                    len: end - start,
                    hint: stride_hint(end - start),
                });
                idx += 1;
            }
            i = end;
        } else {
            i += 1;
        }
    }
    Ok(out)
}

/// Compare two images map-by-map through an ECU definition, mirroring the CLI
/// `compare` command. Returns, per `MapDef`: changed-cell count and Δmax / Δavg
/// in physical units (or an `out_of_range` flag). When `map_name` is given, that
/// one map also carries the full before/after physical grids + axes so the UI can
/// draw a before→after view.
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

    let want_grid = map_name.as_deref();
    let mut maps = Vec::with_capacity(def.maps.len());
    let mut total_changed = 0usize;
    let mut changed_maps = 0usize;

    for m in &def.maps {
        let (pa, pb) = match (decode_cells(&a, m), decode_cells(&b, m)) {
            (Some(pa), Some(pb)) => (pa, pb),
            _ => {
                maps.push(json!({
                    "name": m.name, "rows": m.rows, "cols": m.cols,
                    "unit": m.unit, "out_of_range": true,
                }));
                continue;
            }
        };
        let mut changed = 0usize;
        let mut max_abs = 0f64;
        let mut sum_delta = 0f64;
        for (ra, rb) in pa.iter().zip(pb.iter()) {
            for (x, y) in ra.iter().zip(rb.iter()) {
                let d = y - x;
                if d.abs() > 1e-9 {
                    changed += 1;
                    sum_delta += d;
                    if d.abs() > max_abs {
                        max_abs = d.abs();
                    }
                }
            }
        }
        total_changed += changed;
        if changed > 0 {
            changed_maps += 1;
        }
        let avg = if changed > 0 {
            sum_delta / changed as f64
        } else {
            0.0
        };
        let mut entry = json!({
            "name": m.name,
            "rows": m.rows,
            "cols": m.cols,
            "unit": m.unit,
            "data_type": format!("{:?}", m.data_type),
            "changed": changed,
            "d_max": max_abs,
            "d_avg": avg,
            "out_of_range": false,
        });
        if want_grid == Some(m.name.as_str()) {
            let (x_vals, x_unit) = decode_axis(&a, &m.x_axis, m.cols);
            let (y_vals, y_unit) = decode_axis(&a, &m.y_axis, m.rows);
            entry["stock"] = json!(pa);
            entry["tuned"] = json!(pb);
            entry["x_axis"] = json!({ "name": m.x_axis.as_ref().map(|ax| ax.name.clone()), "unit": x_unit, "values": x_vals });
            entry["y_axis"] = json!({ "name": m.y_axis.as_ref().map(|ax| ax.name.clone()), "unit": y_unit, "values": y_vals });
        }
        maps.push(entry);
    }

    Ok(json!({
        "def": def.id,
        "def_name": def.name,
        "stock_size": a.len(),
        "tuned_size": b.len(),
        "total_changed": total_changed,
        "changed_maps": changed_maps,
        "maps": maps,
    }))
}
