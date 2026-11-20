//! A loaded flash image: map decoding and the edit model.
//!
//! **Decoding** (raw element by data type + byte order, `Scaling::to_physical`,
//! map grids and axes) is the shared codec `opentune_hub::maps`, the one the
//! Tauri app uses, so both apps show the same numbers for the same bytes.
//!
//! **Encoding stays local and REFUSES, never clamps.** `opentune_hub::maps::
//! encode_elem` saturates an out-of-range value to the type's limits (that is
//! the webview editor's documented clamp); the native editor must never store
//! a value other than the one the user asked for, so it rounds to the nearest
//! raw step and refuses anything that does not fit the data type or the map's
//! `min` / `max` ([`encode_elem`], [`Image::edit_cell`]). The pinned test
//! `refuse_not_clamp_is_pinned` fails if that ever regresses.
//!
//! Shape rule (the hub's, also the desktop's write path): a map is
//! `rows.max(1) × cols.max(1)` cells ([`shape`]); a definition with `rows: 0`
//! is a one-row table, both when it is shown and when it is edited.

use opentune_core::ecudef::{AxisDef, DataType, Endian, MapDef, Scaling};
use opentune_hub::maps as codec;
use std::collections::BTreeSet;
use std::path::PathBuf;

/// `(rows, cols)` of a map, each at least 1 (the shared codec's rule).
pub fn shape(m: &MapDef) -> (usize, usize) {
    (m.rows.max(1) as usize, m.cols.max(1) as usize)
}

/// Number of cells of a map ([`shape`]).
pub fn cell_total(m: &MapDef) -> usize {
    let (r, c) = shape(m);
    r.saturating_mul(c)
}

/// Bytes the map's cells occupy ([`shape`] × element width), without overflow.
pub fn data_len(m: &MapDef) -> u64 {
    cell_total(m) as u64 * m.data_type.byte_len() as u64
}

/// One byte change of an edit: offset, the byte before, the byte after.
type Change = (usize, u8, u8);

/// An opened dump: the working bytes (with any unsaved edits) plus what the
/// file holds on disk, so edits can be counted, undone, redone and reverted.
pub struct Image {
    pub path: PathBuf,
    /// Working copy (what every view shows).
    pub bytes: Vec<u8>,
    saved: Vec<u8>,
    /// Offsets whose working byte differs from the saved one.
    changed: BTreeSet<usize>,
    /// One entry per edit operation (a single cell or a whole multi-cell
    /// operation): every byte it wrote, old and new.
    undo: Vec<Vec<Change>>,
    redo: Vec<Vec<Change>>,
    /// Bumped on every change of `bytes` (edit, undo, redo, revert, save).
    generation: u64,
}

/// A cell change: a physical value, or a number of raw steps (±1 LSB).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CellEdit {
    Set(f64),
    Step(i32),
}

/// A cell a multi-cell edit could not store, and why.
#[derive(Clone, Debug, PartialEq)]
pub struct Refusal {
    /// Cell index (row-major).
    pub idx: usize,
    pub reason: String,
}

impl Image {
    pub fn open(path: PathBuf) -> std::io::Result<Self> {
        let bytes = std::fs::read(&path)?;
        Ok(Self::from_bytes(path, bytes))
    }

    pub fn from_bytes(path: PathBuf, bytes: Vec<u8>) -> Self {
        Self {
            path,
            saved: bytes.clone(),
            bytes,
            changed: BTreeSet::new(),
            undo: Vec::new(),
            redo: Vec::new(),
            generation: 0,
        }
    }

    /// Unsaved edits present?
    pub fn is_dirty(&self) -> bool {
        !self.changed.is_empty()
    }

    /// Number of bytes that differ from the file on disk.
    pub fn changed_bytes(&self) -> usize {
        self.changed.len()
    }

    /// What the file holds on disk (the baseline of the unsaved edits).
    pub fn saved_bytes(&self) -> &[u8] {
        &self.saved
    }

    /// Changes whenever the working bytes do (cheap change detection).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Does any byte of cell `idx` of `m` differ from the file on disk?
    pub fn cell_changed(&self, m: &MapDef, idx: usize) -> bool {
        let w = m.data_type.byte_len() as usize;
        let off = m.address as usize + idx * w;
        self.changed.range(off..off + w).next().is_some()
    }

    /// Does any byte of map `m` differ from the file on disk?
    pub fn map_changed(&self, m: &MapDef) -> bool {
        let a = m.address as usize;
        let end = a.saturating_add(data_len(m) as usize);
        self.changed.range(a..end).next().is_some()
    }

    /// The stored bytes of one cell after edit `e`, checked against the data
    /// type and the map's limits: `(offset, bytes, physical value stored)`.
    /// Nothing is changed.
    fn encode_cell(
        &self,
        m: &MapDef,
        idx: usize,
        e: CellEdit,
    ) -> Result<(usize, Vec<u8>, f64), String> {
        if idx >= cell_total(m) {
            return Err(format!("cell {idx} is outside {}", m.name));
        }
        let w = m.data_type.byte_len() as usize;
        let off = m.address as usize + idx * w;
        let cur = read_elem(&self.bytes, off, m.data_type, m.endian)
            .ok_or_else(|| format!("{} runs past the end of the image", m.name))?;
        let raw = match e {
            CellEdit::Set(p) => to_raw(&m.scaling, p)
                .ok_or_else(|| format!("{p} cannot be stored with this map's scaling"))?,
            CellEdit::Step(n) => cur + n as f64,
        };
        let enc = encode_elem(raw, m.data_type, m.endian)?;
        let stored = read_elem(&enc, 0, m.data_type, m.endian).unwrap_or(raw);
        let phys = m.scaling.to_physical(stored);
        let (lo, hi) = (m.min.unwrap_or(f64::MIN), m.max.unwrap_or(f64::MAX));
        // A small tolerance: the limits are physical and scaling is inexact.
        let tol = m.scaling.factor.abs() * 1e-6;
        if phys < lo - tol || phys > hi + tol {
            return Err(format!(
                "{} is outside {}'s limits {} … {}",
                fmt_val(phys),
                m.name,
                m.min.map(fmt_val).unwrap_or_else(|| "−∞".into()),
                m.max.map(fmt_val).unwrap_or_else(|| "∞".into()),
            ));
        }
        Ok((off, enc, phys))
    }

