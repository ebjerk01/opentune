//! Image diffs for a front end: the findmaps region diff and the map-by-map
//! compare of two images against one definition, as the payloads the
//! desktop's Find maps / Compare panels show. The diff clustering and the
//! cell-delta summary are [`opentune_mapfind::diff`], shared with the CLI.

use crate::maps::{decode_axis, decode_cells};
use opentune_core::ecudef::EcuDef;
use opentune_mapfind::diff::{diff_regions, u16_shape, MapDelta, U16Shape};
use serde::Serialize;
use serde_json::{json, Value};

/// One candidate map region of a stock/tuned diff.
#[derive(Serialize)]
pub struct RegionHit {
    /// Position in the list of reported regions (0-based).
    pub index: usize,
    /// First changed byte, `0x%08X`.
    pub start: String,
    /// One past the last changed byte, `0x%08X`.
    pub end: String,
    /// `end - start` in bytes.
    pub len: usize,
    /// The 16-bit table-shape guess (`~16x16 u16?`, `~3 u16?`), or empty.
    pub hint: String,
}

fn stride_hint(len: usize) -> String {
    match u16_shape(len) {
        Some(U16Shape::Grid { rows, cols }) => format!("~{rows}x{cols} u16?"),
        Some(U16Shape::Cells(cells)) => format!("~{cells} u16?"),
        None => String::new(),
    }
}

/// The findmaps diff of two images: changed regions (changes up to `gap`
/// unchanged bytes apart merged) at least `min_len` bytes long.
pub fn findmaps(a: &[u8], b: &[u8], gap: usize, min_len: usize) -> Vec<RegionHit> {
    diff_regions(a, b, gap, min_len)
        .into_iter()
        .enumerate()
        .map(|(idx, (start, end))| RegionHit {
            index: idx,
            start: format!("0x{start:08X}"),
            end: format!("0x{end:08X}"),
            len: end - start,
            hint: stride_hint(end - start),
        })
        .collect()
}

/// Compare two images map-by-map through an ECU definition, mirroring the CLI
/// `compare` command. Returns, per `MapDef`: changed-cell count and Δmax / Δavg
/// in physical units (or an `out_of_range` flag). When `map_name` is given, that
/// one map also carries the full before/after physical grids + axes so the UI can
/// draw a before→after view.
pub fn compare_maps(def: &EcuDef, a: &[u8], b: &[u8], map_name: Option<&str>) -> Value {
    let mut maps = Vec::with_capacity(def.maps.len());
    let mut total_changed = 0usize;
    let mut changed_maps = 0usize;

    for m in &def.maps {
        let (pa, pb) = match (decode_cells(a, m), decode_cells(b, m)) {
            (Some(pa), Some(pb)) => (pa, pb),
            _ => {
                maps.push(json!({
                    "name": m.name, "rows": m.rows, "cols": m.cols,
                    "unit": m.unit, "out_of_range": true,
                }));
                continue;
            }
        };
        let delta = MapDelta::from_pairs(
            pa.iter()
                .zip(pb.iter())
                .flat_map(|(ra, rb)| ra.iter().copied().zip(rb.iter().copied())),
        );
        let changed = delta.changed;
        total_changed += changed;
        if changed > 0 {
            changed_maps += 1;
        }
        let mut entry = json!({
            "name": m.name,
            "rows": m.rows,
            "cols": m.cols,
            "unit": m.unit,
            "data_type": format!("{:?}", m.data_type),
            "changed": changed,
            "d_max": delta.max_abs,
            "d_avg": delta.mean(),
            "out_of_range": false,
        });
        if map_name == Some(m.name.as_str()) {
            let (x_vals, x_unit) = decode_axis(a, &m.x_axis, m.cols);
            let (y_vals, y_unit) = decode_axis(a, &m.y_axis, m.rows);
            entry["stock"] = json!(pa);
            entry["tuned"] = json!(pb);
            entry["x_axis"] = json!({ "name": m.x_axis.as_ref().map(|ax| ax.name.clone()), "unit": x_unit, "values": x_vals });
            entry["y_axis"] = json!({ "name": m.y_axis.as_ref().map(|ax| ax.name.clone()), "unit": y_unit, "values": y_vals });
        }
        maps.push(entry);
    }

    json!({
        "def": def.id,
        "def_name": def.name,
        "stock_size": a.len(),
        "tuned_size": b.len(),
        "total_changed": total_changed,
        "changed_maps": changed_maps,
        "maps": maps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn findmaps_payload() {
        let a = vec![0u8; 64];
        let mut b = a.clone();
        b[4..36].fill(1); // 32 bytes: ~1x16 u16
        b[40] = 1; // a lone byte, dropped by min_len 2
        let hits = serde_json::to_value(findmaps(&a, &b, 2, 2)).unwrap();
        assert_eq!(
            hits,
            json!([{ "index": 0, "start": "0x00000004", "end": "0x00000024",
                     "len": 32, "hint": "~1x16 u16?" }])
        );
        // Gap 16 merges the lone byte: 4..41 is 37 bytes, odd, so no hint.
        let merged = findmaps(&a, &b, 16, 2);
        assert_eq!((merged.len(), merged[0].len), (1, 37));
        assert_eq!(merged[0].hint, "");
        b[41] = 1; // 38 bytes = 19 cells: no usual row width divides them
        assert_eq!(findmaps(&a, &b, 16, 2)[0].hint, "~19 u16?");
    }

    #[test]
    fn compare_payload() {
        let mut def = EcuDef::from_json(
            r#"{ "id": "syn", "name": "Synthetic", "maker": "M", "family": "F",
                "protocol": "iso_tp", "diag_layer": "uds", "bitrate": 500000 }"#,
        )
        .unwrap();
        def.maps = serde_json::from_str(
            r#"[{ "name": "grid", "address": 0, "rows": 2, "cols": 2,
                  "scaling": { "factor": 0.5, "offset": 0 }, "unit": "x" },
                { "name": "same", "address": 8 },
                { "name": "far", "address": 1000 }]"#,
        )
        .unwrap();
        let a = vec![10u8; 16];
        let mut b = a.clone();
        b[1] = 14; // +2.0
        b[3] = 8; // -1.0
        let v = compare_maps(&def, &a, &b, None);
        assert_eq!(
            (v["total_changed"].as_u64(), v["changed_maps"].as_u64()),
            (Some(2), Some(1))
        );
        let grid = &v["maps"][0];
        assert_eq!(grid["changed"], 2);
        assert_eq!(grid["d_max"], 2.0);
        assert_eq!(grid["d_avg"], 0.5);
        assert_eq!(grid["data_type"], "U8");
        assert!(grid.get("stock").is_none());
        assert_eq!(
            (
                v["maps"][1]["changed"].as_u64(),
                v["maps"][1]["d_avg"].as_f64()
            ),
            (Some(0), Some(0.0))
        );
        assert_eq!(v["maps"][2]["out_of_range"], true);
        // The requested map carries the full grids and (index) axes.
        let v = compare_maps(&def, &a, &b, Some("grid"));
        let grid = &v["maps"][0];
        assert_eq!(grid["stock"], json!([[5.0, 5.0], [5.0, 5.0]]));
        assert_eq!(grid["tuned"], json!([[5.0, 7.0], [5.0, 4.0]]));
        assert_eq!(grid["x_axis"]["values"], json!([0.0, 1.0]));
    }
}
