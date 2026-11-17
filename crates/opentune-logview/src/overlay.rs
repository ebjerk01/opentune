//! Map overlay: where in a calibration map did the logged operating points
//! fall?
//!
//! Each log sample is placed on the map's breakpoints by its X (and Y)
//! channel value and counted into the cell it uses:
//!
//! - [`BinMode::Nearest`] — one hit for the nearest breakpoint on each axis
//!   (an exact midpoint goes to the higher breakpoint).
//! - [`BinMode::Bilinear`] — the hit is split over the four surrounding cells
//!   with the same weights an ECU's bilinear interpolation gives them, so the
//!   counts are fractional and still sum to the sample count.
//!
//! A sample outside an axis is **clamped to the edge cell**, as ECU table
//! look-ups do, and also counted in [`MapHits::outside`] so the UI can say how
//! much of the log ran off the map. Samples whose X or Y is missing are
//! skipped and counted.
//!
//! Time-in-cell gives each sample the time until the next sample, capped at
//! [`DWELL_CAP_STEPS`] median steps so a pause in the log does not count as
//! dwell.

use crate::{LogError, Result};
use serde::{Deserialize, Serialize};

/// Cap of one sample's dwell, in median sample steps.
pub const DWELL_CAP_STEPS: f64 = 5.0;

/// How a sample is distributed over the cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BinMode {
    /// Whole hit to the nearest cell.
    #[default]
    Nearest,
    /// Hit split over the surrounding cells by bilinear weight.
    Bilinear,
}

impl BinMode {
    /// `"nearest"` / `"bilinear"`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "nearest" | "" => Some(Self::Nearest),
            "bilinear" | "interpolated" => Some(Self::Bilinear),
            _ => None,
        }
    }
}

/// What [`map_hits`] works on. All slices are aligned with `t`.
#[derive(Debug, Clone, Copy)]
pub struct OverlayInput<'a> {
    /// Sample times (sorted).
    pub t: &'a [f64],
    /// X channel (map columns).
    pub x: &'a [f64],
    /// Y channel (map rows); `None` for a 1-row curve.
    pub y: Option<&'a [f64]>,
    /// Channel whose value range is collected per cell.
    pub value: Option<&'a [f64]>,
    /// Column breakpoints.
    pub x_axis: &'a [f64],
    /// Row breakpoints (one value, or empty, for a curve).
    pub y_axis: &'a [f64],
    /// Distribution mode.
    pub mode: BinMode,
    /// Sample index range `[lo, hi)` to use; `None` = all.
    pub range: Option<(usize, usize)>,
}

/// Samples that fell outside an axis (and were clamped to the edge).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Outside {
    /// X below the smallest breakpoint.
    pub x_low: usize,
    /// X above the largest breakpoint.
    pub x_high: usize,
    /// Y below the smallest breakpoint.
    pub y_low: usize,
    /// Y above the largest breakpoint.
    pub y_high: usize,
    /// Samples outside on at least one axis.
    pub any: usize,
}

/// Per-cell value range of the chosen channel (nearest-cell assignment).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CellValues {
    /// Smallest value seen in the cell.
    pub min: Vec<Vec<Option<f64>>>,
    /// Largest value seen in the cell.
    pub max: Vec<Vec<Option<f64>>>,
    /// Mean value in the cell.
    pub mean: Vec<Vec<Option<f64>>>,
}

/// The overlay grid, `rows x cols` like the map.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MapHits {
    /// Map rows (Y breakpoints).
    pub rows: usize,
    /// Map columns (X breakpoints).
    pub cols: usize,
    /// Distribution mode used.
    pub mode: BinMode,
    /// Hits per cell (fractional in bilinear mode).
    pub hits: Vec<Vec<f64>>,
    /// Time spent per cell, in the log's time unit (seconds unless the log has
    /// no time column).
    pub time: Vec<Vec<f64>>,
    /// Value range of the chosen channel per cell.
    pub values: Option<CellValues>,
    /// Samples in the range.
    pub samples_total: usize,
    /// Samples placed on the map.
    pub samples_used: usize,
    /// Samples skipped because X or Y was missing.
    pub samples_skipped: usize,
    /// Samples clamped to an edge.
    pub outside: Outside,
    /// Largest per-cell hit count.
    pub max_hits: f64,
    /// Largest per-cell time.
    pub max_time: f64,
    /// Dwell cap applied to one sample.
    pub dwell_cap: f64,
}

