//! Raw bin → named maps: import XDF / A2L definitions and promote a found region
//! to a map definition.

use crate::common::parse_u32;
use opentune_core::ecudef::{self, DataType, Endian, MapDef};
use serde_json::{json, Value};
use std::path::Path;

fn parse_data_type(s: &str) -> Result<DataType, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "u8" => Ok(DataType::U8),
        "s8" | "i8" => Ok(DataType::S8),
        "u16" => Ok(DataType::U16),
        "s16" | "i16" => Ok(DataType::S16),
        "u32" => Ok(DataType::U32),
        "s32" | "i32" => Ok(DataType::S32),
        "f32" | "float" => Ok(DataType::F32),
        other => Err(format!(
            "unknown data type '{other}' (use u8/s8/u16/s16/u32/s32/f32)"
        )),
    }
}

fn parse_endian(s: &str) -> Result<Endian, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "big" | "be" | "motorola" => Ok(Endian::Big),
        "little" | "le" | "intel" => Ok(Endian::Little),
        other => Err(format!("unknown endianness '{other}' (use big/little)")),
    }
}

/// Serialize a `MapDef` plus a few derived display fields for the import list.
fn mapdef_summary(m: &MapDef) -> Value {
    let mut v = serde_json::to_value(m).unwrap_or(Value::Null);
    if let Value::Object(ref mut o) = v {
        o.insert("address_hex".into(), json!(format!("0x{:08X}", m.address)));
        o.insert("byte_len".into(), json!(m.byte_len()));
        o.insert(
            "shape".into(),
            json!(if m.rows <= 1 || m.cols <= 1 {
                "1D/scalar"
            } else {
                "2D"
            }),
        );
    }
    v
}

/// Import a TunerPro **XDF** or ASAM **A2L** definition into a list of MapDefs
/// the user can pick from and open in the editor (via [`decode_mapdef`]). Format
/// is sniffed from the extension then the content. For A2L, `base` optionally
/// rebases CPU addresses to file offsets (`addr -= base`), mirroring the CLI.
#[tauri::command]
pub fn import_definitions(path: String, base: Option<String>) -> Result<Value, String> {
    let raw = std::fs::read(&path).map_err(|e| e.to_string())?;
    let src = String::from_utf8_lossy(&raw);
    let ext = Path::new(&path)
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();

    let head = src.trim_start_matches(['\u{feff}', ' ', '\t', '\r', '\n']);
    let looks_a2l = ext == "a2l"
        || head.contains("ASAP2")
        || head.contains("/begin PROJECT")
        || head.contains("/begin MODULE");
    let looks_xdf = ext == "xdf"
        || head.starts_with("XDF")
        || head.contains("<XDFFORMAT")
        || head.contains("%%TABLE%%");

    if looks_a2l && !looks_xdf {
        let import = opentune_a2l::parse_a2l(&src).map_err(|e| e.to_string())?;
        let mut maps = import.maps;
        let base_val = match base.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(s) => Some(parse_u32(s)?),
            None => None,
        };
        if let Some(b) = base_val {
            for m in &mut maps {
                m.address = m.address.wrapping_sub(b);
                if let Some(ax) = m.x_axis.as_mut() {
                    if let Some(a) = ax.address.as_mut() {
                        *a = a.wrapping_sub(b);
                    }
                }
                if let Some(ax) = m.y_axis.as_mut() {
                    if let Some(a) = ax.address.as_mut() {
                        *a = a.wrapping_sub(b);
                    }
                }
            }
        }
        Ok(json!({
            "kind": "a2l",
            "project": import.project,
            "module": import.module,
            "base": base_val,
            "count": maps.len(),
            "maps": maps.iter().map(mapdef_summary).collect::<Vec<_>>(),
        }))
    } else if looks_xdf {
        let import = opentune_xdf::parse_xdf(&src).map_err(|e| e.to_string())?;
        Ok(json!({
            "kind": "xdf",
            "title": import.title,
            "base_offset": import.base_offset,
            "count": import.maps.len(),
            "maps": import.maps.iter().map(mapdef_summary).collect::<Vec<_>>(),
        }))
    } else {
        Err("unrecognised definition file — expected a TunerPro XDF or an ASAM A2L".into())
    }
}

/// Turn a findmaps-discovered region (or a manual selection) into a named
/// `MapDef` the editor can open. Validates dimensions, scaling and that the
/// region fits inside the image, then returns the MapDef for [`decode_mapdef`].
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn promote_region(
    path: String,
    start: String,
    len: Option<u64>,
    name: String,
    rows: u32,
    cols: u32,
    data_type: String,
    endian: String,
    factor: f64,
    offset: f64,
) -> Result<Value, String> {
    let address = parse_u32(&start)?;
    let dt = parse_data_type(&data_type)?;
    let en = parse_endian(&endian)?;
    let rows = rows.max(1);
    let cols = cols.max(1);
    if factor == 0.0 {
        return Err("scaling factor cannot be 0 (physical→raw would divide by zero)".into());
    }
    let name = name.trim();
    if name.is_empty() {
        return Err("give the map a name".into());
    }

    let span = (rows as u64) * (cols as u64) * (dt.byte_len() as u64);
    let file_len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let need = address as u64 + span;
    if need > file_len {
        return Err(format!(
            "a {rows}×{cols} {data_type} map at 0x{address:08X} needs 0x{need:08X} bytes but the image is only 0x{file_len:08X}"
        ));
    }

    let map = MapDef {
        name: name.to_string(),
        address,
        data_type: dt,
        endian: en,
        rows,
        cols,
        scaling: ecudef::Scaling { factor, offset },
        notes: match len {
            Some(l) if l != span => format!(
                "promoted from a {l}-byte region; the {rows}×{cols} {data_type} grid spans {span} bytes"
            ),
            _ => "promoted from a selected region".to_string(),
        },
        ..Default::default()
    };
    serde_json::to_value(&map).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_types_and_endian() {
        assert_eq!(parse_data_type("u16").unwrap(), DataType::U16);
        assert_eq!(parse_data_type("S8").unwrap(), DataType::S8);
        assert_eq!(parse_data_type("f32").unwrap(), DataType::F32);
        assert!(parse_data_type("u24").is_err());
        assert_eq!(parse_endian("big").unwrap(), Endian::Big);
        assert_eq!(parse_endian("LE").unwrap(), Endian::Little);
        assert!(parse_endian("mixed").is_err());
    }
}
