//! Service functions feature — run declarative service procedures (DPF forced
//! regeneration, oil / service-interval reset, SAS calibration, EPB service
//! mode, …) from a `ServiceScript` JSON over the live adapter.
//!
//! A procedure is **data**: an ordered, safe UDS flow described by
//! [`opentune_servicefn::ServiceScript`]. One safe executor
//! ([`opentune_servicefn::run_script`]) runs any of them and **stops on the
//! first failure**, returning a step-by-step report.
//!
//! Two commands back the panel:
//!
//! * [`service_plan`] — a dry run. It validates the script and lists the
//!   planned steps and **sends nothing at all** (no writes, no actions, no I/O);
//!   it does not even open the adapter. A script that needs UDS SecurityAccess
//!   can still be planned.
//! * [`service_run`] — the real thing. It **refuses** unless `ack` is `true`
//!   and the target module's CAN address is known (UI `tx_id`/`rx_id` or the
//!   script's optional top-level `req_id`/`resp_id` — never a silent default to
//!   the engine ECU), then executes the script through the app-wide adapter hub
//!   in one adapter binding. It returns the same step report.
//!
//! OpenTune ships **no OEM seed/key provider**, so `run_script` is always
//! called with `key: None`. A script that needs SecurityAccess therefore errors
//! with `MissingKeyProvider` when run here — surfaced honestly to the operator.

