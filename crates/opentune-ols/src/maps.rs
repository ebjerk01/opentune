//! Map (table) definitions carried inside a WinOLS 5 project, and their
//! conversion to OpenTune [`MapDef`]s.
//!
//! Only the WinOLS 5 record layout (container format version >= 800) is read.
//! Older WinOLS 4.x containers lay their records out differently and are not
//! decoded here: [`parse_maps`] returns an empty list for them and the ROM
//! import still works. The field walk below was reimplemented from the
//! structure documented by the GPL-3.0 ZedSuite and romHEX14 readers; it keeps
//! its own decomposition and validates every record (name, grid, data range,
//! cell type) before trusting it, so a layout this code does not recognise
//! yields nothing rather than a fabricated map.

use crate::reader::Cursor;
use opentune_core::ecudef::{AxisDef, DataType, Endian, MapDef, Scaling};

/// Smallest container format version whose record layout this module reads.
const MIN_FORMAT_VERSION: u32 = 800;
/// Largest grid dimension accepted (rejects misread records).
const MAX_DIM: u32 = 512;
/// Largest ROM a record may address into (16 MiB).
const MAX_ROM_LEN: u32 = 0x0100_0000;
/// Upper bound on a folder-reference entry count (misread guard).
const MAX_FOLDER_ENTRIES: u32 = 0x1000;
/// Upper bound on an axis's inline-point byte count (misread guard).
const MAX_INLINE_BYTES: u32 = 0x0001_0000;

/// A WinOLS cell/axis storage type, mapped to OpenTune's `(DataType, Endian)`.
///
/// WinOLS codes: `1/8/9` = 8-bit; `2` = 16-bit big-endian, `3` = 16-bit little;
/// `4` = 32-bit big, `5` = 32-bit little; `6` = float big, `7` = float little;
/// `10..=13` = 64-bit. OpenTune has no 64-bit calibration type, so those (and
/// any unknown code) return `None` and the caller skips the map with a warning
/// rather than silently narrowing a value.
fn decode_type(code: u32, signed: bool) -> Option<(DataType, Endian)> {
    let (base, endian) = match code {
        1 | 8 | 9 => (DataType::U8, Endian::Big),
        2 => (DataType::U16, Endian::Big),
        3 => (DataType::U16, Endian::Little),
        4 => (DataType::U32, Endian::Big),
        5 => (DataType::U32, Endian::Little),
        6 => return Some((DataType::F32, Endian::Big)),
        7 => return Some((DataType::F32, Endian::Little)),
        _ => return None,
    };
    let typed = if signed {
        match base {
            DataType::U8 => DataType::S8,
            DataType::U16 => DataType::S16,
            DataType::U32 => DataType::S32,
            other => other,
        }
    } else {
        base
    };
    Some((typed, endian))
}

/// Byte width of a WinOLS cell type (matches [`decode_type`]'s widths, plus the
/// 64-bit codes, so the grid/size validation can run before we decide whether
/// OpenTune can store the type).
fn cell_bytes(code: u32) -> u32 {
    match code {
        2 | 3 => 2,
        4..=7 => 4,
        10..=13 => 8,
        _ => 1,
    }
}

/// A raw axis as stored in a map record, before conversion to [`AxisDef`].
#[derive(Debug, Clone, Default)]
struct RawAxis {
    name: String,
    unit: String,
    factor: f64,
    offset: f64,
    /// `0` / `0xFFFF_FFFF` means the axis has no stored breakpoints.
    address: u32,
    type_code: u32,
    signed: bool,
}

impl RawAxis {
    fn has_address(&self) -> bool {
        self.address != 0 && self.address != 0xFFFF_FFFF
    }
}

/// A raw map record, before conversion to [`MapDef`].
#[derive(Debug, Clone, Default)]
struct RawMap {
    name: String,
    comment: String,
    unit: String,
    rows: u32,
    cols: u32,
    type_code: u32,
    signed: bool,
    factor: f64,
    offset: f64,
    /// First byte of the map values, as an image-relative offset.
    start: u32,
    x: RawAxis,
    y: RawAxis,
}

/// A folder-reference field: a name string, two words, then a counted list of
/// `(string, word)` entries. Only the name is kept.
fn folder_ref(c: &mut Cursor) -> Option<String> {
    let name = c.winols5_string()?;
    c.u32()?;
    c.u32()?;
    let count = c.u32()?;
    if count > MAX_FOLDER_ENTRIES {
        return None;
    }
    for _ in 0..count {
        c.winols5_string()?;
        c.u32()?;
    }
    Some(name)
}