    /// Write `enc` at `off` into the working bytes, recording it in `entry`.
    /// Returns whether anything changed.
    fn write(&mut self, off: usize, enc: &[u8], entry: &mut Vec<Change>) -> bool {
        let w = enc.len();
        if self.bytes[off..off + w] == enc[..] {
            return false;
        }
        for (k, &b) in enc.iter().enumerate() {
            entry.push((off + k, self.bytes[off + k], b));
        }
        self.bytes[off..off + w].copy_from_slice(enc);
        true
    }

    /// Record a finished operation: one undo step, redo cleared.
    fn commit(&mut self, entry: Vec<Change>) {
        if entry.is_empty() {
            return;
        }
        for &(o, _, _) in &entry {
            self.track(o);
        }
        self.undo.push(entry);
        self.redo.clear();
        self.generation += 1;
    }

    /// Change one cell of `m`. The value is converted to the stored raw
    /// number (integers round to the nearest step), refused when it does not
    /// fit the data type or the map's `min`/`max`. Returns the physical value
    /// actually stored. One undo step.
    pub fn edit_cell(&mut self, m: &MapDef, idx: usize, e: CellEdit) -> Result<f64, String> {
        let (off, enc, phys) = self.encode_cell(m, idx, e)?;
        let mut entry = Vec::new();
        self.write(off, &enc, &mut entry);
        self.commit(entry);
        Ok(phys)
    }

    /// Change several cells of `m` as ONE operation, all or nothing: every
    /// cell is checked exactly as [`Image::edit_cell`] checks one (refused,
    /// never clamped); when any cell is refused nothing is changed and every
    /// refusal is returned. Otherwise all cells are stored as a single undo
    /// step. Returns how many cells changed. Cells are applied in order, so a
    /// `Step` sees the earlier edits of the same call.
    pub fn edit_cells(
        &mut self,
        m: &MapDef,
        edits: &[(usize, CellEdit)],
    ) -> Result<usize, Vec<Refusal>> {
        self.edit_maps(&[(m, edits)])
            .map(|n| n[0])
            .map_err(|r| r.into_iter().map(|(_, r)| r).collect())
    }

    /// [`Image::edit_cells`] over several maps (a project's edits): ONE
    /// operation and one undo step, all or nothing. Returns the cells changed
    /// per map, or every refusal as `(index into ops, refusal)`.
    pub fn edit_maps(
        &mut self,
        ops: &[(&MapDef, &[(usize, CellEdit)])],
    ) -> Result<Vec<usize>, Vec<(usize, Refusal)>> {
        let mut entry = Vec::new();
        let mut refused = Vec::new();
        let mut counts = vec![0; ops.len()];
        for (k, (m, edits)) in ops.iter().enumerate() {
            for &(idx, e) in edits.iter() {
                match self.encode_cell(m, idx, e) {
                    Ok((off, enc, _)) => {
                        if self.write(off, &enc, &mut entry) {
                            counts[k] += 1;
                        }
                    }
                    Err(reason) => refused.push((k, Refusal { idx, reason })),
                }
            }
        }
        if !refused.is_empty() {
            // Roll back in reverse so repeated cells end at their first old value.
            for &(o, old, _) in entry.iter().rev() {
                self.bytes[o] = old;
            }
            return Err(refused);
        }
        self.commit(entry);
        Ok(counts)
    }

    fn track(&mut self, o: usize) {
        if self.bytes[o] != self.saved[o] {
            self.changed.insert(o);
        } else {
            self.changed.remove(&o);
        }
    }

    /// Undo the last edit operation. Returns the first offset it touched.
    pub fn undo(&mut self) -> Option<usize> {
        let entry = self.undo.pop()?;
        for &(o, old, _) in entry.iter().rev() {
            self.bytes[o] = old;
            self.track(o);
        }
        let first = entry.first().map(|c| c.0);
        self.redo.push(entry);
        self.generation += 1;
        first
    }

