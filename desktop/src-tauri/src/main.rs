// OpenTune desktop — Tauri 2 backend (PUBLIC, calibration-only edition).
//
// This is the open-source OpenTune app. It exposes the public OpenTune core to
// the UI and runs the whole calibration workflow OFFLINE on a flash-image file
// through the mock/file driver — no hardware required:
//   open a dump → identify it → verify/fix checksums → diff for maps →
//   decode a defined map into a heat grid → run a safety-gated (mock) flash →
//   browse a backup vault.
//
// It is deliberately calibration-only: there is NO immobiliser / EEPROM /
// standalone / bench tooling here (that lives in a separate private app). ECU
// definitions, the vehicle catalog and the in-app guide are resolved from
// bundled resources (or a dev path) at startup.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use opentune_core::{
    catalog::{self, CatalogEntry},
    ecudef::{self, AxisDef, DataType, EcuDef, Endian, MapDef},
    ConnectConfig, EcuId, Interface, MemoryRegion, Registry,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tauri::Manager;

// ---------------------------------------------------------------------------
// Data-directory resolution (ecudefs / catalog / guide)
// ---------------------------------------------------------------------------

/// Resolved data directories, found once at startup.
struct AppData {
    ecudefs: Option<PathBuf>,
    catalog: Option<PathBuf>,
    guide: Option<PathBuf>,
}

fn first_existing(cands: &[PathBuf]) -> Option<PathBuf> {
    cands
        .iter()
        .find(|p| !p.as_os_str().is_empty() && p.exists())
        .cloned()
}

/// Build the driver registry. The mock/file driver powers the offline workflow;
/// the hardware drivers are registered so the app can show what a release can
/// talk to (they are inert until a real interface is attached).
fn registry() -> Registry {
    let mut r = Registry::new();
    r.register("mock", "Mock / file driver (offline)", || {
        Box::new(opentune_driver_mock::MockDriver::new())
    });
    r.register("openport2", "OpenPort 2.0 (J2534/CDC-ACM)", || {
        Box::new(opentune_driver_openport2::OpenPort2Driver::new())
    });
    r.register("socketcan", "SocketCAN", || {
        Box::new(opentune_driver_socketcan::SocketCanDriver::new())
    });
    r.register("slcan", "SLCAN (serial CAN)", || {
        Box::new(opentune_driver_slcan::SlcanDriver::new())
    });
    r.register("elm327", "K-line (ELM327)", || {
        Box::new(opentune_driver_elm327::Elm327Driver::new())
    });
    r.register("kkl", "K-line (KKL 409.1 raw)", || {
        Box::new(opentune_driver_kkl::KklDriver::new())
    });
    r
}

/// Open the mock/file driver on `path` (the offline image).
fn open_file(path: &str) -> Result<Box<dyn Interface>, String> {
    let reg = registry();
    let mut drv = reg.make("mock").ok_or("mock driver missing")?;
    let mut cfg = ConnectConfig::default();
    cfg.options.insert("file".into(), path.to_string());
    drv.open(&cfg).map_err(|e| e.to_string())?;
    Ok(drv)
}

fn load_defs(dir: &Option<PathBuf>) -> Vec<EcuDef> {
    dir.as_ref()
        .and_then(|d| ecudef::load_dir(d).ok())
        .unwrap_or_default()
}
fn load_cat(dir: &Option<PathBuf>) -> Vec<CatalogEntry> {
    dir.as_ref()
        .and_then(|d| catalog::load_catalog(d).ok())
        .unwrap_or_default()
}

fn parse_u32(s: &str) -> Result<u32, String> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u32::from_str_radix(h, 16).map_err(|e| e.to_string())
    } else {
        s.parse::<u32>().map_err(|e| e.to_string())
    }
}

