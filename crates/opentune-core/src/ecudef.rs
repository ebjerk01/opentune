//! ECU definitions: data describing a known ECU (protocol, memory map, which
//! checksum and seed/key handlers apply). Loaded from JSON in `ecudefs/`.
use crate::types::{DiagLayer, EcuId, MemoryRegion, Protocol};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EcuDef {
    pub id: String,
    pub name: String,
    pub maker: String,
    pub family: String,
    #[serde(default)]
    pub notes: String,
    pub protocol: Protocol,
    pub diag_layer: DiagLayer,
    pub bitrate: u32,
    #[serde(default)]
    pub tx_id: u32,
    #[serde(default)]
    pub rx_id: u32,
    #[serde(default)]
    pub flash_size: u32,
    #[serde(default)]
    pub regions: Vec<NamedRegion>,
    /// Checksum algorithm id handled by `opentune-checksum` (e.g. "edc16").
    #[serde(default)]
    pub checksum: Option<String>,
    /// Seed/key algorithm id handled by a seed/key plugin.
    #[serde(default)]
    pub seedkey: Option<String>,
    #[serde(default)]
    pub full_read: FullRead,
    /// Calibration maps (tables) known in this ECU's software. Optional: a
    /// definition with no maps still parses. See [`MapDef`] and `docs/MAP-DEFS.md`.
    #[serde(default)]
    pub maps: Vec<MapDef>,
    /// Keys for identifying this ECU from what it reports over diagnostics.
    #[serde(default)]
    pub identify: Identify,
}

/// Identification keys: how an ECU's self-reported identity (part number, SW
/// number) maps to this definition. Matching is prefix/substring based so a
/// family prefix (e.g. "03G906") covers many exact numbers. See `identify`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Identify {
    /// Hardware / spare-part numbers or prefixes (e.g. "0281011", "03G906").
    #[serde(default)]
    pub part_numbers: Vec<String>,
    /// Software / calibration numbers or prefixes (e.g. "1037369105").
    #[serde(default)]
    pub sw_numbers: Vec<String>,
}

fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_uppercase()
}

/// Does `value` match any of `keys` (case/space-insensitive prefix or substring)?
fn key_hit(value: &str, keys: &[String]) -> bool {
    let v = norm(value);
    keys.iter().any(|k| {
        let k = norm(k);
        !k.is_empty() && (v.starts_with(&k) || v.contains(&k))
    })
}

impl EcuDef {
    /// True if an ECU reporting `id` matches this definition's identify keys.
    pub fn matches_id(&self, id: &EcuId) -> bool {
        if let Some(hw) = &id.hw_number {
            if key_hit(hw, &self.identify.part_numbers) {
                return true;
            }
        }
        if let Some(sw) = &id.sw_number {
            if key_hit(sw, &self.identify.sw_numbers) {
                return true;
            }
        }
        false
    }
}

/// Find the first definition whose identify keys match a reported ECU id.
pub fn match_ecu<'a>(defs: &'a [EcuDef], id: &EcuId) -> Option<&'a EcuDef> {
    defs.iter().find(|d| d.matches_id(id))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamedRegion {
    pub name: String,
    pub region: MemoryRegion,
    #[serde(default)]
    pub writable: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FullRead {
    #[default]
    Obd,
    BenchBoot,
}

/// How a stored calibration value is encoded in the flash image.
///
/// The variant names double as the byte width and signedness; endianness is a
/// separate axis (see [`Endian`]) because the same type can appear in either
/// byte order on different targets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataType {
    /// Unsigned 8-bit.
    #[default]
    U8,
    /// Signed 8-bit (two's complement).
    S8,
    /// Unsigned 16-bit.
    U16,
    /// Signed 16-bit (two's complement).
    S16,
    /// Unsigned 32-bit.
    U32,
    /// Signed 32-bit (two's complement).
    S32,
    /// IEEE-754 single-precision float.
    F32,
}

impl DataType {
    /// Width of one stored element in bytes.
    pub fn byte_len(self) -> u32 {
        match self {
            DataType::U8 | DataType::S8 => 1,
            DataType::U16 | DataType::S16 => 2,
            DataType::U32 | DataType::S32 | DataType::F32 => 4,
        }
    }
}

