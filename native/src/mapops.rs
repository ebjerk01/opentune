//! Map editor operations on a rectangular selection, as pure functions.
//!
//! An operation takes the map's current physical values (row-major) and a
//! selection, and returns the physical value it wants in every selected cell.
//! It never touches the image: the caller hands the result to
//! `Image::edit_cells`, which checks every cell against the data type and
//! the map's `min` / `max` (refused, never clamped) and applies the whole
//! operation as ONE undo step, or nothing at all when any cell is refused.
//!
//! The operations mirror the webview editor (`desktop/ui/src/app-maps.js`):
//! Set, Add, Subtract, Multiply by percent (`v × p / 100`), bilinear
//! interpolation between the selection's corners, 3×3 smoothing, plus
//! interpolation along rows or columns only, and copy / paste of a block.
//! Declared inside `mapview.rs` (`mapview::mapops`) because `lib.rs` is shared.

/// A rectangle of cells, inclusive, normalised (`c0 <= c1`, `r0 <= r1`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sel {
    pub c0: usize,
    pub r0: usize,
    pub c1: usize,
    pub r1: usize,
}

impl Sel {
    /// The rectangle spanned by two `(column, row)` corners, in any order.
    pub fn span(a: (usize, usize), b: (usize, usize)) -> Self {
        Self {
            c0: a.0.min(b.0),
            r0: a.1.min(b.1),
            c1: a.0.max(b.0),
            r1: a.1.max(b.1),
        }
    }

    /// One cell.
    pub fn cell(c: usize, r: usize) -> Self {
        Self::span((c, r), (c, r))
    }

    /// The whole map.
    pub fn all(cols: usize, rows: usize) -> Self {
        Self::span((0, 0), (cols.max(1) - 1, rows.max(1) - 1))
    }

    pub fn width(&self) -> usize {
        self.c1 - self.c0 + 1
    }

    pub fn height(&self) -> usize {
        self.r1 - self.r0 + 1
    }

    pub fn count(&self) -> usize {
        self.width() * self.height()
    }

    pub fn contains(&self, c: usize, r: usize) -> bool {
        (self.c0..=self.c1).contains(&c) && (self.r0..=self.r1).contains(&r)
    }

    /// Does the rectangle lie inside a `cols × rows` map?
    pub fn fits(&self, cols: usize, rows: usize) -> bool {
        self.c1 < cols && self.r1 < rows
    }

    /// `(row-major index, column, row)` of every cell, row by row.
    pub fn cells(&self, cols: usize) -> impl Iterator<Item = (usize, usize, usize)> + '_ {
        (self.r0..=self.r1)
            .flat_map(move |r| (self.c0..=self.c1).map(move |c| (r * cols + c, c, r)))
    }
}

/// An operation on the selected cells.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Op {
    /// Every cell = the value.
    Set(f64),
    /// Every cell + the value.
    Add(f64),
    /// Every cell − the value.
    Sub(f64),
    /// Every cell × value / 100 (100 leaves it unchanged).
    MulPct(f64),
    /// Bilinear between the selection's four corner cells (a single row or
    /// column is a straight line between its ends).
    Bilinear,
    /// Each selected row: a straight line between its first and last cell.
    InterpRows,
    /// Each selected column: a straight line between its top and bottom cell.
    InterpCols,
    /// Each cell: the mean of its 3×3 neighbourhood (inside the map), taken
    /// from the values before the operation.
    Smooth,
}

impl Op {
    /// Short name for messages.
    pub fn label(&self) -> &'static str {
        match self {
            Op::Set(_) => "Set",
            Op::Add(_) => "Add",
            Op::Sub(_) => "Subtract",
            Op::MulPct(_) => "Multiply %",
            Op::Bilinear => "Interpolate",
            Op::InterpRows => "Interpolate rows",
            Op::InterpCols => "Interpolate columns",
            Op::Smooth => "Smooth",
        }
    }
}

