//! Relocate a definition's maps onto a **sibling software** read by content
//! signature.
//!
//! Map addresses are software-specific: a definition built for software A
//! (and checked against a read of it) usually does not fit software B of the
//! same family, because tables were inserted, removed or re-ordered. Many tables
//! still exist in B, byte-identical or with a different calibration, at other
//! offsets. [`relocate`] finds each map of the definition in B from what the
//! definition's own read of A says about it:
//!
//! * the **structure**: stored axes and an inline dimension header in front of
//!   them (e.g. Bosch `[rows][cols][Y][X]`). When a map has one it is
//!   **required**: a location whose axes or header differ is never offered,
//!   however similar the cells are;
//! * the **cells**: how many are identical, and how well the surface shape
//!   correlates (calibration differences change values, not the shape). Cell
//!   values are allowed to differ;
//! * the **surroundings**: unique byte windows of A found once in B give a
//!   local alignment; a candidate whose nearest aligned windows on both sides
//!   agree with its shift is *bracketed*;
//! * the **neighbours**: maps placed nearby in A that moved by the same shift.
//!
//! Every placement carries a confidence in `[0, 1]` and the evidence strings
//! that produced it. A map is **refused** (never guessed) when its axes/header
//! are not found intact, when the best candidate is below
//! [`RelocateOptions::min_confidence`], when two candidates are within
//! [`RelocateOptions::margin`] of each other (ambiguous), or when the location
//! is already taken by another map. Two maps whose tables are byte-identical in
//! A cannot be told apart by content; they are only placed by position
//! (bracket or neighbours).
//!
//! [`derive_def`] turns the result into a new definition that keeps only the
//! relocated maps, each with its evidence and confidence in `notes`.
//!
//! ```
//! use opentune_core::ecudef::{DataType, Endian, MapDef};
//! use opentune_mapfind::relocate::{relocate, RelocateOptions};
//!
//! // Source: a 4x8 u16 table at 0x100. Target: the same table 0x40 later.
//! let table: Vec<u8> = (0..32u16).flat_map(|i| (1000 + 37 * i * i).to_be_bytes()).collect();
//! let mut a = vec![0u8; 0x400];
//! a[0x100..0x140].copy_from_slice(&table);
//! let mut b = vec![0u8; 0x400];
//! b[0x140..0x180].copy_from_slice(&table);
//! let map = MapDef {
//!     name: "demo".into(), address: 0x100, data_type: DataType::U16,
//!     endian: Endian::Big, rows: 4, cols: 8, ..Default::default()
//! };
//! let r = relocate(&a, &[map], &b, &RelocateOptions::default());
//! assert_eq!(r.maps[0].address, Some(0x140));
//! ```

use opentune_core::ecudef::{DataType, EcuDef, Endian, MapDef};
use serde::Serialize;

/// Tuning knobs for [`relocate`].
#[derive(Debug, Clone)]
pub struct RelocateOptions {
    /// Placements below this confidence are refused.
    pub min_confidence: f64,
    /// The best candidate must beat the runner-up by at least this much,
    /// otherwise the map is refused as ambiguous.
    pub margin: f64,
    /// How far (bytes, in the source image) alignment windows and neighbouring
    /// maps count as evidence for a map.
    pub radius: u32,
    /// Length of the alignment windows (bytes).
    pub window: usize,
}

impl Default for RelocateOptions {
    fn default() -> Self {
        Self {
            min_confidence: 0.6,
            margin: 0.1,
            radius: 0x800,
            window: 32,
        }
    }
}

/// Outcome for one map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RelocationStatus {
    /// Placed in the target image.
    Relocated,
    /// Not placed; [`MapRelocation::reason`] says why.
    Refused,
}

/// Where one map of the definition landed in the target image.
#[derive(Debug, Clone, Serialize)]
pub struct MapRelocation {
    /// The map's name in the source definition.
    pub name: String,
    /// Data address in the source read.
    pub source_address: u32,
    /// Placed or refused.
    pub status: RelocationStatus,
    /// Data address in the target read (relocated maps only).
    pub address: Option<u32>,
    /// `address - source_address`.
    pub shift: Option<i64>,
    /// Confidence of the placement (or of the best rejected candidate).
    pub confidence: f64,
    /// Fraction of cells identical in source and target.
    pub cells_identical: f64,
    /// Shape similarity of the cells (correlation, `0` when the magnitudes
    /// differ by more than 2x).
    pub shape_similarity: f64,
    /// What the signature contains, e.g. `header + 2 stored axes + cells`.
    pub signature: String,
    /// The facts behind the placement.
    pub evidence: Vec<String>,
    /// Why the map was refused.
    pub reason: Option<String>,
}

