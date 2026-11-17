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
//! OpenTune ships **no OEM seed/key provider**. A script that needs
//! SecurityAccess runs only when it names a `seedkey` id that a plugin
//! resolver ([`SeedKeys`]) knows; the key provider then wraps that
//! plugin-supplied algorithm. Without an id, or with one no plugin knows, the
//! script can be planned but not run, and the report says which id was missing.

use crate::adapter::{self, AdapterHub, ConnKey, Session};
use crate::seedkeys::{self, SeedKeys};
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

// ---------------------------------------------------------------------------
// Seed/key: which handler (if any) answers the script's SecurityAccess
// ---------------------------------------------------------------------------

/// Whether the script needs a key provider at all.
fn needs_key(script: &ServiceScript) -> bool {
    script.requires_security || script.has_security_step()
}

/// Whether the script's `seedkey` id resolves through the plugin resolvers.
fn key_available(script: &ServiceScript, keys: &SeedKeys) -> bool {
    script
        .seedkey
        .as_deref()
        .is_some_and(|id| keys.get(id).is_some())
}

/// Why a secured script cannot run here, naming the unavailable id.
fn missing_key_note(script: &ServiceScript) -> String {
    match script.seedkey.as_deref() {
        Some(id) => format!(
            "This script needs UDS SecurityAccess with seed/key algorithm '{}', which is not available in this build. OpenTune ships no OEM seed/key provider; a plugin must supply it. The script can be planned here but not executed.",
            id.trim()
        ),
        None => "This script needs UDS SecurityAccess but names no seedkey algorithm. OpenTune ships no OEM seed/key provider, so it can be planned here but not executed.".into(),
    }
}

/// Run the executor with the plugin handler for `script.seedkey` as its key
/// provider when it resolves, else with none (the executor then refuses a
/// secured script with `MissingKeyProvider` before sending anything).
fn run_with_keys(
    diag: &mut dyn opentune_servicefn::RawDiag,
    script: &ServiceScript,
    keys: &SeedKeys,
    opts: &RunOptions,
) -> Result<RunReport, ServiceError> {
    match script.seedkey.as_deref().and_then(|id| keys.get(id)) {
        Some(handler) => {
            let kp = move |seed: &[u8], level: u8| seedkeys::compute(handler.as_ref(), seed, level);
            run_script(diag, script, Some(&kp), opts)
        }
        None => run_script(diag, script, None, opts),
    }
}

/// Build a plan report directly from the (validated) script's steps, without
/// running the executor. Used when a script needs SecurityAccess and no key
/// provider is available: the executor refuses a dry run with
/// `MissingKeyProvider`, but the operator may still want to preview the
/// ordered steps. Sends nothing.
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
        "seedkey": script.seedkey,
        "seedkey_available": false,
        "steps": steps,
        "note": missing_key_note(script),
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
    seedkeys: tauri::State<'_, SeedKeys>,
) -> Result<Value, String> {
    let _ = (driver, port, baud, bus, timeout_ms); // planning sends nothing
    plan_script(&load_script(&script_path)?, &seedkeys)
}

/// [`service_plan`] without the Tauri wrapper: a dry run that sends nothing.
fn plan_script(script: &ServiceScript, keys: &SeedKeys) -> Result<Value, String> {
    // A dry run never touches the transport; a no-op RawDiag is all that is
    // needed, and it errors loudly if the executor ever tried to send.
    let mut noop = |_req: &[u8]| -> std::result::Result<Vec<u8>, String> {
        Err("dry-run: no request is sent while planning".to_string())
    };
    let opts = RunOptions {
        dry_run: true,
        skip_waits: true,
    };

    match run_with_keys(&mut noop, script, keys, &opts) {
        Ok(rep) => {
            let needs = needs_key(script);
            let mut v = report_json(&rep);
            v["mode"] = json!("plan");
            v["name"] = json!(script.name);
            v["description"] = json!(script.description);
            v["requires_security"] = json!(script.requires_security);
            v["needs_key_provider"] = json!(needs);
            v["seedkey"] = json!(script.seedkey);
            v["seedkey_available"] = json!(key_available(script, keys));
            if needs {
                if let Some(id) = script.seedkey.as_deref() {
                    v["note"] = json!(format!(
                        "SecurityAccess will use the plugin-supplied seed/key algorithm '{}' (not part of OpenTune).",
                        id.trim()
                    ));
                }
            }
            Ok(v)
        }
        // The script validated but needs SecurityAccess and no plugin supplies
        // its algorithm. Still show the plan rather than hiding the procedure.
        Err(ServiceError::MissingKeyProvider) => Ok(manual_plan(script)),
        Err(e) => Err(e.to_string()),
    }
}

/// Execute a service script for real over the live adapter. **Refuses** unless
/// `ack` is `true` and the target module's address is known (UI `tx_id`/`rx_id`,
/// else the script's `req_id`/`resp_id`). Runs the script step by step in one
/// adapter binding and returns the step-by-step report. A failed step stops the
/// run safely and is reported. A SecurityAccess script runs only when its
/// `seedkey` id resolves through a plugin ([`SeedKeys`]); otherwise it is
/// refused before the adapter is touched, naming the missing id.
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
    seedkeys: tauri::State<'_, SeedKeys>,
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
    check_runnable(&script, &seedkeys)?;
    let sc = script.clone();
    let keys = seedkeys.inner().clone();
    let rep = hub.run("service run", move |s| {
        run_on_session(s, &key, timeout_ms, &sc, &keys, &opts)
    })?;

    let mut v = report_json(&rep);
    v["mode"] = json!("run");
    v["name"] = json!(script.name);
    v["description"] = json!(script.description);
    v["target"] = addr.json(false);
    if let Some(id) = script.seedkey.as_deref().filter(|_| needs_key(&script)) {
        v["seedkey"] = json!(id.trim());
    }
    Ok(v)
}

