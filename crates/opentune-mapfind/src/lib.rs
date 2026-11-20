//! # opentune-mapfind
//!
//! Find candidate calibration maps in **one** ECU image, with no definition and
//! no second (tuned) file to diff against.
//!
//! Calibration tables in Bosch-style firmware (EDC15/16/17, ME7, M-Motronic) are
//! stored next to their breakpoint axes, very often behind a small *count
//! header*:
//!
//! ```text
//! [n1][n2][axis1: n1 values][axis2: n2 values][data: n1*n2 cells]   2-D map
//! [n]     [axis:  n values ][data: n cells]                          1-D curve
//! ```
//!
//! The finder scans for those layouts in 16-bit (big- and little-endian) and
//! 8-bit flavours, plus a header-less fallback (two adjacent monotonic axes
//! followed by a matching data block). Every hit must pass structural checks
//! (counts in range, strictly increasing axes, the block fits the image, the
//! data is not erased flash or ASCII) and is then **scored** from three pieces of
//! evidence: the header (counts that agree with the axes), the axis quality
//! (length, spread, "round" calibration-style breakpoints) and the data surface
//! smoothness (real maps change gradually along both axes; code and random data
//! do not).
//!
//! Results are [`MapCandidate`]s: an address, a shape, the axes, a score in
//! `[0, 1]`, the evidence strings that produced it and **hints** (e.g. "axis
//! looks rpm-like"). A candidate is never given a meaning it has no evidence
//! for: names are `candidate_0x…`, hints are phrased as possibilities over the
//! *raw* values, and [`MapCandidate::to_mapdef`] marks the produced
//! [`MapDef`] as an unverified candidate in its `notes`.
//!
//! [`relocate`] is the companion for a definition that already exists: it
//! moves the maps of a definition built for one software onto a sibling
//! software's read by content signature (axes/header, cells, surroundings).
//!
//! Ideas credited (re-implemented, no code copied): the general approach of
//! scanning for monotonic axis pairs and promoting the following block to a
//! table, and of labelling tables only probabilistically, follows
//! `openremap-core` (MIT, V. Arapidis) — see `NOTICE` in this crate.
//!
//! ```
//! use opentune_mapfind::{find_maps, FindOptions};
//!
//! // A tiny image holding one counted 1-D curve: [4][axis x4][data x4], u16 BE.
//! let mut img = vec![0x5Au8; 64];
//! let words: [u16; 9] = [4, 1000, 2000, 3000, 4000, 100, 150, 190, 200];
//! for (i, w) in words.iter().enumerate() {
//!     img[16 + 2 * i..18 + 2 * i].copy_from_slice(&w.to_be_bytes());
//! }
//! let found = find_maps(&img, &FindOptions::default());
//! assert!(found.iter().any(|c| c.address == 16 + 2 + 8 && c.cols == 4));
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use opentune_core::ecudef::{AxisDef, DataType, Endian, MapDef, Scaling};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};

pub mod diff;
pub mod relocate;

/// How a candidate was located in the image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Layout {
    /// `[n1][n2][axis1][axis2][data]` — counts precede two axes and the grid.
    CountedMap,
    /// `[n][axis][data]` — a count precedes one axis and the curve.
    CountedCurve,
    /// `[axis1][axis2][data]` — no count header; two adjacent monotonic runs.
    AxisPair,
}

impl Layout {
    /// Short human label.
    pub fn as_str(self) -> &'static str {
        match self {
            Layout::CountedMap => "counted-map",
            Layout::CountedCurve => "counted-curve",
            Layout::AxisPair => "axis-pair",
        }
    }
}

/// What an axis' raw breakpoints *look like*. A hint, never a verified meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AxisKind {
    /// Raw range fits engine speed at 1 rpm/bit.
    RpmLike,
    /// Raw range fits a temperature (signed °C, or 0.1 K).
    TemperatureLike,
    /// Ends exactly at 100 / 1000 / 10000 — a percentage (pedal, load).
    PercentLike,
    /// Raw range fits absolute pressure in hPa/mbar.
    PressureLike,
    /// Starts near zero and rises to a load / injected-quantity / air-mass
    /// sized value (resolution unknown).
    LoadLike,
    /// No pattern with enough evidence.
    Unknown,
}

impl AxisKind {
    /// Short human label.
    pub fn as_str(self) -> &'static str {
        match self {
            AxisKind::RpmLike => "rpm-like",
            AxisKind::TemperatureLike => "temperature-like",
            AxisKind::PercentLike => "percent-like",
            AxisKind::PressureLike => "pressure-like",
            AxisKind::LoadLike => "load/IQ-like",
            AxisKind::Unknown => "unknown",
        }
    }
}