/// The value every selected cell of a `cols × rows` map should get:
/// `(row-major index, physical value)`, row by row. Refused (nothing to
/// apply) when the selection is outside the map, the value list does not
/// match the map, a typed value is not finite, or an interpolation has
/// nothing to interpolate between.
pub fn apply(
    op: Op,
    vals: &[f64],
    cols: usize,
    rows: usize,
    sel: Sel,
) -> Result<Vec<(usize, f64)>, String> {
    if cols == 0 || rows == 0 || vals.len() != cols * rows {
        return Err("the map's values do not match its size".into());
    }
    if !sel.fits(cols, rows) {
        return Err("the selection is outside the map".into());
    }
    if let Op::Set(v) | Op::Add(v) | Op::Sub(v) | Op::MulPct(v) = op {
        if !v.is_finite() {
            return Err("type a number first".into());
        }
    }
    let at = |c: usize, r: usize| vals[r * cols + c];
    match op {
        Op::Bilinear if sel.count() < 2 => {
            return Err("interpolation needs a selection of more than one cell".into())
        }
        Op::InterpRows if sel.width() < 2 => {
            return Err("interpolating rows needs at least two columns selected".into())
        }
        Op::InterpCols if sel.height() < 2 => {
            return Err("interpolating columns needs at least two rows selected".into())
        }
        _ => {}
    }
    let lerp = |a: f64, b: f64, t: f64| a + (b - a) * t;
    let frac = |i: usize, i0: usize, n: usize| {
        if n > 1 {
            (i - i0) as f64 / (n - 1) as f64
        } else {
            0.0
        }
    };
    let out = sel
        .cells(cols)
        .map(|(i, c, r)| {
            let v = vals[i];
            let nv = match op {
                Op::Set(x) => x,
                Op::Add(x) => v + x,
                Op::Sub(x) => v - x,
                Op::MulPct(p) => v * p / 100.0,
                Op::Bilinear => {
                    let (u, w) = (frac(r, sel.r0, sel.height()), frac(c, sel.c0, sel.width()));
                    let top = lerp(at(sel.c0, sel.r0), at(sel.c1, sel.r0), w);
                    let bot = lerp(at(sel.c0, sel.r1), at(sel.c1, sel.r1), w);
                    lerp(top, bot, u)
                }
                Op::InterpRows => lerp(at(sel.c0, r), at(sel.c1, r), frac(c, sel.c0, sel.width())),
                Op::InterpCols => lerp(at(c, sel.r0), at(c, sel.r1), frac(r, sel.r0, sel.height())),
                Op::Smooth => {
                    let (mut sum, mut n) = (0.0, 0.0);
                    for rr in r.saturating_sub(1)..=(r + 1).min(rows - 1) {
                        for cc in c.saturating_sub(1)..=(c + 1).min(cols - 1) {
                            sum += at(cc, rr);
                            n += 1.0;
                        }
                    }
                    sum / n
                }
            };
            (i, nv)
        })
        .collect();
    Ok(out)
}

/// A copied block of values, row by row.
pub type Block = Vec<Vec<f64>>;

/// The selected values as a block.
pub fn copy(vals: &[f64], cols: usize, sel: Sel) -> Block {
    (sel.r0..=sel.r1)
        .map(|r| (sel.c0..=sel.c1).map(|c| vals[r * cols + c]).collect())
        .collect()
}

