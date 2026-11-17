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
use std::sync::{Arc, Mutex};
use tauri::{Emitter, Manager};
use tauri_plugin_updater::UpdaterExt;

// Live-diagnostics feature panels (each self-contained; they reuse the shared
// adapter opener `debug_open_diag` and the public engine crates). The guided
// Auto-debug wizard is UI-only (it orchestrates these commands from the browser).
mod feat_adaptation;
mod feat_coding;
mod feat_logging;
mod feat_obd;
mod feat_scan;
mod feat_service;

// ---------------------------------------------------------------------------
// Long-operation guard
// ---------------------------------------------------------------------------
//
// Destructive / long-running operations (a mock flash, a map write-back) must
// never overlap with a software update: an in-place binary replacement while a
// flash is mid-write could corrupt the running session. `OpGuard` is a single
// managed slot naming the one long operation in flight (or `None` when idle).
//
// `begin()` claims the slot and hands back an `OpToken`; dropping the token
// (normally, on panic, or via `?`) frees the slot, so a failed command can
// never leave the app wedged as "busy". The update flow claims the same slot,
// so a flash cannot start mid-update and an update cannot start mid-flash.
#[derive(Clone, Default)]
struct OpGuard(Arc<Mutex<Option<String>>>);

/// RAII release token: the long operation is in flight for as long as this is
/// alive, and the guard slot is cleared the instant it drops.
#[derive(Debug)]
struct OpToken(Arc<Mutex<Option<String>>>);

impl Drop for OpToken {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = None;
        }
    }
}

impl OpGuard {
    /// Claim the single long-operation slot for `name`, or return a clear error
    /// naming whatever is already running. The returned token frees the slot on
    /// drop.
    fn begin(&self, name: &str) -> Result<OpToken, String> {
        let mut slot = self
            .0
            .lock()
            .map_err(|_| "internal error: operation guard was poisoned".to_string())?;
        if let Some(cur) = slot.as_ref() {
            return Err(format!(
                "'{cur}' is already running — wait for it to finish before starting another operation"
            ));
        }
        *slot = Some(name.to_string());
        Ok(OpToken(self.0.clone()))
    }

    /// Name of the long operation currently in flight, if any.
    fn current(&self) -> Option<String> {
        self.0.lock().ok().and_then(|s| s.clone())
    }
}

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
    r.register("elm327obd", "ELM327 serial (OBD diagnostics)", || {
        Box::new(opentune_driver_elm327_obd::Elm327Obd::new())
    });
    r.register("elm327net", "ELM327 over WiFi / Bluetooth", || {
        Box::new(opentune_driver_elm327net::Elm327NetDriver::new())
    });
    r.register("gsusb", "gs_usb / candleLight USB-CAN", || {
        Box::new(opentune_driver_gsusb::GsUsbDriver::new())
    });
    r.register("j2534", "J2534 PassThru (generic)", || {
        Box::new(opentune_driver_j2534::J2534Driver::new())
    });
    r
}

/// Build channel: `"alpha"` when compiled with the `alpha` feature (the Debug
/// panel is available), else `"stable"`. The UI shows the Debug tab only for
/// `"alpha"`.
#[tauri::command]
fn build_channel() -> &'static str {
    if cfg!(feature = "alpha") {
        "alpha"
    } else {
        "stable"
    }
}

// ---------------------------------------------------------------------------
// Debug panel (alpha builds) — live engines + a session log
// ---------------------------------------------------------------------------
//
// These commands drive real adapters and the service engines. They refuse to
// run unless this is an alpha build, and every exchange is appended to a
// session log so a run can be saved and handed back for debugging.

/// A shared, in-memory session log of diagnostic exchanges.
#[derive(Clone, Default)]
struct DebugLog(Arc<Mutex<Vec<String>>>);

impl DebugLog {
    fn push(&self, line: impl Into<String>) {
        if let Ok(mut v) = self.0.lock() {
            v.push(line.into());
        }
    }
}

/// Gate: debug commands run only in an alpha build.
fn alpha_only() -> Result<(), String> {
    if cfg!(feature = "alpha") {
        Ok(())
    } else {
        Err("debug commands are available only in alpha builds".into())
    }
}

/// Uppercase space-separated hex.
fn hexs(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parse a whitespace-tolerant hex string.
fn parse_hex(s: &str) -> Result<Vec<u8>, String> {
    let clean: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if clean.len() % 2 != 0 {
        return Err(format!("hex string '{s}' has an odd number of digits"));
    }
    (0..clean.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&clean[i..i + 2], 16).map_err(|_| format!("invalid hex in '{s}'")))
        .collect()
}

/// Open a driver that exposes a diagnostic transport, applying an optional serial
/// port/baud (for e.g. `elm327obd`) and CAN bus select (`hs`/`ms`). For a switched
/// ELM327 the `bus` sets the CAN speed, which must match the dongle's physical
/// HS/MS-CAN switch. Shared by the probe, the Debug tab, and every feature panel.
pub(crate) fn debug_open_diag(
    driver: &str,
    port: Option<&str>,
    baud: Option<&str>,
    bus: Option<&str>,
) -> Result<Box<dyn Interface>, String> {
    let reg = registry();
    let mut drv = reg
        .make(driver)
        .ok_or_else(|| format!("unknown driver '{driver}'"))?;
    let mut cfg = ConnectConfig::default();
    if let Some(p) = port {
        if !p.trim().is_empty() {
            cfg.options.insert("port".into(), p.to_string());
        }
    }
    if let Some(b) = baud {
        if !b.trim().is_empty() {
            cfg.options.insert("baud".into(), b.to_string());
        }
    }
    if let Some(bus) = bus {
        if !bus.trim().is_empty() {
            cfg.options.insert("bus".into(), bus.to_string());
        }
    }
    drv.open(&cfg).map_err(|e| format!("open {driver}: {e}"))?;
    Ok(drv)
}

