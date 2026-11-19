//! The upload payload: built only from diagnostics-log records
//! ([`crate::diaglog`]), a session summary and the user's note, redacted as
//! a whole ([`opentune_core::redact::redact_json`]) and capped at
//! [`MAX_PAYLOAD`]. Nothing here opens any other file: ECU images, datalogs,
//! definitions and CAN captures cannot reach a payload.

use crate::diaglog::{self, Level, Record, Src};
use opentune_core::redact::{redact_json, RedactContext};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// Hard cap of one payload (and of one HTTP request body).
pub const MAX_PAYLOAD: usize = 2 * 1024 * 1024;
/// Records in a manual / crash payload (the newest are kept).
pub const MAX_RECORDS: usize = 1000;
/// Records in an error-triggered payload.
pub const MAX_ERROR_RECORDS: usize = 500;
/// The payload format.
pub const SCHEMA: &str = "opentune-diagnostics/1";

/// Why a payload was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// The user clicked Send (after the preview).
    Manual,
    /// The previous session crashed.
    Crash,
    /// New errors in this session (auto mode, coalesced).
    Error,
    /// The periodic session summary (auto mode).
    Periodic,
}

impl Trigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Trigger::Manual => "manual",
            Trigger::Crash => "crash",
            Trigger::Error => "error",
            Trigger::Periodic => "periodic",
        }
    }
}

/// The startup mark the frontend makes once its first frame is on screen.
const FIRST_FRAME: &str = "ui: first frame painted";

/// Why a finished session counts as crashed: a panic record, or it never
/// painted its first frame. `None` for a clean one.
pub fn crash_reason(session: &[Record]) -> Option<&'static str> {
    if session
        .iter()
        .any(|r| r.src == Src::Rust && r.level == Level::Error && r.msg.starts_with("panic:"))
    {
        return Some("panic");
    }
    if !session.iter().any(|r| r.msg.starts_with(FIRST_FRAME)) {
        return Some("no first frame");
    }
    None
}

/// The crashed previous session in `all` (every record of the log, the
/// current session last): its header time, the reason and its records. Only
/// a session that starts with its header counts (a fragment left by rotation
/// does not).
pub fn previous_crash(all: &[Record]) -> Option<(String, &'static str, &[Record])> {
    let sessions = diaglog::sessions(all);
    if sessions.len() < 2 {
        return None;
    }
    let prev = sessions[sessions.len() - 2];
    if !prev.first().is_some_and(Record::is_session_header) {
        return None;
    }
    crash_reason(prev).map(|why| (prev[0].ts.clone(), why, prev))
}

/// Version, channel, OS, WebKit and render decision from a session header.
pub fn app_info(header: Option<&Record>, fallback_version: &str, fallback_channel: &str) -> Value {
    let mut out = Map::new();
    if let Some(Value::Object(c)) = header.and_then(|h| h.ctx.as_ref()) {
        for k in [
            "app",
            "version",
            "core",
            "channel",
            "os",
            "arch",
            "os_release",
            "webkit",
            "render",
        ] {
            if let Some(v) = c.get(k) {
                out.insert(k.into(), v.clone());
            }
        }
    }
    out.entry("version")
        .or_insert_with(|| json!(fallback_version));
    out.entry("channel")
        .or_insert_with(|| json!(fallback_channel));
    out.entry("os")
        .or_insert_with(|| json!(std::env::consts::OS));
    out.entry("arch")
        .or_insert_with(|| json!(std::env::consts::ARCH));
    Value::Object(out)
}

/// Counts for one session: errors and warnings by source, panics, and how
/// often each panel was opened. No car, file, image or live data.
pub fn summary(
    session: &[Record],
    all: &[Record],
    panels: &BTreeMap<String, u32>,
    uptime_s: u64,
) -> Value {
    let count = |f: &dyn Fn(&Record) -> bool| session.iter().filter(|r| f(r)).count();
    let sessions = diaglog::sessions(all);
    let finished = sessions.len().saturating_sub(1);
    let crashed = sessions[..finished]
        .iter()
        .filter(|s| s.first().is_some_and(Record::is_session_header) && crash_reason(s).is_some())
        .count();
    json!({
        "uptime_min": uptime_s / 60,
        "session": {
            "errors": count(&|r| r.level == Level::Error),
            "warnings": count(&|r| r.level == Level::Warn),
            "panics": count(&|r| r.src == Src::Rust && r.msg.starts_with("panic:")),
            "js_errors": count(&|r| r.src == Src::Js && r.level >= Level::Warn),
            "cmd_errors": count(&|r| r.src == Src::Cmd && r.level >= Level::Warn),
        },
        "log": { "sessions": sessions.len(), "crashed_sessions": crashed },
        "panels_opened": panels,
    })
}

/// The parts of one payload before redaction.
pub struct Parts<'a> {
    pub trigger: Trigger,
    pub created: String,
    pub label: Option<&'a str>,
    pub app: Value,
    pub summary: Value,
    /// Crashed session: header time and reason.
    pub crash: Option<(&'a str, &'a str)>,
    pub note: Option<&'a str>,
    /// Log records, oldest first (the newest are kept when over the cap).
    pub records: &'a [Record],
}

fn assemble(p: &Parts<'_>, records: &[Record], omitted: usize) -> Value {
    let mut v = json!({
        "schema": SCHEMA,
        "trigger": p.trigger.as_str(),
        "created": p.created,
        "app": p.app,
        "summary": p.summary,
        "records": records,
        "omitted_records": omitted,
    });
    if let Some(l) = p.label.filter(|l| !l.is_empty()) {
        v["label"] = json!(l);
    }
    if let Some((ts, why)) = p.crash {
        v["crash"] = json!({ "session": ts, "reason": why });
    }
    if let Some(n) = p.note.map(str::trim).filter(|n| !n.is_empty()) {
        v["note"] = json!(n.chars().take(4000).collect::<String>());
    }
    v
}

/// The exact bytes to show and send: [`Parts`] as pretty JSON, every string
/// redacted, at most [`MAX_PAYLOAD`] bytes (the oldest records are dropped
/// first and counted in `omitted_records`).
pub fn build(p: &Parts<'_>, ctx: &RedactContext) -> Result<Vec<u8>, String> {
    let mut v = assemble(p, p.records, 0);
    redact_json(&mut v, ctx);
    let total = p.records.len();
    let mut records = match v.get_mut("records").map(Value::take) {
        Some(Value::Array(a)) => a,
        _ => Vec::new(),
    };
    loop {
        let omitted = total - records.len();
        v["records"] = Value::Array(records);
        v["omitted_records"] = json!(omitted);
        let body = serde_json::to_vec_pretty(&v).map_err(|e| e.to_string())?;
        if body.len() <= MAX_PAYLOAD {
            return Ok(body);
        }
        records = match v["records"].take() {
            Value::Array(a) => a,
            _ => Vec::new(),
        };
        if records.is_empty() {
            return Err(format!(
                "the diagnostics payload is {} bytes even without log records (cap {MAX_PAYLOAD})",
                body.len()
            ));
        }
        let drop = (records.len() / 4).max(1);
        records.drain(..drop);
    }
}

/// The newest `n` of `records`.
pub fn newest(records: &[Record], n: usize) -> &[Record] {
    &records[records.len().saturating_sub(n)..]
}
