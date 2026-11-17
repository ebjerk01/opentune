//! OpenTune UniFFI binding — the offline calibration workflow, re-exported.
//!
//! This crate is a *thin, safe* wrapper over the public OpenTune core. It owns
//! no new logic: every function mirrors an operation already proven in the CLI
//! (`crates/opentune-cli`) and the desktop Tauri backend
//! (`desktop/src-tauri/src/main.rs`), driven entirely by the offline mock/file
//! driver so the whole pipeline runs on a local flash-image `.bin` with no
//! hardware:
//!
//! ```text
//! open_image → identify → checksum_verify / checksum_fix →
//! find_maps → decode_map (heat grid) → flash (safety-gated, mock)
//! ```
//!
//! It is deliberately **calibration-only** and **offline-only**: there is no
//! hardware driver, no immobiliser/EEPROM/bench tooling here. Errors surface as
//! [`OpenTuneError`]; all inputs are file paths or plain values, so the binding
//! is memory-safe to call from Kotlin/Swift/Python.
use std::path::Path;

use opentune_core::{
    ecudef::{self, AxisDef, DataType, EcuDef, Endian, MapDef},
    ConnectConfig, EcuId, Interface, MemoryRegion,
};

uniffi::setup_scaffolding!("opentune");

// ===========================================================================
// Error type
// ===========================================================================

/// Every fallible FFI call returns this. It carries a single human-readable
/// message (the underlying `opentune_core::Error`, an I/O error, or a decode
/// error), which is what a mobile UI wants to show the operator.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum OpenTuneError {
    /// A file could not be read or written.
    #[error("I/O error: {msg}")]
    Io { msg: String },
    /// An ECU definition, map, or checksum algorithm was not found.
    #[error("not found: {msg}")]
    NotFound { msg: String },
    /// The image, definition JSON, or an argument was malformed.
    #[error("invalid input: {msg}")]
    Invalid { msg: String },
    /// A core/driver/flash operation failed (safety gate, verify, decode …).
    #[error("operation failed: {msg}")]
    Op { msg: String },
}

impl OpenTuneError {
    fn io(ctx: &str, e: impl std::fmt::Display) -> Self {
        OpenTuneError::Io {
            msg: format!("{ctx}: {e}"),
        }
    }
    fn op(e: impl std::fmt::Display) -> Self {
        OpenTuneError::Op { msg: e.to_string() }
    }
}

type Result<T> = std::result::Result<T, OpenTuneError>;

// ===========================================================================
// Records returned across the FFI boundary
// ===========================================================================

/// Basic facts about an opened flash image.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ImageInfo {
    /// Absolute or relative path the image was opened from.
    pub path: String,
    /// File name component (for display).
    pub name: String,
    /// Size in bytes.
    pub size: u64,
}

/// An ECU definition matched against a reported identity.
#[derive(Debug, Clone, uniffi::Record)]
pub struct DefMatch {
    pub id: String,
    pub name: String,
    pub maker: String,
    pub family: String,
    pub protocol: String,
    pub checksum: Option<String>,
    pub seedkey: Option<String>,
}

/// The identity read from an image plus any matching ECU definition.
#[derive(Debug, Clone, uniffi::Record)]
pub struct Identity {
    pub ecu_name: Option<String>,
    pub hw_number: Option<String>,
    pub sw_number: Option<String>,
    pub supplier: Option<String>,
    pub vin: Option<String>,
    /// Image size in bytes.
    pub size: u64,
    /// The first ECU definition in `ecudefs_dir` whose identify keys matched,
    /// if any. The offline mock driver only reports a name + size, so this is
    /// usually `None` for a raw `.bin`; a real on-car read would populate it.
    pub matched: Option<DefMatch>,
}

/// One checksum-protected block and whether its stored value is correct.
#[derive(Debug, Clone, uniffi::Record)]
pub struct Block {
    pub name: String,
    pub ok: bool,
    /// Formatted region, e.g. `0x0003C000 +131072`.
    pub region: String,
    /// Stored checksum, hex.
    pub stored: String,
    /// Computed checksum, hex.
    pub computed: String,
}

