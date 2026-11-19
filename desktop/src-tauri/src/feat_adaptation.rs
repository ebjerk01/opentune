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
//
// KWP2000 (VAG, TP2.0 / K-line): `adaptation_kwp_read`, `adaptation_kwp_basic`
// and `adaptation_kwp_adapt` drive the engine's `kwp` path on a TP2.0 channel
// (raw-CAN drivers) or a K-line driver at an explicit module address, or on
// the engine's synthetic module (`sim`). Basic settings and adaptation test /
// save are ack-gated here and gated again in the engine (session + a seed/key
// handler for SecurityAccess, resolved through the plugin `SeedKeys`; OpenTune
// ships no VAG login algorithm, so the public app can run them only against
// the simulator with its test-only `kwpsim-test`). The adapter is closed after
// each KWP operation so the TP2.0 channel is torn down.

use crate::adapter::{self, AdapterHub, Addr, ConnKey, Session};
use crate::seedkeys::SeedKeys;
use opentune_adaptation::kwp::{self, AdaptMode, KwpLogin, KwpWriteOptions};
use opentune_adaptation::{AdaptationDef, RawDiag, RoutineOp, WriteOptions};
use opentune_core::protocol::seedkey::SeedKey;
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
                K::KwpGroup { group } => {
                    o["kind"] = json!("kwp_group");
                    o["group"] = json!(group);
                }
                K::KwpBasicSettings { group, .. } => {
                    o["kind"] = json!("kwp_basic_settings");
                    o["group"] = json!(group);
                }
                K::KwpAdaptation {
                    channel,
                    scaling,
                    min,
                    max,
                    ..
                } => {
                    o["kind"] = json!("kwp_adaptation");
                    o["channel"] = json!(channel);
                    o["unit"] = json!(scaling.unit);
                    o["min"] = json!(min);
                    o["max"] = json!(max);
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
        "kwp_addr": def_kwp_addr(&text)?,
        "channels": channels,
    }))
}

// ---------------------------------------------------------------------------
// KWP2000 (VAG, TP2.0 / K-line)
// ---------------------------------------------------------------------------

/// A byte: decimal or `0x` hex.
fn parse_u8(what: &str, s: &str) -> Result<u8, String> {
    parse_num(what, s, 0xFF).map(|v| v as u8)
}

fn parse_num(what: &str, s: &str, max: u32) -> Result<u32, String> {
    let t = s.trim();
    let v = match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(h) => u32::from_str_radix(h, 16).ok(),
        None => t.parse().ok(),
    };
    v.filter(|v| *v <= max)
        .ok_or_else(|| format!("{what} '{s}': expected 0..={max} (decimal or 0x hex)"))
}

/// The definition's optional top-level `kwp_addr` (the module's TP2.0 / K-line
/// address), a number or a string.
fn def_kwp_addr(def_text: &str) -> Result<Option<u8>, String> {
    let v: Value = serde_json::from_str(def_text).map_err(|e| format!("parse def: {e}"))?;
    match v.get("kwp_addr") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n
            .as_u64()
            .filter(|n| *n <= 0xFF)
            .map(|n| Some(n as u8))
            .ok_or_else(|| format!("def kwp_addr {n} is not 0..=255")),
        Some(Value::String(s)) => parse_u8("def kwp_addr", s).map(Some),
        Some(other) => Err(format!("def kwp_addr {other} is not a number")),
    }
}

/// The adapter binding for a KWP2000 module at `addr`: a TP2.0 channel on a
/// raw-CAN driver, or the K-line address on a K-line driver. Other drivers are
/// refused with the reason.
pub fn kwp_key(
    driver: &str,
    port: Option<&str>,
    baud: Option<&str>,
    bus: Option<&str>,
    addr: u8,
) -> Result<ConnKey, String> {
    if kwp::TP20_DRIVERS.contains(&driver) {
        let a = Addr {
            tx_id: addr as u32,
            rx_id: opentune_core::protocol::tp20channel::DEFAULT_TESTER_RX_ID,
        };
        return Ok(ConnKey::new(driver, port, baud, bus, a)
            .with_option("transport", "tp20")
            .with_option("tp20_addr", &format!("0x{addr:02X}")));
    }
    if kwp::KLINE_DRIVERS.contains(&driver) {
        let a = Addr {
            tx_id: addr as u32,
            rx_id: 0,
        };
        return Ok(ConnKey::new(driver, port, baud, bus, a));
    }
    Err(format!(
        "driver '{driver}' cannot carry VAG KWP2000: use a raw-CAN adapter for TP2.0 ({}) or \
         a K-line adapter ({}), or the simulated module",
        kwp::TP20_DRIVERS.join(", "),
        kwp::KLINE_DRIVERS.join(", ")
    ))
}

