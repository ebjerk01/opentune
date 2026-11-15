// OpenTune desktop backend.
//
// Thin Tauri shell: it builds a driver `Registry` from the shared core and
// exposes a few commands the web UI calls. All ECU logic lives in
// `opentune-core` / `-checksum` / `-flash`; this file only marshals between the
// UI and that core.
#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use std::collections::BTreeMap;
use std::sync::Mutex;

use opentune_core::{ConnectConfig, EcuId, Interface, MemoryRegion, Progress, Registry};
use opentune_driver_mock::MockDriver;
use serde::Serialize;
use tauri::State;

/// A driver as shown in the UI's dropdown.
#[derive(Serialize)]
struct DriverInfo {
    id: String,
    display_name: String,
}

/// Shared backend state: the driver registry and the currently open interface.
#[derive(Default)]
struct AppState {
    registry: Registry,
    iface: Mutex<Option<Box<dyn Interface>>>,
}

impl AppState {
    fn new() -> Self {
        let mut registry = Registry::new();
        // Register the drivers this build ships with. Add more here as they are
        // wired up (socketcan, openport2, elm327, …).
        registry.register("mock", "Mock / file driver", || Box::new(MockDriver::new()));
        Self {
            registry,
            iface: Mutex::new(None),
        }
    }
}

/// List the interface drivers this build knows about.
#[tauri::command]
fn list_drivers(state: State<AppState>) -> Vec<DriverInfo> {
    state
        .registry
        .list()
        .iter()
        .map(|e| DriverInfo {
            id: e.id.to_string(),
            display_name: e.display_name.to_string(),
        })
        .collect()
}

/// Construct and open the chosen driver. `options` is passed straight into the
/// driver's `ConnectConfig` (e.g. the mock driver needs `file`).
#[tauri::command]
fn connect(
    state: State<AppState>,
    driver: String,
    options: Option<BTreeMap<String, String>>,
) -> Result<(), String> {
    let mut iface = state
        .registry
        .make(&driver)
        .ok_or_else(|| format!("unknown driver: {driver}"))?;

    let cfg = ConnectConfig {
        options: options.unwrap_or_default(),
        ..Default::default()
    };
    iface.open(&cfg).map_err(|e| e.to_string())?;

    *state.iface.lock().unwrap() = Some(iface);
    Ok(())
}

/// Read ECU identity from the open interface.
#[tauri::command]
fn identify(state: State<AppState>) -> Result<EcuId, String> {
    let mut guard = state.iface.lock().unwrap();
    let iface = guard.as_mut().ok_or("not connected")?;
    iface.read_ecu_id().map_err(|e| e.to_string())
}

/// Read a memory region from the open interface and return the bytes.
#[tauri::command]
fn read_region(state: State<AppState>, address: u32, length: u32) -> Result<Vec<u8>, String> {
    let mut guard = state.iface.lock().unwrap();
    let iface = guard.as_mut().ok_or("not connected")?;
    let region = MemoryRegion::new(address, length);
    // A richer UI would stream `Progress` back to the frontend via events.
    let mut on_progress = |_p: Progress| {};
    iface
        .read_memory(region, &mut on_progress)
        .map_err(|e| e.to_string())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(AppState::new())
        .invoke_handler(tauri::generate_handler![
            list_drivers,
            connect,
            identify,
            read_region
        ])
        .run(tauri::generate_context!())
        .expect("error while running OpenTune desktop");
}