/// Byte order of a multi-byte stored value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Endian {
    /// Most-significant byte first (Motorola). Common on Bosch EDC/ME.
    #[default]
    Big,
    /// Least-significant byte first (Intel).
    Little,
}

/// Linear scaling that converts a raw stored value to a physical value:
/// `physical = raw * factor + offset`, expressed in `unit`.
///
/// This is the DBC/cantools model (factor, offset, unit). It covers the vast
/// majority of OEM maps; a non-linear OEM `COMPU_METHOD` (A2L `RAT_FUNC`,
/// `TAB_INTP`) is intentionally out of scope for v1 and should be captured as a
/// pre-scaled map plus a note rather than an arbitrary expression.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Scaling {
    /// Multiplier applied to the raw value (a.k.a. resolution / LSB).
    #[serde(default = "one")]
    pub factor: f64,
    /// Added after the multiply.
    #[serde(default)]
    pub offset: f64,
}

fn one() -> f64 {
    1.0
}

impl Default for Scaling {
    fn default() -> Self {
        Self {
            factor: 1.0,
            offset: 0.0,
        }
    }
}

impl Scaling {
    /// Convert a raw stored value to its physical value.
    pub fn to_physical(&self, raw: f64) -> f64 {
        raw * self.factor + self.offset
    }
}

/// A map axis: the breakpoint values along one dimension of a table.
///
/// An axis can be described three ways, mirroring A2L's `AXIS_DESCR`:
/// - **inline** — fill `values` with the physical breakpoints (A2L `FIX_AXIS`),
/// - **stored** — give `address` + `data_type` (+ `scaling`), and the axis is
///   read from the image like a 1-D table (A2L `STD_AXIS`),
/// - **shared/reference** — set `axis_ref` to the `name` of another axis defined
///   elsewhere so several maps share one breakpoint table (A2L `COM_AXIS`).
///
/// `count` is the number of breakpoints and should match the map's `rows`/`cols`
/// on this dimension.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AxisDef {
    /// Axis label, e.g. `"rpm"` or `"injected quantity"`.
    #[serde(default)]
    pub name: String,
    /// Unit of the physical breakpoint values, e.g. `"rpm"`, `"mg/stroke"`.
    #[serde(default)]
    pub unit: String,
    /// Number of breakpoints along this axis.
    #[serde(default)]
    pub count: u32,
    /// Inline physical breakpoint values (use this *or* `address`/`axis_ref`).
    #[serde(default)]
    pub values: Vec<f64>,
    /// Address the axis is stored at, when it lives in the image.
    #[serde(default)]
    pub address: Option<u32>,
    /// Stored element type (only meaningful with `address`).
    #[serde(default)]
    pub data_type: DataType,
    /// Byte order of stored elements (only meaningful with `address`).
    #[serde(default)]
    pub endian: Endian,
    /// Raw→physical scaling for stored breakpoints.
    #[serde(default)]
    pub scaling: Scaling,
    /// Name of another axis to reuse instead of defining one here.
    #[serde(default)]
    pub axis_ref: Option<String>,
}

/// A calibration map (table): a scalar, a 1-D curve or a 2-D map stored in the
/// ECU image. See `docs/MAP-DEFS.md` for the full format rationale.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MapDef {
    /// Stable, human-readable map name, e.g. `"injection quantity limiter"`.
    pub name: String,
    /// Free text; use it to flag that the address needs per-variant confirmation.
    #[serde(default)]
    pub notes: String,
    /// Start address of the map data in the flash image.
    pub address: u32,
    /// Stored element type.
    #[serde(default)]
    pub data_type: DataType,
    /// Byte order of stored elements.
    #[serde(default)]
    pub endian: Endian,
    /// Number of rows (Y dimension). `1` for a scalar or a 1-D curve.
    #[serde(default = "one_u32")]
    pub rows: u32,
    /// Number of columns (X dimension). `1` for a scalar.
    #[serde(default = "one_u32")]
    pub cols: u32,
    /// Raw→physical scaling of the map cells.
    #[serde(default)]
    pub scaling: Scaling,
    /// Physical unit of the cells, e.g. `"mg/stroke"`, `"deg"`.
    #[serde(default)]
    pub unit: String,
    /// X (column) axis, if any.
    #[serde(default)]
    pub x_axis: Option<AxisDef>,
    /// Y (row) axis, if any.
    #[serde(default)]
    pub y_axis: Option<AxisDef>,
    /// Optional lower physical bound for edited values (safety clamp / UI hint).
    #[serde(default)]
    pub min: Option<f64>,
    /// Optional upper physical bound for edited values.
    #[serde(default)]
    pub max: Option<f64>,
}

