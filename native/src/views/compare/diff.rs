//! The stock / tuned diff behind the Compare view. Pure (no UI, no `Host`):
//! it runs inside a job and is unit-tested with synthetic images.
//!
//! The region diff and the map-by-map compare are the shared cores the
//! webview app and the CLI use: `opentune_hub::compare::{findmaps,
//! compare_maps}` (built on `opentune_mapfind::diff`); a map's before /
//! after grid is `opentune_hub::maps::{decode_cells, decode_axis}`.

use opentune_core::ecudef::EcuDef;
use serde_json::Value;
use std::sync::Arc;

/// One changed region of the findmaps diff.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Region {
    pub start: usize,
    pub end: usize,
    pub len: usize,
    /// The 16-bit table-shape guess (`~16x16 u16?`), or empty.
    pub hint: String,
    /// Maps of the compared definition this region overlaps (indices).
    pub maps: Vec<usize>,
}

fn parse_hex(s: &str) -> usize {
    usize::from_str_radix(s.trim_start_matches("0x"), 16).unwrap_or(0)
}

/// The maps of `def` whose data overlaps `[start, end)`.
pub(crate) fn overlapping(def: &EcuDef, start: usize, end: usize) -> Vec<usize> {
    def.maps
        .iter()
        .enumerate()
        .filter(|(_, m)| {
            let a = m.address as usize;
            let b = a + m.byte_len() as usize;
            b > a && a < end && start < b
        })
        .map(|(i, _)| i)
        .collect()
}

/// The findmaps regions of `a` → `b` (changes up to `gap` unchanged bytes
/// apart merged, at least `min_len` long), each with the maps of `def` it
/// overlaps.
pub(crate) fn regions(
    a: &[u8],
    b: &[u8],
    gap: usize,
    min_len: usize,
    def: Option<&EcuDef>,
) -> Vec<Region> {
    opentune_hub::compare::findmaps(a, b, gap, min_len.max(1))
        .into_iter()
        .map(|h| {
            let (start, end) = (parse_hex(&h.start), parse_hex(&h.end));
            Region {
                start,
                end,
                len: h.len,
                hint: h.hint,
                maps: def.map(|d| overlapping(d, start, end)).unwrap_or_default(),
            }
        })
        .collect()
}

/// Bytes that differ over the common length, plus the length difference.
pub(crate) fn changed_bytes(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).filter(|(x, y)| x != y).count() + a.len().abs_diff(b.len())
}

/// One map of the map-by-map compare.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MapRow {
    /// Index into the definition's maps.
    pub idx: usize,
    pub name: String,
    pub rows: u32,
    pub cols: u32,
    pub unit: String,
    pub address: u32,
    pub changed: usize,
    /// Largest |Δ| in physical units.
    pub d_max: f64,
    /// Mean Δ over the changed cells, in physical units.
    pub d_avg: f64,
    /// The map runs past the end of one of the images.
    pub out_of_range: bool,
}

/// The map-by-map compare of two images against one definition.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MapsDiff {
    pub def_id: String,
    pub def_name: String,
    pub rows: Vec<MapRow>,
    pub total_changed: usize,
    pub changed_maps: usize,
}

/// `opentune_hub::compare::compare_maps`, typed. Its `maps` list follows the
/// definition's map order, so entry `i` is `def.maps[i]`.
pub(crate) fn compare(def: &EcuDef, a: &[u8], b: &[u8]) -> MapsDiff {
    let v = opentune_hub::compare::compare_maps(def, a, b, None);
    let u = |x: &Value| x.as_u64().unwrap_or(0) as usize;
    let rows = v["maps"]
        .as_array()
        .map(|maps| {
            maps.iter()
                .zip(&def.maps)
                .enumerate()
                .map(|(idx, (m, d))| MapRow {
                    idx,
                    name: d.name.clone(),
                    rows: d.rows,
                    cols: d.cols,
                    unit: d.unit.clone(),
                    address: d.address,
                    changed: u(&m["changed"]),
                    d_max: m["d_max"].as_f64().unwrap_or(0.0),
                    d_avg: m["d_avg"].as_f64().unwrap_or(0.0),
                    out_of_range: m["out_of_range"].as_bool().unwrap_or(false),
                })
                .collect()
        })
        .unwrap_or_default();
    MapsDiff {
        def_id: def.id.clone(),
        def_name: def.name.clone(),
        rows,
        total_changed: u(&v["total_changed"]),
        changed_maps: u(&v["changed_maps"]),
    }
}

