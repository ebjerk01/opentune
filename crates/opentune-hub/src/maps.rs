//! Map cell codecs: decode a `MapDef`'s cells and axes out of an image into
//! physical values, and encode one physical value back into bytes (inverse
//! scaling, rounded and clamped to the data type).

use opentune_core::ecudef::{self, AxisDef, DataType, Endian, MapDef};
use serde_json::{json, Value};

pub fn read_elem(img: &[u8], off: usize, dt: DataType, en: Endian) -> Option<f64> {
    let w = dt.byte_len() as usize;
    if off + w > img.len() {
        return None;
    }
    let s = &img[off..off + w];
    let le = matches!(en, Endian::Little);
    Some(match dt {
        DataType::U8 => s[0] as f64,
        DataType::S8 => (s[0] as i8) as f64,
        DataType::U16 => {
            let v = if le {
                u16::from_le_bytes([s[0], s[1]])
            } else {
                u16::from_be_bytes([s[0], s[1]])
            };
            v as f64
        }
        DataType::S16 => {
            let v = if le {
                i16::from_le_bytes([s[0], s[1]])
            } else {
                i16::from_be_bytes([s[0], s[1]])
            };
            v as f64
        }
        DataType::U32 => {
            let a = [s[0], s[1], s[2], s[3]];
            (if le {
                u32::from_le_bytes(a)
            } else {
                u32::from_be_bytes(a)
            }) as f64
        }
        DataType::S32 => {
            let a = [s[0], s[1], s[2], s[3]];
            (if le {
                i32::from_le_bytes(a)
            } else {
                i32::from_be_bytes(a)
            }) as f64
        }
        DataType::F32 => {
            let a = [s[0], s[1], s[2], s[3]];
            (if le {
                f32::from_le_bytes(a)
            } else {
                f32::from_be_bytes(a)
            }) as f64
        }
    })
}

/// Decode one axis to physical breakpoint values. Supports inline `values`,
/// stored (`address` + type), or falls back to 0..count indices (shared/ref).
pub fn decode_axis(img: &[u8], axis: &Option<AxisDef>, count: u32) -> (Vec<f64>, String) {
    let n = count.max(1) as usize;
    match axis {
        Some(a) if !a.values.is_empty() => (a.values.clone(), a.unit.clone()),
        Some(a) if a.address.is_some() => {
            let addr = a.address.unwrap() as usize;
            let w = a.data_type.byte_len() as usize;
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                match read_elem(img, addr + i * w, a.data_type, a.endian) {
                    Some(raw) => out.push(a.scaling.to_physical(raw)),
                    None => out.push(i as f64),
                }
            }
            (out, a.unit.clone())
        }
        Some(a) => ((0..n).map(|i| i as f64).collect(), a.unit.clone()),
        None => ((0..n).map(|i| i as f64).collect(), String::new()),
    }
}

/// Decode a defined `MapDef` out of `img` into the read-only viewer/editor JSON
/// payload (physical-value grid + axes + scaling + bounds). Shared by the
/// definition-backed `decode_map` and the ad-hoc `decode_mapdef` paths so
/// an imported / promoted map renders identically to a bundled one.
///
/// `flash_kib` is only used to enrich the "image too small" error for a
/// definition-backed map (where we know the ECU's flash size); `None` for an
/// ad-hoc map.
pub fn decode_map_value(img: &[u8], map: &MapDef, flash_kib: Option<u32>) -> Result<Value, String> {
    let rows = map.rows.max(1) as usize;
    let cols = map.cols.max(1) as usize;
    let w = map.data_type.byte_len() as usize;
    let need = map.address as usize + rows * cols * w;
    if need > img.len() {
        let hint = match flash_kib {
            Some(kib) => format!(" — open a full dump for this ECU (its flash is {kib} KiB)"),
            None => " — open a full dump this map fits in".to_string(),
        };
        return Err(format!(
            "map needs 0x{:08X} bytes but the opened image is only 0x{:08X}{hint}",
            need,
            img.len(),
        ));
    }

    let mut cells: Vec<Vec<f64>> = Vec::with_capacity(rows);
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for r in 0..rows {
        let mut row = Vec::with_capacity(cols);
        for c in 0..cols {
            let idx = r * cols + c;
            let off = map.address as usize + idx * w;
            let phys = read_elem(img, off, map.data_type, map.endian)
                .map(|raw| map.scaling.to_physical(raw))
                .unwrap_or(0.0);
            lo = lo.min(phys);
            hi = hi.max(phys);
            row.push(phys);
        }
        cells.push(row);
    }
    let (x_vals, x_unit) = decode_axis(img, &map.x_axis, map.cols);
    let (y_vals, y_unit) = decode_axis(img, &map.y_axis, map.rows);

    Ok(json!({
        "name": map.name,
        "address": format!("0x{:08X}", map.address),
        "rows": rows,
        "cols": cols,
        "unit": map.unit,
        "data_type": format!("{:?}", map.data_type),
        "endian": format!("{:?}", map.endian),
        "scaling": { "factor": map.scaling.factor, "offset": map.scaling.offset },
        "min": lo.is_finite().then_some(lo),
        "max": hi.is_finite().then_some(hi),
        "def_min": map.min,
        "def_max": map.max,
        "cells": cells,
        "x_axis": { "name": map.x_axis.as_ref().map(|a| a.name.clone()), "unit": x_unit, "values": x_vals },
        "y_axis": { "name": map.y_axis.as_ref().map(|a| a.name.clone()), "unit": y_unit, "values": y_vals },
        "notes": map.notes,
    }))
}