/// Where a KWP operation runs.
enum KwpTarget {
    /// The engine's synthetic module behind the core TP2.0 simulator.
    Sim,
    /// A real adapter binding (and the module address, for the UI).
    Real(ConnKey, u8),
}

impl KwpTarget {
    fn json(&self) -> Value {
        match self {
            KwpTarget::Sim => json!({ "sim": true }),
            KwpTarget::Real(k, a) => json!({
                "sim": false,
                "addr": format!("0x{a:02X}"),
                "transport": if kwp::TP20_DRIVERS.contains(&k.driver.as_str()) { "tp20" } else { "kline" },
            }),
        }
    }
}

/// The connection a KWP command names: `sim`, or the driver at the UI's
/// `kwp_addr`, else the definition's. No address is ever assumed.
#[allow(clippy::too_many_arguments)]
fn kwp_target(
    driver: &str,
    port: Option<&str>,
    baud: Option<&str>,
    bus: Option<&str>,
    sim: bool,
    kwp_addr: Option<&str>,
    def_text: &str,
) -> Result<KwpTarget, String> {
    if sim {
        return Ok(KwpTarget::Sim);
    }
    let addr = match kwp_addr.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => parse_u8("module address", s)?,
        None => def_kwp_addr(def_text)?.ok_or(
            "no KWP module address: set the module address (TP2.0 logical / K-line address, \
             e.g. 0x01) or add \"kwp_addr\" to the definition",
        )?,
    };
    Ok(KwpTarget::Real(
        kwp_key(driver, port, baud, bus, addr)?,
        addr,
    ))
}

/// Resolve a seed/key id: the simulator's test-only handler (sim only), else
/// the plugin resolvers. A blank id is no handler.
fn resolve_seed_key(
    keys: &SeedKeys,
    id: Option<&str>,
    sim: bool,
) -> Result<Option<Box<dyn SeedKey>>, String> {
    let Some(id) = id.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if sim {
        if let Some(k) = opentune_adaptation::sim::test_seed_key(id) {
            return Ok(Some(k));
        }
    }
    keys.require(id).map(Some)
}

/// The KWP login options from the UI (session default 0x89, `none` skips;
/// level default 0x03).
fn kwp_session(session: Option<&str>) -> Result<Option<u8>, String> {
    match session.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(Some(kwp::VAG_SESSION)),
        Some(s) if s.eq_ignore_ascii_case("none") => Ok(None),
        Some(s) => parse_u8("session", s).map(Some),
    }
}

fn kwp_level(level: Option<&str>) -> Result<u8, String> {
    match level.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(kwp::VAG_SECURITY_LEVEL),
        Some(s) => parse_u8("security level", s),
    }
}

/// The login and seed/key parameters a KWP command carries, owned so the
/// operation can move them onto the adapter worker (the handler itself is not
/// `Send`, so it is resolved there).
#[derive(Clone)]
struct KwpAuth {
    session: Option<u8>,
    level: u8,
    seedkey: Option<String>,
    keys: SeedKeys,
    sim: bool,
}

impl KwpAuth {
    fn new(
        session: Option<&str>,
        level: Option<&str>,
        seedkey: Option<String>,
        keys: &SeedKeys,
        sim: bool,
    ) -> Result<Self, String> {
        let a = KwpAuth {
            session: kwp_session(session)?,
            level: kwp_level(level)?,
            seedkey,
            keys: keys.clone(),
            sim,
        };
        // Fail on an unknown id before any adapter is opened.
        a.handler()?;
        Ok(a)
    }

    fn handler(&self) -> Result<Option<Box<dyn SeedKey>>, String> {
        resolve_seed_key(&self.keys, self.seedkey.as_deref(), self.sim)
    }

    fn login<'a>(&self, sk: Option<&'a dyn SeedKey>) -> KwpLogin<'a> {
        KwpLogin {
            session: self.session,
            security_level: self.level,
            seed_key: sk,
        }
    }
}