/// The result of [`relocate`].
#[derive(Debug, Clone, Serialize)]
pub struct Relocation {
    /// One entry per source map, in definition order.
    pub maps: Vec<MapRelocation>,
    /// Number of unique byte windows that aligned source and target.
    pub anchors: usize,
    /// Whole-image observations (e.g. an odd offset between the reads).
    pub notes: Vec<String>,
}

impl Relocation {
    /// Number of relocated maps.
    pub fn relocated(&self) -> usize {
        self.maps
            .iter()
            .filter(|m| m.status == RelocationStatus::Relocated)
            .count()
    }
}

/// A sorted index of every distinctive 8-byte window of an image.
struct Index<'a> {
    img: &'a [u8],
    keys: Vec<(u64, u32)>,
}

fn distinct(b: &[u8]) -> usize {
    let mut seen = [false; 256];
    let mut n = 0;
    for &x in b {
        if !seen[x as usize] {
            seen[x as usize] = true;
            n += 1;
        }
    }
    n
}

fn key(b: &[u8]) -> u64 {
    u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

impl<'a> Index<'a> {
    fn new(img: &'a [u8]) -> Self {
        let mut keys = Vec::with_capacity(img.len());
        if img.len() >= 8 {
            for p in 0..=img.len() - 8 {
                let w = &img[p..p + 8];
                if distinct(w) >= 3 {
                    keys.push((key(w), p as u32));
                }
            }
        }
        keys.sort_unstable();
        Self { img, keys }
    }

    /// Every position of `pat` (sorted), stopping after `limit + 1` hits so a
    /// caller can tell "more than `limit`".
    fn find(&self, pat: &[u8], limit: usize) -> Vec<u32> {
        let mut out = Vec::new();
        if pat.is_empty() || pat.len() > self.img.len() {
            return out;
        }
        let probe = (0..pat.len().saturating_sub(7)).find(|&o| distinct(&pat[o..o + 8]) >= 3);
        match probe {
            Some(o) => {
                let k = key(&pat[o..o + 8]);
                let lo = self.keys.partition_point(|e| e.0 < k);
                for &(kk, p) in &self.keys[lo..] {
                    if kk != k {
                        break;
                    }
                    let Some(start) = (p as usize).checked_sub(o) else {
                        continue;
                    };
                    if self.img.get(start..start + pat.len()) == Some(pat) {
                        out.push(start as u32);
                        if out.len() > limit {
                            break;
                        }
                    }
                }
                out.sort_unstable();
            }
            None => {
                // Low-entropy pattern: plain scan.
                for (p, w) in self.img.windows(pat.len()).enumerate() {
                    if w == pat {
                        out.push(p as u32);
                        if out.len() > limit {
                            break;
                        }
                    }
                }
            }
        }
        out
    }
}

/// One fixed part of a map's signature, relative to the data address.
#[derive(Debug, Clone)]
struct Part {
    rel: i64,
    bytes: Vec<u8>,
    label: &'static str,
}

struct Sig {
    data: Vec<u8>,
    cells: Vec<f64>,
    parts: Vec<Part>,
    /// Footprint start/end in the source image (header..data end).
    fs: i64,
    fe: i64,
    pre: (i64, Vec<u8>),
    post: (i64, Vec<u8>),
}

const CTX: usize = 16;

fn read_cells(img: &[u8], addr: i64, n: usize, dt: DataType, e: Endian) -> Option<Vec<f64>> {
    let w = dt.byte_len() as usize;
    let start = usize::try_from(addr).ok()?;
    let b = img.get(start..start.checked_add(n.checked_mul(w)?)?)?;
    Some(
        b.chunks_exact(w)
            .map(|c| {
                let mut a = [0u8; 4];
                a[..w].copy_from_slice(c);
                if e == Endian::Big {
                    a[..w].reverse();
                }
                let u = u32::from_le_bytes(a);
                match dt {
                    DataType::U8 => u as f64,
                    DataType::S8 => u as u8 as i8 as f64,
                    DataType::U16 => u as f64,
                    DataType::S16 => u as u16 as i16 as f64,
                    DataType::U32 => u as f64,
                    DataType::S32 => u as i32 as f64,
                    DataType::F32 => {
                        let f = f32::from_bits(u) as f64;
                        if f.is_finite() {
                            f
                        } else {
                            0.0
                        }
                    }
                }
            })
            .collect(),
    )
}

fn slice(img: &[u8], at: i64, len: usize) -> Option<&[u8]> {
    let s = usize::try_from(at).ok()?;
    img.get(s..s.checked_add(len)?)
}

fn signature(m: &MapDef, a: &[u8]) -> Option<Sig> {
    let addr = m.address as i64;
    let n = m.cell_count() as usize;
    let data = slice(a, addr, m.byte_len() as usize)?.to_vec();
    let cells = read_cells(a, addr, n, m.data_type, m.endian)?;
    let mut parts = Vec::new();
    let mut counts = Vec::new();
    for (ax, label) in [(&m.y_axis, "y-axis"), (&m.x_axis, "x-axis")] {
        let Some(ax) = ax else { continue };
        let Some(at) = ax.address else { continue };
        let len = (ax.count * ax.data_type.byte_len()) as usize;
        let bytes = slice(a, at as i64, len)?.to_vec();
        parts.push(Part {
            rel: at as i64 - addr,
            bytes,
            label,
        });
        counts.push(ax.count as u64);
    }
    let start = parts.iter().map(|p| addr + p.rel).fold(addr, i64::min);
    // An inline dimension header right in front of the stored axes: the counts
    // as u16 (map byte order) or u8, in any order.
    if !parts.is_empty() {
        counts.sort_unstable();
        let k = counts.len();
        for hw in [2usize, 1] {
            let Some(hb) = slice(a, start - (hw * k) as i64, hw * k) else {
                continue;
            };
            let mut vals: Vec<u64> = hb
                .chunks_exact(hw)
                .map(|c| match (hw, m.endian) {
                    (1, _) => c[0] as u64,
                    (_, Endian::Big) => u16::from_be_bytes([c[0], c[1]]) as u64,
                    (_, Endian::Little) => u16::from_le_bytes([c[0], c[1]]) as u64,
                })
                .collect();
            vals.sort_unstable();
            if vals == counts {
                parts.push(Part {
                    rel: start - (hw * k) as i64 - addr,
                    bytes: hb.to_vec(),
                    label: "header",
                });
                break;
            }
        }
    }
    let fs = parts.iter().map(|p| addr + p.rel).fold(addr, i64::min);
    let fe = parts
        .iter()
        .map(|p| addr + p.rel + p.bytes.len() as i64)
        .fold(addr + data.len() as i64, i64::max);
    let pre = slice(a, fs - CTX as i64, CTX)
        .map(|b| (fs - CTX as i64 - addr, b.to_vec()))
        .unwrap_or((0, Vec::new()));
    let post = slice(a, fe, CTX)
        .map(|b| (fe - addr, b.to_vec()))
        .unwrap_or((0, Vec::new()));
    Some(Sig {
        data,
        cells,
        parts,
        fs,
        fe,
        pre,
        post,
    })
}

fn describe(sig: &Sig) -> String {
    let axes = sig.parts.iter().filter(|p| p.label != "header").count();
    let header = sig.parts.iter().any(|p| p.label == "header");
    match (header, axes) {
        (_, 0) => "cells only (no stored axes)".into(),
        (true, n) => format!("header + {n} stored axis/axes + cells"),
        (false, n) => format!("{n} stored axis/axes + cells"),
    }
}

/// Shape similarity: Pearson correlation, 0 when the magnitudes are more than
/// 2x apart; two constant blocks are similar only when equal.
fn similarity(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len() as f64;
    if a.is_empty() {
        return 0.0;
    }
    let ma = a.iter().sum::<f64>() / n;
    let mb = b.iter().sum::<f64>() / n;
    let va: f64 = a.iter().map(|x| (x - ma).powi(2)).sum();
    let vb: f64 = b.iter().map(|x| (x - mb).powi(2)).sum();
    if va == 0.0 || vb == 0.0 {
        return if a == b { 1.0 } else { 0.0 };
    }
    if ma != 0.0 && mb != 0.0 {
        let r = mb / ma;
        if !(0.5..=2.0).contains(&r) {
            return 0.0;
        }
    } else if ma != mb {
        return 0.0;
    }
    let c: f64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| (x - ma) * (y - mb))
        .sum::<f64>()
        / (va * vb).sqrt();
    c.max(0.0)
}