/// Tab-separated text (rows on lines), as spreadsheets copy and paste.
pub fn to_tsv(b: &Block) -> String {
    b.iter()
        .map(|row| {
            row.iter()
                .map(|v| crate::image::fmt_val(*v))
                .collect::<Vec<_>>()
                .join("\t")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse pasted text into a block: rows on lines; cells separated by tabs
/// or `;` (else by spaces); `,` is accepted as the decimal separator. Every
/// row must have the same length and every cell must be a number.
pub fn parse_block(s: &str) -> Result<Block, String> {
    let mut out: Block = Vec::new();
    for line in s.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let cells: Vec<&str> = if line.contains('\t') || line.contains(';') {
            line.split(['\t', ';']).map(str::trim).collect()
        } else {
            line.split_whitespace().collect()
        };
        let row = cells
            .iter()
            .map(|c| crate::mapview::parse_value(c).ok_or_else(|| format!("'{c}' is not a number")))
            .collect::<Result<Vec<f64>, String>>()?;
        out.push(row);
    }
    let w = out.first().map_or(0, Vec::len);
    if w == 0 {
        return Err("nothing to paste".into());
    }
    if out.iter().any(|r| r.len() != w) {
        return Err("the pasted rows have different lengths".into());
    }
    Ok(out)
}

/// Where a pasted block goes: its top-left at the selection's top-left. A
/// single value fills the whole selection. A block that would run past the
/// map's edge is refused, not cut.
pub fn paste(b: &Block, cols: usize, rows: usize, sel: Sel) -> Result<Vec<(usize, f64)>, String> {
    let h = b.len();
    let w = b.first().map_or(0, Vec::len);
    if h == 0 || w == 0 || b.iter().any(|r| r.len() != w) {
        return Err("nothing to paste".into());
    }
    if !sel.fits(cols, rows) {
        return Err("the selection is outside the map".into());
    }
    if h == 1 && w == 1 {
        return Ok(sel.cells(cols).map(|(i, _, _)| (i, b[0][0])).collect());
    }
    if sel.c0 + w > cols || sel.r0 + h > rows {
        return Err(format!(
            "a {w} × {h} block does not fit at column {} row {} of this {cols} × {rows} map",
            sel.c0, sel.r0
        ));
    }
    Ok(b.iter()
        .enumerate()
        .flat_map(|(dr, row)| {
            row.iter()
                .enumerate()
                .map(move |(dc, &v)| ((sel.r0 + dr) * cols + sel.c0 + dc, v))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 4 columns × 3 rows: v = 10 * row + col.
    fn grid() -> Vec<f64> {
        (0..3)
            .flat_map(|r| (0..4).map(move |c| (10 * r + c) as f64))
            .collect()
    }

    fn vals(out: &[(usize, f64)]) -> Vec<f64> {
        out.iter().map(|x| x.1).collect()
    }

    #[test]
    fn selection_geometry() {
        let s = Sel::span((3, 2), (1, 0));
        assert_eq!((s.c0, s.r0, s.c1, s.r1), (1, 0, 3, 2));
        assert_eq!((s.width(), s.height(), s.count()), (3, 3, 9));
        assert!(s.contains(2, 1) && !s.contains(0, 1));
        assert!(s.fits(4, 3) && !s.fits(3, 3));
        let idx: Vec<usize> = Sel::span((1, 1), (2, 2)).cells(4).map(|x| x.0).collect();
        assert_eq!(idx, vec![5, 6, 9, 10]);
        assert_eq!(Sel::all(4, 3), Sel::span((0, 0), (3, 2)));
        assert_eq!(Sel::all(0, 0), Sel::cell(0, 0));
    }

    #[test]
    fn arithmetic_ops() {
        let g = grid();
        let s = Sel::span((1, 1), (2, 1)); // 11, 12
        assert_eq!(
            apply(Op::Set(5.0), &g, 4, 3, s).unwrap(),
            vec![(5, 5.0), (6, 5.0)]
        );
        assert_eq!(
            vals(&apply(Op::Add(2.5), &g, 4, 3, s).unwrap()),
            [13.5, 14.5]
        );
        assert_eq!(
            vals(&apply(Op::Sub(1.0), &g, 4, 3, s).unwrap()),
            [10.0, 11.0]
        );
        assert_eq!(
            vals(&apply(Op::MulPct(110.0), &g, 4, 3, s).unwrap()),
            [12.1, 13.2]
        );
        assert_eq!(
            vals(&apply(Op::MulPct(100.0), &g, 4, 3, s).unwrap()),
            [11.0, 12.0]
        );
        assert!(apply(Op::Set(f64::NAN), &g, 4, 3, s).is_err());
        assert!(apply(Op::Add(f64::INFINITY), &g, 4, 3, s).is_err());
        assert!(
            apply(Op::Set(1.0), &g, 4, 3, Sel::cell(4, 0)).is_err(),
            "outside"
        );
        assert!(
            apply(Op::Set(1.0), &g[..5], 4, 3, s).is_err(),
            "size mismatch"
        );
    }

    #[test]
    fn interpolation_and_smoothing() {
        // Corners 0, 30 / 20, 50 over a 4 × 3 block.
        let mut g = vec![0.0; 12];
        g[0] = 0.0;
        g[3] = 30.0;
        g[8] = 20.0;
        g[11] = 50.0;
        let all = Sel::all(4, 3);
        let b = vals(&apply(Op::Bilinear, &g, 4, 3, all).unwrap());
        assert_eq!(
            b,
            [0.0, 10.0, 20.0, 30.0, 10.0, 20.0, 30.0, 40.0, 20.0, 30.0, 40.0, 50.0]
        );
        // A single row: a straight line between its ends.
        let row = Sel::span((0, 0), (3, 0));
        assert_eq!(
            vals(&apply(Op::Bilinear, &g, 4, 3, row).unwrap()),
            [0.0, 10.0, 20.0, 30.0]
        );
        // Rows only: each row between its own ends (the middle row is 0..0).
        let r = vals(&apply(Op::InterpRows, &g, 4, 3, all).unwrap());
        assert_eq!(&r[4..8], &[0.0, 0.0, 0.0, 0.0]);
        assert_eq!(&r[8..], &[20.0, 30.0, 40.0, 50.0]);
        // Columns only: each column between its top and bottom.
        let c = vals(&apply(Op::InterpCols, &g, 4, 3, all).unwrap());
        assert_eq!((c[4], c[7]), (10.0, 40.0));
        assert_eq!((c[5], c[6]), (0.0, 0.0));
        // Too small a selection is refused.
        assert!(apply(Op::Bilinear, &g, 4, 3, Sel::cell(1, 1)).is_err());
        assert!(apply(Op::InterpRows, &g, 4, 3, Sel::span((1, 0), (1, 2))).is_err());
        assert!(apply(Op::InterpCols, &g, 4, 3, Sel::span((0, 1), (3, 1))).is_err());
        // Smooth: the 3×3 mean from the values before the operation.
        let gg = grid();
        let s = apply(Op::Smooth, &gg, 4, 3, Sel::span((0, 0), (1, 1))).unwrap();
        // (0,0): mean of 0,1,10,11 = 5.5; (1,1): mean of the full 3×3 = 11.
        assert_eq!(s[0], (0, 5.5));
        assert_eq!(s[3], (5, 11.0));
    }

    #[test]
    fn copy_and_paste() {
        let g = grid();
        let b = copy(&g, 4, Sel::span((1, 0), (2, 1)));
        assert_eq!(b, vec![vec![1.0, 2.0], vec![11.0, 12.0]]);
        let t = to_tsv(&b);
        assert_eq!(t, "1\t2\n11\t12");
        assert_eq!(parse_block(&t).unwrap(), b);
        // Spreadsheet text, `;` / spaces, comma decimals, a trailing newline.
        assert_eq!(
            parse_block("1,5;2\r\n3;4,25\n").unwrap(),
            vec![vec![1.5, 2.0], vec![3.0, 4.25]]
        );
        assert_eq!(parse_block("1 2 3").unwrap(), vec![vec![1.0, 2.0, 3.0]]);
        assert!(parse_block("1\tx").is_err());
        assert!(parse_block("1\t2\n3").is_err(), "ragged");
        assert!(parse_block("  \n").is_err());
        // The block lands at the selection's top-left.
        let p = paste(&b, 4, 3, Sel::cell(2, 1)).unwrap();
        assert_eq!(p, vec![(6, 1.0), (7, 2.0), (10, 11.0), (11, 12.0)]);
        // Running past the edge is refused, not cut.
        assert!(paste(&b, 4, 3, Sel::cell(3, 1)).is_err());
        assert!(paste(&b, 4, 3, Sel::cell(0, 2)).is_err());
        // One value fills the selection.
        let one = vec![vec![7.0]];
        assert_eq!(
            paste(&one, 4, 3, Sel::span((0, 0), (1, 1))).unwrap(),
            vec![(0, 7.0), (1, 7.0), (4, 7.0), (5, 7.0)]
        );
        assert!(paste(&Vec::new(), 4, 3, Sel::cell(0, 0)).is_err());
    }
}