/// Run `f` on the target's transport. A real binding is closed afterwards so
/// the TP2.0 channel is torn down (`A8`) and the next panel re-opens cleanly.
fn run_kwp<R, F>(
    hub: &AdapterHub,
    target: &KwpTarget,
    timeout_ms: u64,
    op: &str,
    f: F,
) -> Result<R, String>
where
    R: Send + 'static,
    F: FnOnce(&mut dyn RawDiag) -> Result<R, String> + Send + 'static,
{
    match target {
        KwpTarget::Sim => sim_run(timeout_ms, f),
        KwpTarget::Real(key, _) => {
            let key = key.clone();
            hub.run(op, move |s: &mut Session| {
                let r = s.with_diag(&key, timeout_ms, |d| f(d));
                s.invalidate();
                r
            })
        }
    }
}

/// `f` against a fresh synthetic module over the core TP2.0 simulator.
pub(crate) fn sim_run<R>(
    timeout_ms: u64,
    f: impl FnOnce(&mut dyn RawDiag) -> Result<R, String>,
) -> Result<R, String> {
    use opentune_adaptation::sim::{open_sim_channel, sim_bus, SimKwpModule};
    use std::sync::{Arc, Mutex};
    let unit = Arc::new(Mutex::new(SimKwpModule::default()));
    let ecu = sim_bus(unit.clone(), Default::default());
    let mut ch = open_sim_channel(ecu, unit, 0).map_err(|e| format!("simulated module: {e}"))?;
    let r = f(&mut opentune_core::CoreDiag::new(&mut ch, timeout_ms));
    ch.close();
    r
}

/// A transport for dry runs: refuses every request (nothing is sent).
fn no_io(_: &[u8]) -> Result<Vec<u8>, String> {
    Err("dry run: nothing is sent".into())
}

/// Read a `kwp_group` measuring block or a `kwp_adaptation` channel over
/// KWP2000. Read-only; a seed/key id is optional (some modules refuse the
/// adaptation channel select without SecurityAccess).
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn adaptation_kwp_read(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    sim: Option<bool>,
    kwp_addr: Option<String>,
    def_path: String,
    channel: String,
    timeout_ms: u64,
    session: Option<String>,
    level: Option<String>,
    seedkey: Option<String>,
    hub: tauri::State<'_, AdapterHub>,
    seedkeys: tauri::State<'_, SeedKeys>,
) -> Result<Value, String> {
    let sim = sim.unwrap_or(false);
    let (def, text) = load_def_text(&def_path)?;
    let target = kwp_target(
        &driver,
        port.as_deref(),
        baud.as_deref(),
        bus.as_deref(),
        sim,
        kwp_addr.as_deref(),
        &text,
    )?;
    let auth = KwpAuth::new(
        session.as_deref(),
        level.as_deref(),
        seedkey,
        &seedkeys,
        sim,
    )?;
    let tj = target.json();
    let mut v = run_kwp(&hub, &target, timeout_ms, "KWP adaptation read", move |d| {
        kwp_read_on(d, &def, &channel, &auth)
    })?;
    v["target"] = tj;
    Ok(v)
}

/// The read itself (shared with the tests).
fn kwp_read_on(
    d: &mut dyn RawDiag,
    def: &AdaptationDef,
    channel: &str,
    auth: &KwpAuth,
) -> Result<Value, String> {
    use opentune_adaptation::ChannelKind as K;
    let sk = auth.handler()?;
    let login = auth.login(sk.as_deref());
    let kind = def
        .channel(channel)
        .map(|c| c.kind.clone())
        .ok_or_else(|| format!("unknown channel `{channel}`"))?;
    let r = match kind {
        K::KwpGroup { .. } => kwp::read_group(d, def, channel, &login)
            .map(|r| json!({ "kind": "kwp_group", "reading": r })),
        K::KwpAdaptation { .. } => kwp::read_adaptation(d, def, channel, &login)
            .map(|r| json!({ "kind": "kwp_adaptation", "reading": r })),
        _ => {
            return Err(format!(
                "`{channel}` is not a kwp_group / kwp_adaptation channel"
            ))
        }
    };
    r.map_err(|e| e.to_string())
}

