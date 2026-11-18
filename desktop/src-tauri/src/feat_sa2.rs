//! SA2 script panel: load, disassemble and run a VAG SA2 seed/key script
//! offline, and (Bench tier) arm it as the `sa2` seed/key handler for live
//! sessions.
//!
//! OpenTune ships **no** SA2 script. Every script here is the user's own:
//! pasted as hex or assembler text, loaded from a file, or taken from the
//! user's own flash container (FRF / ZIP / ODX / SGO, with the user's own FRF
//! key file when the container is an encrypted FRF). The interpreter is
//! `opentune-sa2`; container extraction is `opentune-container`.
//!
//! * [`sa2_load`] — read a script source and list the SA2 candidates in it.
//! * [`sa2_inspect`] — disassembly rows + lints, and an optional seed → key
//!   preview with a bounded step trace. Offline; nothing is sent anywhere.
//! * [`sa2_arm`] / [`sa2_status`] — arm (or disarm) a script as seed/key id
//!   `sa2` in the shared [`SeedKeys`], so a service script naming
//!   `"seedkey": "sa2"` (or a plugin command resolving `sa2`) unlocks with it.
//!   Applying a script to a live module is Bench tier: refused on the stable
//!   channel, and it needs an explicit acknowledgement. Held in memory for
//!   this run only.

use crate::common::hexs;
use crate::seedkeys::{ArmedSa2, SeedKeys, SA2_ID};
use crate::state::BuildChannel;
use opentune_sa2::{assemble_text, parse_hex, vm, Op, Program, Step};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::Path;

/// Longest script the panel accepts. Real SA2 scripts are tens of bytes.
pub(crate) const MAX_SCRIPT: usize = 4096;
/// Largest script file read (raw bytes, hex text or assembler source).
const MAX_SCRIPT_FILE: u64 = 1 << 20;
/// Largest container read.
const MAX_CONTAINER: u64 = 512 << 20;
/// Largest FRF key file read.
const MAX_KEY_FILE: u64 = 1 << 20;
/// Trace rows returned by default, and the most a caller may ask for.
const DEFAULT_TRACE_ROWS: usize = 500;
const MAX_TRACE_ROWS: usize = 5000;
/// Highest step budget a caller may set.
const MAX_STEP_BUDGET: u64 = 10 * vm::MAX_STEPS;
/// Bytes of a candidate shown when it is too long to be a script.
const PREVIEW_BYTES: usize = 64;

// ---------------------------------------------------------------------------
// Script text / file decoding
// ---------------------------------------------------------------------------

const MNEMONICS: [&str; 11] = [
    "rsl", "rsr", "add", "sub", "eor", "loop", "next", "bcc", "bra", "finish", ".byte",
];

/// Whether `text` has an SA2 mnemonic as a word. Hex alone cannot tell
/// `add 5` (assembler) from the bytes `AD D5`, so a mnemonic marks assembler.
fn has_mnemonic(text: &str) -> bool {
    text.split(|c: char| c.is_whitespace() || c == ',')
        .any(|w| MNEMONICS.contains(&w.to_ascii_lowercase().as_str()))
}

/// Decode script text: hex, or assembler source (any mnemonic word, or text
/// that is not hex). Returns the bytes and which form it was.
fn script_from_text(text: &str) -> Result<(Vec<u8>, &'static str), String> {
    if text.trim().is_empty() {
        return Err("enter a script: hex bytes (e.g. 87 5A 5A 5A 5A 4C) or SA2 assembler".into());
    }
    let asm = has_mnemonic(text);
    if !asm {
        if let Ok(b) = parse_hex(text) {
            return Ok((b, "hex"));
        }
    }
    assemble_text(text).map(|b| (b, "assembler")).map_err(|e| {
        if asm {
            format!("SA2 assembler: {e}")
        } else {
            format!("not hex or SA2 assembler: {e}")
        }
    })
}

/// Decode a script file: raw bytes when it is not text, else hex or assembler.
fn script_from_file_bytes(raw: Vec<u8>) -> Result<(Vec<u8>, &'static str), String> {
    let text = std::str::from_utf8(&raw)
        .ok()
        .filter(|t| t.chars().all(|c| !c.is_control() || c.is_whitespace()));
    match text {
        None => Ok((raw, "raw bytes")),
        Some(t) => script_from_text(t),
    }
}