/// Round to nearest and clamp into `[lo, hi]`. Used before narrowing a raw value
/// to an integer data type so an out-of-range edit can never wrap around.
fn clamp_round(raw: f64, lo: f64, hi: f64) -> f64 {
    let r = if raw.is_finite() { raw.round() } else { 0.0 };
    r.clamp(lo, hi)
}

/// Inverse of [`read_elem`] for one cell: physical value → raw → bytes, honouring
/// the data type, endianness and linear scaling. For integer types the raw value
/// is rounded to nearest and clamped to the type's representable range (never
/// wraps); floats are stored as `f32`. The caller must guarantee a non-zero
/// `scaling.factor`. Returns exactly `dt.byte_len()` bytes.
pub fn encode_elem(phys: f64, dt: DataType, en: Endian, scaling: ecudef::Scaling) -> Vec<u8> {
    let le = matches!(en, Endian::Little);
    let raw = (phys - scaling.offset) / scaling.factor;
    match dt {
        DataType::U8 => vec![clamp_round(raw, 0.0, u8::MAX as f64) as u8],
        DataType::S8 => vec![clamp_round(raw, i8::MIN as f64, i8::MAX as f64) as i8 as u8],
        DataType::U16 => {
            let v = clamp_round(raw, 0.0, u16::MAX as f64) as u16;
            (if le { v.to_le_bytes() } else { v.to_be_bytes() }).to_vec()
        }
        DataType::S16 => {
            let v = clamp_round(raw, i16::MIN as f64, i16::MAX as f64) as i16;
            (if le { v.to_le_bytes() } else { v.to_be_bytes() }).to_vec()
        }
        DataType::U32 => {
            let v = clamp_round(raw, 0.0, u32::MAX as f64) as u32;
            (if le { v.to_le_bytes() } else { v.to_be_bytes() }).to_vec()
        }
        DataType::S32 => {
            let v = clamp_round(raw, i32::MIN as f64, i32::MAX as f64) as i32;
            (if le { v.to_le_bytes() } else { v.to_be_bytes() }).to_vec()
        }
        DataType::F32 => {
            let v = raw as f32;
            (if le { v.to_le_bytes() } else { v.to_be_bytes() }).to_vec()
        }
    }
}