/// Run basic settings on a `kwp_basic_settings` channel: start, read the
/// measuring block in basic-settings mode `polls` times, stop. ACK-GATED
/// (unless `dry_run`, which returns the plan and opens nothing) and needs a
/// seed/key handler for SecurityAccess.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn adaptation_kwp_basic(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    sim: Option<bool>,
    kwp_addr: Option<String>,
    def_path: String,
    channel: String,
    timeout_ms: u64,
    session: Option<String>,
    level: Option<String>,
    seedkey: Option<String>,
    polls: Option<u32>,
    interval_ms: Option<u64>,
    ack: Option<bool>,
    dry_run: Option<bool>,
    hub: tauri::State<'_, AdapterHub>,
    seedkeys: tauri::State<'_, SeedKeys>,
) -> Result<Value, String> {
    let sim = sim.unwrap_or(false);
    let dry = dry_run.unwrap_or(false);
    if !dry && ack != Some(true) {
        return Err(
            "basic settings refused: confirm you understand it runs a procedure on the module (ack required)"
                .into(),
        );
    }
    let (def, text) = load_def_text(&def_path)?;
    let auth = KwpAuth::new(
        session.as_deref(),
        level.as_deref(),
        seedkey,
        &seedkeys,
        sim,
    )?;
    let opts = KwpWriteOptions {
        dry_run: dry,
        confirmed: ack == Some(true),
        workshop_code: None,
    };
    let polls = polls.unwrap_or(3).min(100);
    let interval_ms = interval_ms.unwrap_or(500).min(10_000);
    let run = move |d: &mut dyn RawDiag| {
        let sk = auth.handler()?;
        let login = auth.login(sk.as_deref());
        kwp::run_basic_settings(d, &def, &channel, polls, interval_ms, &login, &opts)
            .map_err(|e| e.to_string())
            .and_then(|r| serde_json::to_value(r).map_err(|e| e.to_string()))
    };
    if dry {
        let mut v = run(&mut no_io)?;
        v["target"] = json!({ "dry_run": true });
        return Ok(v);
    }
    let target = kwp_target(
        &driver,
        port.as_deref(),
        baud.as_deref(),
        bus.as_deref(),
        sim,
        kwp_addr.as_deref(),
        &text,
    )?;
    let tj = target.json();
    let mut v = run_kwp(&hub, &target, timeout_ms, "KWP basic settings", run)?;
    v["target"] = tj;
    Ok(v)
}

