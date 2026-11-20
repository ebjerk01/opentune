//! # Actuator tests panel (backend)
//!
//! Timed output tests over UDS InputOutputControlByIdentifier (0x2F), driven
//! by the actuator channels an adaptation definition **declares** — a thin
//! layer over `opentune_adaptation::actuator` (plan, run, report) on the
//! app-wide adapter hub.
//!
//! A test is one long-running hub operation ([`AdapterHub::start_op`]): it
//! holds the adapter from the session request to the return to the default
//! session, so no other panel talks to the bus meanwhile ("adapter busy").
//! The engine enforces the duration itself, so the output is handed back at
//! the end even if the window goes away; `actuator_stop` (the STOP button,
//! a panel switch, an adapter disconnect, the window closing) ends it early
//! through a stop flag. Every exchange lands in [`ActuatorState`] as it
//! happens, so `actuator_status` never waits on the busy adapter.
//!
//! Safety gates (all checked before the adapter is touched):
//! * **Bench tier** — live tests run only on the alpha / bench channels;
//!   `actuator_list` / `actuator_plan` work everywhere (nothing is sent).
//! * **Per-run confirmation** — `ack` must be true for every start.
//! * **Declared channels only** — the channel, the option, the value range
//!   and the duration cap come from the definition. A **free-form** IO DID
//!   needs the Debug gate (alpha builds) *and* a second confirmation
//!   (`freeform_ack`), takes a raw state, is capped at
//!   [`FREEFORM_MAX_DURATION_MS`] and needs explicit Target ids.
//! * **Known target** — a test refuses an unknown module address.
//! * **SecurityAccess** — when the definition requires it, a seed/key id that
//!   resolves through [`SeedKeys`] (a plugin, or the user's armed SA2 script;
//!   OpenTune ships none).
//!
//! Commands: `actuator_list`, `actuator_plan`, `actuator_start`,
//! `actuator_status`, `actuator_stop`.
//!
//! The gates, the run on the adapter worker, the live exchange log and the
//! stop / return-of-control path live in [`opentune_hub::ops::actuator`],
//! shared with other front ends; this module keeps the Tauri commands and
//! plugs the session log ([`DebugLog`]) into the hub's `ExchangeLog` seam.

use crate::adapter::{self, AdapterHub};
use crate::seedkeys::SeedKeys;
use crate::state::{BuildChannel, DebugLog};
use opentune_adaptation::actuator::{self, FREEFORM_MAX_DURATION_MS};
pub use opentune_hub::ops::actuator::ActuatorState;
use opentune_hub::ops::actuator::{
    begin, load_def, plan_ask, prepare, status_of, stop, Ask, ExchangeLog,
};
use serde_json::{json, Value};

/// Each finished test's exchange lines go to the session log.
impl ExchangeLog for DebugLog {
    fn push(&self, line: String) {
        DebugLog::push(self, line)
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// The actuator channels a definition declares (no adapter needed).
#[tauri::command]
pub fn actuator_list(def_path: String) -> Result<Value, String> {
    let (def, text) = load_def(&def_path)?;
    let channels = actuator::actuators(&def).map_err(|e| e.to_string())?;
    let target = adapter::def_addr(&text)?.map(|a| a.json(false));
    Ok(json!({
        "id": def.id,
        "name": def.name,
        "module": def.module,
        "requires_security": def.requires_security,
        "security_level": actuator::security_level(&def),
        "target": target,
        "channels": channels,
        "other_channels": def.channels.len() - channels.len(),
        "freeform_max_duration_ms": FREEFORM_MAX_DURATION_MS,
    }))
}

/// Validate a test and return the exact requests it would send. Sends nothing.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn actuator_plan(
    def_path: Option<String>,
    channel: Option<String>,
    option: String,
    value: Option<f64>,
    raw: Option<u64>,
    duration_ms: Option<u64>,
    freeform_did: Option<String>,
    state: Option<String>,
    freeform_security: Option<bool>,
    build: tauri::State<'_, BuildChannel>,
) -> Result<Value, String> {
    let ask = Ask {
        def_path,
        channel,
        option,
        value,
        raw,
        duration_ms: duration_ms.unwrap_or(0),
        freeform_did,
        state,
        freeform_security: freeform_security.unwrap_or(false),
    };
    let p = plan_ask(&ask, &build)?;
    let mut v = serde_json::to_value(&p.plan).map_err(|e| e.to_string())?;
    v["freeform"] = json!(p.freeform);
    Ok(v)
}

/// Start a live test. Bench tier, ack-gated, declared channels only (or the
/// alpha Debug free-form path with `freeform_ack`). Returns once queued; poll
/// `actuator_status`.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn actuator_start(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    tx_id: Option<String>,
    rx_id: Option<String>,
    def_path: Option<String>,
    channel: Option<String>,
    option: String,
    value: Option<f64>,
    raw: Option<u64>,
    duration_ms: Option<u64>,
    timeout_ms: Option<u64>,
    ack: bool,
    seedkey: Option<String>,
    freeform_did: Option<String>,
    state: Option<String>,
    freeform_security: Option<bool>,
    freeform_ack: Option<bool>,
    hub: tauri::State<'_, AdapterHub>,
    keys: tauri::State<'_, SeedKeys>,
    build: tauri::State<'_, BuildChannel>,
    tests: tauri::State<'_, ActuatorState>,
    log: tauri::State<'_, DebugLog>,
) -> Result<Value, String> {
    let ask = Ask {
        def_path,
        channel,
        option,
        value,
        raw,
        duration_ms: duration_ms.unwrap_or(0),
        freeform_did,
        state,
        freeform_security: freeform_security.unwrap_or(false),
    };
    let start = prepare(
        &ask,
        &build,
        &keys,
        (&driver, port.as_deref(), baud.as_deref(), bus.as_deref()),
        tx_id.as_deref(),
        rx_id.as_deref(),
        timeout_ms.unwrap_or(1000).clamp(50, 10_000),
        ack,
        freeform_ack.unwrap_or(false),
        seedkey.as_deref(),
    )?;
    let plan = serde_json::to_value(&start.planned.plan).map_err(|e| e.to_string())?;
    let t = begin(&hub, &tests, &keys, &*log, start)?;
    Ok(json!({ "started": true, "plan": plan, "target": t.target }))
}

/// The current (or last) test: progress, every exchange so far, the report
/// once it has ended.
#[tauri::command]
pub fn actuator_status(tests: tauri::State<'_, ActuatorState>) -> Value {
    status_of(&tests)
}

/// End the running test now: control is handed back to the ECU, then the
/// final status is returned. Safe to call when nothing runs.
#[tauri::command]
pub async fn actuator_stop(
    reason: Option<String>,
    tests: tauri::State<'_, ActuatorState>,
) -> Result<Value, String> {
    stop(&tests, reason.as_deref().unwrap_or("STOP pressed"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack(rel: &str) -> String {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packs/vcar-demo")
            .join(rel)
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn list_shows_only_declared_actuators() {
        let v = actuator_list(pack("adaptation/engine.json")).unwrap();
        let chans = v["channels"].as_array().unwrap();
        assert_eq!(chans.len(), 1);
        assert_eq!(chans[0]["name"], "Radiator fan");
        assert_eq!(chans[0]["value"]["max"], 100.0);
        assert_eq!(chans[0]["options"][0], "return_control_to_ecu");
        assert_eq!(chans[0]["default_state"], "01");
        assert_eq!(v["other_channels"], 3);
        assert_eq!(v["target"]["tx"], "7E0");
    }
}
