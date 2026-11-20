//! A loaded flash image and the thin map-decoding helpers the prototype needs.
//!
//! These mirror what the Tauri backend does in `desktop/src-tauri/src/maps.rs`
//! (raw element read by data type + byte order, then `Scaling::to_physical`).
//! They are deliberately small and local so the webview app stays untouched;
//! the plan is to move the shared logic into a core crate both frontends use.

use opentune_core::ecudef::{AxisDef, DataType, Endian, MapDef, Scaling};
use std::collections::BTreeSet;
use std::path::PathBuf;

/// An opened dump: the working bytes (with any unsaved edits) plus what the
/// file holds on disk, so edits can be counted, undone and reverted.
pub struct Image {
    pub path: PathBuf,
    /// Working copy (what every view shows).
    pub bytes: Vec<u8>,
    saved: Vec<u8>,
    /// Offsets whose working byte differs from the saved one.
    changed: BTreeSet<usize>,
    /// One entry per edit: the bytes it replaced.
    undo: Vec<Vec<(usize, u8)>>,
}

/// A cell change: a physical value, or a number of raw steps (±1 LSB).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CellEdit {
    Set(f64),
    Step(i32),
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

    /// Does any byte of cell `idx` of `m` differ from the file on disk?
    pub fn cell_changed(&self, m: &MapDef, idx: usize) -> bool {
        let w = m.data_type.byte_len() as usize;
        let off = m.address as usize + idx * w;
        self.changed.range(off..off + w).next().is_some()
    }

    /// Change one cell of `m`. The value is converted to the stored raw
    /// number (integers round to the nearest step), refused when it does not
    /// fit the data type or the map's `min`/`max`. Returns the physical value
    /// actually stored.
    pub fn edit_cell(&mut self, m: &MapDef, idx: usize, e: CellEdit) -> Result<f64, String> {
        if idx >= m.cell_count() as usize {
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
        if self.bytes[off..off + w] == enc[..] {
            return Ok(phys);
        }
        let old: Vec<(usize, u8)> = (off..off + w).map(|o| (o, self.bytes[o])).collect();
        self.bytes[off..off + w].copy_from_slice(&enc);
        self.undo.push(old);
        for o in off..off + w {
            self.track(o);
        }
        Ok(phys)
    }

    fn track(&mut self, o: usize) {
        if self.bytes[o] != self.saved[o] {
            self.changed.insert(o);
        } else {
            self.changed.remove(&o);
        }
    }

    /// Undo the last edit. Returns the first offset it touched.
    pub fn undo(&mut self) -> Option<usize> {
        let old = self.undo.pop()?;
        for &(o, b) in &old {
            self.bytes[o] = b;
            self.track(o);
        }
        old.first().map(|(o, _)| *o)
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    /// Drop every unsaved edit.
    pub fn revert(&mut self) {
        self.bytes.clone_from(&self.saved);
        self.changed.clear();
        self.undo.clear();
    }

    /// `bytes` now are what `path` holds on disk (after a save).
    pub fn mark_saved(&mut self, path: PathBuf, bytes: Vec<u8>) {
        *self = Self::from_bytes(path, bytes);
    }

    pub fn name(&self) -> String {
        self.path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

/// One stored element as a raw number, or `None` past the end of the image.
pub fn read_elem(img: &[u8], off: usize, dt: DataType, en: Endian) -> Option<f64> {
    let w = dt.byte_len() as usize;
    let b = img.get(off..off.checked_add(w)?)?;
    let raw = match en {
        Endian::Big => b.iter().fold(0u32, |a, &x| (a << 8) | x as u32),
        Endian::Little => b.iter().rev().fold(0u32, |a, &x| (a << 8) | x as u32),
    };
    Some(match dt {
        DataType::U8 => raw as u8 as f64,
        DataType::S8 => raw as u8 as i8 as f64,
        DataType::U16 => raw as u16 as f64,
        DataType::S16 => raw as u16 as i16 as f64,
        DataType::U32 => raw as f64,
        DataType::S32 => raw as i32 as f64,
        DataType::F32 => f32::from_bits(raw) as f64,
    })
}

/// The raw number a physical value is stored as (before rounding), or
/// `None` when the scaling cannot be inverted.
pub fn to_raw(s: &Scaling, phys: f64) -> Option<f64> {
    let r = (phys - s.offset) / s.factor;
    (s.factor != 0.0 && phys.is_finite() && r.is_finite()).then_some(r)
}

/// Encode a raw number as stored bytes: integers round to nearest and must
/// fit the type (never wrapped or clamped).
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

/// A map's cells as physical values, row-major (`r * cols + c`), or `None`
/// when the map runs past the end of the image.
pub fn map_values(img: &[u8], m: &MapDef) -> Option<Vec<f64>> {
    let w = m.data_type.byte_len() as usize;
    (0..m.cell_count() as usize)
        .map(|i| {
            read_elem(img, m.address as usize + i * w, m.data_type, m.endian)
                .map(|r| m.scaling.to_physical(r))
        })
        .collect()
}

/// Axis breakpoints for `n` cells: inline values, values stored in the image,
/// or plain indices when the axis is absent, shared (`axis_ref`) or unreadable.
pub fn axis_values(img: &[u8], axis: Option<&AxisDef>, n: usize) -> Vec<f64> {
    let index = || (0..n).map(|i| i as f64).collect();
    let Some(a) = axis else { return index() };
    if a.values.len() >= n && n > 0 {
        return a.values[..n].to_vec();
    }
    if let Some(addr) = a.address {
        let w = a.data_type.byte_len() as usize;
        let v: Option<Vec<f64>> = (0..n)
            .map(|i| {
                read_elem(img, addr as usize + i * w, a.data_type, a.endian)
                    .map(|r| a.scaling.to_physical(r))
            })
            .collect();
        if let Some(v) = v {
            return v;
        }
    }
    index()
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
}