#[derive(Debug, Clone)]
struct Cand {
    addr: i64,
    eq: f64,
    sim: f64,
    /// Nearest aligned window before / after the map agrees with the shift.
    before: bool,
    after: bool,
    pre: bool,
    post: bool,
    exact: bool,
    score: f64,
}

fn struct_ok(b: &[u8], sig: &Sig, c: i64) -> bool {
    sig.parts
        .iter()
        .all(|p| slice(b, c + p.rel, p.bytes.len()) == Some(&p.bytes[..]))
}

fn context_match(b: &[u8], c: i64, ctx: &(i64, Vec<u8>)) -> bool {
    !ctx.1.is_empty()
        && distinct(&ctx.1) >= 3
        && slice(b, c + ctx.0, ctx.1.len()) == Some(&ctx.1[..])
}

/// Unique-in-both windows of the source near the maps: `(source pos, shift)`,
/// sorted by position.
fn alignment(
    a: &Index,
    b: &Index,
    regions: &[(i64, i64)],
    win: usize,
    step: usize,
) -> Vec<(i64, i64)> {
    let mut starts: Vec<i64> = Vec::new();
    for &(s, e) in regions {
        let s = s.max(0);
        let e = e.min(a.img.len() as i64 - win as i64);
        let mut p = s - s % step as i64;
        while p <= e {
            starts.push(p);
            p += step as i64;
        }
    }
    starts.sort_unstable();
    starts.dedup();
    let mut out = Vec::new();
    for p in starts {
        let Some(w) = slice(a.img, p, win) else {
            continue;
        };
        if distinct(w) < 6 {
            continue;
        }
        let hb = b.find(w, 1);
        if hb.len() != 1 {
            continue;
        }
        if a.find(w, 1).len() != 1 {
            continue;
        }
        out.push((p, hb[0] as i64 - p));
    }
    out
}