    /// Redo the last undone operation. Returns the first offset it touched.
    pub fn redo(&mut self) -> Option<usize> {
        let entry = self.redo.pop()?;
        for &(o, _, new) in &entry {
            self.bytes[o] = new;
            self.track(o);
        }
        let first = entry.first().map(|c| c.0);
        self.undo.push(entry);
        self.generation += 1;
        first
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Drop every unsaved edit.
    pub fn revert(&mut self) {
        self.bytes.clone_from(&self.saved);
        self.changed.clear();
        self.undo.clear();
        self.redo.clear();
        self.generation += 1;
    }

    /// `bytes` now are what `path` holds on disk (after a save).
    pub fn mark_saved(&mut self, path: PathBuf, bytes: Vec<u8>) {
        let g = self.generation + 1;
        *self = Self::from_bytes(path, bytes);
        self.generation = g;
    }

    pub fn name(&self) -> String {
        self.path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

/// One stored element as a raw number, or `None` past the end of the image
/// (the shared codec's `read_elem`, with an overflow-safe bounds check).
pub fn read_elem(img: &[u8], off: usize, dt: DataType, en: Endian) -> Option<f64> {
    off.checked_add(dt.byte_len() as usize)?;
    codec::read_elem(img, off, dt, en)
}

/// The raw number a physical value is stored as (before rounding), or
/// `None` when the scaling cannot be inverted.
pub fn to_raw(s: &Scaling, phys: f64) -> Option<f64> {
    let r = (phys - s.offset) / s.factor;
    (s.factor != 0.0 && phys.is_finite() && r.is_finite()).then_some(r)
}

/// Encode a raw number as stored bytes: integers round to nearest and must
/// fit the type (never wrapped or clamped). Deliberately NOT
/// `opentune_hub::maps::encode_elem`, which clamps.
pub fn encode_elem(raw: f64, dt: DataType, en: Endian) -> Result<Vec<u8>, String> {
    if !raw.is_finite() {
        return Err("not a number".into());
    }
    let r = raw.round();
    let (lo, hi): (f64, f64) = match dt {
        DataType::U8 => (0.0, u8::MAX as f64),
        DataType::S8 => (i8::MIN as f64, i8::MAX as f64),
        DataType::U16 => (0.0, u16::MAX as f64),
        DataType::S16 => (i16::MIN as f64, i16::MAX as f64),
        DataType::U32 => (0.0, u32::MAX as f64),
        DataType::S32 => (i32::MIN as f64, i32::MAX as f64),
        DataType::F32 => (f32::MIN as f64, f32::MAX as f64),
    };
    let v = if dt == DataType::F32 { raw } else { r };
    if v < lo || v > hi {
        return Err(format!(
            "raw {} does not fit {dt:?} ({} … {})",
            fmt_val(v),
            fmt_val(lo),
            fmt_val(hi)
        ));
    }
    let bits: u32 = match dt {
        DataType::F32 => (v as f32).to_bits(),
        DataType::S8 => v as i8 as u8 as u32,
        DataType::S16 => v as i16 as u16 as u32,
        DataType::S32 => v as i32 as u32,
        _ => v as u32,
    };
    let w = dt.byte_len() as usize;
    let be = bits.to_be_bytes();
    let mut out = be[4 - w..].to_vec();
    if en == Endian::Little {
        out.reverse();
    }
    Ok(out)
}

/// A map's cells as physical values, row-major (`r * cols + c`, [`shape`]),
/// or `None` when the map runs past the end of the image. Decoded by the
/// shared codec (`opentune_hub::maps::decode_cells`).
pub fn map_values(img: &[u8], m: &MapDef) -> Option<Vec<f64>> {
    codec::decode_cells(img, m).map(|g| g.into_iter().flatten().collect())
}

/// Axis breakpoints for `n` cells, decoded by the shared codec
/// (`opentune_hub::maps::decode_axis`: inline values first, else values
/// stored in the image, else indices for an absent or shared axis; an
/// unreadable stored breakpoint is its index). Always exactly `n` values:
/// extra inline values are dropped, missing ones are their index, so the
/// grid can index the axis by column / row.
pub fn axis_values(img: &[u8], axis: Option<&AxisDef>, n: usize) -> Vec<f64> {
    let (mut v, _) = codec::decode_axis(img, &axis.cloned(), n as u32);
    v.truncate(n);
    let have = v.len();
    v.extend((have..n).map(|i| i as f64));
    v
}

/// Compact cell text: integers without decimals, others with up to 2.
pub fn fmt_val(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e9 {
        format!("{}", v as i64)
    } else {
        format!("{v:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentune_core::ecudef::Scaling;

    #[test]
    fn elements_by_type_and_byte_order() {
        let b = [0x12, 0x34, 0xFF, 0xFE, 0x3F, 0x80, 0x00, 0x00];
        assert_eq!(
            read_elem(&b, 0, DataType::U16, Endian::Big),
            Some(0x1234 as f64)
        );
        assert_eq!(
            read_elem(&b, 0, DataType::U16, Endian::Little),
            Some(0x3412 as f64)
        );
        assert_eq!(read_elem(&b, 2, DataType::S16, Endian::Big), Some(-2.0));
        assert_eq!(read_elem(&b, 2, DataType::S8, Endian::Big), Some(-1.0));
        assert_eq!(read_elem(&b, 4, DataType::F32, Endian::Big), Some(1.0));
        assert_eq!(
            read_elem(&b, 7, DataType::U16, Endian::Big),
            None,
            "past the end"
        );
    }

    #[test]
    fn map_cells_are_scaled_row_major() {
        let img = [0, 1, 0, 2, 0, 3, 0, 4];
        let m = MapDef {
            name: "m".into(),
            address: 0,
            data_type: DataType::U16,
            endian: Endian::Big,
            rows: 2,
            cols: 2,
            scaling: Scaling {
                factor: 0.5,
                offset: 1.0,
            },
            ..Default::default()
        };
        assert_eq!(map_values(&img, &m), Some(vec![1.5, 2.0, 2.5, 3.0]));
        let past = MapDef { address: 4, ..m };
        assert_eq!(map_values(&img, &past), None);
    }

    fn scaled_map() -> MapDef {
        MapDef {
            name: "m".into(),
            address: 2,
            data_type: DataType::U16,
            endian: Endian::Big,
            rows: 2,
            cols: 2,
            scaling: Scaling {
                factor: 0.5,
                offset: 1.0,
            },
            ..Default::default()
        }
    }

    #[test]
    fn encode_round_trips_every_type_and_refuses_overflow() {
        for (dt, raw) in [
            (DataType::U8, 200.0),
            (DataType::S8, -5.0),
            (DataType::U16, 0xBEEF as f64),
            (DataType::S16, -1234.0),
            (DataType::U32, 4_000_000_000.0),
            (DataType::S32, -70_000.0),
            (DataType::F32, 1.5),
        ] {
            for en in [Endian::Big, Endian::Little] {
                let b = encode_elem(raw, dt, en).unwrap();
                assert_eq!(b.len(), dt.byte_len() as usize);
                assert_eq!(read_elem(&b, 0, dt, en), Some(raw), "{dt:?} {en:?}");
            }
        }
        assert!(encode_elem(256.0, DataType::U8, Endian::Big).is_err());
        assert!(encode_elem(-1.0, DataType::U16, Endian::Big).is_err());
        assert!(encode_elem(-129.0, DataType::S8, Endian::Big).is_err());
        assert!(encode_elem(f64::NAN, DataType::F32, Endian::Big).is_err());
        assert_eq!(
            encode_elem(2.6, DataType::U16, Endian::Little).unwrap(),
            vec![3, 0],
            "integers round to nearest"
        );
        assert_eq!(
            to_raw(
                &Scaling {
                    factor: 0.0,
                    offset: 0.0
                },
                1.0
            ),
            None
        );
    }

    #[test]
    fn edits_track_undo_and_revert() {
        let m = scaled_map();
        let mut img = Image::from_bytes(
            PathBuf::from("x.bin"),
            vec![9, 9, 0, 1, 0, 2, 0, 3, 0, 4, 9],
        );
        assert!(!img.is_dirty());
        // phys 4.0 -> raw (4-1)/0.5 = 6
        assert_eq!(img.edit_cell(&m, 1, CellEdit::Set(4.0)), Ok(4.0));
        assert_eq!(&img.bytes[4..6], &[0, 6]);
        assert!(img.is_dirty() && img.cell_changed(&m, 1) && !img.cell_changed(&m, 0));
        assert_eq!(img.changed_bytes(), 1, "only the low byte differs");
        // Rounds to the nearest stored step.
        assert_eq!(img.edit_cell(&m, 0, CellEdit::Set(2.2)), Ok(2.0));
        assert_eq!(img.edit_cell(&m, 3, CellEdit::Step(-2)), Ok(2.0));
        assert_eq!(&img.bytes[8..10], &[0, 2]);
        // Out of range for u16: refused, nothing changes.
        let before = img.bytes.clone();
        assert!(img.edit_cell(&m, 2, CellEdit::Set(-10.0)).is_err());
        assert!(
            img.edit_cell(&m, 4, CellEdit::Set(1.0)).is_err(),
            "no cell 4"
        );
        assert_eq!(img.bytes, before);
        // Undo walks back; editing a cell back to its saved value un-dirties it.
        assert_eq!(img.undo(), Some(8));
        assert_eq!(&img.bytes[8..10], &[0, 4]);
        img.edit_cell(&m, 1, CellEdit::Set(2.0)).unwrap();
        assert!(!img.cell_changed(&m, 1));
        assert!(img.is_dirty(), "cell 0 still edited");
        img.revert();
        assert!(!img.is_dirty() && !img.can_undo());
        assert_eq!(img.bytes, vec![9, 9, 0, 1, 0, 2, 0, 3, 0, 4, 9]);
    }

    #[test]
    fn edits_respect_map_limits() {
        let m = MapDef {
            min: Some(1.0),
            max: Some(3.0),
            ..scaled_map()
        };
        let mut img = Image::from_bytes(PathBuf::from("x.bin"), vec![0; 10]);
        assert!(img.edit_cell(&m, 0, CellEdit::Set(3.5)).is_err());
        assert_eq!(img.edit_cell(&m, 0, CellEdit::Set(3.0)), Ok(3.0));
        assert!(img.edit_cell(&m, 0, CellEdit::Step(1)).is_err());
        let mut saved = img.bytes.clone();
        saved[3] = 4;
        img.mark_saved(PathBuf::from("y.bin"), saved);
        assert!(!img.is_dirty() && img.path == std::path::Path::new("y.bin"));
    }

    #[test]
    fn axes_inline_stored_or_index() {
        let img = [0, 10, 0, 20];
        assert_eq!(axis_values(&img, None, 3), vec![0.0, 1.0, 2.0]);
        let inline = AxisDef {
            values: vec![5.0, 6.0],
            ..Default::default()
        };
        assert_eq!(axis_values(&img, Some(&inline), 2), vec![5.0, 6.0]);
        let stored = AxisDef {
            address: Some(0),
            data_type: DataType::U16,
            endian: Endian::Big,
            ..Default::default()
        };
        assert_eq!(axis_values(&img, Some(&stored), 2), vec![10.0, 20.0]);
    }

    // -----------------------------------------------------------------------
    // Edge cases of cell editing
    // -----------------------------------------------------------------------

    fn map(dt: DataType, en: Endian, factor: f64, offset: f64) -> MapDef {
        MapDef {
            name: "edge".into(),
            address: 1,
            data_type: dt,
            endian: en,
            rows: 1,
            cols: 2,
            scaling: Scaling { factor, offset },
            ..Default::default()
        }
    }

    fn image(len: usize) -> Image {
        Image::from_bytes(PathBuf::from("edge.bin"), vec![0; len])
    }

    #[test]
    fn s8_with_a_negative_factor_and_offset() {
        // phys = raw * -0.5 + 10
        let m = map(DataType::S8, Endian::Big, -0.5, 10.0);
        let mut img = image(3);
        assert_eq!(img.edit_cell(&m, 0, CellEdit::Set(12.0)), Ok(12.0));
        assert_eq!(img.bytes[1], 0xFC, "raw -4");
        // raw 127 and -128 are the type's ends; one more step is refused.
        assert_eq!(img.edit_cell(&m, 1, CellEdit::Set(-53.5)), Ok(-53.5));
        assert_eq!(img.bytes[2], 0x7F);
        assert!(
            img.edit_cell(&m, 1, CellEdit::Set(-54.0)).is_err(),
            "raw 128"
        );
        assert!(img.edit_cell(&m, 1, CellEdit::Step(1)).is_err(), "past 127");
        assert_eq!(img.edit_cell(&m, 0, CellEdit::Set(74.0)), Ok(74.0));
        assert_eq!(img.bytes[1], 0x80, "raw -128");
        assert!(
            img.edit_cell(&m, 0, CellEdit::Set(74.5)).is_err(),
            "raw -129"
        );
        assert!(img.edit_cell(&m, 0, CellEdit::Step(-1)).is_err());
        // A raw +1 step lowers the physical value with a negative factor.
        assert_eq!(img.edit_cell(&m, 0, CellEdit::Step(1)), Ok(73.5));
        assert_eq!(img.bytes[1], 0x81);
        assert_eq!(map_values(&img.bytes, &m), Some(vec![73.5, -53.5]));
    }

    #[test]
    fn s32_little_endian_with_a_negative_factor_at_the_type_limits() {
        // phys = raw * -0.25 - 100
        let m = map(DataType::S32, Endian::Little, -0.25, -100.0);
        let mut img = image(9);
        let at_min = i32::MIN as f64 * -0.25 - 100.0;
        let at_max = i32::MAX as f64 * -0.25 - 100.0;
        assert_eq!(img.edit_cell(&m, 0, CellEdit::Set(at_min)), Ok(at_min));
        assert_eq!(&img.bytes[1..5], &[0x00, 0x00, 0x00, 0x80]);
        assert_eq!(img.edit_cell(&m, 1, CellEdit::Set(at_max)), Ok(at_max));
        assert_eq!(&img.bytes[5..9], &[0xFF, 0xFF, 0xFF, 0x7F]);
        let before = img.bytes.clone();
        assert!(img.edit_cell(&m, 0, CellEdit::Set(at_min + 0.25)).is_err());
        assert!(img.edit_cell(&m, 1, CellEdit::Set(at_max - 0.25)).is_err());
        assert!(img.edit_cell(&m, 1, CellEdit::Step(1)).is_err());
        assert_eq!(img.bytes, before, "refused edits change nothing");
        // An ordinary negative raw value, little-endian.
        assert_eq!(img.edit_cell(&m, 0, CellEdit::Set(-99.5)), Ok(-99.5));
        assert_eq!(&img.bytes[1..5], &[0xFE, 0xFF, 0xFF, 0xFF], "raw -2");
        assert_eq!(img.changed_bytes(), 8);
    }

    #[test]
    fn f32_little_endian_with_a_negative_factor() {
        // phys = raw * -2 + 1
        let m = map(DataType::F32, Endian::Little, -2.0, 1.0);
        let mut img = image(9);
        assert_eq!(img.edit_cell(&m, 0, CellEdit::Set(0.0)), Ok(0.0));
        assert_eq!(&img.bytes[1..5], &0.5f32.to_le_bytes(), "raw 0.5");
        // A float steps by 1.0 raw.
        assert_eq!(img.edit_cell(&m, 0, CellEdit::Step(1)), Ok(-2.0));
        assert_eq!(&img.bytes[1..5], &1.5f32.to_le_bytes());
        // Not stored exactly: the stored value is reported.
        let got = img.edit_cell(&m, 1, CellEdit::Set(0.8)).unwrap();
        assert_eq!(got, (0.1f32 as f64) * -2.0 + 1.0);
        // Beyond f32, NaN and infinity are refused.
        let before = img.bytes.clone();
        let huge = f32::MAX as f64 * 4.0;
        assert!(img.edit_cell(&m, 1, CellEdit::Set(-huge)).is_err());
        assert!(img.edit_cell(&m, 1, CellEdit::Set(f64::NAN)).is_err());
        assert!(img.edit_cell(&m, 1, CellEdit::Set(f64::INFINITY)).is_err());
        assert_eq!(img.bytes, before);
        // The largest finite f32 raw is accepted.
        let top = f32::MAX as f64 * -2.0 + 1.0;
        assert!(img.edit_cell(&m, 1, CellEdit::Set(top)).is_ok());
        assert_eq!(&img.bytes[5..9], &f32::MAX.to_le_bytes());
    }

    #[test]
    fn map_limits_are_inclusive_and_exact() {
        // phys = raw * 0.1: the limits are not exactly representable.
        let m = MapDef {
            min: Some(0.5),
            max: Some(25.5),
            ..map(DataType::U8, Endian::Big, 0.1, 0.0)
        };
        let mut img = image(3);
        assert!(img.edit_cell(&m, 0, CellEdit::Set(25.5)).is_ok());
        assert_eq!(img.bytes[1], 255);
        assert!(img.edit_cell(&m, 1, CellEdit::Set(0.5)).is_ok());
        assert_eq!(img.bytes[2], 5);
        assert!(
            img.edit_cell(&m, 1, CellEdit::Step(-1)).is_err(),
            "0.4 < min"
        );
        assert!(img.edit_cell(&m, 0, CellEdit::Set(25.6)).is_err());
        // Rounding to the nearest step can land inside the limits.
        assert!(img.edit_cell(&m, 1, CellEdit::Set(0.46)).is_ok(), "-> 0.5");
        assert!(img.edit_cell(&m, 1, CellEdit::Set(0.44)).is_err(), "-> 0.4");
        // Negative factor: the limits are physical, not raw.
        let n = MapDef {
            min: Some(-200.0),
            max: Some(-10.0),
            ..map(DataType::U8, Endian::Big, -1.0, 0.0)
        };
        let mut img = image(3);
        assert_eq!(img.edit_cell(&n, 0, CellEdit::Set(-200.0)), Ok(-200.0));
        assert_eq!(img.edit_cell(&n, 1, CellEdit::Set(-10.0)), Ok(-10.0));
        assert!(img.edit_cell(&n, 0, CellEdit::Step(1)).is_err(), "-201");
        assert!(img.edit_cell(&n, 1, CellEdit::Step(-1)).is_err(), "-9");
        assert_eq!(img.edit_cell(&n, 1, CellEdit::Step(1)), Ok(-11.0));
    }

    #[test]
    fn undo_after_revert_and_edits_that_change_nothing() {
        let m = map(DataType::U16, Endian::Little, 1.0, 0.0);
        let mut img = Image::from_bytes(PathBuf::from("u.bin"), vec![7, 0x34, 0x12, 0, 0]);
        // Setting the stored value again records no edit.
        assert_eq!(
            img.edit_cell(&m, 0, CellEdit::Set(0x1234 as f64)),
            Ok(4660.0)
        );
        assert!(!img.can_undo() && !img.is_dirty());
        img.edit_cell(&m, 0, CellEdit::Set(1.0)).unwrap();
        img.edit_cell(&m, 1, CellEdit::Set(2.0)).unwrap();
        img.revert();
        assert_eq!(img.undo(), None, "nothing to undo after a revert");
        assert_eq!(img.bytes, vec![7, 0x34, 0x12, 0, 0]);
        assert!(!img.is_dirty());
        // Edit again; undo returns to the file's bytes.
        assert_eq!(
            img.edit_cell(&m, 0, CellEdit::Set(0x12FF as f64)),
            Ok(4863.0)
        );
        assert_eq!(img.changed_bytes(), 1, "only the low byte differs");
        assert!(img.cell_changed(&m, 0) && !img.cell_changed(&m, 1));
        assert_eq!(img.undo(), Some(1));
        assert!(!img.is_dirty() && !img.can_undo());
        // A cell past the image end is refused; it is not a panic.
        let past = MapDef {
            address: 4,
            ..map(DataType::U16, Endian::Little, 1.0, 0.0)
        };
        let e = img.edit_cell(&past, 0, CellEdit::Set(1.0)).unwrap_err();
        assert!(e.contains("past the end"), "{e}");
        assert!(!img.cell_changed(&past, 5), "beyond the image: unchanged");
        // After a save, the old undo history is gone.
        img.edit_cell(&m, 1, CellEdit::Set(9.0)).unwrap();
        let now = img.bytes.clone();
        img.mark_saved(PathBuf::from("u.bin"), now);
        assert_eq!(img.undo(), None);
    }

    #[test]
    fn encode_elem_limits_rounding_and_byte_order() {
        use DataType::*;
        let ok = |raw: f64, dt| encode_elem(raw, dt, Endian::Big).is_ok();
        for (dt, lo, hi) in [
            (U8, 0.0, u8::MAX as f64),
            (S8, i8::MIN as f64, i8::MAX as f64),
            (U16, 0.0, u16::MAX as f64),
            (S16, i16::MIN as f64, i16::MAX as f64),
            (U32, 0.0, u32::MAX as f64),
            (S32, i32::MIN as f64, i32::MAX as f64),
        ] {
            assert!(ok(lo, dt) && ok(hi, dt), "{dt:?} ends");
            assert!(!ok(lo - 1.0, dt) && !ok(hi + 1.0, dt), "{dt:?} beyond");
            // Rounding happens before the range check.
            assert!(ok(hi + 0.49, dt) && !ok(hi + 0.5, dt), "{dt:?} round up");
            assert!(ok(lo - 0.49, dt), "{dt:?} round to lo");
        }
        assert_eq!(encode_elem(-0.4, U8, Endian::Big).unwrap(), vec![0]);
        assert!(encode_elem(f64::INFINITY, U32, Endian::Big).is_err());
        assert!(encode_elem(f64::NEG_INFINITY, F32, Endian::Big).is_err());
        assert!(encode_elem(f32::MAX as f64 * 2.0, F32, Endian::Big).is_err());
        // Byte order of every width.
        assert_eq!(
            encode_elem(-2.0, S32, Endian::Little).unwrap(),
            vec![0xFE, 0xFF, 0xFF, 0xFF]
        );
        assert_eq!(
            encode_elem(0x1234 as f64, U16, Endian::Little).unwrap(),
            vec![0x34, 0x12]
        );
        assert_eq!(
            encode_elem(0x1234 as f64, S16, Endian::Big).unwrap(),
            vec![0x12, 0x34]
        );
        assert_eq!(encode_elem(-1.0, S8, Endian::Little).unwrap(), vec![0xFF]);
        assert_eq!(
            encode_elem(-1.5, F32, Endian::Big).unwrap(),
            (-1.5f32).to_be_bytes().to_vec()
        );
    }

    // -----------------------------------------------------------------------
    // PINNED: refuse, never clamp. Native decodes with the shared codec but
    // must never encode with it (`opentune_hub::maps::encode_elem` clamps).
    // If this test fails, an edit path started clamping: fix the code, do
    // not relax the test.
    // -----------------------------------------------------------------------

    #[test]
    fn refuse_not_clamp_is_pinned() {
        use opentune_hub::maps::encode_elem as hub_encode;
        let one = Scaling {
            factor: 1.0,
            offset: 0.0,
        };
        for (dt, below, above) in [
            (DataType::U8, -1.0, 256.0),
            (DataType::S16, -32769.0, 32768.0),
            (DataType::U16, -1.0, 65536.0),
        ] {
            for en in [Endian::Big, Endian::Little] {
                let m = map(dt, en, 1.0, 0.0);
                let mut img = image(9);
                img.bytes[1..].fill(0x5A);
                img.mark_saved(PathBuf::from("pin.bin"), img.bytes.clone());
                let before = img.bytes.clone();
                for v in [below, above, below - 1000.0, above + 1000.0] {
                    // The shared codec would store a saturated value...
                    let clamped = hub_encode(v, dt, en, one);
                    assert_eq!(clamped.len(), dt.byte_len() as usize);
                    // ...native refuses: one cell, a step, and a batch.
                    let e = img.edit_cell(&m, 0, CellEdit::Set(v)).unwrap_err();
                    assert!(e.contains("does not fit"), "{dt:?} {v}: {e}");
                    let r = img
                        .edit_cells(&m, &[(1, CellEdit::Set(1.0)), (0, CellEdit::Set(v))])
                        .unwrap_err();
                    assert_eq!(r.len(), 1, "{dt:?} {v}");
                    assert_eq!(r[0].idx, 0);
                    assert_eq!(img.bytes, before, "{dt:?} {en:?} {v}: nothing stored");
                    assert!(!img.is_dirty() && !img.can_undo());
                }
                // Stepping past the type's end is refused, not saturated.
                let top = match dt {
                    DataType::U8 => 255.0,
                    DataType::S16 => 32767.0,
                    _ => 65535.0,
                };
                img.edit_cell(&m, 0, CellEdit::Set(top)).unwrap();
                let at_top = img.bytes.clone();
                assert!(img.edit_cell(&m, 0, CellEdit::Step(1)).is_err());
                assert!(img.edit_cells(&m, &[(0, CellEdit::Step(10))]).is_err());
                assert_eq!(img.bytes, at_top);
            }
        }
        // The map's min / max: refused, never clamped to the limit.
        for dt in [DataType::U8, DataType::S16, DataType::U16] {
            let m = MapDef {
                min: Some(10.0),
                max: Some(20.0),
                ..map(dt, Endian::Big, 1.0, 0.0)
            };
            let mut img = image(9);
            img.edit_cell(&m, 0, CellEdit::Set(15.0)).unwrap();
            img.edit_cell(&m, 1, CellEdit::Set(20.0)).unwrap();
            let before = img.bytes.clone();
            for v in [9.0, 21.0, 0.0, 200.0] {
                assert!(
                    img.edit_cell(&m, 0, CellEdit::Set(v)).is_err(),
                    "{dt:?} {v}"
                );
                let r = img
                    .edit_cells(&m, &[(0, CellEdit::Set(v)), (1, CellEdit::Set(v))])
                    .unwrap_err();
                assert_eq!(r.len(), 2, "every refused cell is reported");
            }
            assert!(img.edit_cell(&m, 1, CellEdit::Step(1)).is_err(), "21 > max");
            assert_eq!(img.bytes, before, "{dt:?}: still 15 / 20, not 10 / 20");
            assert_eq!(map_values(&img.bytes, &m).unwrap()[..2], [15.0, 20.0]);
        }
    }

    #[test]
    fn multi_cell_edits_are_one_undo_step_and_redo() {
        let m = map(DataType::U8, Endian::Big, 1.0, 0.0);
        let m = MapDef { cols: 4, ..m };
        let mut img = image(6);
        let g0 = img.generation();
        let n = img
            .edit_cells(
                &m,
                &[
                    (0, CellEdit::Set(1.0)),
                    (1, CellEdit::Set(2.0)),
                    (2, CellEdit::Set(0.0)), // unchanged
                    (3, CellEdit::Step(4)),
                ],
            )
            .unwrap();
        assert_eq!(n, 3);
        assert_eq!(&img.bytes[1..5], &[1, 2, 0, 4]);
        assert!(img.generation() > g0);
        // A repeated cell sees the earlier edit of the same call.
        img.edit_cells(&m, &[(0, CellEdit::Step(1)), (0, CellEdit::Step(1))])
            .unwrap();
        assert_eq!(img.bytes[1], 3);
        // Refused as a whole: the earlier cells of the call roll back too.
        let r = img
            .edit_cells(
                &m,
                &[
                    (0, CellEdit::Step(1)),
                    (0, CellEdit::Step(1)),
                    (9, CellEdit::Set(1.0)),
                ],
            )
            .unwrap_err();
        assert_eq!(r[0].idx, 9);
        assert_eq!(img.bytes[1], 3);
        // Undo walks back one OPERATION at a time; redo replays it.
        assert_eq!(img.undo(), Some(1));
        assert_eq!(&img.bytes[1..5], &[1, 2, 0, 4]);
        assert!(img.can_redo());
        assert_eq!(img.undo(), Some(1));
        assert_eq!(&img.bytes[1..5], &[0, 0, 0, 0]);
        assert!(!img.is_dirty() && !img.can_undo());
        assert_eq!(img.redo(), Some(1));
        assert_eq!(&img.bytes[1..5], &[1, 2, 0, 4]);
        assert!(img.is_dirty() && img.map_changed(&m));
        // A new edit clears the redo history.
        img.edit_cell(&m, 0, CellEdit::Set(9.0)).unwrap();
        assert!(!img.can_redo());
        assert_eq!(img.redo(), None);
        img.revert();
        assert!(!img.can_redo() && !img.map_changed(&m));
    }

    /// The shared codec's shape rule: `rows: 0` (or `cols: 0`) is one row
    /// (column), for decoding AND editing, as in the desktop's write path.
    /// The old local decoder showed no cells and refused every edit (and the
    /// grid could index past the empty value list).
    #[test]
    fn zero_rows_or_cols_is_one_by_the_shared_rule() {
        let m = MapDef {
            rows: 0,
            cols: 3,
            ..map(DataType::U8, Endian::Big, 1.0, 0.0)
        };
        assert_eq!(shape(&m), (1, 3));
        assert_eq!(cell_total(&m), 3);
        assert_eq!(data_len(&m), 3);
        let img_bytes = [9, 1, 2, 3];
        assert_eq!(map_values(&img_bytes, &m), Some(vec![1.0, 2.0, 3.0]));
        let mut img = Image::from_bytes(PathBuf::from("z.bin"), img_bytes.to_vec());
        assert_eq!(img.edit_cell(&m, 2, CellEdit::Set(7.0)), Ok(7.0));
        assert!(img.edit_cell(&m, 3, CellEdit::Set(7.0)).is_err());
        let both = MapDef {
            rows: 0,
            cols: 0,
            ..m
        };
        assert_eq!(map_values(&img_bytes, &both), Some(vec![1.0]));
    }

    /// Axis edge cases where the shared codec differs from the old local
    /// copy; the result is always exactly `n` breakpoints.
    #[test]
    fn axes_follow_the_shared_codec_with_a_fixed_length() {
        let img = [0, 10, 0, 20, 0, 30];
        let stored = AxisDef {
            address: Some(0),
            data_type: DataType::U16,
            endian: Endian::Big,
            ..Default::default()
        };
        // Inline values shorter than the map: the codec prefers them (the old
        // copy fell back to the stored ones); missing breakpoints are indices.
        let short = AxisDef {
            values: vec![5.0],
            ..stored.clone()
        };
        assert_eq!(axis_values(&img, Some(&short), 3), vec![5.0, 1.0, 2.0]);
        // Longer inline values are cut to the map.
        let long = AxisDef {
            values: vec![5.0, 6.0, 7.0, 8.0],
            ..Default::default()
        };
        assert_eq!(axis_values(&img, Some(&long), 2), vec![5.0, 6.0]);
        // A stored axis running past the end: only the unreadable breakpoints
        // become indices (the old copy dropped the whole axis to indices).
        let past = AxisDef {
            address: Some(2),
            ..stored.clone()
        };
        assert_eq!(axis_values(&img, Some(&past), 3), vec![20.0, 30.0, 2.0]);
        // A shared axis (`axis_ref`) shows indices.
        let shared = AxisDef {
            axis_ref: Some("rpm".into()),
            ..Default::default()
        };
        assert_eq!(axis_values(&img, Some(&shared), 2), vec![0.0, 1.0]);
        assert!(axis_values(&img, None, 0).is_empty());
    }

    /// Native's decode IS the shared codec's: the same bytes give the same
    /// physical grid as `opentune_hub::maps::decode_cells` for every type.
    #[test]
    fn decoding_matches_the_shared_codec() {
        let bytes: Vec<u8> = (0..64u32).map(|i| (i * 37 + 11) as u8).collect();
        for dt in [
            DataType::U8,
            DataType::S8,
            DataType::U16,
            DataType::S16,
            DataType::U32,
            DataType::S32,
            DataType::F32,
        ] {
            for en in [Endian::Big, Endian::Little] {
                let m = MapDef {
                    rows: 2,
                    cols: 3,
                    ..map(dt, en, 0.25, -3.0)
                };
                let want: Vec<f64> = opentune_hub::maps::decode_cells(&bytes, &m)
                    .unwrap()
                    .concat();
                let got = map_values(&bytes, &m).unwrap();
                let same = want
                    .iter()
                    .zip(&got)
                    .all(|(a, b)| a == b || (a.is_nan() && b.is_nan()));
                assert!(same && want.len() == got.len(), "{dt:?} {en:?}");
            }
        }
        assert_eq!(
            read_elem(&[1, 2], usize::MAX, DataType::U16, Endian::Big),
            None
        );
    }
}