/// A single checksum correction (what `checksum_fix` wrote).
#[derive(Debug, Clone, uniffi::Record)]
pub struct Fix {
    pub name: String,
    /// Byte offset the corrected value was written at.
    pub offset: u32,
    /// Old stored value.
    pub old: u64,
    /// New (correct) value.
    pub new: u64,
}

/// A candidate map region found by diffing a stock and a tuned image.
#[derive(Debug, Clone, uniffi::Record)]
pub struct Region {
    pub index: u32,
    /// Start byte offset.
    pub start: u32,
    /// End byte offset (exclusive).
    pub end: u32,
    /// Length in bytes.
    pub len: u32,
    /// A loose stride hint, e.g. `~16x16 u16?` (empty when none applies).
    pub hint: String,
}

/// One map axis decoded to physical breakpoint values.
#[derive(Debug, Clone, uniffi::Record)]
pub struct Axis {
    pub name: Option<String>,
    pub unit: String,
    pub values: Vec<f64>,
}

/// A decoded calibration map: physical cell values plus both axes. Read-only —
/// this is the public edition's map viewer, so decoding never writes the image.
#[derive(Debug, Clone, uniffi::Record)]
pub struct Grid {
    pub name: String,
    /// Map data address, hex.
    pub address: String,
    pub rows: u32,
    pub cols: u32,
    pub unit: String,
    pub data_type: String,
    pub endian: String,
    pub factor: f64,
    pub offset: f64,
    /// Minimum decoded physical value in the grid, if finite.
    pub min: Option<f64>,
    /// Maximum decoded physical value in the grid, if finite.
    pub max: Option<f64>,
    /// Lower safety bound declared by the definition, if any.
    pub def_min: Option<f64>,
    /// Upper safety bound declared by the definition, if any.
    pub def_max: Option<f64>,
    /// Row-major grid of physical values (`rows` rows of `cols` each).
    pub cells: Vec<Vec<f64>>,
    pub x_axis: Axis,
    pub y_axis: Axis,
    pub notes: String,
}

/// The outcome of a (mock) safety-gated flash.
#[derive(Debug, Clone, uniffi::Record)]
pub struct FlashReport {
    pub bytes_written: u64,
    pub battery_mv: Option<u32>,
    pub backup_len: u64,
    pub verified: bool,
    /// Formatted region, e.g. `0x00000000 +256 (0x100)`.
    pub region: String,
}

/// A short summary of one ECU definition (for a picker UI).
#[derive(Debug, Clone, uniffi::Record)]
pub struct DefSummary {
    pub id: String,
    pub name: String,
    pub maker: String,
    pub family: String,
    pub protocol: String,
    pub flash_kib: u32,
    pub checksum: Option<String>,
    pub seedkey: Option<String>,
    pub map_count: u32,
    /// The JSON file this definition was loaded from — pass it straight to
    /// [`list_maps`] / [`decode_map`].
    pub path: String,
    pub notes: String,
}

/// A short summary of one map within a definition (for a picker UI).
#[derive(Debug, Clone, uniffi::Record)]
pub struct MapSummary {
    pub name: String,
    pub address: String,
    pub rows: u32,
    pub cols: u32,
    pub data_type: String,
    pub endian: String,
    pub unit: String,
    pub factor: f64,
    pub offset: f64,
    pub byte_len: u32,
    pub notes: String,
}

// ===========================================================================
// Internal helpers (shared with the CLI / desktop logic)
// ===========================================================================

fn parse_u32(s: &str) -> Result<u32> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u32::from_str_radix(h, 16).map_err(|e| OpenTuneError::Invalid {
            msg: format!("bad hex '{s}': {e}"),
        })
    } else {
        s.parse::<u32>().map_err(|e| OpenTuneError::Invalid {
            msg: format!("bad number '{s}': {e}"),
        })
    }
}

