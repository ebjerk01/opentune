// OpenTune — Adaptation feature (value channels + actuator/routine tests).
//
// A thin Tauri command layer over the public `opentune-adaptation` engine: load a
// data-defined AdaptationDef, list its channels, read a value channel over a live
// adapter, run a RoutineControl basic-settings routine, and run an actuator /
// output test. The engine does the UDS work and the honest response checking; this
// module only wires the adapter (via the app-wide `crate::adapter` hub) to it and
// shapes results as JSON for the UI.
//
// SAFETY: `adaptation_actuator` and routine start/stop command real hardware (a
// routine can start a DPF regeneration or a relearn). Both are ack-gated — they
// refuse unless the UI passes `ack = true` — and both refuse an unknown target
// module address instead of defaulting to the engine ECU. They run in the
// extended diagnostic session (`10 03`) inside one adapter binding. The HS/MS-CAN
// bus select must match the module's physical bus (the UI warns about this).

use crate::adapter::{self, AdapterHub, ConnKey, Session};
use opentune_adaptation::{AdaptationDef, RoutineOp, WriteOptions};
use serde_json::{json, Value};

/// Uppercase space-separated hex for response records.
fn hexs(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
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
        .map(|i| {
            u8::from_str_radix(&clean[i..i + 2], 16).map_err(|_| format!("invalid hex in '{s}'"))
        })
        .collect()
}

/// Load, parse and semantically validate an AdaptationDef JSON file; also
/// returns the raw text (for the optional `req_id`/`resp_id` module address).
fn load_def_text(def_path: &str) -> Result<(AdaptationDef, String), String> {
    let s = std::fs::read_to_string(def_path).map_err(|e| format!("read {def_path}: {e}"))?;
    let def = AdaptationDef::from_json(&s).map_err(|e| format!("parse adaptation def: {e}"))?;
    def.validate().map_err(|e| e.to_string())?;
    Ok((def, s))
}

/// Parse a routine op token.
fn parse_op(op: &str) -> Result<RoutineOp, String> {
    match op.trim().to_ascii_lowercase().as_str() {
        "start" => Ok(RoutineOp::Start),
        "stop" => Ok(RoutineOp::Stop),
        "request_results" | "results" | "requestresults" => Ok(RoutineOp::RequestResults),
        other => Err(format!("unknown routine op '{other}'")),
    }
}

/// Start/stop actually run something on the module; request-results only reads.
fn op_mutates(op: RoutineOp) -> bool {
    !matches!(op, RoutineOp::RequestResults)
}

/// Run a routine op in one binding; start/stop inside the extended session.
pub(crate) fn routine_once(
    s: &mut Session,
    key: &ConnKey,
    def: &AdaptationDef,
    channel: &str,
    op: RoutineOp,
    extra: &[u8],
    timeout_ms: u64,
) -> Result<opentune_adaptation::RoutineResult, String> {
    s.with_diag(key, timeout_ms, |d| {
        let mutates = op_mutates(op);
        if mutates {
            adapter::enter_extended_session(d)?;
        }
        let r =
            opentune_adaptation::run_routine(d, def, channel, op, extra).map_err(|e| e.to_string());
        if mutates && r.is_err() {
            // Leave the session only on failure: a started routine (e.g. a
            // regeneration) must keep running; the ECU's session timeout ends
            // the extended session on its own.
            adapter::leave_session(d);
        }
        r
    })
}