/// Map a filesystem error to a clear, user-facing message, flagging the common
/// read-only / permission case so the UI can offer a "Save As…" fallback.
fn write_error(path: &str, e: &std::io::Error) -> String {
    use std::io::ErrorKind::*;
    match e.kind() {
        PermissionDenied => format!(
            "permission denied writing {path} — the file is read-only or owned by another user. Use Save As… to write a copy you own.",
        ),
        _ => format!("could not write {path}: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Drivers
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct DriverInfo {
    id: String,
    name: String,
}

#[tauri::command]
fn list_drivers() -> Vec<DriverInfo> {
    registry()
        .list()
        .iter()
        .map(|e| DriverInfo {
            id: e.id.to_string(),
            name: e.display_name.to_string(),
        })
        .collect()
}

#[tauri::command]
fn data_paths(state: tauri::State<AppData>) -> Value {
    json!({
        "ecudefs": state.ecudefs.as_ref().map(|p| p.display().to_string()),
        "catalog": state.catalog.as_ref().map(|p| p.display().to_string()),
        "guide": state.guide.as_ref().map(|p| p.display().to_string()),
    })
}

// ---------------------------------------------------------------------------
// Image panel: size + hex preview
// ---------------------------------------------------------------------------

#[tauri::command]
fn image_info(path: String) -> Result<Value, String> {
    let meta = std::fs::metadata(&path).map_err(|e| e.to_string())?;
    let name = Path::new(&path)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    Ok(json!({ "path": path, "name": name, "size": meta.len() }))
}

#[tauri::command]
fn hex_preview(path: String, offset: u64, rows: usize) -> Result<Value, String> {
    let total = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let data = std::fs::read(&path).map_err(|e| e.to_string())?;
    let start = offset.min(total) as usize;
    let want = rows.max(1) * 16;
    let end = (start + want).min(data.len());
    let mut lines = Vec::new();
    let mut a = start;
    while a < end {
        let row = &data[a..(a + 16).min(end)];
        let hex: Vec<String> = row.iter().map(|b| format!("{:02X}", b)).collect();
        let ascii: String = row
            .iter()
            .map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' })
            .collect();
        lines.push(json!({ "addr": format!("0x{:08X}", a), "hex": hex, "ascii": ascii }));
        a += 16;
    }
    Ok(json!({ "offset": start, "total": total, "lines": lines }))
}

// ---------------------------------------------------------------------------
// Identify
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct DefMatch {
    id: String,
    name: String,
    maker: String,
    family: String,
    protocol: String,
    checksum: Option<String>,
    seedkey: Option<String>,
}

#[derive(Serialize)]
struct CatMatch {
    make: String,
    models: Vec<String>,
    years: Option<String>,
    family: Option<String>,
    source: Option<String>,
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

fn cat_matches(entries: &[CatalogEntry], id: &EcuId) -> Vec<CatMatch> {
    catalog::match_catalog(entries, id)
        .into_iter()
        .take(16)
        .map(|c| CatMatch {
            make: c.make.clone(),
            models: c.models.clone(),
            years: c.years.clone(),
            family: c.family.clone(),
            source: c.source.clone(),
        })
        .collect()
}

#[tauri::command]
fn identify(path: String, state: tauri::State<AppData>) -> Result<Value, String> {
    let mut drv = open_file(&path)?;
    let id: EcuId = drv.read_ecu_id().map_err(|e| e.to_string())?;
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

    let defs = load_defs(&state.ecudefs);
    let matched = ecudef::match_ecu(&defs, &id).map(def_match);
    let entries = load_cat(&state.catalog);
    let catalog = cat_matches(&entries, &id);

    Ok(json!({
        "ecu_name": id.ecu_name,
        "hw_number": id.hw_number,
        "sw_number": id.sw_number,
        "vin": id.vin,
        "size": size,
        "matched": matched,
        "catalog": catalog,
    }))
}

/// Offline identity matching: the mock driver reports only a name + size, so this
/// lets the operator type a real HW / SW number to exercise ecudef + catalog
/// matching against the bundled databases.
#[tauri::command]
fn match_identity(hw: String, sw: String, state: tauri::State<AppData>) -> Result<Value, String> {
    let id = EcuId {
        hw_number: (!hw.trim().is_empty()).then(|| hw.trim().to_string()),
        sw_number: (!sw.trim().is_empty()).then(|| sw.trim().to_string()),
        ..Default::default()
    };
    let defs = load_defs(&state.ecudefs);
    let matched = ecudef::match_ecu(&defs, &id).map(def_match);
    let entries = load_cat(&state.catalog);
    let catalog = cat_matches(&entries, &id);
    Ok(json!({ "matched": matched, "catalog": catalog }))
}

// ---------------------------------------------------------------------------
// Checksum (public algorithms only: edc16 / edc17 / pcr21 / crc32-tail)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct Block {
    name: String,
    ok: bool,
    region: String,
    stored: String,
    computed: String,
}

#[tauri::command]
fn checksum_verify(path: String, algo: String) -> Result<Vec<Block>, String> {
    let a = opentune_checksum::get(&algo).ok_or_else(|| format!("unknown algo '{algo}'"))?;
    let img = std::fs::read(&path).map_err(|e| e.to_string())?;
    Ok(a.verify(&img)
        .map_err(|e| e.to_string())?
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

/// Recompute and write correct checksums. Writes back to `out_path` when given
/// (Save-As fallback), otherwise in place over `path`. Returns the output path so
/// the UI can re-open the corrected copy.
#[tauri::command]
fn checksum_fix(path: String, algo: String, out_path: Option<String>) -> Result<Value, String> {
    let a = opentune_checksum::get(&algo).ok_or_else(|| format!("unknown algo '{algo}'"))?;
    let mut img = std::fs::read(&path).map_err(|e| e.to_string())?;
    let fixes = a.correct(&mut img).map_err(|e| e.to_string())?;
    let target = out_path.unwrap_or_else(|| path.clone());
    if !fixes.is_empty() || target != path {
        std::fs::write(&target, &img).map_err(|e| write_error(&target, &e))?;
    }
    Ok(json!({
        "out_path": target,
        "fixes": fixes
            .into_iter()
            .map(|f| format!("{} @0x{:08X}: 0x{:08X} -> 0x{:08X}", f.name, f.offset, f.old, f.new))
            .collect::<Vec<_>>(),
    }))
}

// ---------------------------------------------------------------------------
// Maps: findmaps diff + read-only map decode / viewer
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct RegionHit {
    index: usize,
    start: String,
    end: String,
    len: usize,
    hint: String,
}

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

#[tauri::command]
fn findmaps(stock: String, tuned: String, gap: usize, min_len: usize) -> Result<Vec<RegionHit>, String> {
    let a = std::fs::read(&stock).map_err(|e| e.to_string())?;
    let b = std::fs::read(&tuned).map_err(|e| e.to_string())?;
    let n = a.len().min(b.len());
    let mut out = Vec::new();
    let mut i = 0;
    let mut idx = 0;
    while i < n {
        if a[i] != b[i] {
            let start = i;
            let mut end = i + 1;
            let mut since = 0usize;
            let mut j = i + 1;
            while j < n && since <= gap {
                if a[j] != b[j] {
                    end = j + 1;
                    since = 0;
                } else {
                    since += 1;
                }
                j += 1;
            }
            if end - start >= min_len {
                out.push(RegionHit {
                    index: idx,
                    start: format!("0x{start:08X}"),
                    end: format!("0x{end:08X}"),
                    len: end - start,
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

#[derive(Serialize)]
struct DefInfo {
    id: String,
    name: String,
    maker: String,
    family: String,
    protocol: String,
    flash_kib: u32,
    checksum: Option<String>,
    seedkey: Option<String>,
    maps: usize,
    notes: String,
}

#[tauri::command]
fn list_ecudefs(state: tauri::State<AppData>) -> Vec<DefInfo> {
    load_defs(&state.ecudefs)
        .into_iter()
        .map(|d| DefInfo {
            protocol: format!("{:?}", d.protocol),
            flash_kib: d.flash_size / 1024,
            maps: d.maps.len(),
            notes: d.notes.clone(),
            checksum: d.checksum.clone(),
            seedkey: d.seedkey.clone(),
            maker: d.maker.clone(),
            family: d.family.clone(),
            name: d.name.clone(),
            id: d.id.clone(),
        })
        .collect()
}

#[tauri::command]
fn list_maps(ecudef_id: String, state: tauri::State<AppData>) -> Result<Vec<Value>, String> {
    let defs = load_defs(&state.ecudefs);
    let def = defs.iter().find(|d| d.id == ecudef_id).ok_or("ecudef not found")?;
    Ok(def
        .maps
        .iter()
        .map(|m| {
            json!({
                "name": m.name,
                "address": format!("0x{:08X}", m.address),
                "rows": m.rows,
                "cols": m.cols,
                "data_type": format!("{:?}", m.data_type),
                "endian": format!("{:?}", m.endian),
                "unit": m.unit,
                "factor": m.scaling.factor,
                "offset": m.scaling.offset,
                "min": m.min,
                "max": m.max,
                "byte_len": m.byte_len(),
                "notes": m.notes,
            })
        })
        .collect())
}

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
            let v = if le { u16::from_le_bytes([s[0], s[1]]) } else { u16::from_be_bytes([s[0], s[1]]) };
            v as f64
        }
        DataType::S16 => {
            let v = if le { i16::from_le_bytes([s[0], s[1]]) } else { i16::from_be_bytes([s[0], s[1]]) };
            v as f64
        }
        DataType::U32 => {
            let a = [s[0], s[1], s[2], s[3]];
            (if le { u32::from_le_bytes(a) } else { u32::from_be_bytes(a) }) as f64
        }
        DataType::S32 => {
            let a = [s[0], s[1], s[2], s[3]];
            (if le { i32::from_le_bytes(a) } else { i32::from_be_bytes(a) }) as f64
        }
        DataType::F32 => {
            let a = [s[0], s[1], s[2], s[3]];
            (if le { f32::from_le_bytes(a) } else { f32::from_be_bytes(a) }) as f64
        }
    })
}

/// Decode one axis to physical breakpoint values. Supports inline `values`,
/// stored (`address` + type), or falls back to 0..count indices (shared/ref).
fn decode_axis(img: &[u8], axis: &Option<AxisDef>, count: u32) -> (Vec<f64>, String) {
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

/// Decode a defined `MapDef` out of the opened image into a physical-value grid,
/// honouring the data type, endianness and linear scaling. Read-only: this is the
/// public edition's map viewer, so it only reads — it never writes the image.
#[tauri::command]
fn decode_map(
    path: String,
    ecudef_id: String,
    map_name: String,
    state: tauri::State<AppData>,
) -> Result<Value, String> {
    let defs = load_defs(&state.ecudefs);
    let def = defs.iter().find(|d| d.id == ecudef_id).ok_or("ecudef not found")?;
    let map: &MapDef = def.maps.iter().find(|m| m.name == map_name).ok_or("map not found")?;
    let img = std::fs::read(&path).map_err(|e| e.to_string())?;

    let rows = map.rows.max(1) as usize;
    let cols = map.cols.max(1) as usize;
    let w = map.data_type.byte_len() as usize;
    let need = map.address as usize + rows * cols * w;
    if need > img.len() {
        return Err(format!(
            "map needs 0x{:08X} bytes but the opened image is only 0x{:08X} — open a full dump for this ECU (its flash is {} KiB)",
            need,
            img.len(),
            def.flash_size / 1024
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
            let phys = read_elem(&img, off, map.data_type, map.endian)
                .map(|raw| map.scaling.to_physical(raw))
                .unwrap_or(0.0);
            lo = lo.min(phys);
            hi = hi.max(phys);
            row.push(phys);
        }
        cells.push(row);
    }
    let (x_vals, x_unit) = decode_axis(&img, &map.x_axis, map.cols);
    let (y_vals, y_unit) = decode_axis(&img, &map.y_axis, map.rows);

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

// ---------------------------------------------------------------------------
// Map editor: inverse scaling, byte-level write-back, map-by-map compare
// ---------------------------------------------------------------------------

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
fn encode_elem(phys: f64, dt: DataType, en: Endian, scaling: ecudef::Scaling) -> Vec<u8> {
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
fn decode_cells(img: &[u8], map: &MapDef) -> Option<Vec<Vec<f64>>> {
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

/// Write an edited calibration map back into a flash image.
///
/// Safety is the whole point of this command, in order:
/// 1. Refuse on a dimension mismatch (the grid must be exactly `rows × cols`).
/// 2. Refuse on a zero scaling factor (physical→raw would divide by zero).
/// 3. Refuse if the map region runs past the end of the image.
/// 4. Take a **full-image backup into the flash vault FIRST** — if that fails,
///    abort before touching anything, so an edit can never happen un-backed-up.
/// 5. Apply inverse scaling (`raw = (phys − offset) / factor`), round + clamp to
///    the data type's range, and write ONLY the map's byte region (type+endian).
/// 6. Optionally run a checksum fix (reusing [`checksum_fix`] with the def's algo).
///
/// `out_path` writes a Save-As copy instead of editing `path` in place (the
/// read-only / permission fallback). On a write error the message names the
/// safety backup so the original is always recoverable.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn map_write(
    path: String,
    ecudef_id: String,
    map_name: String,
    new_cells: Vec<Vec<f64>>,
    out_path: Option<String>,
    fix_checksum: Option<bool>,
    state: tauri::State<AppData>,
) -> Result<Value, String> {
    let defs = load_defs(&state.ecudefs);
    let def = defs.iter().find(|d| d.id == ecudef_id).ok_or("ecudef not found")?;
    let map: &MapDef = def.maps.iter().find(|m| m.name == map_name).ok_or("map not found")?;

    let rows = map.rows.max(1) as usize;
    let cols = map.cols.max(1) as usize;

    // (1) Dimension guard — reject before reading anything.
    if new_cells.len() != rows || new_cells.iter().any(|r| r.len() != cols) {
        let got_cols = new_cells.first().map(|r| r.len()).unwrap_or(0);
        return Err(format!(
            "dimension mismatch: map '{}' is {rows}×{cols} but the edit is {}×{got_cols} — refusing to write",
            map.name,
            new_cells.len(),
        ));
    }
    // (2) Zero scaling factor guard — cannot invert physical→raw.
    if map.scaling.factor == 0.0 {
        return Err(format!(
            "map '{}' has a zero scaling factor — physical→raw is undefined, refusing to write",
            map.name
        ));
    }

    let mut img = std::fs::read(&path).map_err(|e| e.to_string())?;
    let w = map.data_type.byte_len() as usize;
    let need = map.address as usize + rows * cols * w;
    // (3) Bounds guard — never write past the end of the image.
    if need > img.len() {
        return Err(format!(
            "map needs 0x{:08X} bytes but the image is only 0x{:08X} — open a full dump (its flash is {} KiB)",
            need,
            img.len(),
            def.flash_size / 1024
        ));
    }

    // (4) BACKUP FIRST: archive the whole original image into the flash vault,
    // beside the file being edited. If this fails we abort before any edit.
    let vault_dir = Path::new(&path)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("opentune-vault");
    let ecu = EcuId {
        ecu_name: Some(format!("{} [{}]", def.name, def.id)),
        hw_number: def.identify.part_numbers.first().cloned(),
        sw_number: def.identify.sw_numbers.first().cloned(),
        ..Default::default()
    };
    let whole = MemoryRegion::new(0, img.len() as u32);
    let backup = opentune_flash::vault::save_backup(&vault_dir, &ecu, whole, &img).map_err(|e| {
        format!(
            "could not write the safety backup to {} — aborting before any edit: {e}",
            vault_dir.display()
        )
    })?;

    // (5) Apply the edit: inverse scaling, clamp, write only the map's bytes.
    let mut cells_changed = 0usize;
    for r in 0..rows {
        for c in 0..cols {
            let off = map.address as usize + (r * cols + c) * w;
            let bytes = encode_elem(new_cells[r][c], map.data_type, map.endian, map.scaling);
            if img[off..off + w] != bytes[..] {
                cells_changed += 1;
            }
            img[off..off + w].copy_from_slice(&bytes);
        }
    }

    let target = out_path.unwrap_or_else(|| path.clone());
    std::fs::write(&target, &img).map_err(|e| {
        format!(
            "{} — your original is safe: a full backup was saved to {}",
            write_error(&target, &e),
            backup.display()
        )
    })?;

    // (6) Optional checksum fix, reusing the existing command with the def's algo.
    let mut checksum = Value::Null;
    if fix_checksum.unwrap_or(false) {
        checksum = match &def.checksum {
            Some(algo) => match checksum_fix(target.clone(), algo.clone(), None) {
                Ok(v) => v,
                Err(e) => json!({ "algo": algo, "error": e }),
            },
            None => json!({ "error": "this ECU definition has no checksum algorithm set" }),
        };
    }

    Ok(json!({
        "out_path": target,
        "backup": backup.display().to_string(),
        "in_place": target == path,
        "cells_total": rows * cols,
        "cells_changed": cells_changed,
        "checksum_algo": def.checksum,
        "checksum": checksum,
    }))
}

/// Compare two images map-by-map through an ECU definition, mirroring the CLI
/// `compare` command. Returns, per `MapDef`: changed-cell count and Δmax / Δavg
/// in physical units (or an `out_of_range` flag). When `map_name` is given, that
/// one map also carries the full before/after physical grids + axes so the UI can
/// draw a before→after view.
#[tauri::command]
fn compare_maps(
    def_id: String,
    stock: String,
    tuned: String,
    map_name: Option<String>,
    state: tauri::State<AppData>,
) -> Result<Value, String> {
    let defs = load_defs(&state.ecudefs);
    let def = defs.iter().find(|d| d.id == def_id).ok_or("ecudef not found")?;
    if def.maps.is_empty() {
        return Err(format!("definition '{def_id}' has no maps to compare"));
    }
    let a = std::fs::read(&stock).map_err(|e| e.to_string())?;
    let b = std::fs::read(&tuned).map_err(|e| e.to_string())?;

    let want_grid = map_name.as_deref();
    let mut maps = Vec::with_capacity(def.maps.len());
    let mut total_changed = 0usize;
    let mut changed_maps = 0usize;

    for m in &def.maps {
        let (pa, pb) = match (decode_cells(&a, m), decode_cells(&b, m)) {
            (Some(pa), Some(pb)) => (pa, pb),
            _ => {
                maps.push(json!({
                    "name": m.name, "rows": m.rows, "cols": m.cols,
                    "unit": m.unit, "out_of_range": true,
                }));
                continue;
            }
        };
        let mut changed = 0usize;
        let mut max_abs = 0f64;
        let mut sum_delta = 0f64;
        for (ra, rb) in pa.iter().zip(pb.iter()) {
            for (x, y) in ra.iter().zip(rb.iter()) {
                let d = y - x;
                if d.abs() > 1e-9 {
                    changed += 1;
                    sum_delta += d;
                    if d.abs() > max_abs {
                        max_abs = d.abs();
                    }
                }
            }
        }
        total_changed += changed;
        if changed > 0 {
            changed_maps += 1;
        }
        let avg = if changed > 0 { sum_delta / changed as f64 } else { 0.0 };
        let mut entry = json!({
            "name": m.name,
            "rows": m.rows,
            "cols": m.cols,
            "unit": m.unit,
            "data_type": format!("{:?}", m.data_type),
            "changed": changed,
            "d_max": max_abs,
            "d_avg": avg,
            "out_of_range": false,
        });
        if want_grid == Some(m.name.as_str()) {
            let (x_vals, x_unit) = decode_axis(&a, &m.x_axis, m.cols);
            let (y_vals, y_unit) = decode_axis(&a, &m.y_axis, m.rows);
            entry["stock"] = json!(pa);
            entry["tuned"] = json!(pb);
            entry["x_axis"] =
                json!({ "name": m.x_axis.as_ref().map(|ax| ax.name.clone()), "unit": x_unit, "values": x_vals });
            entry["y_axis"] =
                json!({ "name": m.y_axis.as_ref().map(|ax| ax.name.clone()), "unit": y_unit, "values": y_vals });
        }
        maps.push(entry);
    }

    Ok(json!({
        "def": def.id,
        "def_name": def.name,
        "stock_size": a.len(),
        "tuned_size": b.len(),
        "total_changed": total_changed,
        "changed_maps": changed_maps,
        "maps": maps,
    }))
}

// ---------------------------------------------------------------------------
// Flash (safety-gated, mock)
// ---------------------------------------------------------------------------

#[tauri::command]
fn flash(path: String, addr: String, data_path: String, min_mv: u32) -> Result<Value, String> {
    let addr = parse_u32(&addr)?;
    let bytes = std::fs::read(&data_path).map_err(|e| e.to_string())?;
    let region = MemoryRegion::new(addr, bytes.len() as u32);
    let mut drv = open_file(&path)?;
    let safety = opentune_flash::SafetyConfig {
        min_millivolts: min_mv,
        ..Default::default()
    };
    let rep = opentune_flash::flash_calibration(drv.as_mut(), region, &bytes, &safety, &mut |_| {})
        .map_err(|e| e.to_string())?;
    Ok(json!({
        "bytes_written": rep.bytes_written,
        "battery_mv": rep.battery_millivolts,
        "backup_len": rep.backup.len(),
        "verified": rep.verified,
        "region": format!("0x{:08X} +{} (0x{:08X})", region.address, region.length, region.length),
    }))
}

// ---------------------------------------------------------------------------
// Vault (backup directory listing)
// ---------------------------------------------------------------------------

#[tauri::command]
fn vault_list(dir: String) -> Result<Vec<Value>, String> {
    let p = Path::new(&dir);
    if !p.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(p).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.extension().and_then(|s| s.to_str()) != Some("bin") {
            continue;
        }
        let bin_len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let meta_path = path.with_extension("meta");
        let meta_txt = std::fs::read_to_string(&meta_path).unwrap_or_default();
        out.push(json!({
            "name": path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
            "path": path.display().to_string(),
            "size": bin_len,
            "has_meta": meta_path.exists(),
            "meta": meta_txt,
        }));
    }
    out.sort_by(|a, b| a["name"].as_str().unwrap_or("").cmp(b["name"].as_str().unwrap_or("")));
    Ok(out)
}

// ---------------------------------------------------------------------------
// Guide / in-app wiki — bundled Markdown rendered in the frontend
// ---------------------------------------------------------------------------

/// Pretty section title from a `NN-slug.md` filename: `01-hardware.md` -> "Hardware".
fn guide_title(stem: &str) -> String {
    let slug = stem.splitn(2, '-').nth(1).unwrap_or(stem);
    let mut out = String::new();
    for (i, word) in slug.split(['-', '_']).enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let mut cs = word.chars();
        if let Some(f) = cs.next() {
            out.extend(f.to_uppercase());
            out.push_str(cs.as_str());
        }
    }
    out
}

#[tauri::command]
fn guide_list(state: tauri::State<AppData>) -> Result<Vec<Value>, String> {
    let dir = state.guide.as_ref().ok_or("guide resources not found")?;
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("md"))
        .collect();
    files.sort();
    Ok(files
        .iter()
        .filter_map(|p| {
            let stem = p.file_stem()?.to_str()?;
            Some(json!({ "id": stem, "title": guide_title(stem) }))
        })
        .collect())
}

#[tauri::command]
fn guide_read(id: String, state: tauri::State<AppData>) -> Result<String, String> {
    let dir = state.guide.as_ref().ok_or("guide resources not found")?;
    // Guard against path traversal: only a bare NN-slug stem is accepted.
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err("invalid guide id".into());
    }
    let path = dir.join(format!("{id}.md"));
    std::fs::read_to_string(&path).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Setup
// ---------------------------------------------------------------------------

fn resolve_data(app: &tauri::App) -> AppData {
    let res = app.path().resource_dir().ok();
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")); // desktop/src-tauri
    let repo = manifest.join("../.."); // repo root in dev
    let env_dir = std::env::var("OPENTUNE_DATA").ok().map(PathBuf::from);

    // Try, in order: OPENTUNE_DATA override, the bundled resource dir (several
    // layouts Tauri may produce), a dev copy next to src-tauri, then the repo.
    let find = |name: &str| -> Option<PathBuf> {
        let mut cands: Vec<PathBuf> = Vec::new();
        if let Some(e) = &env_dir {
            cands.push(e.join(name));
        }
        if let Some(r) = &res {
            cands.push(r.join(name));
            cands.push(r.join("data").join(name));
            cands.push(r.join("_up_").join(name));
            cands.push(r.join("_up_").join("_up_").join(name)); // ../../<name> flattening
        }
        cands.push(manifest.join(name));
        cands.push(repo.join(name));
        first_existing(&cands)
    };

    AppData {
        ecudefs: find("ecudefs"),
        catalog: find("catalog"),
        guide: find("guide"),
    }
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let data = resolve_data(app);
            app.manage(data);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_drivers,
            data_paths,
            image_info,
            hex_preview,
            identify,
            match_identity,
            checksum_verify,
            checksum_fix,
            findmaps,
            list_ecudefs,
            list_maps,
            decode_map,
            map_write,
            compare_maps,
            flash,
            vault_list,
            guide_list,
            guide_read,
        ])
        .run(tauri::generate_context!())
        .expect("error while running OpenTune");
}

// ---------------------------------------------------------------------------
// Tests — the pure calibration-editor maths (no Tauri / no filesystem).
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use ecudef::Scaling;

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
        assert_eq!(encode_elem(10.4, DataType::U8, Endian::Big, scaling), vec![10]);
        assert_eq!(encode_elem(10.6, DataType::U8, Endian::Big, scaling), vec![11]);
    }

    #[test]
    fn encode_clamps_to_type_range() {
        let scaling = sc(1.0, 0.0);
        // Over max / under min must saturate, never wrap.
        assert_eq!(encode_elem(999.0, DataType::U8, Endian::Big, scaling), vec![255]);
        assert_eq!(encode_elem(-5.0, DataType::U8, Endian::Big, scaling), vec![0]);
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
        let grid = vec![vec![1.0, 2.5], vec![3.0, 4.5]];
        let w = map.data_type.byte_len() as usize;
        let mut img = vec![0u8; 2 * 2 * w];
        for r in 0..2 {
            for c in 0..2 {
                let off = (r * 2 + c) * w;
                let b = encode_elem(grid[r][c], map.data_type, map.endian, map.scaling);
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