/// Decode every cell of a `MapDef` from `img` into a physical-value grid, or
/// `None` if the map runs past the end of the image (shared by compare).
pub fn decode_cells(img: &[u8], map: &MapDef) -> Option<Vec<Vec<f64>>> {
    let rows = map.rows.max(1) as usize;
    let cols = map.cols.max(1) as usize;
    let w = map.data_type.byte_len() as usize;
    let mut out = Vec::with_capacity(rows);
    for r in 0..rows {
        let mut row = Vec::with_capacity(cols);
        for c in 0..cols {
            let off = map.address as usize + (r * cols + c) * w;
            let raw = read_elem(img, off, map.data_type, map.endian)?;
            row.push(map.scaling.to_physical(raw));
        }
        out.push(row);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentune_core::ecudef::Scaling;

    fn sc(factor: f64, offset: f64) -> Scaling {
        Scaling { factor, offset }
    }

    #[test]
    fn encode_read_round_trip_u16_be() {
        let scaling = sc(0.01, 0.0);
        // phys 50.0 -> raw 5000 = 0x1388, big-endian.
        let bytes = encode_elem(50.0, DataType::U16, Endian::Big, scaling);
        assert_eq!(bytes, vec![0x13, 0x88]);
        let back = read_elem(&bytes, 0, DataType::U16, Endian::Big).unwrap();
        assert!((scaling.to_physical(back) - 50.0).abs() < 1e-9);
    }

    #[test]
    fn encode_rounds_to_nearest() {
        let scaling = sc(1.0, 0.0);
        assert_eq!(
            encode_elem(10.4, DataType::U8, Endian::Big, scaling),
            vec![10]
        );
        assert_eq!(
            encode_elem(10.6, DataType::U8, Endian::Big, scaling),
            vec![11]
        );
    }

    #[test]
    fn encode_clamps_to_type_range() {
        let scaling = sc(1.0, 0.0);
        // Over max / under min must saturate, never wrap.
        assert_eq!(
            encode_elem(999.0, DataType::U8, Endian::Big, scaling),
            vec![255]
        );
        assert_eq!(
            encode_elem(-5.0, DataType::U8, Endian::Big, scaling),
            vec![0]
        );
        assert_eq!(
            encode_elem(-200.0, DataType::S8, Endian::Big, scaling),
            vec![(-128i8) as u8]
        );
        assert_eq!(
            encode_elem(5000.0, DataType::S8, Endian::Big, scaling),
            vec![127u8]
        );
    }

    #[test]
    fn encode_s16_little_endian_round_trip() {
        let scaling = sc(1.0, 0.0);
        let bytes = encode_elem(-2.0, DataType::S16, Endian::Little, scaling);
        assert_eq!(bytes, vec![0xFE, 0xFF]); // -2 i16 LE
        let back = read_elem(&bytes, 0, DataType::S16, Endian::Little).unwrap();
        assert_eq!(back, -2.0);
    }

    #[test]
    fn encode_f32_with_offset_and_factor() {
        let scaling = sc(2.0, 1.0);
        // phys 5.0 -> raw (5-1)/2 = 2.0.
        let bytes = encode_elem(5.0, DataType::F32, Endian::Big, scaling);
        let back = read_elem(&bytes, 0, DataType::F32, Endian::Big).unwrap();
        assert!((scaling.to_physical(back) - 5.0).abs() < 1e-6);
    }

    #[test]
    fn decode_cells_round_trips_through_encode() {
        let map = MapDef {
            name: "t".into(),
            address: 0,
            data_type: DataType::U16,
            endian: Endian::Big,
            rows: 2,
            cols: 2,
            scaling: sc(0.1, 0.0),
            ..Default::default()
        };
        let grid = [vec![1.0, 2.5], vec![3.0, 4.5]];
        let w = map.data_type.byte_len() as usize;
        let mut img = vec![0u8; 2 * 2 * w];
        for (r, row) in grid.iter().enumerate() {
            for (c, &cell) in row.iter().enumerate() {
                let off = (r * 2 + c) * w;
                let b = encode_elem(cell, map.data_type, map.endian, map.scaling);
                img[off..off + w].copy_from_slice(&b);
            }
        }
        let got = decode_cells(&img, &map).expect("in range");
        for r in 0..2 {
            for c in 0..2 {
                assert!((got[r][c] - grid[r][c]).abs() < 1e-6);
            }
        }
    }

    #[test]
    fn decode_cells_none_when_out_of_range() {
        let map = MapDef {
            name: "t".into(),
            address: 0,
            data_type: DataType::U16,
            endian: Endian::Big,
            rows: 4,
            cols: 4,
            scaling: sc(1.0, 0.0),
            ..Default::default()
        };
        // Image too small for a 4x4 u16 map (needs 32 bytes).
        let img = vec![0u8; 8];
        assert!(decode_cells(&img, &map).is_none());
    }
}
