//! Module **Coding** feature for the OpenTune desktop app.
//!
//! Read a control module's configuration over the connected adapter, decode it
//! against a data-defined [`CodingDef`], preview a single `NAME=VALUE` edit as a
//! dry-run diff, and write it back safely. It runs through the app-wide adapter
//! hub ([`crate::adapter`]) and the standalone [`opentune_coding`] engine.
//!
//! Addressing: the target module's CAN ids come from the UI (`tx_id`/`rx_id`)
//! or the def's optional top-level `req_id`/`resp_id`. Reads fall back to the
//! engine ECU (reported as `target.assumed`); **a write refuses an unknown
//! target** rather than landing on the engine.
//!
//! A write runs in ONE adapter binding: extended session (`10 03`), fresh
//! baseline read, write + verify, then a best-effort return to the default
//! session. The report includes the pre-write `backup` so the UI can offer a
//! restore after a failed verify or write error.

use crate::adapter::{self, AdapterHub, ConnKey, Session};
use opentune_coding::{
    decode, read_coding, set, write_coding, Coding, CodingDef, ParamKind, WriteOptions,
    WriteOutcome, WriteReport,
};
use serde_json::{json, Value};

/// Load a [`CodingDef`] from a JSON file path and validate it, together with
/// the def's optional module address. Surfaces the file, parse and validation
/// errors honestly as `Err(String)`.
fn load_def(def_path: &str) -> Result<(CodingDef, String), String> {
    let text = std::fs::read_to_string(def_path)
        .map_err(|e| format!("read coding def '{def_path}': {e}"))?;
    let def = CodingDef::from_json(&text).map_err(|e| format!("parse coding def: {e}"))?;
    def.validate()
        .map_err(|e| format!("invalid coding def: {e}"))?;
    Ok((def, text))
}