/// Read at most `cap` bytes of `path`, refusing larger files.
fn read_capped(path: &str, cap: u64, what: &str) -> Result<Vec<u8>, String> {
    let p = Path::new(path.trim());
    let len = std::fs::metadata(p)
        .map_err(|e| format!("{what} {}: {e}", p.display()))?
        .len();
    if len > cap {
        return Err(format!(
            "{what} {} is {len} bytes — larger than the {cap}-byte limit",
            p.display()
        ));
    }
    std::fs::read(p).map_err(|e| format!("{what} {}: {e}", p.display()))
}

fn check_len(bytes: &[u8]) -> Result<(), String> {
    if bytes.is_empty() {
        return Err("the script is empty".into());
    }
    if bytes.len() > MAX_SCRIPT {
        return Err(format!(
            "{} bytes is longer than any SA2 script ({MAX_SCRIPT}-byte limit) — is this the right field?",
            bytes.len()
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// sa2_load: sources → candidates
// ---------------------------------------------------------------------------

/// One SA2 candidate: the bytes plus whether they decode as a script.
fn candidate(index: usize, field: &str, session: Option<&str>, bytes: &[u8]) -> Value {
    let long = bytes.len() > MAX_SCRIPT;
    let shown = if long { &bytes[..PREVIEW_BYTES] } else { bytes };
    let (decodes, error, lints) = if long {
        (
            false,
            Some(format!(
                "{} bytes — too long to be an SA2 script",
                bytes.len()
            )),
            0,
        )
    } else {
        match Program::parse(bytes) {
            Ok(p) => (true, None, p.lints().len()),
            Err(e) => (false, Some(e.to_string()), 0),
        }
    };
    json!({
        "index": index,
        "field": field,
        "session": session,
        "len": bytes.len(),
        "hex": hexs(shown),
        "truncated": long,
        "decodes": decodes,
        "error": error,
        "lints": lints,
    })
}

/// Read a script source and list the SA2 scripts in it.
///
/// Exactly one source: `text` (hex or assembler), `path` (a script file —
/// raw bytes, hex or assembler — or, with `container: true`, the user's own
/// flash container). `frf_key` is the path of the user's FRF key file, needed
/// only for an encrypted FRF. Offline and read-only.
#[tauri::command]
pub async fn sa2_load(
    text: Option<String>,
    path: Option<String>,
    container: Option<bool>,
    frf_key: Option<String>,
) -> Result<Value, String> {
    load(text, path, container.unwrap_or(false), frf_key)
}

fn load(
    text: Option<String>,
    path: Option<String>,
    container: bool,
    frf_key: Option<String>,
) -> Result<Value, String> {
    let path = path.filter(|p| !p.trim().is_empty());
    let text = text.filter(|t| !t.trim().is_empty());
    match (text, path) {
        (Some(t), None) => {
            let (bytes, form) = script_from_text(&t)?;
            check_len(&bytes)?;
            Ok(json!({
                "source": "text",
                "origin": format!("typed in ({form})"),
                "candidates": [candidate(0, form, None, &bytes)],
            }))
        }
        (None, Some(p)) if !container => {
            let raw = read_capped(&p, MAX_SCRIPT_FILE, "script file")?;
            let (bytes, form) = script_from_file_bytes(raw)?;
            check_len(&bytes)?;
            Ok(json!({
                "source": "file",
                "origin": format!("{} ({form})", file_name(&p)),
                "candidates": [candidate(0, form, None, &bytes)],
            }))
        }
        (None, Some(p)) => from_container(&p, frf_key.as_deref()),
        (None, None) => Err("paste a script, choose a script file or open a container".into()),
        (Some(_), Some(_)) => Err("give one source: script text or a file, not both".into()),
    }
}

fn file_name(p: &str) -> String {
    Path::new(p.trim())
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.trim().to_string())
}

fn from_container(path: &str, frf_key: Option<&str>) -> Result<Value, String> {
    let bytes = read_capped(path, MAX_CONTAINER, "container")?;
    let mut keys = opentune_container::Keys::default();
    if let Some(k) = frf_key.map(str::trim).filter(|k| !k.is_empty()) {
        keys.frf_key = Some(read_capped(k, MAX_KEY_FILE, "FRF key file")?);
    }
    let needs_key = matches!(
        opentune_container::detect(&bytes),
        opentune_container::InputFormat::Frf
    );
    let unpacked = opentune_container::unpack(&bytes, &keys).map_err(|e| {
        if needs_key && keys.frf_key.is_none() {
            format!(
                "{}: not a plain ZIP / ODX / SGO, so it is read as an encrypted FRF — choose your FRF key file ({e})",
                file_name(path)
            )
        } else {
            format!("decode {}: {e}", file_name(path))
        }
    })?;
    let candidates: Vec<Value> = unpacked
        .sa2_scripts
        .iter()
        .enumerate()
        .map(|(i, s)| candidate(i, &s.field, s.session.as_deref(), &s.bytes))
        .collect();
    let note = candidates.is_empty().then_some(
        "No SA2 script found: looked for an ODX SECURITY block whose SECURITY-METHOD is SA2, and an SGO header's SA2 field.",
    );
    Ok(json!({
        "source": "container",
        "origin": file_name(path),
        "container_kind": format!("{:?}", unpacked.kind),
        "needs_frf_key": needs_key,
        "candidates": candidates,
        "note": note,
    }))
}

// ---------------------------------------------------------------------------
// sa2_inspect: disassembly + seed → key preview
// ---------------------------------------------------------------------------

/// Linear disassembly rows, stopping (with the error) at the first byte that
/// does not decode. Branch targets on instruction boundaries get labels.
fn disasm_rows(bytes: &[u8]) -> (Vec<Value>, Option<String>) {
    let mut instrs: Vec<(usize, Op)> = Vec::new();
    let mut error = None;
    let mut at = 0;
    while at < bytes.len() {
        match Op::decode(bytes, at) {
            Ok(op) => {
                instrs.push((at, op));
                at += op.len();
            }
            Err(e) => {
                error = Some(e.to_string());
                break;
            }
        }
    }
    let mut bounds: BTreeSet<usize> = instrs.iter().map(|(o, _)| *o).collect();
    if error.is_none() {
        bounds.insert(bytes.len());
    }
    let labels: BTreeSet<usize> = instrs
        .iter()
        .filter_map(|(o, op)| op.branch_target(*o))
        .filter(|t| bounds.contains(t))
        .collect();
    let mut depth = 0usize;
    let mut out = Vec::with_capacity(instrs.len() + 1);
    for (o, op) in &instrs {
        if *op == Op::Next {
            depth = depth.saturating_sub(1);
        }
        let target = op.branch_target(*o);
        let text = match target.filter(|t| labels.contains(t)) {
            Some(t) => format!("{} L{t:04X}", op.mnemonic()),
            None => op.to_string(),
        };
        out.push(json!({
            "offset": o,
            "bytes": hexs(&bytes[*o..*o + op.len()]),
            "text": text,
            "depth": depth,
            "label": labels.contains(o).then(|| format!("L{o:04X}")),
            "target": target,
        }));
        if matches!(op, Op::Loop(_)) {
            depth += 1;
        }
    }
    if labels.contains(&bytes.len()) {
        out.push(json!({
            "offset": bytes.len(), "bytes": "", "text": "(end)", "depth": 0,
            "label": format!("L{:04X}", bytes.len()), "target": null,
        }));
    }
    (out, error)
}

/// Run the script over `seed` (4 bytes, hex) with a bounded trace.
fn preview(
    bytes: &[u8],
    seed: &str,
    trace: bool,
    trace_limit: usize,
    max_steps: u64,
) -> Result<Value, String> {
    let s = parse_hex(seed).map_err(|e| format!("seed: {e}"))?;
    let s: [u8; 4] = s.as_slice().try_into().map_err(|_| {
        format!(
            "the seed must be exactly 4 bytes (the ECU's 67 11 <seed> reply), got {}",
            s.len()
        )
    })?;
    let seed = u32::from_be_bytes(s);
    let mut rows: Vec<Value> = Vec::new();
    let mut seen = 0u64;
    let mut record = |st: &Step| {
        seen += 1;
        if rows.len() < trace_limit {
            rows.push(json!({
                "offset": st.offset,
                "op": st.op.to_string(),
                "register": format!("{:08X}", st.register),
                "carry": st.carry,
                "loops": st.loop_depth,
            }));
        }
    };
    let cb: Option<&mut dyn FnMut(&Step)> = if trace { Some(&mut record) } else { None };
    let (key, steps, error) = match vm::run_with(bytes, seed, max_steps, cb) {
        Ok(o) => (Some(o.key), o.steps, None),
        Err(e) => (None, seen, Some(e.to_string())),
    };
    Ok(json!({
        "seed": format!("{seed:08X}"),
        "key": key.map(|k| format!("{k:08X}")),
        "key_bytes": key.map(|k| hexs(&k.to_be_bytes())),
        "steps": steps,
        "error": error,
        "trace": trace.then_some(&rows),
        "trace_truncated": trace && seen > rows.len() as u64,
        "trace_limit": trace_limit,
    }))
}

/// Disassemble a script (hex or assembler text) and, with a `seed`, compute
/// the key it gives. `trace` adds the executed instructions (at most
/// `trace_limit` rows, default 500, max 5000); `max_steps` caps the run
/// (default the VM's budget). Offline: nothing is sent to a vehicle.
#[tauri::command]
pub async fn sa2_inspect(
    script: String,
    seed: Option<String>,
    trace: Option<bool>,
    trace_limit: Option<usize>,
    max_steps: Option<u64>,
) -> Result<Value, String> {
    inspect(
        &script,
        seed.as_deref(),
        trace.unwrap_or(false),
        trace_limit,
        max_steps,
    )
}

fn inspect(
    script: &str,
    seed: Option<&str>,
    trace: bool,
    trace_limit: Option<usize>,
    max_steps: Option<u64>,
) -> Result<Value, String> {
    let (bytes, form) = script_from_text(script)?;
    check_len(&bytes)?;
    let (rows, error) = disasm_rows(&bytes);
    let program = Program::parse(&bytes).ok();
    let lints: Vec<String> = program
        .as_ref()
        .map(|p| p.lints().iter().map(ToString::to_string).collect())
        .unwrap_or_default();
    let run = match seed.map(str::trim).filter(|s| !s.is_empty()) {
        None => Value::Null,
        Some(s) => {
            let limit = trace_limit
                .unwrap_or(DEFAULT_TRACE_ROWS)
                .clamp(1, MAX_TRACE_ROWS);
            let budget = max_steps.unwrap_or(vm::MAX_STEPS).clamp(1, MAX_STEP_BUDGET);
            match preview(&bytes, s, trace, limit, budget) {
                Ok(v) => v,
                Err(e) => json!({ "error": e }),
            }
        }
    };
    Ok(json!({
        "len": bytes.len(),
        "hex": hexs(&bytes),
        "input": form,
        "decodes": program.is_some(),
        "error": error,
        "rows": rows,
        "lints": lints,
        "listing": program.as_ref().map(Program::listing),
        "source": program.as_ref().map(Program::to_source),
        "run": run,
    }))
}

// ---------------------------------------------------------------------------
// sa2_arm / sa2_status: the `sa2` seed/key handler for live sessions
// ---------------------------------------------------------------------------

fn status(keys: &SeedKeys, channel: &BuildChannel) -> Value {
    let armed = keys.armed_sa2();
    json!({
        "armed": armed.is_some(),
        "id": SA2_ID,
        "len": armed.as_ref().map(|a| a.script.len()),
        "hex": armed.as_ref().map(|a| hexs(&a.script)),
        "label": armed.as_ref().map(|a| a.label.clone()),
        "live_allowed": channel.debug_enabled(),
        "shadowed_by_plugin": keys.plugin(SA2_ID).is_some(),
    })
}

/// Whether a script can be armed (or is armed), and which.
#[tauri::command]
pub async fn sa2_status(
    seedkeys: tauri::State<'_, SeedKeys>,
    channel: tauri::State<'_, BuildChannel>,
) -> Result<Value, String> {
    Ok(status(&seedkeys, &channel))
}

/// Arm `script` (hex or assembler text) as seed/key id `sa2` for this run, or
/// disarm with no script. Bench tier: refused on the stable channel, and
/// arming needs `ack` (an armed script is sent to modules as SecurityAccess
/// keys, and wrong keys count against the module's attempt limit).
#[tauri::command]
pub async fn sa2_arm(
    script: Option<String>,
    label: Option<String>,
    ack: bool,
    seedkeys: tauri::State<'_, SeedKeys>,
    channel: tauri::State<'_, BuildChannel>,
) -> Result<Value, String> {
    arm(script.as_deref(), label, ack, &seedkeys, &channel)
}

/// Fixed probe seeds for the arm-time check (arbitrary values).
const PROBE_SEEDS: [u32; 3] = [0x0000_0000, 0xFFFF_FFFF, 0x1234_5678];

fn arm(
    script: Option<&str>,
    label: Option<String>,
    ack: bool,
    keys: &SeedKeys,
    channel: &BuildChannel,
) -> Result<Value, String> {
    let Some(script) = script.filter(|s| !s.trim().is_empty()) else {
        keys.arm_sa2(None);
        return Ok(status(keys, channel));
    };
    if !channel.debug_enabled() {
        return Err(
            "using an SA2 script on a live module is a Bench-tier feature (alpha / bench builds) until it is vehicle-tested; the offline preview works in every build"
                .into(),
        );
    }
    if !ack {
        return Err("tick the confirmation box: the armed script answers SecurityAccess on real modules, and wrong keys count against their attempt limit".into());
    }
    let (bytes, _) = script_from_text(script)?;
    check_len(&bytes)?;
    let program = Program::parse(&bytes).map_err(|e| format!("refusing to arm: {e}"))?;
    for seed in PROBE_SEEDS {
        vm::run(&bytes, seed)
            .map_err(|e| format!("refusing to arm: seed {seed:08X} fails: {e}"))?;
    }
    keys.arm_sa2(Some(ArmedSa2 {
        script: bytes,
        label: label.map(|l| l.trim().to_string()).unwrap_or_default(),
    }));
    let mut v = status(keys, channel);
    v["lints"] = json!(program
        .lints()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>());
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testutil::TmpDir;

    // Synthetic scripts written for these tests (never an OEM script).
    // eor 0xFFFFFFFF; finish — key = NOT seed.
    const NOT_HEX: &str = "87 FF FF FF FF 4C";
    // loop 3 { rsl; bcc +5; eor 0x5A5A5A5A }; add 0x100; finish
    // (the opentune-container synthetic ODX fixture's script).
    const LOOPY: &str = "68 03 81 4A 05 87 5A 5A 5A 5A 49 93 00 00 01 00 4C";

    fn bench() -> BuildChannel {
        BuildChannel::new(BuildChannel::BENCH)
    }

    #[test]
    fn text_is_hex_or_assembler() {
        assert_eq!(script_from_text("0x87,0xFF ff:ff FF 4c").unwrap().1, "hex");
        let (b, f) = script_from_text("87 FF FF FF FF 4C").unwrap();
        assert_eq!((b.len(), f), (6, "hex"));
        // "add 5" would be the hex bytes AD D5; it is read as assembler.
        let (b, f) = script_from_text("add 5\nfinish").unwrap();
        assert_eq!((b, f), (vec![0x93, 0, 0, 0, 5, 0x4C], "assembler"));
        assert!(script_from_text("  ").is_err());
        assert!(script_from_text("zz").unwrap_err().contains("not hex"));
        assert!(script_from_text("frobnicate 1 ; x")
            .unwrap_err()
            .contains("assembler"));
        assert!(script_from_text("eor 1 2")
            .unwrap_err()
            .starts_with("SA2 assembler"));
    }

    #[test]
    fn inspect_lists_lints_and_previews_a_key() {
        let v = inspect(LOOPY, Some("00000001"), true, None, None).unwrap();
        assert_eq!(v["len"], 17);
        assert_eq!(v["decodes"], true);
        let rows = v["rows"].as_array().unwrap();
        assert_eq!(rows[0]["text"], "loop 3");
        assert_eq!(rows[1]["depth"], 1, "loop body is indented");
        assert_eq!(rows[2]["text"], "bcc L000A");
        assert_eq!(rows[4]["label"], "L000A");
        assert!(v["lints"].as_array().unwrap().is_empty());
        assert!(v["listing"].as_str().unwrap().contains("loop 3"));
        let run = &v["run"];
        assert_eq!(run["seed"], "00000001");
        let key = run["key"].as_str().unwrap();
        let want = vm::run(&parse_hex(LOOPY).unwrap(), 1).unwrap();
        assert_eq!(key, format!("{want:08X}"));
        assert_eq!(run["key_bytes"], hexs(&want.to_be_bytes()));
        let trace = run["trace"].as_array().unwrap();
        assert_eq!(trace.len() as u64, run["steps"].as_u64().unwrap());
        assert_eq!(trace.last().unwrap()["op"], "finish");
        assert_eq!(run["trace_truncated"], false);

        // Without a seed there is no run; without trace there are no rows.
        assert!(inspect(NOT_HEX, None, false, None, None).unwrap()["run"].is_null());
        let v = inspect(NOT_HEX, Some("00 00 00 01"), false, None, None).unwrap();
        assert_eq!(v["run"]["key"], "FFFFFFFE");
        assert!(v["run"]["trace"].is_null());
    }

    #[test]
    fn trace_and_steps_are_bounded() {
        // loop 255 { loop 255 { rsl; next }; next }; finish — 130k+ steps.
        let src = "loop 255\nloop 255\nrsl\nnext\nnext\nfinish";
        let v = inspect(src, Some("12345678"), true, Some(10), None).unwrap();
        assert_eq!(v["run"]["trace"].as_array().unwrap().len(), 10);
        assert_eq!(v["run"]["trace_truncated"], true);
        assert!(v["run"]["error"].is_null());
        // A tiny budget stops the run with the VM's typed error, keeping the
        // trace so far.
        let v = inspect(src, Some("12345678"), true, Some(5000), Some(100)).unwrap();
        assert!(v["run"]["key"].is_null());
        assert!(v["run"]["error"].as_str().unwrap().contains("step budget"));
        assert_eq!(v["run"]["trace"].as_array().unwrap().len(), 100);
        // The trace limit is clamped.
        let v = inspect(src, Some("12345678"), true, Some(1_000_000), None).unwrap();
        assert_eq!(v["run"]["trace"].as_array().unwrap().len(), MAX_TRACE_ROWS);
    }

    #[test]
    fn bad_scripts_and_seeds_are_reported_not_fatal() {
        // Unknown opcode after one good instruction: rows up to it + error.
        let v = inspect("81 FF", None, false, None, None).unwrap();
        assert_eq!(v["decodes"], false);
        assert_eq!(v["rows"].as_array().unwrap().len(), 1);
        assert!(
            v["error"].as_str().unwrap().contains("0xFF"),
            "{}",
            v["error"]
        );
        // A lint (no finish) does not stop anything.
        let v = inspect("81", Some("00000001"), false, None, None).unwrap();
        assert!(v["lints"][0].as_str().unwrap().contains("finish"));
        assert_eq!(v["run"]["key"], "00000002");
        // Seed errors land in run.error.
        let v = inspect(NOT_HEX, Some("0102"), false, None, None).unwrap();
        assert!(v["run"]["error"].as_str().unwrap().contains("4 bytes"));
        assert!(inspect(&"81 ".repeat(MAX_SCRIPT + 1), None, false, None, None).is_err());
    }

    #[test]
    fn load_text_file_and_container() {
        let v = load(Some(NOT_HEX.into()), None, false, None).unwrap();
        assert_eq!(v["source"], "text");
        assert_eq!(v["candidates"][0]["decodes"], true);

        let dir = TmpDir::new("sa2");
        let raw = dir.join("script.bin");
        std::fs::write(&raw, [0x87, 0xFF, 0xFF, 0xFF, 0xFF, 0x4C]).unwrap();
        let v = load(None, Some(raw.display().to_string()), false, None).unwrap();
        assert_eq!(v["candidates"][0]["hex"], "87 FF FF FF FF 4C");
        assert!(v["origin"].as_str().unwrap().contains("raw bytes"));
        let asm = dir.join("script.sa2");
        std::fs::write(&asm, "; synthetic\neor 0xFFFFFFFF\nfinish\n").unwrap();
        let v = load(None, Some(asm.display().to_string()), false, None).unwrap();
        assert_eq!(v["candidates"][0]["hex"], "87 FF FF FF FF 4C");

        // The opentune-container synthetic ODX (a SECURITY block, method SA2).
        let odx = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/opentune-container/tests/fixtures/synthetic-sa2.odx"
        );
        let v = load(None, Some(odx.into()), true, None).unwrap();
        assert_eq!(v["source"], "container");
        assert_eq!(v["container_kind"], "Odx");
        let c = &v["candidates"][0];
        assert_eq!(
            (c["field"].as_str(), c["session"].as_str()),
            (Some("FW-SIGNATURE"), Some("SES_SYNTHETIC"))
        );
        assert_eq!(c["hex"], LOOPY);
        assert_eq!(c["decodes"], true);

        // Anything else is read as an encrypted FRF, which needs the key file.
        let e = load(None, Some(raw.display().to_string()), true, None).unwrap_err();
        assert!(e.contains("FRF key"), "{e}");
        assert!(load(None, None, false, None).is_err());
        assert!(load(Some("4C".into()), Some("x".into()), false, None).is_err());
        assert!(load(
            None,
            Some(dir.join("missing").display().to_string()),
            false,
            None
        )
        .is_err());
    }

    #[test]
    fn candidates_flag_fields_that_are_not_scripts() {
        let c = candidate(1, "FW-CHECKSUM", None, &[0xDE, 0xAD]);
        assert_eq!(c["decodes"], false);
        assert!(c["error"].as_str().is_some());
        let big = vec![0x81u8; MAX_SCRIPT + 1];
        let c = candidate(2, "FW-SIGNATURE", None, &big);
        assert_eq!(
            (c["truncated"].as_bool(), c["decodes"].as_bool()),
            (Some(true), Some(false))
        );
        assert_eq!(c["hex"].as_str().unwrap().split(' ').count(), PREVIEW_BYTES);
    }

    #[test]
    fn arming_is_bench_tier_acknowledged_and_checked() {
        let keys = SeedKeys::default();
        let stable = BuildChannel::new(BuildChannel::STABLE);
        assert_eq!(status(&keys, &stable)["live_allowed"], false);
        let e = arm(Some(NOT_HEX), None, true, &keys, &stable).unwrap_err();
        assert!(e.contains("Bench"), "{e}");
        let e = arm(Some(NOT_HEX), None, false, &keys, &bench()).unwrap_err();
        assert!(e.contains("confirmation"), "{e}");
        // Scripts that do not decode, or fail when run, are refused.
        assert!(arm(Some("81 FF"), None, true, &keys, &bench()).is_err());
        assert!(arm(Some("68 00 49 4C"), None, true, &keys, &bench()).is_err());
        assert!(keys.armed_sa2().is_none());

        let v = arm(
            Some(NOT_HEX),
            Some(" my.odx #0 ".into()),
            true,
            &keys,
            &bench(),
        )
        .unwrap();
        assert_eq!(v["armed"], true);
        assert_eq!(v["label"], "my.odx #0");
        assert_eq!(v["hex"], NOT_HEX);
        assert_eq!(v["shadowed_by_plugin"], false);
        let h = keys.get("sa2").unwrap();
        assert_eq!(
            h.compute(&[0, 0, 0, 1], 0x11).unwrap(),
            [0xFF, 0xFF, 0xFF, 0xFE]
        );
        // Disarming needs neither the channel nor an acknowledgement.
        let v = arm(None, None, false, &keys, &stable).unwrap();
        assert_eq!(v["armed"], false);
        assert!(keys.get("sa2").is_none());
    }
}
