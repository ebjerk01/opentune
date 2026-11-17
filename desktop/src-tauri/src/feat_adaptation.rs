// OpenTune — Adaptation feature (value channels + actuator/routine tests).
//
// A thin Tauri command layer over the public `opentune-adaptation` engine: load a
// data-defined AdaptationDef, list its channels, read a value channel over a live
// adapter, run a RoutineControl basic-settings routine, and run an actuator /
// output test. The engine does the UDS work and the honest response checking; this
// module only wires the adapter transport (via `crate::debug_open_diag` +
// `opentune_core::CoreDiag`) to it and shapes results as JSON for the UI.
//
// SAFETY: `adaptation_actuator` commands real hardware. It is ack-gated — it
// refuses unless the UI passes `ack = true` — mirroring the engine's own
// return-control-to-ECU guarantee. The HS/MS-CAN bus select must match the
// module's physical bus (the UI warns about this and the dongle switch).

use opentune_adaptation::{AdaptationDef, RoutineOp, WriteOptions};
use serde_json::{json, Value};

/// Uppercase space-separated hex for response records.
fn hexs(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect::<Vec<_>>().join(" ")
}

/// Parse a whitespace-tolerant hex string; empty input yields an empty vector.
fn parse_hex(s: &str) -> Result<Vec<u8>, String> {
    let clean: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if clean.is_empty() {
        return Ok(Vec::new());
    }
    if !clean.len().is_multiple_of(2) {
        return Err(format!("hex string '{s}' has an odd number of digits"));
    }
    (0..clean.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&clean[i..i + 2], 16).map_err(|_| format!("invalid hex in '{s}'")))
        .collect()
}

/// Load, parse and semantically validate an AdaptationDef JSON file.
fn load_def(def_path: &str) -> Result<AdaptationDef, String> {
    let s = std::fs::read_to_string(def_path).map_err(|e| format!("read {def_path}: {e}"))?;
    let def = AdaptationDef::from_json(&s).map_err(|e| format!("parse adaptation def: {e}"))?;
    def.validate().map_err(|e| e.to_string())?;
    Ok(def)
}

/// Load a definition and return its metadata plus the channel list (name + kind +
/// a few per-kind hints the UI uses to decide read vs. run). No adapter needed.
#[tauri::command]
pub fn adaptation_list(def_path: String) -> Result<Value, String> {
    let def = load_def(&def_path)?;
    let channels: Vec<Value> = def
        .channels
        .iter()
        .map(|ch| {
            use opentune_adaptation::ChannelKind as K;
            let mut o = json!({ "name": ch.name, "note": ch.note });
            match &ch.kind {
                K::Value { did, field, min, max } => {
                    o["kind"] = json!("value");
                    o["did"] = json!(did);
                    o["unit"] = json!(field.scaling.unit);
                    o["min"] = json!(min);
                    o["max"] = json!(max);
                }
                K::Routine { id, .. } => {
                    o["kind"] = json!("routine");
                    o["id"] = json!(id);
                }
                K::Actuator { did, .. } => {
                    o["kind"] = json!("actuator");
                    o["did"] = json!(did);
                }
            }
            o
        })
        .collect();
    Ok(json!({
        "id": def.id,
        "name": def.name,
        "module": def.module,
        "requires_security": def.requires_security,
        "channels": channels,
    }))
}

/// Read a value channel (UDS 0x22) over a live adapter and decode its numeric field.
#[tauri::command]
pub async fn adaptation_read(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    def_path: String,
    channel: String,
    timeout_ms: u64,
) -> Result<Value, String> {
    let def = load_def(&def_path)?;
    let mut drv = crate::debug_open_diag(&driver, port.as_deref(), baud.as_deref(), bus.as_deref())?;
    let t = drv.diag().ok_or_else(|| "adapter has no diagnostic transport".to_string())?;
    let mut diag = opentune_core::CoreDiag::new(t, timeout_ms);
    let r = opentune_adaptation::read_value(&mut diag, &def, &channel).map_err(|e| e.to_string())?;
    Ok(json!({
        "channel": r.channel,
        "raw": r.raw,
        "engineering": r.engineering,
        "unit": r.unit,
        "requires_security": def.requires_security,
    }))
}

/// Run a RoutineControl (UDS 0x31) routine channel. `op` selects the sub-function
/// (`start` / `stop` / `request_results`); `extra` optionally overrides the
/// channel's start/stop data record (hex, whitespace tolerant).
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn adaptation_routine(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    def_path: String,
    channel: String,
    timeout_ms: u64,
    op: String,
    extra: Option<String>,
) -> Result<Value, String> {
    let def = load_def(&def_path)?;
    let op = match op.trim().to_ascii_lowercase().as_str() {
        "start" => RoutineOp::Start,
        "stop" => RoutineOp::Stop,
        "request_results" | "results" | "requestresults" => RoutineOp::RequestResults,
        other => return Err(format!("unknown routine op '{other}'")),
    };
    let extra = parse_hex(extra.as_deref().unwrap_or(""))?;
    let mut drv = crate::debug_open_diag(&driver, port.as_deref(), baud.as_deref(), bus.as_deref())?;
    let t = drv.diag().ok_or_else(|| "adapter has no diagnostic transport".to_string())?;
    let mut diag = opentune_core::CoreDiag::new(t, timeout_ms);
    let r = opentune_adaptation::run_routine(&mut diag, &def, &channel, op, &extra)
        .map_err(|e| e.to_string())?;
    Ok(json!({
        "channel": r.channel,
        "data": hexs(&r.data),
        "value": r.value,
        "unit": r.unit,
        "security_required": r.security_required,
    }))
}

/// Run an actuator / output test (UDS 0x2F) on an actuator channel. ACK-GATED:
/// refuses unless `ack` is true, because this commands real hardware. The engine
/// always issues returnControlToECU afterwards; the report surfaces the dangerous
/// case where control could not be handed back. `state` optionally overrides the
/// channel's default control-state record (hex, whitespace tolerant).
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn adaptation_actuator(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    def_path: String,
    channel: String,
    timeout_ms: u64,
    ack: bool,
    state: Option<String>,
) -> Result<Value, String> {
    if !ack {
        return Err("actuator test refused: confirm you understand it commands the ECU (ack required)".into());
    }
    let def = load_def(&def_path)?;
    let drive = parse_hex(state.as_deref().unwrap_or(""))?;
    let mut drv = crate::debug_open_diag(&driver, port.as_deref(), baud.as_deref(), bus.as_deref())?;
    let t = drv.diag().ok_or_else(|| "adapter has no diagnostic transport".to_string())?;
    let mut diag = opentune_core::CoreDiag::new(t, timeout_ms);
    let r = opentune_adaptation::run_actuator_test(&mut diag, &def, &channel, &drive, &WriteOptions::default())
        .map_err(|e| e.to_string())?;
    use opentune_adaptation::ActuatorOutcome as O;
    let (outcome, message, safe) = match &r.outcome {
        O::Planned => ("planned", String::new(), true),
        O::Completed => ("completed", String::new(), true),
        O::ApplyFailed { message } => ("apply_failed", message.clone(), r.control_returned),
        O::ControlReturnFailed { message } => ("control_return_failed", message.clone(), false),
    };
    Ok(json!({
        "channel": r.channel,
        "did": r.did,
        "applied": r.applied,
        "control_returned": r.control_returned,
        "response": hexs(&r.response),
        "status": r.status,
        "status_unit": r.status_unit,
        "outcome": outcome,
        "message": message,
        "safe": safe,
        "security_required": r.security_required,
    }))
}