/// A map's before / after grid with its axes.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Grid {
    pub stock: Vec<Vec<f64>>,
    pub tuned: Vec<Vec<f64>>,
    pub x: Vec<f64>,
    pub y: Vec<f64>,
    pub x_unit: String,
    pub y_unit: String,
}

impl Grid {
    /// `(lo, hi)` over both grids (a flat map gets a unit span).
    pub fn range(&self) -> (f64, f64) {
        let mut lo = f64::INFINITY;
        let mut hi = f64::NEG_INFINITY;
        for v in self.stock.iter().chain(&self.tuned).flatten() {
            lo = lo.min(*v);
            hi = hi.max(*v);
        }
        if !lo.is_finite() {
            return (0.0, 1.0);
        }
        if hi <= lo {
            hi = lo + 1.0;
        }
        (lo, hi)
    }

    /// Tuned − stock per cell.
    pub fn delta(&self) -> Vec<Vec<f64>> {
        self.stock
            .iter()
            .zip(&self.tuned)
            .map(|(a, b)| a.iter().zip(b).map(|(x, y)| y - x).collect())
            .collect()
    }
}

/// Decode map `idx` of `def` in both images (`None` when it does not fit
/// one of them).
pub(crate) fn grid(def: &EcuDef, idx: usize, a: &[u8], b: &[u8]) -> Option<Grid> {
    use opentune_hub::maps::{decode_axis, decode_cells};
    let m = def.maps.get(idx)?;
    let stock = decode_cells(a, m)?;
    let tuned = decode_cells(b, m)?;
    let (x, x_unit) = decode_axis(a, &m.x_axis, m.cols);
    let (y, y_unit) = decode_axis(a, &m.y_axis, m.rows);
    Some(Grid {
        stock,
        tuned,
        x,
        y,
        x_unit,
        y_unit,
    })
}

/// One side of the compare.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Side {
    /// The dump opened in the Flash view (its working image).
    Opened,
    File(std::path::PathBuf),
}

/// The finished compare.
#[derive(Clone, Debug)]
pub(crate) struct Outcome {
    pub stock_name: String,
    pub tuned_name: String,
    pub stock_side: Side,
    pub tuned_side: Side,
    pub a: Arc<Vec<u8>>,
    pub b: Arc<Vec<u8>>,
    pub diff_bytes: usize,
    pub gap: usize,
    pub min_len: usize,
    pub regions: Vec<Region>,
    /// The definition compared against (a copy: the grids decode with it).
    pub def: Option<EcuDef>,
    pub maps: Option<MapsDiff>,
    pub took_ms: f64,
}