/// One breakpoint axis of a candidate, read from the image.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AxisCandidate {
    /// Byte offset of the first breakpoint.
    pub address: u32,
    /// Number of breakpoints.
    pub count: u32,
    /// Bytes per breakpoint (1 or 2).
    pub width: u8,
    /// Breakpoints are monotonic only when read as two's-complement.
    pub signed: bool,
    /// Byte order of the breakpoints.
    pub endian: Endian,
    /// The raw breakpoint values.
    pub values: Vec<i64>,
    /// Heuristic quality in `[0, 1]`.
    pub quality: f64,
    /// What the raw values look like (hint only).
    pub kind: AxisKind,
    /// Why [`kind`](Self::kind) was chosen, in words.
    pub hint: String,
}

impl AxisCandidate {
    fn data_type(&self) -> DataType {
        data_type(self.width, self.signed)
    }

    /// Byte length of the stored axis.
    pub fn byte_len(&self) -> u32 {
        self.count * self.width as u32
    }
}

/// A candidate calibration map found by [`find_maps`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MapCandidate {
    /// Byte offset of the first data cell.
    pub address: u32,
    /// Rows (Y dimension, slow-varying). `1` for a curve.
    pub rows: u32,
    /// Columns (X dimension, fast-varying).
    pub cols: u32,
    /// Bytes per cell (1 or 2).
    pub cell_width: u8,
    /// Cells read as two's complement give the smoother surface.
    pub signed: bool,
    /// Byte order of the cells and axes.
    pub endian: Endian,
    /// How the candidate was located.
    pub layout: Layout,
    /// Byte offset of the count header, for counted layouts.
    pub header_address: Option<u32>,
    /// Column axis (fast-varying dimension).
    pub x_axis: AxisCandidate,
    /// Row axis, for 2-D maps.
    pub y_axis: Option<AxisCandidate>,
    /// Smallest raw cell value.
    pub data_min: i64,
    /// Largest raw cell value.
    pub data_max: i64,
    /// Data-surface smoothness in `[0, 1]` (1 = perfectly smooth).
    pub smoothness: f64,
    /// Overall score in `[0, 1]`; candidates are ranked by it.
    pub score: f64,
    /// The structural facts that produced this candidate.
    pub evidence: Vec<String>,
    /// Possible interpretations, each phrased as unverified.
    pub hints: Vec<String>,
}

impl MapCandidate {
    /// Number of data cells.
    pub fn cells(&self) -> u32 {
        self.rows * self.cols
    }

    /// Byte length of the data block.
    pub fn data_len(&self) -> u32 {
        self.cells() * self.cell_width as u32
    }

    /// `[start, end)` of everything the candidate occupies: header, axes, data.
    pub fn footprint(&self) -> (u32, u32) {
        let mut start = self.address.min(self.x_axis.address);
        if let Some(y) = &self.y_axis {
            start = start.min(y.address);
        }
        if let Some(h) = self.header_address {
            start = start.min(h);
        }
        (start, self.address + self.data_len())
    }

    /// Stable candidate name: never a claimed meaning.
    pub fn name(&self) -> String {
        format!("candidate_0x{:06X}", self.address)
    }

    /// Cell data type of the candidate.
    pub fn data_type(&self) -> DataType {
        data_type(self.cell_width, self.signed)
    }

    /// Human description of the cell type, e.g. `u16 BE`.
    pub fn cell_desc(&self) -> String {
        let s = if self.signed { "s" } else { "u" };
        if self.cell_width == 1 {
            format!("{s}8")
        } else {
            format!("{s}16 {}", endian_str(self.endian))
        }
    }