/// A coding snapshot as `[{ did, hex }]` (block order), for the UI's backup.
fn coding_json(c: &Coding) -> Value {
    json!(c
        .blocks
        .iter()
        .map(|b| json!({
            "did": format!("0x{:04X}", b.did),
            "hex": b.bytes.iter().map(|x| format!("{x:02X}")).collect::<Vec<_>>().join(" "),
        }))
        .collect::<Vec<_>>())
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
        // The module's coding as read immediately before the write: what to
        // restore after a verify failure or a write error.
        "backup": coding_json(&r.backup),
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

/// Read the coding over the bound adapter and decode it into the panel's shape.
fn read_json(
    s: &mut Session,
    key: &ConnKey,
    def: &CodingDef,
    timeout_ms: u64,
) -> Result<Value, String> {
    let coding = s.with_diag(key, timeout_ms, |d| {
        read_coding(d, def).map_err(|e| e.to_string())
    })?;
    let decoded = decode(def, &coding).map_err(|e| e.to_string())?;

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

/// Dry-run a single `NAME=VALUE` edit against the live coding (nothing sent
/// beyond reads).
fn preview(
    s: &mut Session,
    key: &ConnKey,
    def: &CodingDef,
    timeout_ms: u64,
    name: &str,
    value: u64,
) -> Result<WriteReport, String> {
    s.with_diag(key, timeout_ms, |d| {
        let baseline = read_coding(d, def).map_err(|e| e.to_string())?;
        let desired = set(def, &baseline, name, value).map_err(|e| e.to_string())?;
        write_coding(
            d,
            def,
            &baseline,
            &desired,
            &WriteOptions {
                dry_run: true,
                ..Default::default()
            },
        )
        .map_err(|e| e.to_string())
    })
}

/// Apply a single `NAME=VALUE` edit in ONE adapter binding: extended session
/// (`10 03`) first — most modules reject `2E` in the default session — then a
/// fresh baseline, the write + read-back verify, and a best-effort `10 01`.
pub(crate) fn write_one(
    s: &mut Session,
    key: &ConnKey,
    def: &CodingDef,
    timeout_ms: u64,
    name: &str,
    value: u64,
) -> Result<WriteReport, String> {
    s.with_diag(key, timeout_ms, |d| {
        adapter::enter_extended_session(d)?;
        let r = (|| {
            let baseline = read_coding(d, def).map_err(|e| e.to_string())?;
            let desired = set(def, &baseline, name, value).map_err(|e| e.to_string())?;
            write_coding(d, def, &baseline, &desired, &WriteOptions::default())
                .map_err(|e| e.to_string())
        })();
        adapter::leave_session(d);
        r
    })
}

/// Read the module's coding and decode every parameter.
///
/// Reads each configuration DID declared by the def from the target module
/// (UI `tx_id`/`rx_id`, else the def's `req_id`/`resp_id`, else the engine ECU —
/// flagged `target.assumed`), decodes the bit-fields to raw value + label, and
/// returns the def header plus a parameter array.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn coding_read(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    tx_id: Option<String>,
    rx_id: Option<String>,
    def_path: String,
    timeout_ms: u64,
    hub: tauri::State<'_, AdapterHub>,
) -> Result<Value, String> {
    let (def, text) = load_def(&def_path)?;
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
    let mut v = hub.run("coding read", move |s| read_json(s, &key, &def, timeout_ms))?;
    v["target"] = addr.json(assumed);
    Ok(v)
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
    tx_id: Option<String>,
    rx_id: Option<String>,
    def_path: String,
    timeout_ms: u64,
    name: String,
    value: u64,
    hub: tauri::State<'_, AdapterHub>,
) -> Result<Value, String> {
    let (def, text) = load_def(&def_path)?;
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
    let report = hub.run("coding preview", move |s| {
        preview(s, &key, &def, timeout_ms, &name, value)
    })?;
    let mut v = report_json(&report);
    v["target"] = addr.json(assumed);
    Ok(v)
}

/// A coding write changes the module: refuse it unless the UI passed an
/// explicit `ack = true` (the same pattern as adaptation routines / actuator
/// tests, service runs and clearing DTCs).
fn require_ack(ack: Option<bool>) -> Result<(), String> {
    if ack == Some(true) {
        Ok(())
    } else {
        Err("coding write refused: preview the change and confirm it (ack required)".into())
    }
}

/// Apply a single `NAME=VALUE` edit to the module.
///
/// ACK-GATED: refuses unless `ack` is true (the UI asks for it after showing
/// the previewed diff). Refuses unless the target module's address is known (UI `tx_id`/`rx_id` or
/// the def's `req_id`/`resp_id`). In one adapter binding: enters the extended
/// session, re-reads a fresh backup, merges only the changed bytes, writes the
/// affected block(s), reads back to verify, and returns to the default session.
/// Returns the outcome, the diff and the pre-write `backup`. Writing can
/// immobilise a module; OpenTune performs no SecurityAccess unlock, so a def
/// with `requiresSecurity` will typically be refused by the module (surfaced).
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn coding_write(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    tx_id: Option<String>,
    rx_id: Option<String>,
    def_path: String,
    timeout_ms: u64,
    name: String,
    value: u64,
    ack: Option<bool>,
    hub: tauri::State<'_, AdapterHub>,
) -> Result<Value, String> {
    require_ack(ack)?;
    let (def, text) = load_def(&def_path)?;
    let addr = adapter::write_addr(
        adapter::resolve(tx_id.as_deref(), rx_id.as_deref(), Some(&text))?,
        "coding write",
    )?;
    let key = ConnKey::new(
        &driver,
        port.as_deref(),
        baud.as_deref(),
        bus.as_deref(),
        addr,
    );
    let report = hub.run("coding write", move |s| {
        write_one(s, &key, &def, timeout_ms, &name, value)
    })?;
    let mut v = report_json(&report);
    v["target"] = addr.json(false);
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{testkit::SimBus, Addr};
    use opentune_udssim::server::UdsServer;

    const BCM: Addr = Addr {
        tx_id: 0x726,
        rx_id: 0x72E,
    };

    fn def() -> CodingDef {
        CodingDef::from_json(
            r#"{"id":"bcm","name":"BCM","req_id":"726","resp_id":"72E",
                "addressing":{"kind":"did","dids":[1536]},
                "params":[{"name":"DRL","block":0,"byte":0,"bit":0,"len":1,"kind":"bool"}]}"#,
        )
        .unwrap()
    }

    #[test]
    fn write_targets_the_module_in_extended_session_and_keeps_backup() {
        let bus = SimBus::default();
        let mut engine = UdsServer::new();
        engine.set_did(0x0600, vec![0x00, 0x00]);
        bus.add(0x7E0, 0x7E8, engine);
        let mut bcm = UdsServer::new();
        bcm.set_did(0x0600, vec![0x00, 0x11]);
        bus.add(0x726, 0x72E, bcm);

        let mut s = bus.session();
        let key = ConnKey::new("sim", None, None, None, BCM);
        let r = write_one(&mut s, &key, &def(), 100, "DRL", 1).unwrap();
        assert!(
            matches!(r.outcome, WriteOutcome::Verified),
            "{:?}",
            r.outcome
        );

        // Nothing went to the engine; the BCM saw 10 03 before its 2E.
        assert!(bus.sent_to(0x7E0).is_empty());
        let bcm_reqs = bus.sent_to(0x726);
        let ext = bcm_reqs
            .iter()
            .position(|q| q == &vec![0x10, 0x03])
            .unwrap();
        let wr = bcm_reqs
            .iter()
            .position(|q| q.first() == Some(&0x2E))
            .unwrap();
        assert!(
            ext < wr,
            "extended session must precede the write: {bcm_reqs:02X?}"
        );
        assert_eq!(bcm_reqs.last().unwrap(), &vec![0x10, 0x01]);
        assert_eq!(
            bus.ecus.borrow()[&(0x726, 0x72E)].did(0x0600),
            Some(&[0x01, 0x11][..])
        );

        let v = report_json(&r);
        assert_eq!(v["backup"][0]["did"], "0x0600");
        assert_eq!(v["backup"][0]["hex"], "00 11");
    }

    #[test]
    fn write_needs_explicit_ack() {
        assert!(require_ack(None).is_err());
        assert!(require_ack(Some(false)).is_err());
        assert!(require_ack(None).unwrap_err().contains("ack required"));
        assert!(require_ack(Some(true)).is_ok());
    }

    #[test]
    fn def_address_is_used_and_unknown_write_target_refused() {
        let text = r#"{"id":"x","name":"x","addressing":{"kind":"did","dids":[1]},"params":[]}"#;
        let none = adapter::resolve(None, None, Some(text)).unwrap();
        assert!(adapter::write_addr(none, "coding write").is_err());
        let with = r#"{"id":"x","req_id":"726","resp_id":"72E"}"#;
        assert_eq!(adapter::resolve(None, None, Some(with)).unwrap(), Some(BCM));
    }
}