/// Refuse a secured script whose key provider is unavailable, before the
/// adapter is claimed or anything is sent.
fn check_runnable(script: &ServiceScript, keys: &SeedKeys) -> Result<(), String> {
    script.validate().map_err(|e| e.to_string())?;
    if needs_key(script) && !key_available(script, keys) {
        return Err(missing_key_note(script));
    }
    Ok(())
}

/// Execute `script` for real on an adapter session (the body of
/// [`service_run`]'s hub operation). The resolved handler is created on the
/// adapter worker thread, so it never has to be `Send`.
fn run_on_session(
    s: &mut Session,
    key: &ConnKey,
    timeout_ms: u64,
    script: &ServiceScript,
    keys: &SeedKeys,
    opts: &RunOptions,
) -> Result<RunReport, String> {
    s.with_diag(key, timeout_ms, |d| {
        run_with_keys(d, script, keys, opts).map_err(|e| match e {
            ServiceError::MissingKeyProvider => missing_key_note(script),
            e => e.to_string(),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::Addr;
    use crate::seedkeys::testkeys::{first, vcar};

    const AIRBAG: Addr = Addr {
        tx_id: 0x715,
        rx_id: 0x77F,
    };

    fn airbag_script(seedkey: Option<&str>) -> ServiceScript {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packs/vcar-demo/services/airbag-lamp-test.json");
        let mut s = ServiceScript::from_json(&std::fs::read_to_string(p).unwrap()).unwrap();
        assert!(needs_key(&s), "the demo airbag script is secured");
        s.seedkey = seedkey.map(str::to_string);
        s
    }

    fn run(script: &ServiceScript, keys: &SeedKeys, car: &str) -> Result<RunReport, String> {
        let hub = AdapterHub::with_registry();
        let key = ConnKey::new("vcar", Some(car), None, Some("hs"), AIRBAG);
        let (sc, keys) = (script.clone(), keys.clone());
        let opts = RunOptions {
            dry_run: false,
            skip_waits: true,
        };
        check_runnable(script, &keys)?;
        hub.run("service run", move |s| {
            run_on_session(s, &key, 500, &sc, &keys, &opts)
        })
    }

    #[test]
    fn a_seedkey_script_unlocks_through_the_plugin_resolver() {
        let keys = SeedKeys::new(vec![first, vcar]);
        let script = airbag_script(Some("vcar-test"));

        let plan = plan_script(&script, &keys).unwrap();
        assert_eq!(plan["outcome"]["kind"], "planned");
        assert_eq!(plan["seedkey_available"], true);
        assert!(plan["note"].as_str().unwrap().contains("plugin-supplied"));

        let rep = run(&script, &keys, "demo@service-seedkey").unwrap();
        assert!(rep.succeeded(), "{rep:?}");
        assert!(rep.security_unlocked);
        let sa = rep
            .steps
            .iter()
            .find(|s| s.op.starts_with("security_access"))
            .unwrap();
        assert_eq!(sa.status, StepStatus::Ok, "{}", sa.detail);
        assert!(sa.detail.starts_with("unlocked"), "{}", sa.detail);
    }

    #[test]
    fn an_unknown_or_missing_seedkey_keeps_plan_only() {
        let keys = SeedKeys::new(vec![first, vcar]);
        for id in [Some("oem-x"), None] {
            let script = airbag_script(id);
            let plan = plan_script(&script, &keys).unwrap();
            assert_eq!(plan["needs_key_provider"], true);
            assert_eq!(plan["seedkey_available"], false);
            assert!(plan["steps"]
                .as_array()
                .unwrap()
                .iter()
                .all(|s| s["status"] == "planned"));
            let note = plan["note"].as_str().unwrap().to_string();
            let e = run(&script, &keys, "demo@service-nokey").unwrap_err();
            assert_eq!(e, note, "run refuses with the same explanation");
            match id {
                Some(id) => assert!(note.contains(&format!("'{id}'")), "{note}"),
                None => assert!(note.contains("names no seedkey"), "{note}"),
            }
        }
        // The public app (no resolvers) cannot run even the test id.
        let e = run(
            &airbag_script(Some("vcar-test")),
            &SeedKeys::default(),
            "demo@service-public",
        )
        .unwrap_err();
        assert!(
            e.contains("'vcar-test'") && e.contains("not available"),
            "{e}"
        );
    }

    #[test]
    fn an_unsecured_script_needs_no_key() {
        let script = opentune_servicefn::examples::oil_service_reset();
        let plan = plan_script(&script, &SeedKeys::default()).unwrap();
        assert_eq!(plan["needs_key_provider"], false);
        assert!(plan.get("note").is_none());
        check_runnable(&script, &SeedKeys::default()).unwrap();
    }
}