use crate::adapter::{self, AdapterHub, ConnKey};
use opentune_servicefn::{
    run_script, RunOptions, RunOutcome, RunReport, ServiceError, ServiceScript, StepReport,
    StepStatus,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Serialization helpers (RunReport & friends do not derive Serialize)
// ---------------------------------------------------------------------------

/// Uppercase space-separated hex, e.g. `"50 03"`.
fn hexs(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Stable lowercase token for a step outcome.
fn status_str(s: StepStatus) -> &'static str {
    match s {
        StepStatus::Ok => "ok",
        StepStatus::Skipped => "skipped",
        StepStatus::Failed => "failed",
    }
}

/// Serialize one [`StepReport`] into the shape the table renders
/// (`index | op | status | detail`, with optional request/response hex).
fn step_json(sr: &StepReport) -> Value {
    json!({
        "index": sr.index,
        "op": sr.op,
        "status": status_str(sr.status),
        "detail": sr.detail,
        "request": sr.request.as_deref().map(hexs),
        "response": sr.response.as_deref().map(hexs),
    })
}

/// Serialize the overall [`RunOutcome`].
fn outcome_json(o: &RunOutcome) -> Value {
    match o {
        RunOutcome::Completed => json!({ "kind": "completed" }),
        RunOutcome::Planned => json!({ "kind": "planned" }),
        RunOutcome::Failed { at } => json!({ "kind": "failed", "at": at }),
    }
}

/// Serialize a whole [`RunReport`].
fn report_json(rep: &RunReport) -> Value {
    json!({
        "script_id": rep.script_id,
        "dry_run": rep.dry_run,
        "outcome": outcome_json(&rep.outcome),
        "security_unlocked": rep.security_unlocked,
        "steps": rep.steps.iter().map(step_json).collect::<Vec<_>>(),
    })
}

/// Build a plan report directly from the (validated) script's steps, without
/// running the executor. Used when a script needs SecurityAccess: the executor
/// refuses a dry run with `MissingKeyProvider`, but the operator may still want
/// to preview the ordered steps. Sends nothing.
fn manual_plan(script: &ServiceScript) -> Value {
    let steps: Vec<Value> = script
        .steps
        .iter()
        .enumerate()
        .map(|(i, s)| {
            json!({
                "index": i,
                "op": s.label(),
                "status": "planned",
                "detail": "plan only — not sent (SecurityAccess required)",
                "request": Value::Null,
                "response": Value::Null,
            })
        })
        .collect();
    json!({
        "mode": "plan",
        "script_id": script.id,
        "name": script.name,
        "description": script.description,
        "dry_run": true,
        "outcome": { "kind": "planned" },
        "security_unlocked": false,
        "requires_security": script.requires_security,
        "needs_key_provider": true,
        "steps": steps,
        "note": "This script needs UDS SecurityAccess. OpenTune ships no OEM seed/key provider, so it can be planned here but not executed.",
    })
}

// ---------------------------------------------------------------------------
// Script loading
// ---------------------------------------------------------------------------

/// Read and parse a [`ServiceScript`] from a JSON file on disk, with its text.
fn load_script_text(path: &str) -> Result<(ServiceScript, String), String> {
    let txt = std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
    let s =
        ServiceScript::from_json(&txt).map_err(|e| format!("invalid service script JSON: {e}"))?;
    Ok((s, txt))
}

fn load_script(path: &str) -> Result<ServiceScript, String> {
    load_script_text(path).map(|(s, _)| s)
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Plan a service script (DRY RUN): validate it and list every step it would
/// run, **sending nothing** — no adapter is even opened. Always safe. A script
/// that needs SecurityAccess is still planned (it just cannot be executed here).
///
/// `driver`/`port`/`baud`/`bus` are accepted for a uniform call shape but are
/// unused: planning touches no hardware.
#[tauri::command]
pub fn service_plan(
    driver: Option<String>,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    script_path: String,
    timeout_ms: u64,
) -> Result<Value, String> {
    let _ = (driver, port, baud, bus, timeout_ms); // planning sends nothing
    let script = load_script(&script_path)?;

    // A dry run never touches the transport; a no-op RawDiag is all that is
    // needed, and it errors loudly if the executor ever tried to send.
    let mut noop = |_req: &[u8]| -> std::result::Result<Vec<u8>, String> {
        Err("dry-run: no request is sent while planning".to_string())
    };
    let opts = RunOptions {
        dry_run: true,
        skip_waits: true,
    };

    match run_script(&mut noop, &script, None, &opts) {
        Ok(rep) => {
            let mut v = report_json(&rep);
            v["mode"] = json!("plan");
            v["name"] = json!(script.name);
            v["description"] = json!(script.description);
            v["requires_security"] = json!(script.requires_security);
            v["needs_key_provider"] = json!(script.requires_security || script.has_security_step());
            Ok(v)
        }
        // The script validated but needs SecurityAccess (no key provider ships).
        // Still show the plan rather than hiding the whole procedure.
        Err(ServiceError::MissingKeyProvider) => Ok(manual_plan(&script)),
        Err(e) => Err(e.to_string()),
    }
}

/// Execute a service script for real over the live adapter. **Refuses** unless
/// `ack` is `true` and the target module's address is known (UI `tx_id`/`rx_id`,
/// else the script's `req_id`/`resp_id`). Runs the script step by step in one
/// adapter binding (`key: None` — no OEM seed/key provider ships, so a
/// SecurityAccess script errors with `MissingKeyProvider`), and returns the
/// step-by-step report. A failed step stops the run safely and is reported.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn service_run(
    driver: Option<String>,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    tx_id: Option<String>,
    rx_id: Option<String>,
    script_path: String,
    timeout_ms: u64,
    ack: bool,
    hub: tauri::State<'_, AdapterHub>,
) -> Result<Value, String> {
    if !ack {
        return Err(
            "refusing to run: tick the confirmation box — Run sends real UDS writes/actions to the module"
                .into(),
        );
    }
    let (script, text) = load_script_text(&script_path)?;
    let addr = adapter::write_addr(
        adapter::resolve(tx_id.as_deref(), rx_id.as_deref(), Some(&text))?,
        "service run",
    )?;
    let driver = driver.unwrap_or_else(|| "elm327obd".to_string());
    let key = ConnKey::new(
        &driver,
        port.as_deref(),
        baud.as_deref(),
        bus.as_deref(),
        addr,
    );

    let opts = RunOptions {
        dry_run: false,
        skip_waits: false,
    };
    let sc = script.clone();
    let rep = hub.run("service run", move |s| {
        s.with_diag(&key, timeout_ms, |d| {
            run_script(d, &sc, None, &opts).map_err(|e| e.to_string())
        })
    })?;

    let mut v = report_json(&rep);
    v["mode"] = json!("run");
    v["name"] = json!(script.name);
    v["description"] = json!(script.description);
    v["target"] = addr.json(false);
    Ok(v)
}