    /// Lower the candidate to an OpenTune [`MapDef`] (raw scaling, stored axes),
    /// flagged as an unverified candidate in `notes`.
    pub fn to_mapdef(&self) -> MapDef {
        let axis = |a: &AxisCandidate, label: &str| AxisDef {
            name: if a.kind == AxisKind::Unknown {
                label.to_string()
            } else {
                format!("{label} ({}?)", a.kind.as_str())
            },
            unit: String::new(),
            count: a.count,
            values: Vec::new(),
            address: Some(a.address),
            data_type: a.data_type(),
            endian: a.endian,
            scaling: Scaling::default(),
            axis_ref: None,
        };
        MapDef {
            name: self.name(),
            notes: format!(
                "CANDIDATE — auto-detected by opentune-mapfind from a single image \
                 (score {:.2}, layout {}). Meaning, scaling and units are UNVERIFIED; \
                 confirm before any read or write. Evidence: {}.{}",
                self.score,
                self.layout.as_str(),
                self.evidence.join("; "),
                if self.hints.is_empty() {
                    String::new()
                } else {
                    format!(" Hints (unverified): {}.", self.hints.join("; "))
                }
            ),
            address: self.address,
            data_type: self.data_type(),
            endian: self.endian,
            rows: self.rows,
            cols: self.cols,
            scaling: Scaling::default(),
            unit: String::new(),
            x_axis: Some(axis(&self.x_axis, "x axis")),
            y_axis: self.y_axis.as_ref().map(|y| axis(y, "y axis")),
            min: None,
            max: None,
        }
    }
}

/// Tuning knobs for [`find_maps`].
#[derive(Debug, Clone)]
pub struct FindOptions {
    /// Shortest axis considered (breakpoints).
    pub min_axis_len: usize,
    /// Longest axis considered (breakpoints).
    pub max_axis_len: usize,
    /// Candidates scoring below this are dropped.
    pub min_score: f64,
    /// Keep at most this many candidates (0 = all).
    pub max_candidates: usize,
    /// Also scan 8-bit counted layouts (`[n1:u8][n2:u8][u8 axes][u8 data]`).
    pub byte_layouts: bool,
    /// Also scan the header-less axis-pair layout (16-bit only).
    pub headerless: bool,
}

impl Default for FindOptions {
    fn default() -> Self {
        Self {
            min_axis_len: 3,
            max_axis_len: 32,
            min_score: 0.5,
            max_candidates: 0,
            byte_layouts: true,
            headerless: true,
        }
    }
}

fn data_type(width: u8, signed: bool) -> DataType {
    match (width, signed) {
        (1, false) => DataType::U8,
        (1, true) => DataType::S8,
        (_, false) => DataType::U16,
        (_, true) => DataType::S16,
    }
}

fn endian_of(le: bool) -> Endian {
    if le {
        Endian::Little
    } else {
        Endian::Big
    }
}

fn endian_str(e: Endian) -> &'static str {
    match e {
        Endian::Big => "BE",
        Endian::Little => "LE",
    }
}

/// Read one raw value of `w` bytes (1 or 2).
fn read(img: &[u8], off: usize, w: usize, le: bool, signed: bool) -> Option<i64> {
    match w {
        1 => {
            let b = *img.get(off)?;
            Some(if signed { b as i8 as i64 } else { b as i64 })
        }
        _ => {
            let s = img.get(off..off.checked_add(2)?)?;
            let u = if le {
                u16::from_le_bytes([s[0], s[1]])
            } else {
                u16::from_be_bytes([s[0], s[1]])
            };
            Some(if signed { u as i16 as i64 } else { u as i64 })
        }
    }
}

fn read_n(img: &[u8], off: usize, n: usize, w: usize, le: bool, signed: bool) -> Option<Vec<i64>> {
    if off.checked_add(n.checked_mul(w)?)? > img.len() {
        return None;
    }
    (0..n)
        .map(|i| read(img, off + i * w, w, le, signed))
        .collect()
}

fn strictly_increasing(v: &[i64]) -> bool {
    v.windows(2).all(|p| p[1] > p[0])
}

fn all_ones(w: usize) -> i64 {
    if w == 1 {
        0xFF
    } else {
        0xFFFF
    }
}