/// Where one value sits on an axis.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Loc {
    lo: usize,
    hi: usize,
    /// Weight of `hi` (0 = all on `lo`).
    frac: f64,
    below: bool,
    above: bool,
}

impl Loc {
    fn nearest(&self) -> usize {
        if self.frac >= 0.5 {
            self.hi
        } else {
            self.lo
        }
    }
}

/// Check an axis is usable: finite and strictly monotonic (either way).
fn check_axis(axis: &[f64], which: &str) -> Result<()> {
    if axis.iter().any(|v| !v.is_finite()) {
        return Err(LogError::Input(format!(
            "the map's {which} axis has non-finite breakpoints"
        )));
    }
    if axis.len() < 2 {
        return Ok(());
    }
    let inc = axis.windows(2).all(|w| w[1] > w[0]);
    let dec = axis.windows(2).all(|w| w[1] < w[0]);
    if !inc && !dec {
        return Err(LogError::Input(format!(
            "the map's {which} axis is not strictly monotonic ({axis:?}); check the axis address and scaling before overlaying a log"
        )));
    }
    Ok(())
}

/// Locate `v` on a strictly monotonic axis, clamping outside values.
fn locate(axis: &[f64], v: f64) -> Loc {
    let n = axis.len();
    if n < 2 {
        return Loc {
            lo: 0,
            hi: 0,
            frac: 0.0,
            below: false,
            above: false,
        };
    }
    // Work on an increasing view: index k of the view is axis[n-1-k] when the
    // axis decreases.
    let dec = axis[1] < axis[0];
    let at = |k: usize| if dec { axis[n - 1 - k] } else { axis[k] };
    let map = |k: usize| if dec { n - 1 - k } else { k };
    let (min, max) = (at(0), at(n - 1));
    if v <= min {
        let i = map(0);
        return Loc {
            lo: i,
            hi: i,
            frac: 0.0,
            below: v < min,
            above: false,
        };
    }
    if v >= max {
        let i = map(n - 1);
        return Loc {
            lo: i,
            hi: i,
            frac: 0.0,
            below: false,
            above: v > max,
        };
    }
    // First view index with at(k) > v, so the segment is [k-1, k].
    let (mut l, mut r) = (0usize, n - 1);
    while r - l > 1 {
        let m = (l + r) / 2;
        if at(m) <= v {
            l = m;
        } else {
            r = m;
        }
    }
    let frac = (v - at(l)) / (at(r) - at(l));
    Loc {
        lo: map(l),
        hi: map(r),
        frac,
        below: false,
        above: false,
    }
}

