//! Module **Coding** feature for the OpenTune desktop app.
//!
//! Read a control module's configuration over the connected adapter, decode it
//! against a data-defined [`CodingDef`], preview a single `NAME=VALUE` edit as a
//! dry-run diff, and write it back safely. This module is self-contained: it
//! reuses the shared adapter opener (`crate::debug_open_diag`) and the standalone
//! [`opentune_coding`] engine, and adds nothing to the app's shared state.
//!
//! Wire it from `main.rs` with `mod feat_coding;` and by listing the three
//! `#[tauri::command]` functions in the `tauri::generate_handler!` invocation.

use opentune_coding::{
    decode, read_coding, set, write_coding, CodingDef, ParamKind, WriteOptions, WriteOutcome,
    WriteReport,
};
use serde_json::{json, Value};

/// Load a [`CodingDef`] from a JSON file path and validate it. Surfaces the file,
/// parse and validation errors honestly as `Err(String)`.
fn load_def(def_path: &str) -> Result<CodingDef, String> {
    let text = std::fs::read_to_string(def_path)
        .map_err(|e| format!("read coding def '{def_path}': {e}"))?;
    let def = CodingDef::from_json(&text).map_err(|e| format!("parse coding def: {e}"))?;
    def.validate()
        .map_err(|e| format!("invalid coding def: {e}"))?;
    Ok(def)
}

/// Present a parameter's kind as a short tag for the UI.
fn kind_tag(k: ParamKind) -> &'static str {
    match k {
        ParamKind::Enum => "enum",
        ParamKind::Bool => "bool",
        ParamKind::Number => "number",
    }
}

/// Serialize a [`WriteOutcome`] to JSON the frontend can branch on.
fn outcome_json(o: &WriteOutcome) -> Value {
    match o {
        WriteOutcome::Planned => json!({ "kind": "planned" }),
        WriteOutcome::NoChange => json!({ "kind": "no_change" }),
        WriteOutcome::Verified => json!({ "kind": "verified" }),
        WriteOutcome::Unverified => json!({ "kind": "unverified" }),
        WriteOutcome::VerifyFailed { did } => {
            json!({ "kind": "verify_failed", "did": format!("0x{did:04X}") })
        }
        WriteOutcome::WriteError {
            uncertain_did,
            message,
        } => json!({
            "kind": "write_error",
            "did": format!("0x{uncertain_did:04X}"),
            "message": message,
        }),
    }
}

/// Serialize a [`WriteReport`] (planned or executed) to JSON: the raw byte diff,
/// the decoded parameter diff, the DIDs written, the outcome and the security hint.
fn report_json(r: &WriteReport) -> Value {
    json!({
        "outcome": outcome_json(&r.outcome),
        "securityRequired": r.security_required,
        "written": r.written.iter().map(|d| format!("0x{d:04X}")).collect::<Vec<_>>(),
        "byteChanges": r.byte_changes.iter().map(|c| json!({
            "block": c.block,
            "did": format!("0x{:04X}", c.did),
            "byte": c.byte,
            "old": format!("{:02X}", c.old),
            "new": format!("{:02X}", c.new),
        })).collect::<Vec<_>>(),
        "paramChanges": r.param_changes.iter().map(|p| json!({
            "name": p.name,
            "old": { "value": p.old.0, "label": p.old.1 },
            "new": { "value": p.new.0, "label": p.new.1 },
        })).collect::<Vec<_>>(),
    })
}

/// Read the module's coding and decode every parameter.
///
/// Opens the selected adapter, reads each configuration DID declared by the def,
/// decodes the bit-fields to raw value + label, and returns the def header plus a
/// parameter array (each with its kind, note and enum options for the editor).
#[tauri::command]
pub async fn coding_read(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    def_path: String,
    timeout_ms: u64,
) -> Result<Value, String> {
    let def = load_def(&def_path)?;

    let mut drv =
        crate::debug_open_diag(&driver, port.as_deref(), baud.as_deref(), bus.as_deref())?;
    let t = drv
        .diag()
        .ok_or_else(|| "adapter has no diagnostic transport".to_string())?;
    let mut diag = opentune_core::CoreDiag::new(t, timeout_ms);

    let coding = read_coding(&mut diag, &def).map_err(|e| e.to_string())?;
    let decoded = decode(&def, &coding).map_err(|e| e.to_string())?;

    // `decode` preserves definition order, so zip aligns each decoded value with
    // its parameter metadata.
    let params: Vec<Value> = def
        .params
        .iter()
        .zip(decoded.iter())
        .map(|(p, d)| {
            json!({
                "name": d.name,
                "value": d.value,
                "label": d.label,
                "kind": kind_tag(p.kind),
                "note": p.note,
                "options": p.options.iter()
                    .map(|o| json!({ "value": o.value, "label": o.label }))
                    .collect::<Vec<_>>(),
            })
        })
        .collect();

    Ok(json!({
        "def": {
            "id": def.id,
            "name": def.name,
            "module": def.module,
            "requiresSecurity": def.requires_security,
        },
        "params": params,
    }))
}

/// Preview a single `NAME=VALUE` edit as a dry-run.
///
/// Reads the live coding as the baseline, applies the edit in-memory (validating
/// the value against the parameter's bit length and declared options), then runs
/// the write engine in `dry_run` mode so nothing is written. Returns the planned
/// byte- and parameter-level diff.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn coding_set_preview(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    def_path: String,
    timeout_ms: u64,
    name: String,
    value: u64,
) -> Result<Value, String> {
    let def = load_def(&def_path)?;

    let mut drv =
        crate::debug_open_diag(&driver, port.as_deref(), baud.as_deref(), bus.as_deref())?;
    let t = drv
        .diag()
        .ok_or_else(|| "adapter has no diagnostic transport".to_string())?;
    let mut diag = opentune_core::CoreDiag::new(t, timeout_ms);

    let baseline = read_coding(&mut diag, &def).map_err(|e| e.to_string())?;
    let desired = set(&def, &baseline, &name, value).map_err(|e| e.to_string())?;
    let report = write_coding(
        &mut diag,
        &def,
        &baseline,
        &desired,
        &WriteOptions {
            dry_run: true,
            ..Default::default()
        },
    )
    .map_err(|e| e.to_string())?;

    Ok(report_json(&report))
}

/// Apply a single `NAME=VALUE` edit to the module.
///
/// Reads the live coding as the baseline, applies the edit, then writes it back:
/// the engine re-reads a fresh backup, merges only the changed bytes, writes the
/// affected block(s) and reads back to verify. Returns the outcome and the diff.
/// Writing can immobilise a module; the caller must have unlocked SecurityAccess
/// first when the def's `requiresSecurity` flag is set (surfaced in the report).
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn coding_write(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    def_path: String,
    timeout_ms: u64,
    name: String,
    value: u64,
) -> Result<Value, String> {
    let def = load_def(&def_path)?;

    let mut drv =
        crate::debug_open_diag(&driver, port.as_deref(), baud.as_deref(), bus.as_deref())?;
    let t = drv
        .diag()
        .ok_or_else(|| "adapter has no diagnostic transport".to_string())?;
    let mut diag = opentune_core::CoreDiag::new(t, timeout_ms);

    let baseline = read_coding(&mut diag, &def).map_err(|e| e.to_string())?;
    let desired = set(&def, &baseline, &name, value).map_err(|e| e.to_string())?;
    let report = write_coding(
        &mut diag,
        &def,
        &baseline,
        &desired,
        &WriteOptions::default(),
    )
    .map_err(|e| e.to_string())?;

    Ok(report_json(&report))
}
