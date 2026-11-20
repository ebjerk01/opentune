//! Two-image helpers: cluster the byte differences between a stock and a
//! tuned image into candidate map regions (`findmaps` diff mode), guess a
//! region's 16-bit table shape, and summarise the cell deltas of one decoded
//! map (`compare`).
//!
//! These are the shared cores of the CLI's `findmaps <stock> <tuned>` /
//! `compare` commands and the desktop's Find maps / Compare panels; each front
//! end keeps its own presentation (text lines, JSON payload).
//!
//! ```
//! use opentune_mapfind::diff::{diff_regions, u16_shape, U16Shape};
//!
//! let stock = vec![0u8; 64];
//! let mut tuned = stock.clone();
//! tuned[8] = 1; // two changes 3 bytes apart merge into one region ...
//! tuned[12] = 1;
//! tuned[40] = 1; // ... a lone byte is shorter than min_len 2
//! assert_eq!(diff_regions(&stock, &tuned, 16, 2), vec![(8, 13)]);
//! assert_eq!(u16_shape(32), Some(U16Shape::Grid { rows: 1, cols: 16 }));
//! assert_eq!(u16_shape(6), Some(U16Shape::Cells(3)));
//! assert_eq!(u16_shape(5), None);
//! ```

/// Cluster the byte differences between two images into candidate map regions,
/// merging changes separated by up to `gap` unchanged bytes. Returns the
/// half-open `(start, end)` byte ranges at least `min_len` bytes long, in
/// address order. Only the common prefix (`min(a.len(), b.len())`) is
/// compared.
pub fn diff_regions(a: &[u8], b: &[u8], gap: usize, min_len: usize) -> Vec<(usize, usize)> {
    let n = a.len().min(b.len());
    let mut regions = Vec::new();
    let mut i = 0;
    while i < n {
        if a[i] != b[i] {
            let start = i;
            let mut end = i + 1;
            let mut since_change = 0usize;
            let mut j = i + 1;
            while j < n && since_change <= gap {
                if a[j] != b[j] {
                    end = j + 1;
                    since_change = 0;
                } else {
                    since_change += 1;
                }
                j += 1;
            }
            regions.push((start, end));
            i = end;
        } else {
            i += 1;
        }
    }
    regions
        .into_iter()
        .filter(|(s, e)| e - s >= min_len)
        .collect()
}

/// A guess at the shape of a region as a table of 16-bit cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum U16Shape {
    /// `rows x cols` cells, `cols` the first of 16, 12, 10, 8 that divides
    /// the cell count.
    Grid {
        /// Cell count / `cols`.
        rows: usize,
        /// The row width that divides the cell count.
        cols: usize,
    },
    /// This many cells, with none of the usual row widths dividing them.
    Cells(usize),
}

/// Guess whether a region of `len` bytes looks like a table of 16-bit cells:
/// `None` when it is shorter than 4 bytes or of odd length.
pub fn u16_shape(len: usize) -> Option<U16Shape> {
    // `%` rather than `is_multiple_of` (Rust 1.87): this crate keeps the
    // workspace MSRV. Same result, the divisors are all non-zero.
    if len >= 4 && len % 2 == 0 {
        let cells = len / 2;
        for &w in &[16usize, 12, 10, 8] {
            if cells % w == 0 {
                return Some(U16Shape::Grid {
                    rows: cells / w,
                    cols: w,
                });
            }
        }
        return Some(U16Shape::Cells(cells));
    }
    None
}

/// A cell counts as changed when its physical value moved by more than this.
pub const CHANGE_EPSILON: f64 = 1e-9;

/// The cell-delta summary of one map between two images.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MapDelta {
    /// Cells whose physical value changed (by more than [`CHANGE_EPSILON`]).
    pub changed: usize,
    /// The largest absolute change (`0.0` when nothing changed).
    pub max_abs: f64,
    /// The sum of the signed changes of the changed cells.
    pub sum_delta: f64,
}

impl MapDelta {
    /// Summarise `(stock, tuned)` physical cell values, in cell order.
    pub fn from_pairs(cells: impl IntoIterator<Item = (f64, f64)>) -> Self {
        let mut changed = 0usize;
        let mut max_abs = 0f64;
        let mut sum_delta = 0f64;
        for (x, y) in cells {
            let d = y - x;
            if d.abs() > CHANGE_EPSILON {
                changed += 1;
                sum_delta += d;
                if d.abs() > max_abs {
                    max_abs = d.abs();
                }
            }
        }
        MapDelta {
            changed,
            max_abs,
            sum_delta,
        }
    }

    /// The mean signed change over the changed cells (`0.0` when none).
    pub fn mean(&self) -> f64 {
        if self.changed > 0 {
            self.sum_delta / self.changed as f64
        } else {
            0.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_merge_within_gap_and_drop_short_ones() {
        let a = vec![0u8; 100];
        let mut b = a.clone();
        // Changes at 10 and 14 (3 unchanged between) merge with gap 3.
        b[10] = 1;
        b[14] = 1;
        // A lone change is shorter than min_len 2.
        b[50] = 1;
        // A run of 4 at the very end.
        b[96..100].copy_from_slice(&[1, 1, 1, 1]);
        assert_eq!(diff_regions(&a, &b, 3, 2), vec![(10, 15), (96, 100)]);
        // Gap 2 splits the first pair; min_len 1 keeps everything.
        assert_eq!(
            diff_regions(&a, &b, 2, 1),
            vec![(10, 11), (14, 15), (50, 51), (96, 100)]
        );
        // Only the common prefix is compared.
        assert_eq!(diff_regions(&a[..12], &b, 16, 1), vec![(10, 11)]);
        assert!(diff_regions(&a, &a, 16, 1).is_empty());
    }

    #[test]
    fn u16_shapes() {
        assert_eq!(u16_shape(2), None);
        assert_eq!(u16_shape(7), None);
        assert_eq!(u16_shape(4), Some(U16Shape::Cells(2)));
        assert_eq!(u16_shape(512), Some(U16Shape::Grid { rows: 16, cols: 16 }));
        assert_eq!(u16_shape(48), Some(U16Shape::Grid { rows: 2, cols: 12 }));
        assert_eq!(u16_shape(20), Some(U16Shape::Grid { rows: 1, cols: 10 }));
        assert_eq!(u16_shape(16), Some(U16Shape::Grid { rows: 1, cols: 8 }));
    }

    #[test]
    fn map_delta_counts_signed_changes() {
        let d = MapDelta::from_pairs([(1.0, 1.0), (1.0, 3.0), (5.0, 4.0), (2.0, 2.0 + 1e-12)]);
        assert_eq!(d.changed, 2);
        assert_eq!(d.max_abs, 2.0);
        assert_eq!(d.sum_delta, 1.0);
        assert_eq!(d.mean(), 0.5);
        assert_eq!(MapDelta::from_pairs([(1.0, 1.0)]).mean(), 0.0);
    }
}