fn one_u32() -> u32 {
    1
}

impl MapDef {
    /// Total number of cells (`rows * cols`).
    pub fn cell_count(&self) -> u32 {
        self.rows.saturating_mul(self.cols)
    }

    /// Size of the map data block in bytes (`cells * element width`).
    pub fn byte_len(&self) -> u32 {
        self.cell_count().saturating_mul(self.data_type.byte_len())
    }
}

impl EcuDef {
    pub fn from_json(s: &str) -> crate::Result<Self> {
        serde_json::from_str(s).map_err(|e| crate::Error::Config(e.to_string()))
    }
}

/// Load every `*.json` ECU definition in a directory.
pub fn load_dir(dir: &Path) -> crate::Result<Vec<EcuDef>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|s| s.to_str()) == Some("json") {
            out.push(EcuDef::from_json(&std::fs::read_to_string(&path)?)?);
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapdef_json_round_trip() {
        // A 2-D map with a stored X axis and a shared Y axis reference.
        let json = r#"{
            "name": "injection quantity limiter",
            "notes": "placeholder address - confirm per software variant",
            "address": 458752,
            "data_type": "u16",
            "endian": "big",
            "rows": 16,
            "cols": 12,
            "scaling": { "factor": 0.01, "offset": 0.0 },
            "unit": "mg/stroke",
            "x_axis": {
                "name": "rpm",
                "unit": "rpm",
                "count": 12,
                "address": 458240,
                "data_type": "u16",
                "endian": "big",
                "scaling": { "factor": 1.0 }
            },
            "y_axis": { "name": "boost", "axis_ref": "boost_axis", "count": 16 },
            "min": 0.0,
            "max": 80.0
        }"#;

        let map: MapDef = serde_json::from_str(json).expect("MapDef must parse");
        assert_eq!(map.name, "injection quantity limiter");
        assert_eq!(map.data_type, DataType::U16);
        assert_eq!(map.endian, Endian::Big);
        assert_eq!(map.cell_count(), 16 * 12);
        assert_eq!(map.byte_len(), 16 * 12 * 2);
        assert_eq!(map.scaling.to_physical(5000.0), 50.0);

        let x = map.x_axis.as_ref().expect("x axis present");
        assert_eq!(x.address, Some(458240));
        assert_eq!(x.count, 12);
        let y = map.y_axis.as_ref().expect("y axis present");
        assert_eq!(y.axis_ref.as_deref(), Some("boost_axis"));

        // Round-trip: serialize and parse back to an equal value.
        let out = serde_json::to_string(&map).expect("serialize");
        let back: MapDef = serde_json::from_str(&out).expect("re-parse");
        assert_eq!(map, back);
    }

    #[test]
    fn mapdef_defaults_fill_in() {
        // Only the required fields; everything else must default.
        let map: MapDef = serde_json::from_str(r#"{ "name": "scalar", "address": 1024 }"#)
            .expect("minimal parse");
        assert_eq!(map.data_type, DataType::U8);
        assert_eq!(map.endian, Endian::Big);
        assert_eq!(map.rows, 1);
        assert_eq!(map.cols, 1);
        assert_eq!(map.scaling.factor, 1.0);
        assert_eq!(map.scaling.offset, 0.0);
        assert!(map.x_axis.is_none());
        assert!(map.min.is_none());
    }

    #[test]
    fn ecudef_without_maps_still_parses() {
        // Back-compat: an EcuDef JSON with no `maps` key yields an empty Vec.
        let json = r#"{
            "id": "x", "name": "X", "maker": "M", "family": "F",
            "protocol": "iso_tp", "diag_layer": "uds", "bitrate": 500000
        }"#;
        let def = EcuDef::from_json(json).expect("parse without maps");
        assert!(def.maps.is_empty());
    }
}