/// Load a definition and return its metadata plus the channel list (name + kind +
/// a few per-kind hints the UI uses to decide read vs. run). No adapter needed.
#[tauri::command]
pub fn adaptation_list(def_path: String) -> Result<Value, String> {
    let (def, text) = load_def_text(&def_path)?;
    let target = adapter::def_addr(&text)?.map(|a| a.json(false));
    let channels: Vec<Value> = def
        .channels
        .iter()
        .map(|ch| {
            use opentune_adaptation::ChannelKind as K;
            let mut o = json!({ "name": ch.name, "note": ch.note });
            match &ch.kind {
                K::Value {
                    did,
                    field,
                    min,
                    max,
                } => {
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
        "target": target,
        "channels": channels,
    }))
}

/// Read a value channel (UDS 0x22) over a live adapter and decode its numeric
/// field. Target: UI `tx_id`/`rx_id`, else the def's `req_id`/`resp_id`, else the
/// engine ECU (flagged `target.assumed`).
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn adaptation_read(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    tx_id: Option<String>,
    rx_id: Option<String>,
    def_path: String,
    channel: String,
    timeout_ms: u64,
    hub: tauri::State<'_, AdapterHub>,
) -> Result<Value, String> {
    let (def, text) = load_def_text(&def_path)?;
    let (addr, assumed) = adapter::read_addr(adapter::resolve(
        tx_id.as_deref(),
        rx_id.as_deref(),
        Some(&text),
    )?);
    let key = ConnKey::new(
        &driver,
        port.as_deref(),
        baud.as_deref(),
        bus.as_deref(),
        addr,
    );
    let requires_security = def.requires_security;
    let r = hub.run("adaptation read", move |s| {
        s.with_diag(&key, timeout_ms, |d| {
            opentune_adaptation::read_value(d, &def, &channel).map_err(|e| e.to_string())
        })
    })?;
    Ok(json!({
        "channel": r.channel,
        "raw": r.raw,
        "engineering": r.engineering,
        "unit": r.unit,
        "requires_security": requires_security,
        "target": addr.json(assumed),
    }))
}

/// Run a RoutineControl (UDS 0x31) routine channel. `op` selects the sub-function
/// (`start` / `stop` / `request_results`); `extra` optionally overrides the
/// channel's start/stop data record (hex, whitespace tolerant).
///
/// ACK-GATED for `start`/`stop` (a routine can start a DPF regeneration or a
/// relearn): refuses unless `ack` is true, and refuses an unknown target module
/// address. `request_results` only reads and needs neither.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn adaptation_routine(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    tx_id: Option<String>,
    rx_id: Option<String>,
    def_path: String,
    channel: String,
    timeout_ms: u64,
    op: String,
    extra: Option<String>,
    ack: Option<bool>,
    hub: tauri::State<'_, AdapterHub>,
) -> Result<Value, String> {
    let op = parse_op(&op)?;
    if op_mutates(op) && ack != Some(true) {
        return Err(
            "routine start/stop refused: confirm you understand it runs a procedure on the module (ack required)"
                .into(),
        );
    }
    let (def, text) = load_def_text(&def_path)?;
    let extra = parse_hex(extra.as_deref().unwrap_or(""))?;
    let resolved = adapter::resolve(tx_id.as_deref(), rx_id.as_deref(), Some(&text))?;
    let (addr, assumed) = if op_mutates(op) {
        (adapter::write_addr(resolved, "routine start/stop")?, false)
    } else {
        adapter::read_addr(resolved)
    };
    let key = ConnKey::new(
        &driver,
        port.as_deref(),
        baud.as_deref(),
        bus.as_deref(),
        addr,
    );
    let r = hub.run("adaptation routine", move |s| {
        routine_once(s, &key, &def, &channel, op, &extra, timeout_ms)
    })?;
    Ok(json!({
        "channel": r.channel,
        "data": hexs(&r.data),
        "value": r.value,
        "unit": r.unit,
        "security_required": r.security_required,
        "target": addr.json(assumed),
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
    tx_id: Option<String>,
    rx_id: Option<String>,
    def_path: String,
    channel: String,
    timeout_ms: u64,
    ack: bool,
    state: Option<String>,
    hub: tauri::State<'_, AdapterHub>,
) -> Result<Value, String> {
    if !ack {
        return Err(
            "actuator test refused: confirm you understand it commands the ECU (ack required)"
                .into(),
        );
    }
    let (def, text) = load_def_text(&def_path)?;
    let drive = parse_hex(state.as_deref().unwrap_or(""))?;
    let addr = adapter::write_addr(
        adapter::resolve(tx_id.as_deref(), rx_id.as_deref(), Some(&text))?,
        "actuator test",
    )?;
    let key = ConnKey::new(
        &driver,
        port.as_deref(),
        baud.as_deref(),
        bus.as_deref(),
        addr,
    );
    let r = hub.run("actuator test", move |s| {
        s.with_diag(&key, timeout_ms, |d| {
            adapter::enter_extended_session(d)?;
            let r = opentune_adaptation::run_actuator_test(
                d,
                &def,
                &channel,
                &drive,
                &WriteOptions::default(),
            )
            .map_err(|e| e.to_string());
            // The engine has already returned control to the ECU.
            adapter::leave_session(d);
            r
        })
    })?;
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
        "target": addr.json(false),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{testkit::SimBus, Addr};
    use opentune_udssim::server::UdsServer;

    const ABS: Addr = Addr {
        tx_id: 0x760,
        rx_id: 0x768,
    };

    #[test]
    fn routine_start_runs_on_the_target_in_extended_session() {
        let def = AdaptationDef::from_json(
            r#"{"id":"abs","name":"ABS","channels":[
                {"name":"bleed","kind":"routine","id":4660}]}"#,
        )
        .unwrap();
        let bus = SimBus::default();
        bus.add(0x7E0, 0x7E8, UdsServer::new());
        let mut abs = UdsServer::new();
        abs.set_routine(0x1234, vec![0x00]);
        bus.add(0x760, 0x768, abs);

        let mut s = bus.session();
        let key = ConnKey::new("sim", None, None, None, ABS);
        routine_once(&mut s, &key, &def, "bleed", RoutineOp::Start, &[], 100).unwrap();
        assert!(bus.sent_to(0x7E0).is_empty(), "engine must not be touched");
        let reqs = bus.sent_to(0x760);
        assert_eq!(reqs[0], vec![0x10, 0x03]);
        assert_eq!(&reqs[1][..4], &[0x31, 0x01, 0x12, 0x34]);
    }

    #[test]
    fn routine_ops_classified() {
        assert!(op_mutates(parse_op("start").unwrap()));
        assert!(op_mutates(parse_op("Stop").unwrap()));
        assert!(!op_mutates(parse_op("results").unwrap()));
        assert!(parse_op("erase").is_err());
    }
}
