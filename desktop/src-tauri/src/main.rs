// OpenTune desktop — Tauri backend.
//
// Commands expose the OpenTune core to the UI. The offline workflow operates on
// a flash image file through the mock driver, so the whole app works with no
// hardware: open a dump, identify it, verify/fix checksums, find candidate maps,
// and run a safety-gated (mock) flash. ECU definitions and the vehicle catalog
// are resolved from bundled resources (or a dev path) at startup.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use opentune_core::{
    catalog::{self, CatalogEntry},
    ecudef::{self, EcuDef},
    ConnectConfig, EcuId, Interface, MemoryRegion, Registry,
};
use serde::Serialize;
use std::path::{Path, PathBuf};
use tauri::Manager;

/// Resolved data directories (ECU definitions + vehicle catalog).
struct AppData {
    ecudefs: Option<PathBuf>,
    catalog: Option<PathBuf>,
}

fn first_existing(cands: &[PathBuf]) -> Option<PathBuf> {
    cands.iter().find(|p| p.exists()).cloned()
}

fn registry() -> Registry {
    let mut r = Registry::new();
    r.register("mock", "Mock / file driver", || {
        Box::new(opentune_driver_mock::MockDriver::new())
    });
    r.register("openport2", "OpenPort 2.0 (J2534/CDC-ACM)", || {
        Box::new(opentune_driver_openport2::OpenPort2Driver::new())
    });
    r.register("socketcan", "SocketCAN", || {
        Box::new(opentune_driver_socketcan::SocketCanDriver::new())
    });
    r.register("elm327", "K-line (ELM327)", || {
        Box::new(opentune_driver_elm327::Elm327Driver::new())
    });
    r.register("kkl", "K-line (KKL 409.1 raw)", || {
        Box::new(opentune_driver_kkl::KklDriver::new())
    });
    r
}

/// Open the mock/file driver on `path`.
fn open_file(path: &str) -> Result<Box<dyn Interface>, String> {
    let reg = registry();
    let mut drv = reg.make("mock").ok_or("mock driver missing")?;
    let mut cfg = ConnectConfig::default();
    cfg.options.insert("file".into(), path.to_string());
    drv.open(&cfg).map_err(|e| e.to_string())?;
    Ok(drv)
}

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

#[derive(Serialize)]
struct DefMatch {
    id: String,
    name: String,
    family: String,
    protocol: String,
    checksum: Option<String>,
    seedkey: Option<String>,
}

#[derive(Serialize)]
struct CatMatch {
    make: String,
    models: Vec<String>,
    family: Option<String>,
    source: Option<String>,
}

#[derive(Serialize)]
struct IdentifyResult {
    ecu_name: Option<String>,
    hw_number: Option<String>,
    sw_number: Option<String>,
    vin: Option<String>,
    size: u64,
    matched: Option<DefMatch>,
    catalog: Vec<CatMatch>,
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

#[tauri::command]
fn identify(path: String, state: tauri::State<AppData>) -> Result<IdentifyResult, String> {
    let mut drv = open_file(&path)?;
    let id: EcuId = drv.read_ecu_id().map_err(|e| e.to_string())?;
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

    let defs = load_defs(&state.ecudefs);
    let matched = ecudef::match_ecu(&defs, &id).map(|d| DefMatch {
        id: d.id.clone(),
        name: d.name.clone(),
        family: d.family.clone(),
        protocol: format!("{:?}", d.protocol),
        checksum: d.checksum.clone(),
        seedkey: d.seedkey.clone(),
    });

    let entries = load_cat(&state.catalog);
    let catalog = catalog::match_catalog(&entries, &id)
        .into_iter()
        .take(12)
        .map(|c| CatMatch {
            make: c.make.clone(),
            models: c.models.clone(),
            family: c.family.clone(),
            source: c.source.clone(),
        })
        .collect();

    Ok(IdentifyResult {
        ecu_name: id.ecu_name,
        hw_number: id.hw_number,
        sw_number: id.sw_number,
        vin: id.vin,
        size,
        matched,
        catalog,
    })
}

#[derive(Serialize)]
struct Block {
    name: String,
    ok: bool,
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
            stored: format!("0x{:08X}", b.stored),
            computed: format!("0x{:08X}", b.computed),
        })
        .collect())
}

#[tauri::command]
fn checksum_fix(path: String, algo: String) -> Result<Vec<String>, String> {
    let a = opentune_checksum::get(&algo).ok_or_else(|| format!("unknown algo '{algo}'"))?;
    let mut img = std::fs::read(&path).map_err(|e| e.to_string())?;
    let fixes = a.correct(&mut img).map_err(|e| e.to_string())?;
    if !fixes.is_empty() {
        std::fs::write(&path, &img).map_err(|e| e.to_string())?;
    }
    Ok(fixes
        .into_iter()
        .map(|f| format!("{} @0x{:08X}: 0x{:08X} -> 0x{:08X}", f.name, f.offset, f.old, f.new))
        .collect())
}

#[derive(Serialize)]
struct Region {
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
fn findmaps(stock: String, tuned: String, gap: usize, min_len: usize) -> Result<Vec<Region>, String> {
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
                out.push(Region {
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
    family: String,
    protocol: String,
    flash_kib: u32,
    checksum: Option<String>,
}

#[tauri::command]
fn list_ecudefs(state: tauri::State<AppData>) -> Vec<DefInfo> {
    load_defs(&state.ecudefs)
        .into_iter()
        .map(|d| DefInfo {
            id: d.id,
            name: d.name,
            family: d.family,
            protocol: format!("{:?}", d.protocol),
            flash_kib: d.flash_size / 1024,
            checksum: d.checksum,
        })
        .collect()
}

#[derive(Serialize)]
struct FlashResult {
    bytes_written: usize,
    battery_mv: Option<u32>,
    backup_len: usize,
    verified: bool,
}

#[tauri::command]
fn flash(path: String, addr: String, data_path: String, min_mv: u32) -> Result<FlashResult, String> {
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
    Ok(FlashResult {
        bytes_written: rep.bytes_written,
        battery_mv: rep.battery_millivolts,
        backup_len: rep.backup.len(),
        verified: rep.verified,
    })
}

fn parse_u32(s: &str) -> Result<u32, String> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix("0x") {
        u32::from_str_radix(h, 16).map_err(|e| e.to_string())
    } else {
        s.parse::<u32>().map_err(|e| e.to_string())
    }
}

fn resolve_data(app: &tauri::App) -> AppData {
    let res = app.path().resource_dir().ok();
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")); // desktop/src-tauri
    let repo = manifest.join("../.."); // repo root in dev
    let env_dir = std::env::var("OPENTUNE_DATA").ok().map(PathBuf::from);

    let ecudefs = first_existing(&[
        env_dir.clone().map(|d| d.join("ecudefs")).unwrap_or_default(),
        res.clone().map(|r| r.join("ecudefs")).unwrap_or_default(),
        repo.join("ecudefs"),
    ]);
    let catalog = first_existing(&[
        env_dir.map(|d| d.join("catalog")).unwrap_or_default(),
        res.map(|r| r.join("catalog")).unwrap_or_default(),
        repo.join("catalog"),
    ])
    .map(|p| {
        // identify matches against the opendbc sub-catalog if present
        let sub = p.join("opendbc");
        if sub.exists() {
            sub
        } else {
            p
        }
    });

    AppData { ecudefs, catalog }
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
            identify,
            checksum_verify,
            checksum_fix,
            findmaps,
            list_ecudefs,
            flash,
        ])
        .run(tauri::generate_context!())
        .expect("error while running OpenTune");
}