/// The nearest alignment window before `fs` and after `fe` (within `radius`):
/// do they agree with `shift`?
fn bracket(anchors: &[(i64, i64)], fs: i64, fe: i64, shift: i64, win: i64, r: i64) -> (bool, bool) {
    let i = anchors.partition_point(|x| x.0 + win <= fs);
    let before = i
        .checked_sub(1)
        .map(|j| anchors[j])
        .filter(|x| fs - x.0 <= r)
        .is_some_and(|x| x.1 == shift);
    let k = anchors.partition_point(|x| x.0 < fe);
    let after = anchors
        .get(k)
        .filter(|x| x.0 - fe <= r)
        .is_some_and(|x| x.1 == shift);
    (before, after)
}

fn hex_shift(s: i64) -> String {
    if s < 0 {
        format!("-0x{:X}", -s)
    } else {
        format!("+0x{s:X}")
    }
}

/// Find each of `maps` (addresses valid for `source`) in `target`.
pub fn relocate(
    source: &[u8],
    maps: &[MapDef],
    target: &[u8],
    opts: &RelocateOptions,
) -> Relocation {
    let ia = Index::new(source);
    let ib = Index::new(target);
    let r = opts.radius as i64;
    let win = opts.window.max(8);
    let sigs: Vec<Option<Sig>> = maps.iter().map(|m| signature(m, source)).collect();
    let regions: Vec<(i64, i64)> = sigs
        .iter()
        .flatten()
        .map(|s| (s.fs - r, s.fe + r))
        .collect();
    let anchors = alignment(&ia, &ib, &regions, win, 8);
    let dup: Vec<Vec<usize>> = sigs
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let Some(s) = s else { return Vec::new() };
            sigs.iter()
                .enumerate()
                .filter(|(j, o)| *j != i && o.as_ref().is_some_and(|o| o.data == s.data))
                .map(|(j, _)| j)
                .collect()
        })
        .collect();

    // Candidates per map.
    let mut cands: Vec<Vec<Cand>> = Vec::with_capacity(maps.len());
    for (i, (m, sig)) in maps.iter().zip(&sigs).enumerate() {
        let Some(sig) = sig else {
            cands.push(Vec::new());
            continue;
        };
        let addr = m.address as i64;
        let mut at: Vec<i64> = Vec::new();
        if !sig.parts.is_empty() {
            // Search for the contiguous structure if it is contiguous, else
            // for the longest part; every part is verified afterwards.
            let mut ps = sig.parts.clone();
            ps.sort_by_key(|p| p.rel);
            let contiguous = ps
                .windows(2)
                .all(|w| w[0].rel + w[0].bytes.len() as i64 == w[1].rel);
            let (rel, pat) = if contiguous {
                (ps[0].rel, ps.iter().flat_map(|p| p.bytes.clone()).collect())
            } else {
                let p = ps.iter().max_by_key(|p| p.bytes.len()).expect("parts");
                (p.rel, p.bytes.clone())
            };
            for pos in ib.find(&pat, 256) {
                at.push(pos as i64 - rel);
            }
        } else {
            for pos in ib.find(&sig.data, 64) {
                at.push(pos as i64);
            }
            let rowb = (m.cols * m.data_type.byte_len()) as usize;
            if rowb >= 8 {
                for (ri, row) in sig.data.chunks_exact(rowb).enumerate() {
                    if distinct(row) < 3 {
                        continue;
                    }
                    let hs = ib.find(row, 16);
                    if hs.len() <= 16 {
                        at.extend(hs.iter().map(|&h| h as i64 - (ri * rowb) as i64));
                    }
                }
            }
            for ctx in [&sig.pre, &sig.post] {
                if ctx.1.is_empty() || distinct(&ctx.1) < 3 {
                    continue;
                }
                let hs = ib.find(&ctx.1, 16);
                if hs.len() <= 16 {
                    at.extend(hs.iter().map(|&h| h as i64 - ctx.0));
                }
            }
        }
        let lo = anchors.partition_point(|x| x.0 < sig.fs - r);
        for &(p, sh) in &anchors[lo..] {
            if p > sig.fe + r {
                break;
            }
            at.push(addr + sh);
        }
        at.sort_unstable();
        at.dedup();
        let n = sig.cells.len();
        let mut list = Vec::new();
        for c in at {
            if !sig.parts.is_empty() && !struct_ok(target, sig, c) {
                continue;
            }
            let Some(raw) = slice(target, c, sig.data.len()) else {
                continue;
            };
            if raw.iter().filter(|&&x| x == 0xFF).count() * 2 > raw.len() {
                continue; // erased flash
            }
            let Some(cb) = read_cells(target, c, n, m.data_type, m.endian) else {
                continue;
            };
            let eq = sig.cells.iter().zip(&cb).filter(|(x, y)| x == y).count() as f64 / n as f64;
            let sim = similarity(&sig.cells, &cb);
            let (before, after) = bracket(&anchors, sig.fs, sig.fe, c - addr, win as i64, r);
            let mut score = if sig.parts.is_empty() {
                let ctx = match (before, after) {
                    (true, true) => 0.3,
                    (true, false) | (false, true) => 0.15,
                    _ => 0.0,
                };
                ctx + 0.45 * eq + 0.25 * sim
            } else {
                let ctx = match (before, after) {
                    (true, true) => 0.15,
                    (true, false) | (false, true) => 0.08,
                    _ => 0.0,
                };
                0.6 + 0.25 * eq.max(sim) + ctx
            };
            if !dup[i].is_empty() && !(before || after) {
                score = score.min(0.5); // content cannot tell twins apart
            }
            list.push(Cand {
                addr: c,
                eq,
                sim,
                before,
                after,
                pre: context_match(target, c, &sig.pre),
                post: context_match(target, c, &sig.post),
                exact: raw == &sig.data[..],
                score: score.min(1.0),
            });
        }
        list.sort_by(|x, y| y.score.total_cmp(&x.score).then(x.addr.cmp(&y.addr)));
        cands.push(list);
    }

    // Assignment: strongest maps first, one map per location; then repeat with
    // a bonus for shifts that neighbouring placed maps share.
    #[derive(Clone)]
    enum Out {
        Open,
        Placed(Cand, f64, Vec<String>),
        Refused(String, Option<(Cand, f64)>),
    }
    let mut out: Vec<Out> = vec![Out::Open; maps.len()];
    let len_of = |i: usize| maps[i].byte_len() as i64;
    let mut changed = true;
    let mut round = 0;
    while changed && round < 6 {
        changed = false;
        let placed: Vec<(usize, i64, i64)> = out
            .iter()
            .enumerate()
            .filter_map(|(j, o)| match o {
                Out::Placed(c, _, _) => {
                    Some((j, maps[j].address as i64, c.addr - maps[j].address as i64))
                }
                _ => None,
            })
            .collect();
        let bonus = |i: usize, c: &Cand| -> (f64, Vec<String>) {
            if round == 0 {
                return (0.0, Vec::new());
            }
            let a = maps[i].address as i64;
            let sh = c.addr - a;
            let near: Vec<&(usize, i64, i64)> = placed
                .iter()
                .filter(|(j, ja, js)| *j != i && (ja - a).abs() <= r && *js == sh)
                .collect();
            if near.is_empty() {
                return (0.0, Vec::new());
            }
            let both = near.iter().any(|x| x.1 < a) && near.iter().any(|x| x.1 > a);
            let mut b = if both { 0.2 } else { 0.1 };
            if !dup[i].is_empty() && c.score <= 0.5 {
                b += 0.25; // position decides between byte-identical twins
            }
            let names: Vec<&str> = near
                .iter()
                .take(3)
                .map(|x| maps[x.0].name.as_str())
                .collect();
            (
                b,
                vec![format!(
                    "shift {} agrees with neighbouring placed map(s): {}{}",
                    hex_shift(sh),
                    names.join(", "),
                    if near.len() > 3 { ", …" } else { "" }
                )],
            )
        };
        let claimed: Vec<(i64, i64, usize)> = out
            .iter()
            .enumerate()
            .filter_map(|(j, o)| match o {
                Out::Placed(c, _, _) => Some((c.addr, c.addr + len_of(j), j)),
                _ => None,
            })
            .collect();
        let mut claimed = claimed;
        let mut order: Vec<(usize, f64)> = (0..maps.len())
            .filter(|&i| !matches!(out[i], Out::Placed(..)))
            .map(|i| {
                let best = cands[i]
                    .iter()
                    .map(|c| c.score + bonus(i, c).0)
                    .fold(0.0, f64::max);
                (i, best)
            })
            .collect();
        order.sort_by(|x, y| y.1.total_cmp(&x.1).then(x.0.cmp(&y.0)));
        for (i, _) in order {
            if sigs[i].is_none() {
                out[i] = Out::Refused("map does not fit inside the source image".into(), None);
                continue;
            }
            if cands[i].is_empty() {
                out[i] = Out::Refused(no_candidate_reason(&ib, sigs[i].as_ref().unwrap()), None);
                continue;
            }
            let mut scored: Vec<(Cand, f64, Vec<String>)> = cands[i]
                .iter()
                .map(|c| {
                    let (b, ev) = bonus(i, c);
                    (c.clone(), (c.score + b).min(1.0), ev)
                })
                .collect();
            scored.sort_by(|x, y| y.1.total_cmp(&x.1).then(x.0.addr.cmp(&y.0.addr)));
            let free: Vec<&(Cand, f64, Vec<String>)> = scored
                .iter()
                .filter(|(c, _, _)| {
                    !claimed
                        .iter()
                        .any(|&(s, e, _)| s < c.addr + len_of(i) && c.addr < e)
                })
                .collect();
            let top = &scored[0];
            let Some(best) = free.first() else {
                let by = claimed
                    .iter()
                    .find(|&&(s, e, _)| s < top.0.addr + len_of(i) && top.0.addr < e)
                    .map(|x| maps[x.2].name.clone())
                    .unwrap_or_default();
                out[i] = Out::Refused(
                    format!(
                        "its best location 0x{:X} is already taken by '{by}'",
                        top.0.addr
                    ),
                    Some((top.0.clone(), top.1)),
                );
                continue;
            };
            if best.1 < opts.min_confidence {
                out[i] = Out::Refused(
                    format!(
                        "low confidence: best candidate 0x{:X} scores {:.2} < {:.2}",
                        best.0.addr, best.1, opts.min_confidence
                    ),
                    Some((best.0.clone(), best.1)),
                );
                continue;
            }
            let close: Vec<String> = free
                .iter()
                .skip(1)
                .filter(|x| best.1 - x.1 < opts.margin)
                .map(|x| format!("0x{:X}", x.0.addr))
                .collect();
            if !close.is_empty() {
                out[i] = Out::Refused(
                    format!(
                        "ambiguous: 0x{:X} ({:.2}) vs {} within {:.2}",
                        best.0.addr,
                        best.1,
                        close.iter().take(4).cloned().collect::<Vec<_>>().join(", "),
                        opts.margin
                    ),
                    Some((best.0.clone(), best.1)),
                );
                continue;
            }
            claimed.push((best.0.addr, best.0.addr + len_of(i), i));
            out[i] = Out::Placed(best.0.clone(), best.1, best.2.clone());
            changed = true;
        }
        round += 1;
        if round == 1 {
            changed = true; // always run the neighbour round once
        }
    }

    let mut result = Vec::with_capacity(maps.len());
    let mut odd = 0usize;
    let mut placed_wide = 0usize;
    let mut odd_example = 0i64;
    for (i, m) in maps.iter().enumerate() {
        let sig = sigs[i].as_ref();
        let signature = sig.map(describe).unwrap_or_default();
        let twins: Vec<&str> = dup[i].iter().map(|&j| maps[j].name.as_str()).collect();
        match &out[i] {
            Out::Placed(c, conf, extra) => {
                let shift = c.addr - m.address as i64;
                if m.data_type.byte_len() >= 2 && m.address % 2 == 0 {
                    placed_wide += 1;
                    if shift % 2 != 0 {
                        odd += 1;
                        odd_example = shift;
                    }
                }
                let mut ev = evidence(sig.unwrap(), c, shift);
                ev.extend(extra.iter().cloned());
                if !twins.is_empty() {
                    ev.push(format!(
                        "source table is byte-identical to {}: placed by position, not content",
                        twins.join(", ")
                    ));
                }
                result.push(MapRelocation {
                    name: m.name.clone(),
                    source_address: m.address,
                    status: RelocationStatus::Relocated,
                    address: Some(c.addr as u32),
                    shift: Some(shift),
                    confidence: round2(*conf),
                    cells_identical: round2(c.eq),
                    shape_similarity: round2(c.sim),
                    signature,
                    evidence: ev,
                    reason: None,
                });
            }
            Out::Refused(why, best) => {
                let (conf, eq, sim, ev) = match (best, sig) {
                    (Some((c, s)), Some(sig)) => {
                        (*s, c.eq, c.sim, evidence(sig, c, c.addr - m.address as i64))
                    }
                    _ => (0.0, 0.0, 0.0, Vec::new()),
                };
                let mut why = why.clone();
                if !twins.is_empty() && !why.contains("taken") {
                    why.push_str(&format!(
                        " (source table is byte-identical to {})",
                        twins.join(", ")
                    ));
                }
                result.push(MapRelocation {
                    name: m.name.clone(),
                    source_address: m.address,
                    status: RelocationStatus::Refused,
                    address: None,
                    shift: None,
                    confidence: round2(conf),
                    cells_identical: round2(eq),
                    shape_similarity: round2(sim),
                    signature,
                    evidence: ev,
                    reason: Some(why),
                });
            }
            Out::Open => unreachable!("every map is decided"),
        }
    }
    let mut notes = Vec::new();
    if placed_wide > 0 && odd * 2 > placed_wide {
        notes.push(format!(
            "{odd} of {placed_wide} multi-byte maps moved by an odd number of bytes (e.g. {}): \
             the target read looks offset by one byte against the source (a read missing its \
             first byte?). Addresses are offsets into the target file as given.",
            hex_shift(odd_example)
        ));
    }
    Relocation {
        maps: result,
        anchors: anchors.len(),
        notes,
    }
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn evidence(sig: &Sig, c: &Cand, shift: i64) -> Vec<String> {
    let mut ev = Vec::new();
    if !sig.parts.is_empty() {
        let labels: Vec<&str> = sig.parts.iter().map(|p| p.label).collect();
        ev.push(format!(
            "{} byte-identical in the target",
            labels.join(" + ")
        ));
    }
    if c.exact {
        ev.push("cells byte-identical".into());
    } else {
        ev.push(format!(
            "cells {:.0}% identical, shape similarity {:.2}",
            c.eq * 100.0,
            c.sim
        ));
    }
    match (c.before, c.after) {
        (true, true) => ev.push(format!(
            "bracketed: the nearest aligned windows before and after moved by the same {}",
            hex_shift(shift)
        )),
        (true, false) => ev.push(format!(
            "the nearest aligned window before moved by the same {}",
            hex_shift(shift)
        )),
        (false, true) => ev.push(format!(
            "the nearest aligned window after moved by the same {}",
            hex_shift(shift)
        )),
        _ => {}
    }
    if c.pre && c.post {
        ev.push("the 16 bytes before and after are identical".into());
    } else if c.pre || c.post {
        ev.push(format!(
            "the 16 bytes {} are identical",
            if c.pre { "before" } else { "after" }
        ));
    }
    ev
}