/// Read one axis sub-record.
fn read_axis(c: &mut Cursor) -> Option<RawAxis> {
    let name = folder_ref(c)?;
    let unit = c.winols5_string()?;
    let factor = c.f64()?;
    let offset = c.f64()?;
    c.u32()?; // axis kind
    let address = c.u32()?;
    let type_code = c.u32()?;
    c.u32()?;
    c.u32()?; // declared cell bits
    c.u16()?; // "backwards" flag — direction only, not needed for a MapDef
    c.skip(8)?;
    c.skip(4)?;
    c.skip(4)?;
    let signed = c.u8()? != 0;
    let inline = c.u32()?;
    if inline > MAX_INLINE_BYTES {
        return None;
    }
    c.skip(inline as usize)?;
    c.u32()?;
    c.u32()?; // axis identifier header word
    c.winols5_string()?; // axis id-name
    let folders = c.u32()?;
    if folders > MAX_FOLDER_ENTRIES {
        return None;
    }
    for _ in 0..folders {
        folder_ref(c)?;
    }
    c.u32()?;
    Some(RawAxis {
        name,
        unit,
        factor,
        offset,
        address,
        type_code,
        signed,
    })
}

/// Read one map record, starting at the comment field (just after the
/// `0xFFFF_FFFF` record introducer). Returns the record and the cursor offset
/// just past its second axis, or `None` if any field is implausible.
fn read_record(data: &[u8], at: usize) -> Option<(RawMap, usize)> {
    let mut c = Cursor::new(data, at);
    let comment = folder_ref(&mut c)?;
    c.u8()?;
    let name = c.winols5_string()?;
    if name.trim().is_empty() || name.len() > 120 {
        return None;
    }
    c.u32()?;
    c.u32()?;
    c.u32()?;
    c.u32()?; // kennfeld type
    c.u32()?;
    let type_code = c.u32()?;
    c.u32()?;
    c.u32()?; // declared cell bits
    c.u32()?;
    c.winols5_string()?; // id-name
    c.u32()?;
    c.u32()?;
    c.u32()?;
    c.u8()?;
    c.u32()?;
    c.skip(6 * 8)?;
    c.skip(6 * 8)?;
    c.u8()?; // cell inverse
    let signed = c.u8()? != 0;
    c.u8()?;
    c.u8()?;
    // The record states columns (X points) then rows (Y points); the two axis
    // sub-records further down are in the same X-then-Y order.
    let cols = c.u32()?;
    let rows = c.u32()?;
    c.u32()?;
    c.u32()?;
    c.u32()?; // precision
    c.winols5_string()?; // label
    let unit = c.winols5_string()?;
    let factor = c.f64()?;
    let offset = c.f64()?;
    let start = c.u32()?;
    let end = c.u32()?;
    let base = c.u32()?;
    c.f64()?;
    for _ in 0..5 {
        c.u32()?;
    }

    // Plausibility: the block must describe a grid-sized region inside the ROM.
    if !(1..=MAX_DIM).contains(&rows) || !(1..=MAX_DIM).contains(&cols) {
        return None;
    }
    if !(1..=13).contains(&type_code) {
        return None;
    }
    if start >= end || end > base || base > MAX_ROM_LEN {
        return None;
    }
    let cells = u64::from(rows) * u64::from(cols);
    if u64::from(end - start) != cells * u64::from(cell_bytes(type_code)) {
        return None;
    }

    let x = read_axis(&mut c)?;
    let y = read_axis(&mut c)?;
    let record = RawMap {
        name,
        comment,
        unit,
        rows,
        cols,
        type_code,
        signed,
        factor,
        offset,
        start,
        x,
        y,
    };
    Some((record, c.pos()))
}

/// Read the format version straight from the header (kept local so this module
/// can gate itself without depending on the container module's `Header`).
fn format_version(data: &[u8]) -> Option<u32> {
    if data.len() < 24 || &data[4..15] != b"WinOLS File" || data[15] != 0 {
        return None;
    }
    if u32::from_le_bytes([data[0], data[1], data[2], data[3]]) != 0x0000_000B {
        return None;
    }
    Some(u32::from_le_bytes([data[16], data[17], data[18], data[19]]))
}