fn read_file(path: &str) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| OpenTuneError::io(&format!("read {path}"), e))
}

/// Open the offline mock/file driver on `path` (mirrors the CLI/desktop helper).
fn open_file(path: &str) -> Result<Box<dyn Interface>> {
    let mut drv: Box<dyn Interface> = Box::new(opentune_driver_mock::MockDriver::new());
    let mut cfg = ConnectConfig::default();
    cfg.options.insert("file".into(), path.to_string());
    drv.open(&cfg).map_err(OpenTuneError::op)?;
    Ok(drv)
}

/// Load a single ECU definition JSON file.
fn load_def_file(ecudef_path: &str) -> Result<EcuDef> {
    let txt = std::fs::read_to_string(ecudef_path)
        .map_err(|e| OpenTuneError::io(&format!("read {ecudef_path}"), e))?;
    EcuDef::from_json(&txt).map_err(|e| OpenTuneError::Invalid {
        msg: format!("parse {ecudef_path}: {e}"),
    })
}

fn def_match(d: &EcuDef) -> DefMatch {
    DefMatch {
        id: d.id.clone(),
        name: d.name.clone(),
        maker: d.maker.clone(),
        family: d.family.clone(),
        protocol: format!("{:?}", d.protocol),
        checksum: d.checksum.clone(),
        seedkey: d.seedkey.clone(),
    }
}