/// Classify an axis from its raw values. Returns the kind and the reason.
///
/// Only raw-value evidence is used; every reason is phrased as "fits", never
/// as a claim of what the axis is.
pub fn classify_axis(values: &[i64], width: u8, signed: bool) -> (AxisKind, String) {
    let (Some(&first), Some(&last)) = (values.first(), values.last()) else {
        return (AxisKind::Unknown, String::new());
    };
    let n = values.len();
    let range = format!("raw {first}..{last}");
    if signed && (-50..=-1).contains(&first) && (30..=160).contains(&last) {
        return (
            AxisKind::TemperatureLike,
            format!("{range} (signed, crosses zero) fits a °C temperature axis"),
        );
    }
    if width == 1 {
        return (
            AxisKind::Unknown,
            format!("{range}: 8-bit axis, scaling unknown"),
        );
    }
    if !signed && (2230..=2930).contains(&first) && last <= 4330 && last - first >= 300 {
        return (
            AxisKind::TemperatureLike,
            format!(
                "{range} fits 0.1 K ({:.0}..{:.0} °C)",
                first as f64 / 10.0 - 273.15,
                last as f64 / 10.0 - 273.15
            ),
        );
    }
    if (0..=last / 10).contains(&first) && matches!(last, 100 | 1000 | 10000) {
        return (
            AxisKind::PercentLike,
            format!("{range} ends exactly at {last}: fits a percentage (pedal/load)"),
        );
    }
    let round10 = values.iter().filter(|v| *v % 10 == 0).count() as f64 / n as f64;
    if (0..=1500).contains(&first) && (3500..=8000).contains(&last) && n >= 4 && round10 >= 0.5 {
        return (
            AxisKind::RpmLike,
            format!("{range}, mostly multiples of 10: fits engine speed at 1 rpm/bit"),
        );
    }
    if (500..=1100).contains(&first) && (1200..=3500).contains(&last) {
        return (
            AxisKind::PressureLike,
            format!("{range} fits absolute pressure in hPa/mbar"),
        );
    }
    if first >= 0 && first <= last / 10 && (200..=10000).contains(&last) {
        return (
            AxisKind::LoadLike,
            format!(
                "{range} starts near zero: fits load / injected quantity / air mass \
                 (resolution unknown)"
            ),
        );
    }
    (AxisKind::Unknown, format!("{range}: no recognised pattern"))
}

/// Validate and score `n` breakpoints at `off`. `None` when not a plausible axis.
fn axis_at(img: &[u8], off: usize, n: usize, w: usize, le: bool) -> Option<AxisCandidate> {
    let u = read_n(img, off, n, w, le, false)?;
    let (vals, signed) = if strictly_increasing(&u) {
        (u, false)
    } else {
        let s = read_n(img, off, n, w, le, true)?;
        if !strictly_increasing(&s) {
            return None;
        }
        (s, true)
    };
    let first = vals[0];
    let last = vals[n - 1];
    if !signed && last == all_ones(w) {
        return None; // runs into erased flash
    }
    let span = (last - first) as f64;
    let mag = first.abs().max(last.abs()).max(1) as f64;
    if span / mag < 0.05 {
        return None; // tight cluster: addresses/pointers, not breakpoints
    }
    let steps: Vec<i64> = vals.windows(2).map(|p| p[1] - p[0]).collect();
    let index_like = steps.iter().all(|s| *s == 1);
    let mut q = if index_like {
        0.3
    } else {
        let len_bonus = ((n as f64 - 3.0) / 5.0).clamp(0.0, 1.0);
        let round = if w == 2 {
            vals.iter().filter(|v| *v % 10 == 0).count() as f64 / n as f64
        } else {
            0.4
        };
        0.5 + 0.25 * len_bonus + 0.25 * round
    };
    let smin = steps.iter().copied().min().unwrap_or(1);
    let smax = steps.iter().copied().max().unwrap_or(1);
    if smin > 0 && smax / smin > 200 {
        q *= 0.7; // wildly uneven spacing
    }
    let (kind, hint) = classify_axis(&vals, w as u8, signed);
    Some(AxisCandidate {
        address: off as u32,
        count: n as u32,
        width: w as u8,
        signed,
        endian: endian_of(le && w == 2),
        values: vals,
        quality: q.min(1.0),
        kind,
        hint,
    })
}