/// Run the whole compare (blocking: call it from a job).
pub(crate) fn run(
    stock: (String, Side, Vec<u8>),
    tuned: (String, Side, Vec<u8>),
    def: Option<EcuDef>,
    gap: usize,
    min_len: usize,
) -> Outcome {
    let t = std::time::Instant::now();
    let (a, b) = (stock.2, tuned.2);
    let regions = regions(&a, &b, gap, min_len, def.as_ref());
    let maps = def
        .as_ref()
        .filter(|d| !d.maps.is_empty())
        .map(|d| compare(d, &a, &b));
    Outcome {
        stock_name: stock.0,
        tuned_name: tuned.0,
        stock_side: stock.1,
        tuned_side: tuned.1,
        diff_bytes: changed_bytes(&a, &b),
        a: Arc::new(a),
        b: Arc::new(b),
        gap,
        min_len,
        regions,
        def,
        maps,
        took_ms: t.elapsed().as_secs_f64() * 1000.0,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A synthetic definition: a 2×2 u8 map at 0, a 1-cell map at 8 and one
    /// past the end of the images.
    pub(crate) fn syn_def() -> EcuDef {
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
        def
    }

    #[test]
    fn regions_carry_their_bounds_and_overlapping_maps() {
        let a = vec![0u8; 64];
        let mut b = a.clone();
        b[2..4].fill(1); // inside "grid" (0..4)
        b[20..52].fill(1); // 32 bytes: ~1x16 u16
        let def = syn_def();
        let r = regions(&a, &b, 2, 2, Some(&def));
        assert_eq!(r.len(), 2);
        assert_eq!((r[0].start, r[0].end, r[0].len), (2, 4, 2));
        assert_eq!(r[0].maps, vec![0]);
        assert_eq!((r[1].start, r[1].end), (20, 52));
        assert_eq!(r[1].hint, "~1x16 u16?");
        assert!(r[1].maps.is_empty());
        // Without a definition, no maps; min_len 0 behaves as 1.
        assert!(regions(&a, &b, 2, 0, None)
            .iter()
            .all(|r| r.maps.is_empty()));
        assert!(regions(&a, &a, 16, 8, None).is_empty());
    }

    #[test]
    fn overlap_is_half_open() {
        let def = syn_def();
        assert_eq!(overlapping(&def, 4, 8), Vec::<usize>::new());
        assert_eq!(overlapping(&def, 3, 9), vec![0, 1]);
        assert_eq!(overlapping(&def, 8, 9), vec![1]);
    }

    #[test]
    fn maps_compare_is_typed_per_definition_map() {
        let def = syn_def();
        let a = vec![10u8; 16];
        let mut b = a.clone();
        b[1] = 14; // +2.0
        b[3] = 8; // -1.0
        let d = compare(&def, &a, &b);
        assert_eq!((d.total_changed, d.changed_maps), (2, 1));
        assert_eq!(d.rows.len(), 3);
        let g = &d.rows[0];
        assert_eq!((g.idx, g.name.as_str(), g.changed), (0, "grid", 2));
        assert_eq!((g.d_max, g.d_avg), (2.0, 0.5));
        assert_eq!((d.rows[1].changed, d.rows[1].out_of_range), (0, false));
        assert!(d.rows[2].out_of_range);
        assert_eq!(d.def_id, "syn");
    }

    #[test]
    fn grids_decode_both_sides_and_their_delta() {
        let def = syn_def();
        let a = vec![10u8; 16];
        let mut b = a.clone();
        b[1] = 14;
        let g = grid(&def, 0, &a, &b).unwrap();
        assert_eq!(g.stock, vec![vec![5.0, 5.0], vec![5.0, 5.0]]);
        assert_eq!(g.tuned, vec![vec![5.0, 7.0], vec![5.0, 5.0]]);
        assert_eq!(g.delta(), vec![vec![0.0, 2.0], vec![0.0, 0.0]]);
        assert_eq!(g.range(), (5.0, 7.0));
        assert_eq!(g.x, vec![0.0, 1.0]);
        assert!(grid(&def, 2, &a, &b).is_none(), "past the end");
        assert!(grid(&def, 9, &a, &b).is_none());
        let flat = Grid {
            stock: vec![vec![3.0]],
            tuned: vec![vec![3.0]],
            x: vec![],
            y: vec![],
            x_unit: String::new(),
            y_unit: String::new(),
        };
        assert_eq!(flat.range(), (3.0, 4.0));
    }

    #[test]
    fn run_combines_everything() {
        let a = vec![10u8; 64];
        let mut b = a.clone();
        b[1] = 14;
        b.push(0); // a size difference counts as changed bytes
        let o = run(
            ("s.bin".into(), Side::Opened, a),
            ("t.bin".into(), Side::File("t.bin".into()), b),
            Some(syn_def()),
            16,
            1,
        );
        assert_eq!(o.diff_bytes, 2);
        assert_eq!(o.regions.len(), 1);
        assert_eq!(o.regions[0].maps, vec![0]);
        assert_eq!(o.maps.as_ref().unwrap().changed_maps, 1);
        assert_eq!(changed_bytes(&[1, 2], &[1, 2]), 0);
        // A definition without maps compares regions only.
        let mut empty = syn_def();
        empty.maps.clear();
        let o = run(
            ("s".into(), Side::Opened, vec![0; 8]),
            ("t".into(), Side::Opened, vec![1; 8]),
            Some(empty),
            16,
            1,
        );
        assert!(o.maps.is_none());
    }
}