/// Send one raw UDS/OBD request to a live (or mock) adapter and return the hex
/// response. The exchange is appended to the session log. Async so the blocking
/// serial I/O never runs on the UI thread (a sync command would freeze the app).
#[tauri::command]
async fn debug_raw_request(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    request: String,
    timeout_ms: u64,
    log: tauri::State<'_, DebugLog>,
) -> Result<String, String> {
    alpha_only()?;
    let req = parse_hex(&request)?;
    let mut drv = debug_open_diag(&driver, port.as_deref(), baud.as_deref(), bus.as_deref())?;
    let transport = drv
        .diag()
        .ok_or_else(|| format!("driver '{driver}' has no diagnostic transport"))?;
    let t0 = std::time::Instant::now();
    match transport.request(&req, timeout_ms) {
        Ok(resp) => {
            let ms = t0.elapsed().as_millis();
            let hex = hexs(&resp);
            log.push(format!("-> {}  <- {hex}  ({ms} ms)", hexs(&req)));
            Ok(hex)
        }
        Err(e) => {
            let m = format!("{e}");
            log.push(format!("-> {}  ERR {m}", hexs(&req)));
            Err(m)
        }
    }
}

/// Probe for an adapter: open the selected driver (which, for `elm327obd`, runs
/// the ELM327 handshake) and report whether it answered, its firmware banner,
/// battery voltage and capabilities. A general app-wide command (not debug-only):
/// the sidebar shows the adapter status everywhere, and the Debug tab shows the
/// detail. Async so the handshake never blocks the UI thread.
#[tauri::command]
async fn probe_adapter(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    log: tauri::State<'_, DebugLog>,
) -> Result<Value, String> {
    match debug_open_diag(&driver, port.as_deref(), baud.as_deref(), bus.as_deref()) {
        Ok(mut drv) => {
            let info = drv.info();
            let caps = drv.capabilities();
            let battery_mv = drv.battery_millivolts().ok().flatten();
            log.push(format!(
                "probe {driver}: FOUND {} {}",
                info.display_name,
                info.firmware.clone().unwrap_or_default()
            ));
            Ok(json!({
                "connected": true,
                "driver": info.id,
                "name": info.display_name,
                "firmware": info.firmware,
                "serial": info.serial,
                "battery_mv": battery_mv,
                "capabilities": {
                    "can": caps.can,
                    "can_fd": caps.can_fd,
                    "kline": caps.kline,
                    "read_memory": caps.read_memory,
                    "write_memory": caps.write_memory,
                    "bench_boot": caps.bench_boot,
                    "battery_voltage": caps.battery_voltage,
                    "programming_voltage": caps.programming_voltage,
                },
            }))
        }
        Err(e) => {
            log.push(format!("probe {driver}: {e}"));
            Ok(json!({ "connected": false, "error": e }))
        }
    }
}

/// Run the in-memory UDS conformance server offline: seed DIDs, handle raw
/// requests, return the hex responses. No hardware.
#[tauri::command]
fn debug_simulate(
    dids: Vec<String>,
    requests: Vec<String>,
    require_security_write: bool,
    log: tauri::State<DebugLog>,
) -> Result<Vec<String>, String> {
    use opentune_udssim::server::{ServerConfig, UdsServer};
    alpha_only()?;
    let cfg = ServerConfig {
        require_security_for_write: require_security_write,
        ..Default::default()
    };
    let mut server = UdsServer::with_config(cfg);
    for d in &dids {
        let (id, hx) = d
            .split_once('=')
            .ok_or_else(|| format!("expected ID=HEX, got '{d}'"))?;
        let did = u16::from_str_radix(id.trim().trim_start_matches("0x"), 16)
            .map_err(|_| format!("invalid DID '{id}'"))?;
        server.set_did(did, parse_hex(hx)?);
    }
    let mut out = Vec::new();
    for r in &requests {
        let req = parse_hex(r)?;
        let resp = server.handle(&req);
        let hex = hexs(&resp);
        log.push(format!("sim -> {}  <- {hex}", hexs(&req)));
        out.push(hex);
    }
    Ok(out)
}

/// Return the current session log.
#[tauri::command]
fn debug_log_dump(log: tauri::State<DebugLog>) -> Vec<String> {
    log.0.lock().map(|v| v.clone()).unwrap_or_default()
}

