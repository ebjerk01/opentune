//! A loaded flash image and the thin map-decoding helpers the prototype needs.
//!
//! These mirror what the Tauri backend does in `desktop/src-tauri/src/maps.rs`
//! (raw element read by data type + byte order, then `Scaling::to_physical`).
//! They are deliberately small and local so the webview app stays untouched;
//! the plan is to move the shared logic into a core crate both frontends use.

use opentune_core::ecudef::{AxisDef, DataType, Endian, MapDef};
use std::path::PathBuf;

/// An opened dump.
pub struct Image {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
}

impl Image {
    pub fn open(path: PathBuf) -> std::io::Result<Self> {
        let bytes = std::fs::read(&path)?;
        Ok(Self { path, bytes })
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
