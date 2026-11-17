//! # Live datalogging panel (backend)
//!
//! Two Tauri commands that back the "Live log" panel. The design is deliberately
//! stateless: the live loop is driven from the frontend, which calls
//! [`log_poll_once`] on an interval, so each poll opens the adapter, reads the
//! selected parameters once, and drops the adapter again. That keeps the backend
//! free of a long-lived session to manage and is perfectly adequate at the poll
//! rates a diagnostic adapter sustains (an ELM327 is slow — a few parameters per
//! second).
//!
//! Parameter definitions are RomRaider-style `logger.xml` files parsed by
//! [`opentune_logdef`]. Only UDS `0x22` (ReadDataByIdentifier) parameters are
//! actually driven today (see [`opentune_logdef::poll_once`]); OBD-II PID and
//! raw-memory/SSM reads come back with an explanatory `note` rather than a value,
//! which the UI shows verbatim.

use serde_json::{json, Value};

/// Read and parse the `logger.xml` at `path` into a [`opentune_logdef::LogDef`].
fn load_def(path: &str) -> Result<opentune_logdef::LogDef, String> {
    let xml = std::fs::read_to_string(path).map_err(|e| format!("read '{path}': {e}"))?;
    opentune_logdef::parse(&xml).map_err(|e| format!("parse '{path}': {e}"))
}

/// Parse a `logger.xml` definition and return the list of available parameters.
///
/// Each entry is `{ id, name, units }`. A definition can list the same parameter
/// id under several ECU variants; the UI only needs one checkbox per id, so
/// duplicates are collapsed to the first occurrence while preserving order.
#[tauri::command]
pub fn log_params(def_path: String) -> Result<Value, String> {
    let def = load_def(&def_path)?;

    let mut seen = std::collections::HashSet::new();
    let mut params = Vec::new();
    for (_proto, _ecu, p) in def.params() {
        if !seen.insert(p.id.clone()) {
            continue;
        }
        params.push(json!({
            "id": p.id,
            "name": p.name,
            "units": p.units,
        }));
    }

    Ok(json!({
        "path": def_path,
        "version": def.version,
        "count": params.len(),
        "params": params,
    }))
}

/// Open the adapter, poll the selected parameters **once**, and return the
/// samples. An empty `ids` selects every enabled parameter in the definition.
///
/// This is the per-tick worker for the live loop: the frontend calls it on an
/// interval. Each call opens the driver, wraps its diagnostic transport in a
/// [`CoreDiag`](opentune_core::CoreDiag), reads, and lets the adapter close as it
/// drops. Every parameter always yields a sample; a failed or unsupported read is
/// carried in `note`, never silently dropped.
#[tauri::command]
pub async fn log_poll_once(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    def_path: String,
    timeout_ms: u64,
    ids: Vec<String>,
) -> Result<Value, String> {
    let def = load_def(&def_path)?;

    let mut drv =
        crate::debug_open_diag(&driver, port.as_deref(), baud.as_deref(), bus.as_deref())?;
    let t = drv
        .diag()
        .ok_or_else(|| "adapter has no diagnostic transport".to_string())?;
    let mut diag = opentune_core::CoreDiag::new(t, timeout_ms);

    // poll_once wants &[&str]; borrow the owned ids.
    let id_refs: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
    let samples = opentune_logdef::poll_once(&mut diag, &def, &id_refs);

    let rows: Vec<Value> = samples
        .iter()
        .map(|s| {
            json!({
                "paramId": s.param_id,
                "name": s.name,
                "raw": s.raw,
                "value": s.value,
                "units": s.units,
                "note": s.note,
            })
        })
        .collect();

    Ok(json!({
        "ts": chrono_millis(),
        "count": rows.len(),
        "samples": rows,
    }))
}

/// Milliseconds since the Unix epoch, for a per-tick timestamp. Avoids pulling in
/// a date crate: the frontend only uses it to order/stamp rows.
fn chrono_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