/// Clear the session log.
#[tauri::command]
fn debug_log_clear(log: tauri::State<DebugLog>) {
    if let Ok(mut v) = log.0.lock() {
        v.clear();
    }
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

/// Decode a defined `MapDef` out of `img` into the read-only viewer/editor JSON
/// payload (physical-value grid + axes + scaling + bounds). Shared by the
/// definition-backed [`decode_map`] and the ad-hoc [`decode_mapdef`] paths so
/// an imported / promoted map renders identically to a bundled one.
///
/// `flash_kib` is only used to enrich the "image too small" error for a
/// definition-backed map (where we know the ECU's flash size); `None` for an
/// ad-hoc map.
fn decode_map_value(img: &[u8], map: &MapDef, flash_kib: Option<u32>) -> Result<Value, String> {
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
    decode_map_value(&img, map, Some(def.flash_size / 1024))
}

/// Decode an **ad-hoc** `MapDef` (imported from an XDF/A2L or promoted from a
/// findmaps region) out of the opened image, without it belonging to any bundled
/// definition. Returns the same viewer/editor payload as [`decode_map`], plus a
/// `mapdef` field echoing the exact MapDef so the frontend can hand it back to
/// [`mapdef_write`] for a byte-level write. This closes the import/promote → edit
/// loop the CLI importers could not reach.
#[tauri::command]
fn decode_mapdef(path: String, map: MapDef) -> Result<Value, String> {
    let img = std::fs::read(&path).map_err(|e| e.to_string())?;
    let mut v = decode_map_value(&img, &map, None)?;
    v["mapdef"] = serde_json::to_value(&map).map_err(|e| e.to_string())?;
    Ok(v)
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
/// Shared write-back core for both the definition-backed [`map_write`] and the
/// ad-hoc [`mapdef_write`]. Carries the full safety sequence (dimension / zero-
/// factor / bounds guards, backup-first, byte-level write, optional checksum
/// fix). `ecu` labels the vault backup; `algo` is the checksum id to run after a
/// successful write (or `None` to skip even when `fix_checksum` is set, which is
/// reported honestly).
#[allow(clippy::too_many_arguments)]
fn write_map_core(
    path: &str,
    map: &MapDef,
    new_cells: &[Vec<f64>],
    out_path: Option<String>,
    fix_checksum: bool,
    algo: Option<String>,
    ecu: EcuId,
    flash_kib: Option<u32>,
) -> Result<Value, String> {
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

    let mut img = std::fs::read(path).map_err(|e| e.to_string())?;
    let w = map.data_type.byte_len() as usize;
    let need = map.address as usize + rows * cols * w;
    // (3) Bounds guard — never write past the end of the image.
    if need > img.len() {
        let hint = match flash_kib {
            Some(kib) => format!(" — open a full dump (its flash is {kib} KiB)"),
            None => " — open a full dump this map fits in".to_string(),
        };
        return Err(format!(
            "map needs 0x{:08X} bytes but the image is only 0x{:08X}{hint}",
            need,
            img.len(),
        ));
    }

    // (4) BACKUP FIRST: archive the whole original image into the flash vault,
    // beside the file being edited. If this fails we abort before any edit.
    let vault_dir = Path::new(path)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("opentune-vault");
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

    let target = out_path.unwrap_or_else(|| path.to_string());
    std::fs::write(&target, &img).map_err(|e| {
        format!(
            "{} — your original is safe: a full backup was saved to {}",
            write_error(&target, &e),
            backup.display()
        )
    })?;

    // (6) Optional checksum fix, reusing the existing command with the map's algo.
    let mut checksum = Value::Null;
    if fix_checksum {
        checksum = match &algo {
            Some(a) => match checksum_fix(target.clone(), a.clone(), None) {
                Ok(v) => v,
                Err(e) => json!({ "algo": a, "error": e }),
            },
            None => json!({ "error": "no checksum algorithm is set for this map — pick one on the Checksum panel and re-run it manually" }),
        };
    }

    Ok(json!({
        "out_path": target,
        "backup": backup.display().to_string(),
        "in_place": target == path,
        "cells_total": rows * cols,
        "cells_changed": cells_changed,
        "checksum_algo": algo,
        "checksum": checksum,
    }))
}

#[tauri::command]
fn map_write(
    path: String,
    ecudef_id: String,
    map_name: String,
    new_cells: Vec<Vec<f64>>,
    out_path: Option<String>,
    fix_checksum: Option<bool>,
    state: tauri::State<AppData>,
    guard: tauri::State<OpGuard>,
) -> Result<Value, String> {
    // A map write-back takes a full-image backup and rewrites the image; hold the
    // long-operation slot so a software update cannot land in the middle of it.
    let _op = guard.begin("map write-back")?;
    let defs = load_defs(&state.ecudefs);
    let def = defs.iter().find(|d| d.id == ecudef_id).ok_or("ecudef not found")?;
    let map: &MapDef = def.maps.iter().find(|m| m.name == map_name).ok_or("map not found")?;

    let ecu = EcuId {
        ecu_name: Some(format!("{} [{}]", def.name, def.id)),
        hw_number: def.identify.part_numbers.first().cloned(),
        sw_number: def.identify.sw_numbers.first().cloned(),
        ..Default::default()
    };
    write_map_core(
        &path,
        map,
        &new_cells,
        out_path,
        fix_checksum.unwrap_or(false),
        def.checksum.clone(),
        ecu,
        Some(def.flash_size / 1024),
    )
}

/// Write-back for an **ad-hoc** `MapDef` (imported/promoted, not in any bundled
/// definition). Same full safety sequence as [`map_write`]; the checksum family
/// is supplied by the caller (`checksum_algo`) since an ad-hoc map has no
/// definition to read one from.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn mapdef_write(
    path: String,
    map: MapDef,
    new_cells: Vec<Vec<f64>>,
    out_path: Option<String>,
    fix_checksum: Option<bool>,
    checksum_algo: Option<String>,
    guard: tauri::State<OpGuard>,
) -> Result<Value, String> {
    // Same long-operation guard as the definition-backed write path.
    let _op = guard.begin("map write-back")?;
    let ecu = EcuId {
        ecu_name: Some(format!("ad-hoc map '{}'", map.name)),
        ..Default::default()
    };
    // Normalise an empty string to "no algorithm".
    let algo = checksum_algo.filter(|a| !a.trim().is_empty());
    write_map_core(
        &path,
        &map,
        &new_cells,
        out_path,
        fix_checksum.unwrap_or(false),
        algo,
        ecu,
        None,
    )
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
fn flash(
    path: String,
    addr: String,
    data_path: String,
    min_mv: u32,
    guard: tauri::State<OpGuard>,
) -> Result<Value, String> {
    // Hold the long-operation slot for the whole flash so a software update
    // cannot replace the binary mid-write (freed automatically on return/error).
    let _op = guard.begin("flash")?;
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
// Feature 1 — offline "Analyze bin" (no hardware)
// ---------------------------------------------------------------------------

/// Map a detected structural family name to the checksum algorithm id that
/// verifies its protected blocks.
fn family_to_algo(family: &str) -> Option<&'static str> {
    match family {
        "EDC16" => Some("edc16"),
        "EDC17" => Some("edc17"),
        "PCR2.1" => Some("pcr21"),
        "EDC15" => Some("edc15"),
        _ => None,
    }
}

/// Serialize a parsed part number into a readable breakdown (shared by Analyze
/// and the part-decoder utility panel).
fn part_breakdown(pn: &opentune_vin::PartNumber) -> Value {
    json!({
        "normalized": pn.normalized,
        "kind": format!("{:?}", pn.kind),
        "supplier": pn.supplier,
        "vag_type_group": pn.vag_type_group,
        "vag_main_group": pn.vag_main_group,
        "vag_subgroup": pn.vag_subgroup,
        "vag_version": pn.vag_version,
        "bosch_family": pn.bosch_family,
        "bosch_consecutive": pn.bosch_consecutive,
        "flags": pn.flags,
    })
}

/// Fully-offline analysis of a raw flash image: structural ECU family, identity
/// strings, catalog/definition match, checksum blocks and any known maps —
/// everything determinable with NO hardware. This is the honest fix for the old
/// `identify` gap (the mock driver reports almost nothing on a raw `.bin`).
///
/// `ecudefs_dir` / `catalog_dir` override the bundled data directories; both
/// fall back to the resolved app data. Confidence is graded by opentune-scan,
/// which never assigns a car it cannot justify from a DB/VIN/part match.
#[tauri::command]
fn analyze_image(
    path: String,
    ecudefs_dir: Option<String>,
    catalog_dir: Option<String>,
    state: tauri::State<AppData>,
) -> Result<Value, String> {
    let ecudefs_dir = ecudefs_dir.map(PathBuf::from).or_else(|| state.ecudefs.clone());
    let catalog_dir = catalog_dir.map(PathBuf::from).or_else(|| state.catalog.clone());
    let ecudefs = load_defs(&ecudefs_dir);
    let catalog = load_cat(&catalog_dir);
    let ctx = opentune_scan::ScanContext {
        catalog,
        ecudefs: ecudefs.clone(),
    };

    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    let p = Path::new(&path);
    let root = p.parent().unwrap_or_else(|| Path::new("."));
    let rec = opentune_scan::scan_file(&ctx, root, p, &bytes);

    // Checksum blocks for the detected family (best effort).
    let algo_id = family_to_algo(rec.ecu_family);
    let checksum_blocks: Vec<Value> = match algo_id.and_then(opentune_checksum::get) {
        Some(a) => a
            .verify(&bytes)
            .map(|blocks| {
                blocks
                    .into_iter()
                    .map(|b| {
                        json!({
                            "name": b.name,
                            "ok": b.ok,
                            "region": format!("0x{:08X} +{}", b.region.address, b.region.length),
                            "stored": format!("0x{:08X}", b.stored),
                            "computed": format!("0x{:08X}", b.computed),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        None => Vec::new(),
    };

    // Known maps from a matched definition.
    let maps: Vec<Value> = rec
        .ecudef_match
        .as_ref()
        .and_then(|hit| ecudefs.iter().find(|d| d.id == hit.id))
        .map(|def| {
            def.maps
                .iter()
                .map(|m| {
                    json!({
                        "name": m.name,
                        "address": format!("0x{:08X}", m.address),
                        "rows": m.rows,
                        "cols": m.cols,
                        "data_type": format!("{:?}", m.data_type),
                        "unit": m.unit,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    // Identity-string breakdowns.
    let best_part = rec
        .part_numbers
        .first()
        .or_else(|| rec.bosch_numbers.first())
        .or_else(|| rec.mercedes_numbers.first());
    let part_decoded = best_part.map(|p| part_breakdown(&opentune_vin::parse_part_number(p)));

    Ok(json!({
        "path": path,
        "size": bytes.len(),
        "size_class": rec.size_class,
        "family": rec.ecu_family,
        "family_how": rec.family_how,
        "part_numbers": rec.part_numbers,
        "bosch_numbers": rec.bosch_numbers,
        "sw_numbers": rec.sw_numbers,
        "mercedes_numbers": rec.mercedes_numbers,
        "epk_hits": rec.epk_hits,
        "vin": rec.vin,
        "vin_decoded": rec.vin_decoded,
        "part_decoded": part_decoded,
        "catalog_match": rec.catalog_match,
        "ecudef_match": rec.ecudef_match,
        "assignment": rec.assigned_core,
        "confidence": rec.confidence_core.as_str(),
        "checksum_algo": algo_id,
        "checksum_blocks": checksum_blocks,
        "maps": maps,
    }))
}

// ---------------------------------------------------------------------------
// Feature 2 — raw-bin → named maps: import XDF/A2L, promote a region
// ---------------------------------------------------------------------------

fn parse_data_type(s: &str) -> Result<DataType, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "u8" => Ok(DataType::U8),
        "s8" | "i8" => Ok(DataType::S8),
        "u16" => Ok(DataType::U16),
        "s16" | "i16" => Ok(DataType::S16),
        "u32" => Ok(DataType::U32),
        "s32" | "i32" => Ok(DataType::S32),
        "f32" | "float" => Ok(DataType::F32),
        other => Err(format!("unknown data type '{other}' (use u8/s8/u16/s16/u32/s32/f32)")),
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
            json!(if m.rows <= 1 || m.cols <= 1 { "1D/scalar" } else { "2D" }),
        );
    }
    v
}

/// Import a TunerPro **XDF** or ASAM **A2L** definition into a list of MapDefs
/// the user can pick from and open in the editor (via [`decode_mapdef`]). Format
/// is sniffed from the extension then the content. For A2L, `base` optionally
/// rebases CPU addresses to file offsets (`addr -= base`), mirroring the CLI.
#[tauri::command]
fn import_definitions(path: String, base: Option<String>) -> Result<Value, String> {
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
fn promote_region(
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

// ---------------------------------------------------------------------------
// Feature 4 — utility panels: VIN / part decoder, DTC lookup, seed/key calc
// ---------------------------------------------------------------------------

#[tauri::command]
fn vin_decode(vin: String) -> Result<Value, String> {
    let v = opentune_vin::decode_vin(&vin).map_err(|e| format!("{e:?}"))?;
    Ok(json!({
        "vin": v.vin,
        "wmi": v.wmi,
        "vds": v.vds,
        "vis": v.vis,
        "manufacturer": v.wmi_info.manufacturer,
        "country": v.wmi_info.country,
        "check_digit": v.check_digit.to_string(),
        "check_digit_valid": v.check_digit_valid,
        "model_year": {
            "code": v.model_year.code.to_string(),
            "candidates": v.model_year.candidates,
            "resolved": v.model_year.resolved,
            "ambiguous": v.model_year.resolved.is_none(),
        },
        "plant_code": v.plant_code.to_string(),
        "serial": v.serial,
        "vds_descriptor": v.vds_descriptor,
        "flags": v.flags,
    }))
}

#[tauri::command]
fn part_decode(part: String) -> Result<Value, String> {
    if part.trim().is_empty() {
        return Err("enter a part number".into());
    }
    Ok(part_breakdown(&opentune_vin::parse_part_number(&part)))
}

#[tauri::command]
fn dtc_lookup(code: String) -> Result<Value, String> {
    let d = opentune_dtc::lookup(&code)
        .ok_or("not a recognisable DTC — it must start with P, C, B or U (e.g. P0101)")?;
    let system = match d.system {
        opentune_dtc::System::Powertrain => "Powertrain (engine / transmission / emissions)",
        opentune_dtc::System::Chassis => "Chassis (ABS / steering / suspension)",
        opentune_dtc::System::Body => "Body (airbags / lighting / HVAC / comfort)",
        opentune_dtc::System::Network => "Network / communication (CAN bus / modules)",
    };
    Ok(json!({
        "code": d.code,
        "description": d.description,
        "system": system,
        "system_letter": d.system.letter().to_string(),
    }))
}

/// Generic, fully-transparent seed→key transforms. The public edition ships NO
/// OEM / vehicle-specific seed/key routines (those are not open), so this is a
/// calculator the user drives: they pick a transform and supply its parameters.
/// It never pretends to compute a real ECU's key from a bundled secret.
#[tauri::command]
fn seedkey_algos() -> Value {
    json!([
        { "id": "identity", "name": "Identity (key = seed)", "params": [],
          "desc": "Echoes the seed. Some access levels accept the seed unchanged." },
        { "id": "xor-mask", "name": "XOR mask", "params": ["mask"],
          "desc": "key[i] = seed[i] XOR mask[i] (mask repeats if shorter)." },
        { "id": "add-const", "name": "Add constant (per byte)", "params": ["add"],
          "desc": "key[i] = seed[i] + k (mod 256)." },
        { "id": "xor-add", "name": "XOR then add (demo shape)", "params": ["mask", "add"],
          "desc": "key[i] = (seed[i] XOR m) + k (mod 256), m = first mask byte." },
        { "id": "byteswap", "name": "Reverse byte order", "params": [],
          "desc": "key = seed reversed." }
    ])
}

/// Parse a hex blob: ignores spaces, `0x`, `-`, `:`; errors on odd length / junk.
fn parse_hex_bytes(s: &str) -> Result<Vec<u8>, String> {
    let cleaned: String = s
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect();
    if cleaned.is_empty() {
        return Err("empty seed".into());
    }
    if cleaned.len() % 2 != 0 {
        return Err("hex has an odd number of digits".into());
    }
    (0..cleaned.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&cleaned[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

/// Compute a key from a seed with a user-selected generic transform. Honest by
/// construction: no OEM algorithm is bundled, so an unknown/empty algo id is a
/// hard error rather than a fabricated key.
#[tauri::command]
fn seedkey_compute(
    seed: String,
    algo: String,
    mask: Option<String>,
    add: Option<String>,
) -> Result<Value, String> {
    let s = parse_hex_bytes(&seed)?;
    let add_val: u8 = match add.as_deref().map(str::trim).filter(|x| !x.is_empty()) {
        Some(x) => parse_u32(x)? as u8,
        None => 0,
    };
    let mask_bytes = match mask.as_deref().map(str::trim).filter(|x| !x.is_empty()) {
        Some(x) => parse_hex_bytes(x)?,
        None => Vec::new(),
    };

    let key: Vec<u8> = match algo.as_str() {
        "identity" => s.clone(),
        "byteswap" => s.iter().rev().copied().collect(),
        "add-const" => s.iter().map(|&b| b.wrapping_add(add_val)).collect(),
        "xor-mask" => {
            if mask_bytes.is_empty() {
                return Err("XOR mask needs a mask value".into());
            }
            s.iter()
                .enumerate()
                .map(|(i, &b)| b ^ mask_bytes[i % mask_bytes.len()])
                .collect()
        }
        "xor-add" => {
            let m = *mask_bytes.first().ok_or("XOR-add needs a mask byte")?;
            s.iter().map(|&b| (b ^ m).wrapping_add(add_val)).collect()
        }
        "" => return Err("pick a transform first".into()),
        other => {
            return Err(format!(
                "no seed/key algorithm '{other}' is available — the public edition ships no OEM routines; pick a generic transform or supply one"
            ))
        }
    };

    let hex = key.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" ");
    Ok(json!({
        "key": hex,
        "algo": algo,
        "note": "Generic transform — NOT an OEM algorithm. The public edition bundles no vehicle-specific seed/key routines.",
    }))
}

// ---------------------------------------------------------------------------
// Tuning project files (.otproj)
// ---------------------------------------------------------------------------
//
// A project is a small JSON side-file that records *what you changed* against a
// base flash image — never a copy of the image itself. It bundles:
//   * the base image path plus a content fingerprint (size + a fast FNV-1a hash)
//     so a later open can detect that the referenced image has changed,
//   * the selected ECU definition + checksum algorithm,
//   * one edit record per map you touched (the edited physical-value grid, the
//     originally-decoded baseline grid, and free-text notes).
//
// On open the backend re-reads the referenced image, re-fingerprints it, and
// reports whether it still matches. The *values* are restored verbatim onto a
// freshly-decoded map (see the frontend): re-apply is absolute-value restore,
// not a delta replay, so a fingerprint mismatch is surfaced as a hard warning.

const PROJECT_FORMAT: &str = "opentune-project";
const PROJECT_VERSION: u32 = 1;

/// 64-bit FNV-1a over the whole file. This is a fast *change detector*, not a
/// cryptographic hash — it only has to catch "this is a different image".
fn fnv1a64(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Read a file and return `(size, hex-fingerprint)` for mismatch detection.
fn file_fingerprint(path: &str) -> Result<(u64, String), String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    Ok((data.len() as u64, format!("{:016x}", fnv1a64(&data))))
}

/// Base-image reference: path + the fingerprint captured when the project was saved.
#[derive(Debug, Clone, Default, Serialize, serde::Deserialize)]
struct BaseImageRef {
    #[serde(default)]
    path: String,
    #[serde(default)]
    size: u64,
    /// Hex FNV-1a of the image at save time; empty when unknown.
    #[serde(default)]
    hash: String,
}

/// One edited map inside a project. Either definition-backed (`def_id` +
/// `map_name`) or ad-hoc (`mapdef` carries the full MapDef). `cells` is the
/// edited physical-value grid; `baseline` is the originally-decoded grid, kept so
/// the UI can show a change count without re-reading the image.
#[derive(Debug, Clone, Default, Serialize, serde::Deserialize)]
struct MapEdit {
    #[serde(default)]
    kind: String, // "def" | "adhoc"
    #[serde(default)]
    def_id: Option<String>,
    #[serde(default)]
    map_name: String,
    #[serde(default)]
    mapdef: Option<MapDef>,
    #[serde(default)]
    cells: Vec<Vec<f64>>,
    #[serde(default)]
    baseline: Vec<Vec<f64>>,
    #[serde(default)]
    notes: String,
}

/// A whole tuning session as saved to disk. All fields default so an older or
/// hand-written project still loads.
#[derive(Debug, Clone, Default, Serialize, serde::Deserialize)]
struct Project {
    #[serde(default)]
    format: String,
    #[serde(default)]
    version: u32,
    #[serde(default)]
    name: String,
    #[serde(default)]
    notes: String,
    #[serde(default)]
    ecudef_id: Option<String>,
    #[serde(default)]
    checksum_algo: Option<String>,
    #[serde(default)]
    base_image: BaseImageRef,
    #[serde(default)]
    edits: Vec<MapEdit>,
    #[serde(default)]
    created: Option<String>,
    #[serde(default)]
    saved: Option<String>,
}

/// Save a tuning project to `path`. The backend authoritatively (re)computes the
/// base-image fingerprint from the current file so the saved hash always matches
/// what a later [`project_open`] will recompute, and stamps the format/version.
/// Returns a small summary for the UI.
#[tauri::command]
fn project_save(path: String, mut project: Project) -> Result<Value, String> {
    project.format = PROJECT_FORMAT.to_string();
    if project.version == 0 {
        project.version = PROJECT_VERSION;
    }
    // Refresh the base-image fingerprint from the referenced file when it exists;
    // if it cannot be read we keep whatever the caller supplied (so a project can
    // still be saved when the image is temporarily unavailable).
    let mut image_ok = false;
    if !project.base_image.path.trim().is_empty() {
        if let Ok((size, hash)) = file_fingerprint(&project.base_image.path) {
            project.base_image.size = size;
            project.base_image.hash = hash;
            image_ok = true;
        }
    }

    let json = serde_json::to_string_pretty(&project).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| write_error(&path, &e))?;

    Ok(json!({
        "path": path,
        "name": project.name,
        "edits": project.edits.len(),
        "image_fingerprinted": image_ok,
        "base_image": {
            "path": project.base_image.path,
            "size": project.base_image.size,
            "hash": project.base_image.hash,
        },
    }))
}

/// Open a tuning project from `path`. Parses it, then re-fingerprints the
/// referenced base image and reports `image_status`:
///   * `"match"`    — the image is present and its fingerprint matches,
///   * `"mismatch"` — present but changed since the project was saved,
///   * `"missing"`  — the referenced path cannot be read,
///   * `"no_image"` — the project records no base image,
/// so the frontend can warn before the stored edits are restored onto it.
#[tauri::command]
fn project_open(path: String) -> Result<Value, String> {
    let txt = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let project: Project = serde_json::from_str(&txt)
        .map_err(|e| format!("not a valid OpenTune project ({path}): {e}"))?;

    let img_path = project.base_image.path.clone();
    let stored_hash = project.base_image.hash.clone();
    let stored_size = project.base_image.size;

    let (image_status, current_size, current_hash) = if img_path.trim().is_empty() {
        ("no_image", None, None)
    } else {
        match file_fingerprint(&img_path) {
            Ok((size, hash)) => {
                let matches = if stored_hash.is_empty() {
                    // No stored hash to compare against — fall back to a size check.
                    stored_size == 0 || stored_size == size
                } else {
                    hash == stored_hash
                };
                (
                    if matches { "match" } else { "mismatch" },
                    Some(size),
                    Some(hash),
                )
            }
            Err(_) => ("missing", None, None),
        }
    };

    let project_val = serde_json::to_value(&project).map_err(|e| e.to_string())?;
    Ok(json!({
        "path": path,
        "project": project_val,
        "image_status": image_status,
        "image_path": img_path,
        "stored_size": stored_size,
        "stored_hash": stored_hash,
        "current_size": current_size,
        "current_hash": current_hash,
    }))
}

// ---------------------------------------------------------------------------
// Software update (Tauri updater plugin)
// ---------------------------------------------------------------------------
//
// The updater PLUGIN is registered in `main()` and its endpoint + minisign
// public key live in `tauri.conf.json`. These commands are the missing invocation
// layer: check → (user confirms) → download+install → relaunch. Signature
// verification is NOT done here by hand — the plugin verifies every downloaded
// bundle against the configured `pubkey` inside `download()` before it is
// installed, so a tampered or wrongly-signed artifact fails the download step and
// never reaches disk. We only surface that failure to the UI.
//
// The whole install is gated behind [`OpGuard`]: it refuses to start while a
// flash or map write-back is in flight, and while it runs those refuse to start,
// so an in-place binary replacement can never race a destructive operation.

/// Event channel the install flow emits download progress on. The frontend
/// listens for `{ downloaded, total }` (total is `null` until the server sends a
/// content-length).
const UPDATE_PROGRESS_EVENT: &str = "update://progress";

/// Report the running version and whether a long operation currently blocks an
/// update. Lets the UI disable the "Install update" affordance proactively
/// instead of only learning on refusal.
#[tauri::command]
fn update_status(app: tauri::AppHandle, guard: tauri::State<OpGuard>) -> Value {
    let busy = guard.current();
    json!({
        "current_version": app.package_info().version.to_string(),
        "busy": busy.is_some(),
        "busy_op": busy,
    })
}

/// Check the configured endpoint for a newer release. Does NOT download or
/// install — it only tells the UI what is available so it can ask the user.
/// Offline (no network) this returns an error the UI shows as "check failed",
/// which is the honest outcome.
#[tauri::command]
async fn update_check(app: tauri::AppHandle) -> Result<Value, String> {
    let current = app.package_info().version.to_string();
    let updater = app
        .updater()
        .map_err(|e| format!("updater unavailable: {e}"))?;
    match updater.check().await {
        Ok(Some(update)) => Ok(json!({
            "available": true,
            "current_version": update.current_version,
            "version": update.version,
            "notes": update.body,
            "date_unix": update.date.map(|d| d.unix_timestamp()),
        })),
        Ok(None) => Ok(json!({
            "available": false,
            "current_version": current,
        })),
        Err(e) => Err(format!("update check failed: {e}")),
    }
}

/// Download and install the available update, emitting [`UPDATE_PROGRESS_EVENT`]
/// as bytes arrive. Refuses while a flash / map write-back is running, and claims
/// the same guard for its own duration. On success the app is NOT auto-restarted;
/// the UI confirms and calls [`update_relaunch`] so the user is never surprised by
/// a restart. Signature verification is enforced by the plugin's `download()`; a
/// bad signature or an interrupted download surfaces here as a hard error and
/// nothing is installed.
#[tauri::command]
async fn update_install(
    app: tauri::AppHandle,
    guard: tauri::State<'_, OpGuard>,
) -> Result<Value, String> {
    // Claim the long-operation slot for the whole install (freed on drop).
    let _op = guard.begin("software update")?;

    let updater = app
        .updater()
        .map_err(|e| format!("updater unavailable: {e}"))?;
    let update = match updater
        .check()
        .await
        .map_err(|e| format!("update check failed: {e}"))?
    {
        Some(u) => u,
        None => {
            return Ok(json!({ "installed": false, "up_to_date": true }));
        }
    };

    let version = update.version.clone();
    let app_ev = app.clone();
    let mut downloaded: u64 = 0;

    // `download_and_install` verifies the minisign signature against the bundled
    // pubkey inside `download()`; a mismatch or a truncated/interrupted download
    // returns Err here and nothing is written over the installed app.
    update
        .download_and_install(
            move |chunk_len, content_len| {
                downloaded += chunk_len as u64;
                let _ = app_ev.emit(
                    UPDATE_PROGRESS_EVENT,
                    json!({ "downloaded": downloaded, "total": content_len }),
                );
            },
            || {},
        )
        .await
        .map_err(|e| format!("update download/install failed (signature or network): {e}"))?;

    Ok(json!({ "installed": true, "up_to_date": false, "version": version }))
}

/// Relaunch the app so a freshly-installed update takes effect. Called only after
/// [`update_install`] reports success and the user confirms. Never returns.
#[tauri::command]
fn update_relaunch(app: tauri::AppHandle) {
    app.restart();
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
            app.manage(OpGuard::default());
            app.manage(DebugLog::default());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            build_channel,
            probe_adapter,
            debug_raw_request,
            debug_simulate,
            debug_log_dump,
            debug_log_clear,
            feat_coding::coding_read,
            feat_coding::coding_set_preview,
            feat_coding::coding_write,
            feat_scan::autoscan_run,
            feat_scan::autoscan_clear,
            feat_adaptation::adaptation_list,
            feat_adaptation::adaptation_read,
            feat_adaptation::adaptation_actuator,
            feat_adaptation::adaptation_routine,
            feat_service::service_plan,
            feat_service::service_run,
            feat_logging::log_params,
            feat_logging::log_poll_once,
            feat_obd::obd_live,
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
            decode_mapdef,
            map_write,
            mapdef_write,
            compare_maps,
            analyze_image,
            import_definitions,
            promote_region,
            vin_decode,
            part_decode,
            dtc_lookup,
            seedkey_algos,
            seedkey_compute,
            project_save,
            project_open,
            flash,
            vault_list,
            guide_list,
            guide_read,
            update_status,
            update_check,
            update_install,
            update_relaunch,
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

    // --- new-feature pure logic ------------------------------------------

    #[test]
    fn family_algo_mapping() {
        assert_eq!(family_to_algo("EDC16"), Some("edc16"));
        assert_eq!(family_to_algo("EDC17"), Some("edc17"));
        assert_eq!(family_to_algo("PCR2.1"), Some("pcr21"));
        assert_eq!(family_to_algo("EDC15"), Some("edc15"));
        assert_eq!(family_to_algo("Unknown"), None);
    }

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

    #[test]
    fn hex_parsing() {
        assert_eq!(parse_hex_bytes("AA BB CC").unwrap(), vec![0xAA, 0xBB, 0xCC]);
        assert_eq!(parse_hex_bytes("de:ad:be:ef").unwrap(), vec![0xDE, 0xAD, 0xBE, 0xEF]);
        assert!(parse_hex_bytes("ABC").is_err()); // odd
        assert!(parse_hex_bytes("").is_err());
    }

    #[test]
    fn seedkey_transforms() {
        // identity
        let r = seedkey_compute("0102".into(), "identity".into(), None, None).unwrap();
        assert_eq!(r["key"], "01 02");
        // byteswap
        let r = seedkey_compute("0102".into(), "byteswap".into(), None, None).unwrap();
        assert_eq!(r["key"], "02 01");
        // xor-mask (mask repeats)
        let r = seedkey_compute("F0F0".into(), "xor-mask".into(), Some("FF".into()), None).unwrap();
        assert_eq!(r["key"], "0F 0F");
        // add-const mod 256 wraps
        let r = seedkey_compute("FE".into(), "add-const".into(), None, Some("3".into())).unwrap();
        assert_eq!(r["key"], "01");
        // xor-add
        let r = seedkey_compute("0A".into(), "xor-add".into(), Some("0F".into()), Some("0x01".into())).unwrap();
        assert_eq!(r["key"], "06"); // (0x0A ^ 0x0F) + 1 = 0x05 + 1 = 0x06
    }

    #[test]
    fn seedkey_unknown_algo_is_honest_error() {
        assert!(seedkey_compute("00".into(), "vag-edc17".into(), None, None).is_err());
        assert!(seedkey_compute("00".into(), "".into(), None, None).is_err());
        // xor-mask without a mask must error, not fabricate.
        assert!(seedkey_compute("00".into(), "xor-mask".into(), None, None).is_err());
    }

    // --- tuning project (.otproj) save/open ------------------------------

    /// A per-test scratch directory under the system temp dir, removed on drop.
    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new(tag: &str) -> Self {
            let mut p = std::env::temp_dir();
            let uniq = format!(
                "otproj-test-{tag}-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            p.push(uniq);
            std::fs::create_dir_all(&p).unwrap();
            TmpDir(p)
        }
        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn sample_project(img_path: &str) -> Project {
        Project {
            name: "demo tune".into(),
            notes: "stage 1".into(),
            ecudef_id: Some("edc16-demo".into()),
            checksum_algo: Some("edc16".into()),
            base_image: BaseImageRef {
                path: img_path.to_string(),
                ..Default::default()
            },
            edits: vec![MapEdit {
                kind: "def".into(),
                def_id: Some("edc16-demo".into()),
                map_name: "injection limiter".into(),
                cells: vec![vec![10.0, 12.5], vec![14.0, 16.5]],
                baseline: vec![vec![10.0, 10.0], vec![10.0, 10.0]],
                notes: "+25% top end".into(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn fnv1a_detects_change() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325); // FNV-1a 64 offset basis
        assert_ne!(fnv1a64(b"abc"), fnv1a64(b"abd"));
        assert_eq!(fnv1a64(b"opentune"), fnv1a64(b"opentune"));
    }

    #[test]
    fn project_round_trips_and_image_matches() {
        let dir = TmpDir::new("rt");
        let img = dir.join("stock.bin");
        std::fs::write(&img, vec![0xAAu8; 2048]).unwrap();
        let proj_path = dir.join("tune.otproj");

        let save = project_save(
            proj_path.to_string_lossy().into_owned(),
            sample_project(&img.to_string_lossy()),
        )
        .expect("save ok");
        assert_eq!(save["edits"], 1);
        assert_eq!(save["image_fingerprinted"], true);
        // The save authoritatively fills size + a non-empty hash.
        assert_eq!(save["base_image"]["size"], 2048);
        assert!(save["base_image"]["hash"].as_str().unwrap().len() == 16);

        let open = project_open(proj_path.to_string_lossy().into_owned()).expect("open ok");
        assert_eq!(open["image_status"], "match");
        let p = &open["project"];
        assert_eq!(p["format"], PROJECT_FORMAT);
        assert_eq!(p["version"], PROJECT_VERSION);
        assert_eq!(p["name"], "demo tune");
        assert_eq!(p["ecudef_id"], "edc16-demo");
        assert_eq!(p["checksum_algo"], "edc16");
        // Edits survive verbatim.
        assert_eq!(p["edits"][0]["map_name"], "injection limiter");
        assert_eq!(p["edits"][0]["cells"][1][1], 16.5);
        assert_eq!(p["edits"][0]["baseline"][0][0], 10.0);
        assert_eq!(p["edits"][0]["notes"], "+25% top end");
    }

    #[test]
    fn project_open_flags_image_mismatch() {
        let dir = TmpDir::new("mismatch");
        let img = dir.join("stock.bin");
        std::fs::write(&img, vec![0x00u8; 1024]).unwrap();
        let proj_path = dir.join("tune.otproj");
        project_save(
            proj_path.to_string_lossy().into_owned(),
            sample_project(&img.to_string_lossy()),
        )
        .unwrap();

        // Change the referenced image after saving the project.
        std::fs::write(&img, vec![0xFFu8; 1024]).unwrap();
        let open = project_open(proj_path.to_string_lossy().into_owned()).unwrap();
        assert_eq!(open["image_status"], "mismatch");
        assert_ne!(open["current_hash"], open["stored_hash"]);

        // A size change is likewise a mismatch.
        std::fs::write(&img, vec![0xFFu8; 2048]).unwrap();
        let open2 = project_open(proj_path.to_string_lossy().into_owned()).unwrap();
        assert_eq!(open2["image_status"], "mismatch");
        assert_eq!(open2["current_size"], 2048);
    }

    #[test]
    fn project_open_reports_missing_and_no_image() {
        let dir = TmpDir::new("missing");
        let proj_path = dir.join("tune.otproj");

        // Referenced image that does not exist.
        project_save(
            proj_path.to_string_lossy().into_owned(),
            sample_project(&dir.join("gone.bin").to_string_lossy()),
        )
        .unwrap();
        let open = project_open(proj_path.to_string_lossy().into_owned()).unwrap();
        assert_eq!(open["image_status"], "missing");

        // A project with no base image at all.
        let mut p = sample_project("");
        p.base_image = BaseImageRef::default();
        let proj2 = dir.join("noimg.otproj");
        project_save(proj2.to_string_lossy().into_owned(), p).unwrap();
        let open2 = project_open(proj2.to_string_lossy().into_owned()).unwrap();
        assert_eq!(open2["image_status"], "no_image");
    }

    #[test]
    fn project_open_rejects_garbage() {
        let dir = TmpDir::new("garbage");
        let bad = dir.join("bad.otproj");
        std::fs::write(&bad, b"this is not json").unwrap();
        assert!(project_open(bad.to_string_lossy().into_owned()).is_err());
    }

    // --- long-operation guard (update ↔ flash/write mutual exclusion) -----

    #[test]
    fn op_guard_blocks_second_operation() {
        let g = OpGuard::default();
        let tok = g.begin("flash").expect("first op claims the slot");
        assert_eq!(g.current().as_deref(), Some("flash"));
        // A software update must be refused while a flash holds the slot, and the
        // error must name what is running.
        let err = g.begin("software update").unwrap_err();
        assert!(err.contains("flash"), "error should name the running op: {err}");
        drop(tok);
    }

    #[test]
    fn op_guard_releases_on_drop() {
        let g = OpGuard::default();
        {
            let _tok = g.begin("flash").unwrap();
            assert!(g.current().is_some());
        } // token dropped here (mirrors a command returning, via `?`, or panicking)
        assert!(g.current().is_none(), "slot must free when the token drops");
        // The slot is reusable afterwards (e.g. an update after the flash finished).
        let _tok2 = g
            .begin("software update")
            .expect("slot reusable after release");
        assert_eq!(g.current().as_deref(), Some("software update"));
    }

    #[test]
    fn op_guard_update_blocks_flash_symmetrically() {
        let g = OpGuard::default();
        let _upd = g.begin("software update").unwrap();
        // While an update holds the slot, a flash is refused too (no destructive
        // write can race an in-place binary replacement).
        assert!(g.begin("flash").is_err());
    }
}
