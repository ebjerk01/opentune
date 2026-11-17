// OpenTune — full-vehicle Auto-scan + DTCs feature (backend).
//
// Thin Tauri command layer over the public `opentune-autoscan` engine: load a
// vehicle's module list (data), open the selected adapter's diagnostic
// transport, wrap it in a `CoreDiag`, and run the read-only whole-vehicle scan
// or the explicit clear-all. All decoding/validation lives in the engine; this
// file only moves bytes in (connection + module JSON) and JSON out.
//
// Part of the public OpenTune desktop app; uses only the public OpenTune crates.

use serde_json::{json, Value};

/// Serialise one scanned module into the shape the Auto-scan panel renders:
/// name / present / identity (VIN + DIDs) / DTC list + count / non-fatal error.
fn module_to_json(m: &opentune_autoscan::ModuleResult) -> Value {
    let identity = m.identity.as_ref().map(|id| {
        let dids: Vec<Value> = id
            .dids
            .iter()
            .map(|d| {
                json!({
                    "did": format!("0x{:04X}", d.did),
                    "label": d.label,
                    "text": d.text,
                    "raw": d
                        .raw
                        .iter()
                        .map(|b| format!("{b:02X}"))
                        .collect::<Vec<_>>()
                        .join(" "),
                })
            })
            .collect();
        json!({ "vin": id.vin, "dids": dids })
    });

    let dtcs: Vec<Value> = m
        .dtcs
        .iter()
        .map(|d| {
            json!({
                "code": d.code,
                "description": d.description,
                "system": d.system.letter().to_string(),
                "status": d.status.map(|s| format!("0x{s:02X}")),
            })
        })
        .collect();

    json!({
        "name": m.name,
        "present": m.present,
        "identity": identity,
        "dtcs": dtcs,
        "dtcCount": m.dtcs.len(),
        "error": m.error,
    })
}

/// Run a read-only full-vehicle auto-scan.
///
/// Loads a `VehicleModules` JSON list from `modules_path`, opens the selected
/// adapter's diagnostic transport, and probes every module for presence,
/// identity (UDS) and stored DTCs. Never writes to a module.
#[tauri::command]
pub async fn autoscan_run(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    modules_path: String,
    timeout_ms: u64,
) -> Result<Value, String> {
    let text = std::fs::read_to_string(&modules_path)
        .map_err(|e| format!("read module list '{modules_path}': {e}"))?;
    let modules = opentune_autoscan::VehicleModules::from_json(&text)
        .map_err(|e| format!("parse module list: {e}"))?;

    let mut drv = crate::debug_open_diag(&driver, port.as_deref(), baud.as_deref(), bus.as_deref())?;
    let t = drv
        .diag()
        .ok_or_else(|| "adapter has no diagnostic transport".to_string())?;
    let mut diag = opentune_core::CoreDiag::new(t, timeout_ms);

    let report = opentune_autoscan::scan(&mut diag, &modules);

    let present_count = report.present().count();
    let dtc_total = report.dtc_count();
    let modules_json: Vec<Value> = report.modules.iter().map(module_to_json).collect();

    Ok(json!({
        "vehicle": modules.vehicle,
        "moduleCount": report.modules.len(),
        "presentCount": present_count,
        "dtcTotal": dtc_total,
        "modules": modules_json,
    }))
}

/// Clear stored DTCs on every present module in the list.
///
/// The engine's only mutating operation, kept out of [`autoscan_run`]: the UI
/// invokes it explicitly from the "Clear all DTCs" button.
#[tauri::command]
pub async fn autoscan_clear(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    modules_path: String,
    timeout_ms: u64,
) -> Result<Value, String> {
    let text = std::fs::read_to_string(&modules_path)
        .map_err(|e| format!("read module list '{modules_path}': {e}"))?;
    let modules = opentune_autoscan::VehicleModules::from_json(&text)
        .map_err(|e| format!("parse module list: {e}"))?;

    let mut drv = crate::debug_open_diag(&driver, port.as_deref(), baud.as_deref(), bus.as_deref())?;
    let t = drv
        .diag()
        .ok_or_else(|| "adapter has no diagnostic transport".to_string())?;
    let mut diag = opentune_core::CoreDiag::new(t, timeout_ms);

    let results = opentune_autoscan::clear_all(&mut diag, &modules);

    let cleared_count = results.iter().filter(|r| r.cleared).count();
    let results_json: Vec<Value> = results
        .iter()
        .map(|r| {
            json!({
                "name": r.name,
                "cleared": r.cleared,
                "error": r.error,
            })
        })
        .collect();

    Ok(json!({
        "vehicle": modules.vehicle,
        "clearedCount": cleared_count,
        "results": results_json,
    }))
}