/// Bin the log over the map axes.
pub fn map_hits(inp: &OverlayInput) -> Result<MapHits> {
    let n = inp.t.len();
    if inp.x.len() != n
        || inp.y.is_some_and(|y| y.len() != n)
        || inp.value.is_some_and(|v| v.len() != n)
    {
        return Err(LogError::Input(
            "channel lengths do not match the time base".into(),
        ));
    }
    if inp.x_axis.is_empty() {
        return Err(LogError::Input("the map has no X breakpoints".into()));
    }
    check_axis(inp.x_axis, "X")?;
    check_axis(inp.y_axis, "Y")?;
    let cols = inp.x_axis.len();
    let rows = inp.y_axis.len().max(1);
    if rows > 1 && inp.y.is_none() {
        return Err(LogError::Input("a 2-D map needs a Y channel".into()));
    }
    let (lo, hi) = inp.range.unwrap_or((0, n));
    let (lo, hi) = (lo.min(n), hi.min(n).max(lo.min(n)));

    // Median step for the dwell cap.
    let mut steps: Vec<f64> = inp.t[lo..hi]
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|d| *d > 0.0)
        .collect();
    let median = if steps.is_empty() {
        0.0
    } else {
        let m = steps.len() / 2;
        steps.select_nth_unstable_by(m, f64::total_cmp);
        steps[m]
    };
    let cap = median * DWELL_CAP_STEPS;

    let grid = || vec![vec![0.0f64; cols]; rows];
    let mut hits = grid();
    let mut time = grid();
    let mut vmin: Vec<Vec<Option<f64>>> = vec![vec![None; cols]; rows];
    let mut vmax = vmin.clone();
    let mut vsum = grid();
    let mut vcnt = grid();
    let mut outside = Outside::default();
    let mut used = 0usize;
    let mut skipped = 0usize;

    for i in lo..hi {
        let xv = inp.x[i];
        let yv = match inp.y {
            Some(y) if rows > 1 => y[i],
            _ => 0.0,
        };
        if !xv.is_finite() || !yv.is_finite() {
            skipped += 1;
            continue;
        }
        let lx = locate(inp.x_axis, xv);
        let ly = if rows > 1 {
            locate(inp.y_axis, yv)
        } else {
            Loc {
                lo: 0,
                hi: 0,
                frac: 0.0,
                below: false,
                above: false,
            }
        };
        outside.x_low += lx.below as usize;
        outside.x_high += lx.above as usize;
        outside.y_low += ly.below as usize;
        outside.y_high += ly.above as usize;
        outside.any += (lx.below || lx.above || ly.below || ly.above) as usize;
        used += 1;

        let dt = if i + 1 < inp.t.len() {
            (inp.t[i + 1] - inp.t[i]).max(0.0).min(cap)
        } else {
            median
        };
        match inp.mode {
            BinMode::Nearest => {
                let (r, c) = (ly.nearest(), lx.nearest());
                hits[r][c] += 1.0;
                time[r][c] += dt;
            }
            BinMode::Bilinear => {
                for (r, wr) in [(ly.lo, 1.0 - ly.frac), (ly.hi, ly.frac)] {
                    for (c, wc) in [(lx.lo, 1.0 - lx.frac), (lx.hi, lx.frac)] {
                        let w = wr * wc;
                        if w > 0.0 {
                            hits[r][c] += w;
                            time[r][c] += w * dt;
                        }
                    }
                }
            }
        }
        if let Some(vals) = inp.value {
            let v = vals[i];
            if v.is_finite() {
                let (r, c) = (ly.nearest(), lx.nearest());
                vmin[r][c] = Some(vmin[r][c].map_or(v, |m: f64| m.min(v)));
                vmax[r][c] = Some(vmax[r][c].map_or(v, |m: f64| m.max(v)));
                vsum[r][c] += v;
                vcnt[r][c] += 1.0;
            }
        }
    }

    let max_of = |g: &Vec<Vec<f64>>| g.iter().flatten().cloned().fold(0.0, f64::max);
    let values = inp.value.map(|_| CellValues {
        mean: vsum
            .iter()
            .zip(&vcnt)
            .map(|(s, c)| {
                s.iter()
                    .zip(c)
                    .map(|(s, c)| (*c > 0.0).then(|| s / c))
                    .collect()
            })
            .collect(),
        min: vmin,
        max: vmax,
    });
    Ok(MapHits {
        rows,
        cols,
        mode: inp.mode,
        max_hits: max_of(&hits),
        max_time: max_of(&time),
        hits,
        time,
        values,
        samples_total: hi - lo,
        samples_used: used,
        samples_skipped: skipped,
        outside,
        dwell_cap: cap,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(x: &[f64], y: &[f64], xa: &[f64], ya: &[f64], mode: BinMode) -> MapHits {
        let t: Vec<f64> = (0..x.len()).map(|i| i as f64 * 0.1).collect();
        map_hits(&OverlayInput {
            t: &t,
            x,
            y: Some(y),
            value: Some(x),
            x_axis: xa,
            y_axis: ya,
            mode,
            range: None,
        })
        .unwrap()
    }

    const XA: [f64; 3] = [10.0, 20.0, 40.0];
    const YA: [f64; 2] = [1000.0, 2000.0];

    #[test]
    fn nearest_exact_edges_and_midpoints() {
        // Exactly on the first and last breakpoints, a midpoint (goes high),
        // and just under a midpoint (goes low).
        let h = run(
            &[10.0, 40.0, 15.0, 29.9],
            &[1000.0, 2000.0, 1000.0, 2000.0],
            &XA,
            &YA,
            BinMode::Nearest,
        );
        assert_eq!(h.hits[0][0], 1.0);
        assert_eq!(h.hits[1][2], 1.0);
        assert_eq!(h.hits[0][1], 1.0);
        assert_eq!(h.hits[1][1], 1.0);
        assert_eq!(h.outside, Outside::default());
        assert_eq!(h.samples_used, 4);
    }

    #[test]
    fn outside_values_clamp_to_edges_and_are_counted() {
        let h = run(
            &[5.0, 99.0, 20.0],
            &[500.0, 9000.0, f64::NAN],
            &XA,
            &YA,
            BinMode::Nearest,
        );
        assert_eq!(h.hits[0][0], 1.0); // below both axes
        assert_eq!(h.hits[1][2], 1.0); // above both axes
        assert_eq!(h.outside.x_low, 1);
        assert_eq!(h.outside.x_high, 1);
        assert_eq!(h.outside.y_low, 1);
        assert_eq!(h.outside.y_high, 1);
        assert_eq!(h.outside.any, 2);
        assert_eq!(h.samples_skipped, 1);
        assert_eq!(h.samples_used, 2);
    }

    #[test]
    fn bilinear_weights_sum_to_one_and_split_correctly() {
        // x = 15 is halfway 10..20, y = 1250 is a quarter of 1000..2000.
        let h = run(&[15.0], &[1250.0], &XA, &YA, BinMode::Bilinear);
        let total: f64 = h.hits.iter().flatten().sum();
        assert!((total - 1.0).abs() < 1e-12);
        assert!((h.hits[0][0] - 0.375).abs() < 1e-12);
        assert!((h.hits[0][1] - 0.375).abs() < 1e-12);
        assert!((h.hits[1][0] - 0.125).abs() < 1e-12);
        assert!((h.hits[1][1] - 0.125).abs() < 1e-12);
        // Outside a bilinear sample goes wholly to the edge cell.
        let h = run(&[100.0], &[100.0], &XA, &YA, BinMode::Bilinear);
        assert_eq!(h.hits[0][2], 1.0);
    }

    #[test]
    fn decreasing_axis_works() {
        let h = run(
            &[39.0, 11.0],
            &[1000.0, 1000.0],
            &[40.0, 20.0, 10.0],
            &YA,
            BinMode::Nearest,
        );
        assert_eq!(h.hits[0][0], 1.0);
        assert_eq!(h.hits[0][2], 1.0);
    }

    #[test]
    fn non_monotonic_axis_is_refused() {
        let t = [0.0];
        let r = map_hits(&OverlayInput {
            t: &t,
            x: &[1.0],
            y: None,
            value: None,
            x_axis: &[1.0, 3.0, 2.0],
            y_axis: &[],
            mode: BinMode::Nearest,
            range: None,
        });
        assert!(r.is_err());
    }

    #[test]
    fn curve_needs_no_y_and_dwell_is_capped() {
        // 1 Hz samples with one 100 s gap: the gap counts as 5 s.
        let t = [0.0, 1.0, 2.0, 102.0, 103.0];
        let x = [10.0, 10.0, 10.0, 40.0, 40.0];
        let h = map_hits(&OverlayInput {
            t: &t,
            x: &x,
            y: None,
            value: None,
            x_axis: &XA,
            y_axis: &[],
            mode: BinMode::Nearest,
            range: None,
        })
        .unwrap();
        assert_eq!(h.rows, 1);
        assert_eq!(h.hits[0][0], 3.0);
        assert_eq!(h.time[0][0], 1.0 + 1.0 + 5.0);
        assert_eq!(h.time[0][2], 1.0 + 1.0); // last sample gets the median step
    }

    #[test]
    fn per_cell_value_range() {
        let h = run(
            &[10.0, 11.0, 12.0, 40.0],
            &[1000.0; 4],
            &XA,
            &YA,
            BinMode::Nearest,
        );
        let v = h.values.unwrap();
        assert_eq!(v.min[0][0], Some(10.0));
        assert_eq!(v.max[0][0], Some(12.0));
        assert_eq!(v.mean[0][0], Some(11.0));
        assert_eq!(v.min[0][1], None);
    }
}