/// Why a map has no candidate: for a structured map, the closest partial match.
fn no_candidate_reason(ib: &Index, sig: &Sig) -> String {
    if sig.parts.is_empty() {
        return "no candidate: neither the cells nor the surrounding bytes were found".into();
    }
    // Tally, per implied data address, which parts match.
    let mut tally: std::collections::BTreeMap<i64, Vec<&'static str>> = Default::default();
    for p in &sig.parts {
        if p.bytes.len() < 4 {
            continue;
        }
        let hs = ib.find(&p.bytes, 64);
        if hs.len() > 64 {
            continue;
        }
        for h in hs {
            tally.entry(h as i64 - p.rel).or_default().push(p.label);
        }
    }
    let best = tally.iter().max_by_key(|(a, v)| (v.len(), -**a));
    match best {
        Some((at, v)) => format!(
            "axes/header not found intact; closest: {} at the right offsets for data 0x{:X}, \
             the rest differs (axes/header must match)",
            v.join(" + "),
            at
        ),
        None => "axes/header not found anywhere in the target".into(),
    }
}

/// The honesty label (`CONFIDENCE: HIGH/MEDIUM/LOW`) a source map carries.
fn source_label(notes: &str) -> &'static str {
    let u = notes.to_ascii_uppercase();
    if u.contains("CONFIDENCE: HIGH") {
        "HIGH"
    } else if u.contains("CONFIDENCE: MEDIUM") {
        "MEDIUM"
    } else if u.contains("CONFIDENCE: LOW") {
        "LOW"
    } else {
        "none"
    }
}

