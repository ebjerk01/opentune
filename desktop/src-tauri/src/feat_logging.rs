//! # Live datalogging panel (backend)
//!
//! Two Tauri commands that back the "Live log" panel. The live loop is driven
//! from the frontend, which calls [`log_poll_once`] on an interval. Each poll
//! goes through the app-wide adapter hub ([`crate::adapter`]), which reuses the
//! already-opened adapter between ticks (no full ELM327 re-init per tick) and
//! rejects the tick with "adapter busy" while another panel owns the adapter.
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

/// Poll the selected parameters **once** and return the samples. An empty
/// `ids` selects every enabled parameter in the definition.
///
/// This is the per-tick worker for the live loop. Reads go to the target module
/// (UI `tx_id`/`rx_id`, else the engine ECU). Every parameter always yields a
/// sample; a failed or unsupported read is carried in `note`, never silently
/// dropped. When no parameter produced a value the cached adapter is dropped so
/// the next tick re-opens it (recovers from an unplugged / reset adapter).
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn log_poll_once(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    tx_id: Option<String>,
    rx_id: Option<String>,
    def_path: String,
    timeout_ms: u64,
    ids: Vec<String>,
    hub: tauri::State<'_, crate::adapter::AdapterHub>,
) -> Result<Value, String> {
    let def = load_def(&def_path)?;
    let (addr, assumed) = crate::adapter::read_addr(crate::adapter::resolve(
        tx_id.as_deref(),
        rx_id.as_deref(),
        None,
    )?);
    let key = crate::adapter::ConnKey::new(
        &driver,
        port.as_deref(),
        baud.as_deref(),
        bus.as_deref(),
        addr,
    );

    let samples = hub.run("live log", move |s| {
        let samples = s.with_diag(&key, timeout_ms, |d| {
            // poll_once wants &[&str]; borrow the owned ids.
            let id_refs: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
            Ok(opentune_logdef::poll_once(d, &def, &id_refs))
        })?;
        if !samples.is_empty() && samples.iter().all(|x| x.value.is_none()) {
            s.invalidate();
        }
        Ok(samples)
    })?;

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
        "target": addr.json(assumed),
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