/// Mean absolute difference over all value pairs (O(n log n)).
fn mean_pair_diff(v: &[i64]) -> f64 {
    let n = v.len();
    if n < 2 {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_unstable();
    let total: f64 = s
        .iter()
        .enumerate()
        .map(|(i, x)| *x as f64 * (2.0 * i as f64 - n as f64 + 1.0))
        .sum();
    total / (n * (n - 1) / 2) as f64
}

/// Smoothness of a row-major `rows x cols` surface in `[0, 1]`.
///
/// Along each direction the mean step between neighbours is compared with the
/// mean difference between *any* two cells: for unordered data the two are the
/// same (ratio ~1), for a smooth calibration surface neighbours differ far less.
/// Normalising by the pair mean (not the min..max range) keeps a few outliers
/// from making a noisy block look smooth.
fn smoothness(v: &[i64], rows: usize, cols: usize) -> f64 {
    let pair = mean_pair_diff(v);
    if pair == 0.0 {
        return 0.5; // constant block: plausible (flat limiter) but uninformative
    }
    let score = |sum: i64, cnt: usize| {
        if cnt == 0 {
            return None;
        }
        let r = sum as f64 / cnt as f64 / pair;
        Some(1.0 - (r / 0.8).min(1.0))
    };
    let (mut fs, mut fc) = (0i64, 0usize);
    for r in 0..rows {
        for c in 1..cols {
            fs += (v[r * cols + c] - v[r * cols + c - 1]).abs();
            fc += 1;
        }
    }
    let (mut ss, mut sc) = (0i64, 0usize);
    for r in 1..rows {
        for c in 0..cols {
            ss += (v[r * cols + c] - v[(r - 1) * cols + c]).abs();
            sc += 1;
        }
    }
    match (score(fs, fc), score(ss, sc)) {
        (Some(a), Some(b)) => (a + b) / 2.0,
        (Some(a), None) | (None, Some(a)) => a,
        (None, None) => 0.0,
    }
}

/// The best reading of a data block.
struct Block {
    smooth: f64,
    signed: bool,
    /// `true` when the *first* stored axis varies fastest.
    first_fastest: bool,
    min: i64,
    max: i64,
}

/// Evaluate `n1 * n2` cells at `off` (stored with axis2 fastest or axis1
/// fastest — both are tried). `None` when the block is erased / text.
fn block_at(img: &[u8], off: usize, n1: usize, n2: usize, w: usize, le: bool) -> Option<Block> {
    let cells = n1 * n2;
    let bytes = img.get(off..off.checked_add(cells * w)?)?;
    let raw_u = read_n(img, off, cells, w, le, false)?;
    // Hard filters: erased flash, ASCII text.
    let erased = raw_u.iter().filter(|v| **v == all_ones(w)).count();
    if erased * 2 > cells {
        return None;
    }
    if bytes.len() >= 8 {
        let printable = bytes.iter().filter(|b| (0x20..0x7F).contains(*b)).count();
        if printable * 10 >= bytes.len() * 9 && bytes.iter().any(|b| b.is_ascii_alphabetic()) {
            return None;
        }
    }
    let top_bit = if w == 1 { 0x80 } else { 0x8000 };
    let needs_signed = raw_u.iter().any(|v| *v & top_bit != 0);
    let mut readings = vec![(raw_u, false)];
    if needs_signed {
        readings.push((read_n(img, off, cells, w, le, true)?, true));
    }
    let mut best: Option<Block> = None;
    for (vals, signed) in readings {
        let a = smoothness(&vals, n1, n2);
        // Stored with axis1 fastest, the stream reads as n2 rows of n1 cells.
        let b = if n1 > 1 && n2 > 1 {
            smoothness(&vals, n2, n1)
        } else {
            -1.0
        };
        let (smooth, first_fastest) = if b > a + 1e-9 { (b, true) } else { (a, false) };
        if best.as_ref().map_or(true, |x| smooth > x.smooth + 1e-9) {
            best = Some(Block {
                smooth,
                signed,
                first_fastest,
                min: vals.iter().copied().min()?,
                max: vals.iter().copied().max()?,
            });
        }
    }
    best
}

/// One scan format: count width, axis width, cell width, byte order.
#[derive(Clone, Copy)]
struct Fmt {
    cw: usize,
    aw: usize,
    dw: usize,
    le: bool,
}

fn fmt_desc(f: Fmt) -> String {
    if f.aw == 1 && f.dw == 1 {
        "u8".into()
    } else {
        format!("u{} {}", f.aw * 8, if f.le { "LE" } else { "BE" })
    }
}

fn axis_desc(a: &AxisCandidate) -> String {
    format!(
        "{} strictly increasing {}{} breakpoints @0x{:06X} ({}..{})",
        a.count,
        if a.signed { "s" } else { "u" },
        a.width as u32 * 8,
        a.address,
        a.values.first().copied().unwrap_or(0),
        a.values.last().copied().unwrap_or(0)
    )
}

fn map_hints(x: &AxisCandidate, y: Option<&AxisCandidate>) -> Vec<String> {
    let mut hints = Vec::new();
    for (label, a) in [("x axis", Some(x)), ("y axis", y)] {
        if let Some(a) = a {
            if a.kind != AxisKind::Unknown {
                hints.push(format!("{label} {}: {}", a.kind.as_str(), a.hint));
            }
        }
    }
    let kinds: Vec<AxisKind> = std::iter::once(x.kind).chain(y.map(|a| a.kind)).collect();
    let loadish = kinds.iter().any(|k| {
        matches!(
            k,
            AxisKind::LoadLike | AxisKind::PressureLike | AxisKind::PercentLike
        )
    });
    if kinds.contains(&AxisKind::RpmLike) && loadish {
        hints.push(
            "rpm x load-type axes: the shape of fuel/torque/boost/timing-type maps \
             (role unverified)"
                .into(),
        );
    }
    hints
}

/// Score weights for one layout: `base + wq * axis_quality + ws * smoothness`.
#[derive(Clone, Copy)]
struct Weights {
    base: f64,
    wq: f64,
    ws: f64,
}

/// Assemble a candidate from two axes (`a1` stored first) and its block.
#[allow(clippy::too_many_arguments)]
fn build_2d(
    layout: Layout,
    header: Option<u32>,
    a1: AxisCandidate,
    a2: AxisCandidate,
    data_off: usize,
    f: Fmt,
    blk: Block,
    w: Weights,
    mut evidence: Vec<String>,
) -> MapCandidate {
    let q = (a1.quality + a2.quality) / 2.0;
    // Larger grids carry more evidence: many cells are unlikely to be smooth
    // by chance. Worth at most +0.05 (128+ cells).
    let size = 0.05 * ((a1.count * a2.count) as f64 / 128.0).min(1.0);
    let score = (w.base + w.wq * q + w.ws * blk.smooth + size).min(1.0);
    let (n1, n2) = (a1.count, a2.count);
    evidence.push(format!("axis1: {}", axis_desc(&a1)));
    evidence.push(format!("axis2: {}", axis_desc(&a2)));
    evidence.push(format!(
        "data {n1}x{n2} {}{} cells @0x{:06X}, raw {}..{}, smoothness {:.2} ({} varies fastest)",
        if blk.signed { "s" } else { "u" },
        f.dw * 8,
        data_off,
        blk.min,
        blk.max,
        blk.smooth,
        if blk.first_fastest { "axis1" } else { "axis2" }
    ));
    // MapDef is row-major: the fast-varying axis is the column (x) axis.
    let (rows, cols, x, y) = if blk.first_fastest {
        (n2, n1, a1, a2)
    } else {
        (n1, n2, a2, a1)
    };
    let hints = map_hints(&x, Some(&y));
    MapCandidate {
        address: data_off as u32,
        rows,
        cols,
        cell_width: f.dw as u8,
        signed: blk.signed,
        endian: endian_of(f.le && (f.dw == 2 || f.aw == 2)),
        layout,
        header_address: header,
        x_axis: x,
        y_axis: Some(y),
        data_min: blk.min,
        data_max: blk.max,
        smoothness: blk.smooth,
        score,
        evidence,
        hints,
    }
}

fn try_counted_map(img: &[u8], off: usize, n1: usize, n2: usize, f: Fmt) -> Option<MapCandidate> {
    let a1_off = off + 2 * f.cw;
    let a2_off = a1_off + n1 * f.aw;
    let d_off = a2_off + n2 * f.aw;
    if d_off + n1 * n2 * f.dw > img.len() {
        return None;
    }
    let a1 = axis_at(img, a1_off, n1, f.aw, f.le)?;
    let a2 = axis_at(img, a2_off, n2, f.aw, f.le)?;
    let blk = block_at(img, d_off, n1, n2, f.dw, f.le)?;
    let ev = vec![format!(
        "count header @0x{off:06X} = [{n1}, {n2}] ({}) matches both axis lengths and the \
         {}-cell block",
        fmt_desc(f),
        n1 * n2
    )];
    let w = Weights {
        base: 0.30,
        wq: 0.25,
        ws: 0.45,
    };
    Some(build_2d(
        Layout::CountedMap,
        Some(off as u32),
        a1,
        a2,
        d_off,
        f,
        blk,
        w,
        ev,
    ))
}

fn try_counted_curve(img: &[u8], off: usize, n: usize, f: Fmt) -> Option<MapCandidate> {
    let a_off = off + f.cw;
    let d_off = a_off + n * f.aw;
    let axis = axis_at(img, a_off, n, f.aw, f.le)?;
    let blk = block_at(img, d_off, 1, n, f.dw, f.le)?;
    let score = (0.25 + 0.30 * axis.quality + 0.45 * blk.smooth).min(1.0);
    let evidence = vec![
        format!(
            "count header @0x{off:06X} = [{n}] ({}) matches the axis length",
            fmt_desc(f)
        ),
        format!("axis: {}", axis_desc(&axis)),
        format!(
            "data 1x{n} {}{} cells @0x{d_off:06X}, raw {}..{}, smoothness {:.2}",
            if blk.signed { "s" } else { "u" },
            f.dw * 8,
            blk.min,
            blk.max,
            blk.smooth
        ),
    ];
    let hints = map_hints(&axis, None);
    Some(MapCandidate {
        address: d_off as u32,
        rows: 1,
        cols: n as u32,
        cell_width: f.dw as u8,
        signed: blk.signed,
        endian: endian_of(f.le && (f.dw == 2 || f.aw == 2)),
        layout: Layout::CountedCurve,
        header_address: Some(off as u32),
        x_axis: axis,
        y_axis: None,
        data_min: blk.min,
        data_max: blk.max,
        smoothness: blk.smooth,
        score,
        evidence,
        hints,
    })
}

const FORMATS: [Fmt; 5] = [
    Fmt {
        cw: 2,
        aw: 2,
        dw: 2,
        le: false,
    },
    Fmt {
        cw: 2,
        aw: 2,
        dw: 2,
        le: true,
    },
    Fmt {
        cw: 2,
        aw: 2,
        dw: 1,
        le: false,
    },
    Fmt {
        cw: 2,
        aw: 2,
        dw: 1,
        le: true,
    },
    Fmt {
        cw: 1,
        aw: 1,
        dw: 1,
        le: false,
    },
];

fn scan_counted(img: &[u8], f: Fmt, o: &FindOptions, out: &mut Vec<MapCandidate>) {
    // 8-bit layouts are far noisier: demand longer axes.
    let min_axis = if f.cw == 1 {
        o.min_axis_len.max(4)
    } else {
        o.min_axis_len
    };
    let min_curve = if f.cw == 1 { 6 } else { min_axis.max(4) };
    let range = min_axis..=o.max_axis_len;
    let mut off = 0usize;
    while off + 2 * f.cw <= img.len() {
        if let Some(n1) = read(img, off, f.cw, f.le, false) {
            let n1 = n1 as usize;
            if range.contains(&n1) {
                if let Some(n2) = read(img, off + f.cw, f.cw, f.le, false) {
                    let n2 = n2 as usize;
                    if range.contains(&n2) {
                        if let Some(c) = try_counted_map(img, off, n1, n2, f) {
                            out.push(c);
                        }
                    }
                }
                if n1 >= min_curve {
                    if let Some(c) = try_counted_curve(img, off, n1, f) {
                        out.push(c);
                    }
                }
            }
        }
        off += f.cw;
    }
}

/// Header-less fallback: maximal strictly increasing 16-bit runs; two adjacent
/// runs followed by a matching block form a candidate.
fn scan_axis_pairs(img: &[u8], le: bool, o: &FindOptions, out: &mut Vec<MapCandidate>) {
    let min_len = o.min_axis_len.max(4);
    let words = img.len() / 2;
    let word = |i: usize| read(img, i * 2, 2, le, false).unwrap_or(0);
    let mut runs: HashMap<usize, usize> = HashMap::new(); // start offset -> len
    let mut order: Vec<usize> = Vec::new();
    let mut i = 0usize;
    while i < words {
        let mut j = i + 1;
        while j < words && word(j) > word(j - 1) {
            j += 1;
        }
        let len = j - i;
        if (min_len..=o.max_axis_len).contains(&len) {
            runs.insert(i * 2, len);
            order.push(i * 2);
        }
        i = j;
    }
    let f = Fmt {
        cw: 2,
        aw: 2,
        dw: 2,
        le,
    };
    let w = Weights {
        base: 0.10,
        wq: 0.30,
        ws: 0.45,
    };
    for start in order {
        let len1 = runs[&start];
        let s2 = start + len1 * 2;
        let Some(&len2) = runs.get(&s2) else { continue };
        // Maximal runs can swallow neighbours: the first data row often keeps
        // rising past the second axis, and bytes before the first axis can
        // extend it. Try trimming the front of axis1 (a little) and the tail of
        // axis2 (fully) and keep the best-scoring reading.
        let mut best: Option<MapCandidate> = None;
        for trim in 0..=len1.saturating_sub(min_len).min(4) {
            let s1 = start + trim * 2;
            let n1 = len1 - trim;
            // A count header in front means the counted scan owns this table.
            for n2 in min_len..=len2 {
                if s1 >= 4
                    && read(img, s1 - 4, 2, le, false) == Some(n1 as i64)
                    && read(img, s1 - 2, 2, le, false) == Some(n2 as i64)
                {
                    continue;
                }
                let d_off = s2 + n2 * 2;
                let (Some(a1), Some(a2)) =
                    (axis_at(img, s1, n1, 2, le), axis_at(img, s2, n2, 2, le))
                else {
                    continue;
                };
                let Some(blk) = block_at(img, d_off, n1, n2, 2, le) else {
                    continue;
                };
                let ev = vec![format!(
                    "no count header: two adjacent monotonic runs ({n1} + {n2}) followed by a \
                     block of their product size"
                )];
                let c = build_2d(Layout::AxisPair, None, a1, a2, d_off, f, blk, w, ev);
                if best.as_ref().map_or(true, |b| c.score > b.score) {
                    best = Some(c);
                }
            }
        }
        out.extend(best);
    }
}

/// Scan `image` for candidate calibration maps, best first.
///
/// Overlapping candidates are resolved in favour of the higher score, so the
/// result is a set of disjoint footprints.
pub fn find_maps(image: &[u8], opts: &FindOptions) -> Vec<MapCandidate> {
    let mut all = Vec::new();
    for f in FORMATS {
        if f.cw == 1 && !opts.byte_layouts {
            continue;
        }
        scan_counted(image, f, opts, &mut all);
    }
    if opts.headerless {
        scan_axis_pairs(image, false, opts, &mut all);
        scan_axis_pairs(image, true, opts, &mut all);
    }
    all.retain(|c| c.score >= opts.min_score);
    let pref = |l: Layout| match l {
        Layout::CountedMap => 0,
        Layout::CountedCurve => 1,
        Layout::AxisPair => 2,
    };
    all.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(pref(a.layout).cmp(&pref(b.layout)))
            .then(b.cells().cmp(&a.cells()))
            .then(a.address.cmp(&b.address))
    });
    // Greedy disjoint-footprint selection.
    let mut taken: BTreeMap<u32, u32> = BTreeMap::new();
    let mut out = Vec::new();
    for c in all {
        let (s, e) = c.footprint();
        let before = taken.range(..=s).next_back().is_some_and(|(_, &pe)| pe > s);
        let after = taken.range(s..).next().is_some_and(|(&ns, _)| ns < e);
        if before || after {
            continue;
        }
        taken.insert(s, e);
        out.push(c);
        if opts.max_candidates > 0 && out.len() >= opts.max_candidates {
            break;
        }
    }
    out
}