/// Test (temporary) or save a raw value on a `kwp_adaptation` channel.
/// ACK-GATED (unless `dry_run`), needs a seed/key handler for SecurityAccess,
/// and a save needs the module's 6-byte workshop code (`wsc`, 12 hex digits).
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn adaptation_kwp_adapt(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    sim: Option<bool>,
    kwp_addr: Option<String>,
    def_path: String,
    channel: String,
    timeout_ms: u64,
    session: Option<String>,
    level: Option<String>,
    seedkey: Option<String>,
    mode: String,
    value: String,
    wsc: Option<String>,
    ack: Option<bool>,
    dry_run: Option<bool>,
    hub: tauri::State<'_, AdapterHub>,
    seedkeys: tauri::State<'_, SeedKeys>,
) -> Result<Value, String> {
    let sim = sim.unwrap_or(false);
    let dry = dry_run.unwrap_or(false);
    let mode = match mode.trim().to_ascii_lowercase().as_str() {
        "test" => AdaptMode::Test,
        "save" => AdaptMode::Save,
        other => return Err(format!("unknown adaptation mode '{other}' (test or save)")),
    };
    if !dry && ack != Some(true) {
        return Err(
            "adaptation test/save refused: confirm you understand it changes the module (ack required)"
                .into(),
        );
    }
    let raw = parse_num("value", &value, 0xFFFF)? as u16;
    let wsc = match wsc.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => None,
        Some(s) => {
            let b = parse_hex(s)?;
            Some(
                <[u8; kwp::WORKSHOP_CODE_LEN]>::try_from(b.as_slice()).map_err(|_| {
                    format!("workshop code '{s}': expected 6 bytes (12 hex digits)")
                })?,
            )
        }
    };
    let (def, text) = load_def_text(&def_path)?;
    let auth = KwpAuth::new(
        session.as_deref(),
        level.as_deref(),
        seedkey,
        &seedkeys,
        sim,
    )?;
    let opts = KwpWriteOptions {
        dry_run: dry,
        confirmed: ack == Some(true),
        workshop_code: wsc,
    };
    let run = move |d: &mut dyn RawDiag| {
        let sk = auth.handler()?;
        let login = auth.login(sk.as_deref());
        kwp::write_adaptation(d, &def, &channel, raw, mode, &login, &opts)
            .map_err(|e| e.to_string())
            .and_then(|r| serde_json::to_value(r).map_err(|e| e.to_string()))
    };
    if dry {
        let mut v = run(&mut no_io)?;
        v["target"] = json!({ "dry_run": true });
        return Ok(v);
    }
    let target = kwp_target(
        &driver,
        port.as_deref(),
        baud.as_deref(),
        bus.as_deref(),
        sim,
        kwp_addr.as_deref(),
        &text,
    )?;
    let tj = target.json();
    let mut v = run_kwp(&hub, &target, timeout_ms, "KWP adaptation", run)?;
    v["target"] = tj;
    Ok(v)
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

    fn kwp_def() -> AdaptationDef {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates/opentune-adaptation/tests/fixtures/kwp-sim.json");
        AdaptationDef::from_json(&std::fs::read_to_string(p).unwrap()).unwrap()
    }

    #[test]
    fn kwp_keys_target_tp20_or_kline_and_refuse_the_rest() {
        let k = kwp_key("slcan", Some("/dev/ttyACM0"), None, None, 0x01).unwrap();
        let o =
            opentune_core::protocol::tp20channel::Tp20Options::from_config(&k.config().unwrap())
                .unwrap()
                .unwrap();
        assert_eq!((o.addr, o.rx_id), (0x01, 0x300));
        let k = kwp_key("kkl", Some("/dev/ttyUSB0"), None, None, 0x01).unwrap();
        assert_eq!(k.config().unwrap().tx_id, 0x01);
        assert!(kwp_key("elm327obd", None, None, None, 1)
            .unwrap_err()
            .contains("cannot carry VAG KWP2000"));
        // No address is assumed for real hardware.
        let text = r#"{"id":"x","name":"x","channels":[]}"#;
        assert!(kwp_target("slcan", None, None, None, false, None, text).is_err());
        let with = r#"{"id":"x","name":"x","kwp_addr":"0x02","channels":[]}"#;
        assert!(matches!(
            kwp_target("slcan", None, None, None, false, None, with).unwrap(),
            KwpTarget::Real(_, 0x02)
        ));
        assert!(matches!(
            kwp_target("elm327obd", None, None, None, true, None, text).unwrap(),
            KwpTarget::Sim
        ));
    }

    #[test]
    fn kwp_seed_keys_resolve_sim_only_test_key_and_plugins() {
        let keys = SeedKeys::default();
        assert!(resolve_seed_key(&keys, Some("kwpsim-test"), true)
            .unwrap()
            .is_some());
        // The simulator's test key is not offered for real hardware.
        assert!(resolve_seed_key(&keys, Some("kwpsim-test"), false).is_err());
        assert!(resolve_seed_key(&keys, Some(" "), false).unwrap().is_none());
        let keys = SeedKeys::new(vec![crate::seedkeys::testkeys::first]);
        assert_eq!(
            resolve_seed_key(&keys, Some("shared"), false)
                .unwrap()
                .unwrap()
                .id(),
            "first"
        );
    }

    #[test]
    fn kwp_flows_on_the_simulated_module() {
        let def = kwp_def();
        let auth = KwpAuth::new(None, None, None, &SeedKeys::default(), true).unwrap();
        let v = sim_run(500, |d| kwp_read_on(d, &def, "Adaptation 1", &auth)).unwrap();
        assert_eq!(v["reading"]["raw"], 0x0200);
        let v = sim_run(500, |d| kwp_read_on(d, &def, "Group 1", &auth)).unwrap();
        assert_eq!(v["reading"]["cells"][0]["value"], 90.0);
        // A save with the simulator's test key, confirmed.
        let auth = KwpAuth::new(
            None,
            None,
            Some("kwpsim-test".into()),
            &SeedKeys::default(),
            true,
        )
        .unwrap();
        let opts = KwpWriteOptions {
            dry_run: false,
            confirmed: true,
            workshop_code: Some([0; 6]),
        };
        let r = sim_run(500, |d| {
            let sk = auth.handler()?;
            kwp::write_adaptation(
                d,
                &def,
                "Adaptation 1",
                0x0300,
                AdaptMode::Save,
                &auth.login(sk.as_deref()),
                &opts,
            )
            .map_err(|e| e.to_string())
        })
        .unwrap();
        assert!(r.is_ok(), "{r:?}");
        // Unknown ids fail before anything is opened.
        assert!(KwpAuth::new(None, None, Some("vag".into()), &SeedKeys::default(), true).is_err());
    }

    #[test]
    fn routine_ops_classified() {
        assert!(op_mutates(parse_op("start").unwrap()));
        assert!(op_mutates(parse_op("Stop").unwrap()));
        assert!(!op_mutates(parse_op("results").unwrap()));
        assert!(parse_op("erase").is_err());
    }
}