/// Read one stored element honouring width + byte order (as in CLI/desktop).
fn read_elem(img: &[u8], off: usize, dt: DataType, en: Endian) -> Option<f64> {
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
            (if le {
                u16::from_le_bytes([s[0], s[1]])
            } else {
                u16::from_be_bytes([s[0], s[1]])
            }) as f64
        }
        DataType::S16 => {
            (if le {
                i16::from_le_bytes([s[0], s[1]])
            } else {
                i16::from_be_bytes([s[0], s[1]])
            }) as f64
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

/// Decode one axis to physical breakpoints: inline `values`, stored
/// (`address` + type), or a 0..count index fallback (shared/ref axis).
fn decode_axis(img: &[u8], axis: &Option<AxisDef>, count: u32) -> Axis {
    let n = count.max(1) as usize;
    match axis {
        Some(a) if !a.values.is_empty() => Axis {
            name: (!a.name.is_empty()).then(|| a.name.clone()),
            unit: a.unit.clone(),
            values: a.values.clone(),
        },
        Some(a) if a.address.is_some() => {
            let addr = a.address.unwrap() as usize;
            let w = a.data_type.byte_len() as usize;
            let values = (0..n)
                .map(
                    |i| match read_elem(img, addr + i * w, a.data_type, a.endian) {
                        Some(raw) => a.scaling.to_physical(raw),
                        None => i as f64,
                    },
                )
                .collect();
            Axis {
                name: (!a.name.is_empty()).then(|| a.name.clone()),
                unit: a.unit.clone(),
                values,
            }
        }
        Some(a) => Axis {
            name: (!a.name.is_empty()).then(|| a.name.clone()),
            unit: a.unit.clone(),
            values: (0..n).map(|i| i as f64).collect(),
        },
        None => Axis {
            name: None,
            unit: String::new(),
            values: (0..n).map(|i| i as f64).collect(),
        },
    }
}

/// A loose "is this a table of u16 cells?" hint (as in CLI/desktop).
fn stride_hint(len: usize) -> String {
    if len >= 4 && len % 2 == 0 {
        let cells = len / 2;
        for &w in &[16usize, 12, 10, 8] {
            if cells % w == 0 {
                return format!("~{}x{} u16?", cells / w, w);
            }
        }
        return format!("~{cells} u16?");
    }
    String::new()
}

// ===========================================================================
// Exported FFI surface
// ===========================================================================

/// Open a flash image and return its basic facts (path, name, size). Validates
/// that the file exists and is readable.
#[uniffi::export]
pub fn open_image(path: String) -> Result<ImageInfo> {
    let meta =
        std::fs::metadata(&path).map_err(|e| OpenTuneError::io(&format!("stat {path}"), e))?;
    if !meta.is_file() {
        return Err(OpenTuneError::Invalid {
            msg: format!("{path} is not a file"),
        });
    }
    let name = Path::new(&path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(ImageInfo {
        path,
        name,
        size: meta.len(),
    })
}

/// Read the ECU identity from an image (via the offline mock driver) and match
/// it against every `*.json` definition in `ecudefs_dir`.
#[uniffi::export]
pub fn identify(path: String, ecudefs_dir: String) -> Result<Identity> {
    let mut drv = open_file(&path)?;
    let id: EcuId = drv.read_ecu_id().map_err(OpenTuneError::op)?;
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

    let defs = ecudef::load_dir(Path::new(&ecudefs_dir)).unwrap_or_default();
    let matched = ecudef::match_ecu(&defs, &id).map(def_match);

    Ok(Identity {
        ecu_name: id.ecu_name,
        hw_number: id.hw_number,
        sw_number: id.sw_number,
        supplier: id.supplier,
        vin: id.vin,
        size,
        matched,
    })
}

/// Verify every checksum-protected block for `algo` — any id
/// `opentune_checksum::get` accepts (`edc16` / `edc17` / `pcr21` /
/// `crc32-tail` / `edc15c6` / `me7` / `dq250` / `dq381` / `tcu-ccitt` /
/// `zf8hp` / `haldex`).
#[uniffi::export]
pub fn checksum_verify(path: String, algo: String) -> Result<Vec<Block>> {
    let a = opentune_checksum::get(&algo).ok_or_else(|| OpenTuneError::NotFound {
        msg: format!("unknown checksum algo '{algo}'"),
    })?;
    let img = read_file(&path)?;
    Ok(a.verify(&img)
        .map_err(OpenTuneError::op)?
        .into_iter()
        .map(|b| Block {
            name: b.name,
            ok: b.ok,
            region: format!("0x{:08X} +{}", b.region.address, b.region.length),
            stored: format!("0x{:08X}", b.stored),
            computed: format!("0x{:08X}", b.computed),
        })
        .collect())
}

/// Correct checksums for `algo` **in place** over `path` (or to `out_path` when
/// given — the Save-As fallback). Returns the corrections applied; an empty list
/// means the image was already correct.
#[uniffi::export]
pub fn checksum_fix(path: String, algo: String, out_path: Option<String>) -> Result<Vec<Fix>> {
    let a = opentune_checksum::get(&algo).ok_or_else(|| OpenTuneError::NotFound {
        msg: format!("unknown checksum algo '{algo}'"),
    })?;
    let mut img = read_file(&path)?;
    let fixes = a.correct(&mut img).map_err(OpenTuneError::op)?;
    let target = out_path.unwrap_or_else(|| path.clone());
    if !fixes.is_empty() || target != path {
        std::fs::write(&target, &img)
            .map_err(|e| OpenTuneError::io(&format!("write {target}"), e))?;
    }
    Ok(fixes
        .into_iter()
        .map(|f| Fix {
            name: f.name,
            offset: f.offset,
            old: f.old,
            new: f.new,
        })
        .collect())
}

/// Find candidate calibration-map regions by clustering the byte diff of a
/// stock and a tuned image. Uses the project defaults (`gap = 16`, merging
/// changes closer than 16 unchanged bytes; `min_len = 2`).
#[uniffi::export]
pub fn find_maps(stock: String, tuned: String) -> Result<Vec<Region>> {
    const GAP: usize = 16;
    const MIN_LEN: usize = 2;
    let a = read_file(&stock)?;
    let b = read_file(&tuned)?;
    let n = a.len().min(b.len());
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut idx = 0u32;
    while i < n {
        if a[i] != b[i] {
            let start = i;
            let mut end = i + 1;
            let mut since = 0usize;
            let mut j = i + 1;
            while j < n && since <= GAP {
                if a[j] != b[j] {
                    end = j + 1;
                    since = 0;
                } else {
                    since += 1;
                }
                j += 1;
            }
            if end - start >= MIN_LEN {
                out.push(Region {
                    index: idx,
                    start: start as u32,
                    end: end as u32,
                    len: (end - start) as u32,
                    hint: stride_hint(end - start),
                });
                idx += 1;
            }
            i = end;
        } else {
            i += 1;
        }
    }
    Ok(out)
}

/// List every `*.json` ECU definition in `ecudefs_dir` (for a picker UI). Each
/// entry carries the JSON file `path`, which [`list_maps`] / [`decode_map`] take.
#[uniffi::export]
pub fn list_ecudefs(ecudefs_dir: String) -> Result<Vec<DefSummary>> {
    let dir = Path::new(&ecudefs_dir);
    let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| OpenTuneError::io(&format!("read dir {ecudefs_dir}"), e))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    entries.sort();
    let mut out = Vec::new();
    for p in entries {
        // Skip files that don't parse as a definition rather than failing the
        // whole listing (a catalog dir could sit beside an ecudefs dir).
        let Ok(txt) = std::fs::read_to_string(&p) else {
            continue;
        };
        let Ok(d) = EcuDef::from_json(&txt) else {
            continue;
        };
        out.push(DefSummary {
            id: d.id.clone(),
            name: d.name.clone(),
            maker: d.maker.clone(),
            family: d.family.clone(),
            protocol: format!("{:?}", d.protocol),
            flash_kib: d.flash_size / 1024,
            checksum: d.checksum.clone(),
            seedkey: d.seedkey.clone(),
            map_count: d.maps.len() as u32,
            path: p.to_string_lossy().into_owned(),
            notes: d.notes.clone(),
        });
    }
    Ok(out)
}

/// List the maps defined in a single ECU-definition JSON file (for a picker UI).
#[uniffi::export]
pub fn list_maps(ecudef_path: String) -> Result<Vec<MapSummary>> {
    let def = load_def_file(&ecudef_path)?;
    Ok(def
        .maps
        .iter()
        .map(|m| MapSummary {
            name: m.name.clone(),
            address: format!("0x{:08X}", m.address),
            rows: m.rows,
            cols: m.cols,
            data_type: format!("{:?}", m.data_type),
            endian: format!("{:?}", m.endian),
            unit: m.unit.clone(),
            factor: m.scaling.factor,
            offset: m.scaling.offset,
            byte_len: m.byte_len(),
            notes: m.notes.clone(),
        })
        .collect())
}

/// Decode a named map out of `path` through the definition JSON at `ecudef_path`
/// into a physical-value heat grid (with both axes). Read-only.
#[uniffi::export]
pub fn decode_map(path: String, ecudef: String, map: String) -> Result<Grid> {
    let def = load_def_file(&ecudef)?;
    let m: &MapDef =
        def.maps
            .iter()
            .find(|md| md.name == map)
            .ok_or_else(|| OpenTuneError::NotFound {
                msg: format!("map '{map}' not in {ecudef}"),
            })?;
    let img = read_file(&path)?;

    let rows = m.rows.max(1) as usize;
    let cols = m.cols.max(1) as usize;
    let w = m.data_type.byte_len() as usize;
    let need = m.address as usize + rows * cols * w;
    if need > img.len() {
        return Err(OpenTuneError::Invalid {
            msg: format!(
                "map needs 0x{:08X} bytes but the image is only 0x{:08X} — open a full dump (flash is {} KiB)",
                need,
                img.len(),
                def.flash_size / 1024
            ),
        });
    }

    let mut cells: Vec<Vec<f64>> = Vec::with_capacity(rows);
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for r in 0..rows {
        let mut row = Vec::with_capacity(cols);
        for c in 0..cols {
            let off = m.address as usize + (r * cols + c) * w;
            let phys = read_elem(&img, off, m.data_type, m.endian)
                .map(|raw| m.scaling.to_physical(raw))
                .unwrap_or(0.0);
            lo = lo.min(phys);
            hi = hi.max(phys);
            row.push(phys);
        }
        cells.push(row);
    }

    Ok(Grid {
        name: m.name.clone(),
        address: format!("0x{:08X}", m.address),
        rows: rows as u32,
        cols: cols as u32,
        unit: m.unit.clone(),
        data_type: format!("{:?}", m.data_type),
        endian: format!("{:?}", m.endian),
        factor: m.scaling.factor,
        offset: m.scaling.offset,
        min: lo.is_finite().then_some(lo),
        max: hi.is_finite().then_some(hi),
        def_min: m.min,
        def_max: m.max,
        cells,
        x_axis: decode_axis(&img, &m.x_axis, m.cols),
        y_axis: decode_axis(&img, &m.y_axis, m.rows),
        notes: m.notes.clone(),
    })
}

/// Write `data` to `path` at `addr` through the full safety orchestrator
/// (voltage gate → verified backup → write → read-back verify), driven by the
/// offline mock/file driver. This is a real exercise of the flash pipeline
/// against a file — no hardware is touched.
#[uniffi::export]
pub fn flash(path: String, addr: String, data: Vec<u8>, min_mv: u32) -> Result<FlashReport> {
    let addr = parse_u32(&addr)?;
    let region = MemoryRegion::new(addr, data.len() as u32);
    let mut drv = open_file(&path)?;
    let safety = opentune_flash::SafetyConfig {
        min_millivolts: min_mv,
        ..Default::default()
    };
    let rep = opentune_flash::flash_calibration(drv.as_mut(), region, &data, &safety, &mut |_| {})
        .map_err(OpenTuneError::op)?;
    Ok(FlashReport {
        bytes_written: rep.bytes_written as u64,
        battery_mv: rep.battery_millivolts,
        backup_len: rep.backup.len() as u64,
        verified: rep.verified,
        region: format!(
            "0x{:08X} +{} (0x{:08X})",
            region.address, region.length, region.length
        ),
    })
}

// ===========================================================================
// Tests — exercise the real offline pipeline on a synthesised image.
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a throw-away image file with a known byte pattern.
    fn tmp_image(bytes: &[u8]) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.bin");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(bytes).unwrap();
        let p = path.to_string_lossy().into_owned();
        (dir, p)
    }

    #[test]
    fn open_image_reports_size() {
        let (_d, path) = tmp_image(&[0u8; 512]);
        let info = open_image(path.clone()).unwrap();
        assert_eq!(info.size, 512);
        assert_eq!(info.name, "image.bin");
        assert_eq!(info.path, path);
    }

    #[test]
    fn open_image_missing_is_error() {
        assert!(open_image("/no/such/file.bin".into()).is_err());
    }

    #[test]
    fn identify_reads_mock_identity() {
        let (_d, path) = tmp_image(&[0xAAu8; 256]);
        // No ecudefs dir -> matched is None, but identity still reads.
        let id = identify(path, "/nonexistent-ecudefs".into()).unwrap();
        assert_eq!(id.size, 256);
        assert_eq!(id.ecu_name.as_deref(), Some("mock-ecu"));
        assert!(id.matched.is_none());
    }

    #[test]
    fn crc32_tail_verify_then_fix_roundtrips() {
        // crc32-tail is self-contained: a zeroed image has a bad tail; fixing it
        // must then verify clean.
        let (_d, path) = tmp_image(&[0u8; 1024]);
        let before = checksum_verify(path.clone(), "crc32-tail".into()).unwrap();
        assert!(!before.is_empty());

        let fixes = checksum_fix(path.clone(), "crc32-tail".into(), None).unwrap();
        assert!(!fixes.is_empty(), "a zeroed tail should need correction");

        let after = checksum_verify(path, "crc32-tail".into()).unwrap();
        assert!(after.iter().all(|b| b.ok), "all blocks OK after fix");
    }

    #[test]
    fn find_maps_detects_changed_region() {
        let stock = vec![0u8; 1024];
        let mut tuned = stock.clone();
        for b in tuned.iter_mut().take(40).skip(32) {
            *b = 0xFF;
        }
        // Keep the TempDirs alive until the assertions run.
        let (_d1, sp) = tmp_image(&stock);
        let (_d2, tp) = tmp_image(&tuned);
        let regions = find_maps(sp, tp).unwrap();
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].start, 32);
        assert_eq!(regions[0].end, 40);
    }

    #[test]
    fn decode_map_reads_heat_grid() {
        // A 2x2 u16 big-endian map at offset 0 with factor 0.5.
        // bytes: [00 02][00 04 | 00 06][00 08]  -> raw 2,4,6,8 -> phys 1,2,3,4
        let img = vec![0x00, 0x02, 0x00, 0x04, 0x00, 0x06, 0x00, 0x08];
        let (_di, ip) = tmp_image(&img);

        let def = r#"{
            "id": "t", "name": "T", "maker": "M", "family": "F",
            "protocol": "iso_tp", "diag_layer": "uds", "bitrate": 500000,
            "maps": [{
                "name": "demo", "address": 0, "data_type": "u16", "endian": "big",
                "rows": 2, "cols": 2, "scaling": { "factor": 0.5, "offset": 0.0 },
                "unit": "x"
            }]
        }"#;
        let dir = tempfile::tempdir().unwrap();
        let dp = dir.path().join("t.json");
        std::fs::write(&dp, def).unwrap();
        let dps = dp.to_string_lossy().into_owned();

        let maps = list_maps(dps.clone()).unwrap();
        assert_eq!(maps.len(), 1);
        assert_eq!(maps[0].name, "demo");

        let grid = decode_map(ip, dps, "demo".into()).unwrap();
        assert_eq!(grid.rows, 2);
        assert_eq!(grid.cols, 2);
        assert_eq!(grid.cells, vec![vec![1.0, 2.0], vec![3.0, 4.0]]);
        assert_eq!(grid.min, Some(1.0));
        assert_eq!(grid.max, Some(4.0));
    }

    #[test]
    fn decode_map_out_of_range_errors() {
        let (_di, ip) = tmp_image(&[0u8; 4]);
        let def = r#"{
            "id": "t", "name": "T", "maker": "M", "family": "F",
            "protocol": "iso_tp", "diag_layer": "uds", "bitrate": 500000,
            "maps": [{ "name": "big", "address": 0, "data_type": "u16",
                       "rows": 16, "cols": 16 }]
        }"#;
        let dir = tempfile::tempdir().unwrap();
        let dp = dir.path().join("t.json");
        std::fs::write(&dp, def).unwrap();
        let err = decode_map(ip, dp.to_string_lossy().into_owned(), "big".into());
        assert!(err.is_err());
    }

    #[test]
    fn flash_writes_and_verifies() {
        let (_d, path) = tmp_image(&[0u8; 64]);
        let report = flash(path.clone(), "0x0".into(), vec![0x11u8; 16], 12_000).unwrap();
        assert_eq!(report.bytes_written, 16);
        assert!(report.verified);
        assert_eq!(report.backup_len, 16);
        assert_eq!(report.battery_mv, Some(12_600));
        // The write persisted to the file.
        let after = std::fs::read(&path).unwrap();
        assert_eq!(&after[0..16], &[0x11u8; 16]);
    }

    #[test]
    fn flash_voltage_gate_can_block() {
        let (_d, path) = tmp_image(&[0u8; 64]);
        // Mock reports 12_600 mV; require more than that -> safety abort.
        let r = flash(path, "0x0".into(), vec![0x22u8; 8], 13_000);
        assert!(r.is_err());
    }
}