/// How well a known [`MapDef`] is recovered by a candidate list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchKind {
    /// Same data address, same cell count and cell width.
    Exact,
    /// The candidate's axes sit at the definition's stored axis addresses, but
    /// the data address or shape differs.
    Axes,
    /// The candidate's data overlaps the definition's data.
    Overlap,
    /// Not found.
    Missed,
}

impl MatchKind {
    fn strength(self) -> u8 {
        match self {
            MatchKind::Exact => 3,
            MatchKind::Axes => 2,
            MatchKind::Overlap => 1,
            MatchKind::Missed => 0,
        }
    }
}

/// Where a known map definition landed in the candidate ranking.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DefMatch {
    /// The definition's map name.
    pub name: String,
    /// The definition's data address.
    pub address: u32,
    /// 0-based rank of the best matching candidate.
    pub rank: Option<usize>,
    /// Match strength.
    pub kind: MatchKind,
}

/// Match known definitions against a ranked candidate list (recall check).
pub fn match_defs(cands: &[MapCandidate], defs: &[MapDef]) -> Vec<DefMatch> {
    defs.iter()
        .map(|d| {
            let d_end = d.address.saturating_add(d.byte_len());
            let axes: Vec<u32> = [&d.x_axis, &d.y_axis]
                .iter()
                .filter_map(|a| a.as_ref().and_then(|a| a.address))
                .collect();
            let mut best = (MatchKind::Missed, None);
            for (i, c) in cands.iter().enumerate() {
                let cand_axes: Vec<u32> = std::iter::once(c.x_axis.address)
                    .chain(c.y_axis.as_ref().map(|y| y.address))
                    .collect();
                let kind = if c.address == d.address
                    && c.cells() == d.cell_count()
                    && c.cell_width as u32 == d.data_type.byte_len()
                {
                    MatchKind::Exact
                } else if !axes.is_empty() && axes.iter().all(|a| cand_axes.contains(a)) {
                    MatchKind::Axes
                } else if c.address < d_end && d.address < c.address + c.data_len() {
                    MatchKind::Overlap
                } else {
                    continue;
                };
                if kind.strength() > best.0.strength() {
                    best = (kind, Some(i));
                }
                if kind == MatchKind::Exact {
                    break;
                }
            }
            DefMatch {
                name: d.name.clone(),
                address: d.address,
                rank: best.1,
                kind: best.0,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests;