/// All map records in the project, scanned in file order. `region_end` bounds
/// the scan (pass the version-data start so the ROM image itself is never
/// scanned as records). Returns the raw records; conversion / warnings happen
/// in [`to_map_defs`].
fn parse_raw_maps(data: &[u8], region_end: usize) -> Vec<RawMap> {
    let Some(version) = format_version(data) else {
        return Vec::new();
    };
    if version < MIN_FORMAT_VERSION {
        return Vec::new();
    }
    let end = region_end.min(data.len());
    let mut maps = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut pos = 24usize;
    while pos + 8 < end {
        let introducer = &data[pos..pos + 4];
        if introducer == [0xFF, 0xFF, 0xFF, 0xFF] {
            let at = pos + 4;
            let len = i32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]);
            if (-3..=crate::reader::MAX_STRING_LEN as i32).contains(&len) {
                if let Some((map, after)) = read_record(data, at) {
                    if seen.insert((map.start, map.name.clone())) {
                        maps.push(map);
                    }
                    pos = after;
                    continue;
                }
            }
        }
        pos += 1;
    }
    maps
}

/// Parse the project's maps and convert them to OpenTune [`MapDef`]s.
///
/// Returns the converted maps plus a list of per-map warnings for anything that
/// could not be represented faithfully (a 64-bit cell type, an axis whose
/// stored type OpenTune cannot encode, a zero scaling factor). A map is skipped
/// — never fabricated — when its cell type has no OpenTune equivalent.
pub fn to_map_defs(data: &[u8], region_end: usize) -> (Vec<MapDef>, Vec<String>) {
    let raws = parse_raw_maps(data, region_end);
    let mut maps = Vec::with_capacity(raws.len());
    let mut warnings = Vec::new();
    for raw in raws {
        match convert(&raw, &mut warnings) {
            Some(def) => maps.push(def),
            None => warnings.push(format!(
                "map '{}' skipped: cell type {} has no OpenTune equivalent (likely 64-bit)",
                raw.name, raw.type_code
            )),
        }
    }
    (maps, warnings)
}

/// Convert one stored axis to an [`AxisDef`] with `count` breakpoints. A stored
/// axis whose type OpenTune cannot encode keeps its name/unit/count but drops
/// the address, and a warning is pushed.
fn convert_axis(
    raw: &RawAxis,
    count: u32,
    map_name: &str,
    warnings: &mut Vec<String>,
) -> Option<AxisDef> {
    let has_label = !raw.name.trim().is_empty() || !raw.unit.trim().is_empty();
    if !raw.has_address() && !has_label {
        return None;
    }
    let mut axis = AxisDef {
        name: raw.name.trim().to_string(),
        unit: raw.unit.trim().to_string(),
        count,
        scaling: Scaling {
            factor: if raw.factor == 0.0 { 1.0 } else { raw.factor },
            offset: raw.offset,
        },
        ..AxisDef::default()
    };
    if raw.has_address() {
        match decode_type(raw.type_code, raw.signed) {
            Some((dt, endian)) => {
                axis.address = Some(raw.address);
                axis.data_type = dt;
                axis.endian = endian;
            }
            None => warnings.push(format!(
                "map '{map_name}': axis at {:#x} has cell type {} with no OpenTune equivalent; address dropped",
                raw.address, raw.type_code
            )),
        }
    }
    Some(axis)
}

/// Convert one raw record to a [`MapDef`], or `None` if its cell type cannot be
/// represented in OpenTune at all.
fn convert(raw: &RawMap, warnings: &mut Vec<String>) -> Option<MapDef> {
    let (data_type, endian) = decode_type(raw.type_code, raw.signed)?;

    let mut notes = raw.comment.trim().to_string();
    let factor = if raw.factor == 0.0 {
        if !notes.is_empty() {
            notes.push_str("; ");
        }
        notes.push_str("stored scaling factor was 0; defaulted to 1.0");
        1.0
    } else {
        raw.factor
    };

    let x_axis = convert_axis(&raw.x, raw.cols, &raw.name, warnings);
    let y_axis = convert_axis(&raw.y, raw.rows, &raw.name, warnings);

    Some(MapDef {
        name: raw.name.clone(),
        notes,
        address: raw.start,
        data_type,
        endian,
        rows: raw.rows,
        cols: raw.cols,
        scaling: Scaling {
            factor,
            offset: raw.offset,
        },
        unit: raw.unit.trim().to_string(),
        x_axis,
        y_axis,
        min: None,
        max: None,
    })
}