/// The label of a relocated map: never above the source map's own label, and
/// capped by the relocation score (>= 0.9 keeps it, >= 0.75 at most MEDIUM,
/// below that LOW).
fn relocated_label(notes: &str, score: f64) -> &'static str {
    let rank = |l: &str| match l {
        "HIGH" => 3,
        "MEDIUM" => 2,
        _ => 1,
    };
    let cap = if score >= 0.9 {
        3
    } else if score >= 0.75 {
        2
    } else {
        1
    };
    match rank(source_label(notes)).min(cap) {
        3 => "HIGH",
        2 => "MEDIUM",
        _ => "LOW",
    }
}

/// A new definition from `def` keeping only the relocated maps, each moved to
/// its target address with its evidence and confidence in `notes`.
///
/// `id` names the derived definition; `source` and `target` describe the two
/// reads (e.g. file name and software number) for the provenance notes.
pub fn derive_def(def: &EcuDef, rel: &Relocation, id: &str, source: &str, target: &str) -> EcuDef {
    let mut out = def.clone();
    out.id = id.to_string();
    out.name = format!("{} (relocated)", def.name);
    let refused: Vec<String> = rel
        .maps
        .iter()
        .filter(|m| m.status == RelocationStatus::Refused)
        .map(|m| m.name.clone())
        .collect();
    out.notes = format!(
        "DERIVED by `opentune ecudefs relocate` from definition '{}' (built for read {source}) \
         onto read {target}. {} of {} map(s) relocated by content signature; {} refused{}. \
         Each map's notes carry its confidence and evidence: VERIFY every map against your own \
         read before any write.{} Source definition notes: {}",
        def.id,
        rel.relocated(),
        rel.maps.len(),
        refused.len(),
        if refused.is_empty() {
            String::new()
        } else {
            format!(" ({})", refused.join("; "))
        },
        if rel.notes.is_empty() {
            String::new()
        } else {
            format!(" {}", rel.notes.join(" "))
        },
        def.notes
    );
    // The derived maps were found in one real read and nothing more: record the
    // derivation as a source and cap the evidence at real-image-tested.
    if let Some(p) = out.provenance.as_mut() {
        p.sources.push(format!(
            "derived by `opentune ecudefs relocate` from definition '{}' onto read {target}",
            def.id
        ));
        let rank = |e: &str| {
            opentune_core::provenance::EVIDENCE_LEVELS
                .iter()
                .position(|l| *l == e)
        };
        if rank(&p.evidence) > rank("real-image-tested") {
            p.evidence = "real-image-tested".into();
        }
    }
    out.maps = def
        .maps
        .iter()
        .zip(&rel.maps)
        .filter_map(|(m, r)| {
            let (addr, shift) = (r.address?, r.shift?);
            let mut m = m.clone();
            m.address = addr;
            for ax in [&mut m.x_axis, &mut m.y_axis].into_iter().flatten() {
                if let Some(a) = ax.address {
                    ax.address = Some((a as i64 + shift) as u32);
                }
            }
            m.notes = format!(
                "CONFIDENCE: {} (relocated; source label {}, relocation score {:.2}). \
                 RELOCATED from 0x{:X} ({source}) to 0x{addr:X} ({target}), shift {}, \
                 confidence {:.2}. Signature: {}. Evidence: {}. Original notes: {}",
                relocated_label(&m.notes, r.confidence),
                source_label(&m.notes),
                r.confidence,
                r.source_address,
                hex_shift(shift),
                r.confidence,
                r.signature,
                r.evidence.join("; "),
                m.notes
            );
            Some(m)
        })
        .collect();
    out
}
