//! The SA2 panel's operations, without UI (port of the webview app's
//! `feat_sa2.rs`): read a script source and list its SA2 candidates,
//! disassemble a script, run it over a seed with a bounded trace, and arm it
//! as seed/key id `sa2` (Bench tier, refused here on other channels).
//!
//! OpenTune ships **no** SA2 script: every script is the user's own (typed,
//! a file, or taken from the user's own flash container with the user's own
//! FRF key file). The interpreter is `opentune-sa2`; container extraction is
//! `opentune-container`; text / file decoding is `opentune_hub::sa2script`.

use crate::host::{bench_channel, BuildChannel};
use opentune_hub::sa2script::{
    check_len, read_capped, script_from_file_bytes, script_from_text, MAX_SCRIPT, MAX_SCRIPT_FILE,
};
use opentune_hub::seedkeys::{ArmedSa2, SeedKeys, SA2_ID};
use opentune_sa2::{parse_hex, vm, Op, Program, Step};
use std::collections::BTreeSet;
use std::path::Path;

/// Largest container read.
const MAX_CONTAINER: u64 = 512 << 20;
/// Largest FRF key file read.
const MAX_KEY_FILE: u64 = 1 << 20;
/// Trace rows kept by default, and the most a caller may ask for.
pub(crate) const DEFAULT_TRACE_ROWS: usize = 500;
pub(crate) const MAX_TRACE_ROWS: usize = 5000;
/// Highest step budget a caller may set.
const MAX_STEP_BUDGET: u64 = 10 * vm::MAX_STEPS;
/// Bytes of a candidate shown when it is too long to be a script.
const PREVIEW_BYTES: usize = 64;
/// Fixed probe seeds for the arm-time check (arbitrary values).
const PROBE_SEEDS: [u32; 3] = [0x0000_0000, 0xFFFF_FFFF, 0x1234_5678];

pub(crate) fn hexs(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

// ---------------------------------------------------------------------------
// Loading: sources -> candidates
// ---------------------------------------------------------------------------

/// Where a script comes from (exactly one source).
#[derive(Clone, Debug)]
pub(crate) enum Source {
    /// A script file: raw bytes, hex or assembler.
    File(std::path::PathBuf),
    /// The user's own flash container (FRF / ZIP / ODX / SGO); an encrypted
    /// FRF needs the user's FRF key file.
    Container {
        path: std::path::PathBuf,
        frf_key: Option<std::path::PathBuf>,
    },
}

/// One SA2 candidate: the bytes plus whether they decode as a script.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub index: usize,
    /// Where it came from (`hex`, `assembler`, `raw bytes`, an ODX field
    /// name or `SGO`).
    pub field: String,
    pub session: Option<String>,
    pub len: usize,
    /// The bytes as hex (the first [`PREVIEW_BYTES`] when too long).
    pub hex: String,
    /// Too long to be a script (only a preview is kept).
    pub truncated: bool,
    pub decodes: bool,
    pub error: Option<String>,
    pub lints: usize,
}

/// What a source held.
#[derive(Clone, Debug)]
pub(crate) struct Loaded {
    /// `file` or `container` (typed text goes straight to [`inspect`]).
    pub source: &'static str,
    pub origin: String,
    pub container_kind: Option<String>,
    pub candidates: Vec<Candidate>,
    /// Why a container listed nothing.
    pub note: Option<&'static str>,
}

impl Loaded {
    /// The candidate to preselect: the first that decodes, else the first.
    pub fn preferred(&self) -> Option<usize> {
        self.candidates
            .iter()
            .position(|c| c.decodes)
            .or((!self.candidates.is_empty()).then_some(0))
    }
}

pub(crate) fn candidate(
    index: usize,
    field: &str,
    session: Option<&str>,
    bytes: &[u8],
) -> Candidate {
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
    Candidate {
        index,
        field: field.to_string(),
        session: session.map(str::to_string),
        len: bytes.len(),
        hex: hexs(shown),
        truncated: long,
        decodes,
        error,
        lints,
    }
}

/// Read a script source and list the SA2 scripts in it. Offline, read-only.
pub(crate) fn load(src: &Source) -> Result<Loaded, String> {
    match src {
        Source::File(p) => {
            let raw = read_capped(&p.to_string_lossy(), MAX_SCRIPT_FILE, "script file")?;
            let (bytes, form) = script_from_file_bytes(raw)?;
            check_len(&bytes)?;
            Ok(Loaded {
                source: "file",
                origin: format!("{} ({form})", file_name(p)),
                container_kind: None,
                candidates: vec![candidate(0, form, None, &bytes)],
                note: None,
            })
        }
        Source::Container { path, frf_key } => from_container(path, frf_key.as_deref()),
    }
}

fn from_container(path: &Path, frf_key: Option<&Path>) -> Result<Loaded, String> {
    let bytes = read_capped(&path.to_string_lossy(), MAX_CONTAINER, "container")?;
    let mut keys = opentune_container::Keys::default();
    if let Some(k) = frf_key.filter(|k| !k.as_os_str().is_empty()) {
        keys.frf_key = Some(read_capped(
            &k.to_string_lossy(),
            MAX_KEY_FILE,
            "FRF key file",
        )?);
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
    let candidates: Vec<Candidate> = unpacked
        .sa2_scripts
        .iter()
        .enumerate()
        .map(|(i, s)| candidate(i, &s.field, s.session.as_deref(), &s.bytes))
        .collect();
    let note = candidates.is_empty().then_some(
        "No SA2 script found: looked for an ODX SECURITY block whose SECURITY-METHOD is SA2, and an SGO header's SA2 field.",
    );
    Ok(Loaded {
        source: "container",
        origin: file_name(path),
        container_kind: Some(format!("{:?}", unpacked.kind)),
        candidates,
        note,
    })
}

// ---------------------------------------------------------------------------
// Disassembly
// ---------------------------------------------------------------------------

/// One disassembly row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Row {
    pub offset: usize,
    pub bytes: String,
    pub text: String,
    /// Loop nesting (for indentation).
    pub depth: usize,
    /// `Lxxxx` when a branch lands here.
    pub label: Option<String>,
    pub target: Option<usize>,
}

/// Linear disassembly rows, stopping (with the error) at the first byte that
/// does not decode. Branch targets on instruction boundaries get labels.
pub(crate) fn disasm_rows(bytes: &[u8]) -> (Vec<Row>, Option<String>) {
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
        out.push(Row {
            offset: *o,
            bytes: hexs(&bytes[*o..*o + op.len()]),
            text,
            depth,
            label: labels.contains(o).then(|| format!("L{o:04X}")),
            target,
        });
        if matches!(op, Op::Loop(_)) {
            depth += 1;
        }
    }
    if labels.contains(&bytes.len()) {
        out.push(Row {
            offset: bytes.len(),
            bytes: String::new(),
            text: "(end)".into(),
            depth: 0,
            label: Some(format!("L{:04X}", bytes.len())),
            target: None,
        });
    }
    (out, error)
}

/// A disassembled script.
#[derive(Clone, Debug)]
pub(crate) struct Inspection {
    pub bytes: Vec<u8>,
    /// `hex` or `assembler`.
    pub input: &'static str,
    pub decodes: bool,
    /// Where the disassembly stopped.
    pub error: Option<String>,
    pub rows: Vec<Row>,
    pub lints: Vec<String>,
    /// The program as SA2 assembler (when it decodes).
    pub source: Option<String>,
}

/// Disassemble a script (hex or assembler text). Offline.
pub(crate) fn inspect(script: &str) -> Result<Inspection, String> {
    let (bytes, input) = script_from_text(script)?;
    check_len(&bytes)?;
    let (rows, error) = disasm_rows(&bytes);
    let program = Program::parse(&bytes).ok();
    let lints = program
        .as_ref()
        .map(|p| p.lints().iter().map(ToString::to_string).collect())
        .unwrap_or_default();
    Ok(Inspection {
        source: program.as_ref().map(Program::to_source),
        decodes: program.is_some(),
        bytes,
        input,
        error,
        rows,
        lints,
    })
}

// ---------------------------------------------------------------------------
// Seed -> key preview
// ---------------------------------------------------------------------------

/// One executed instruction of a traced run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TraceRow {
    pub offset: usize,
    pub op: String,
    pub register: u32,
    pub carry: bool,
    pub loops: usize,
}

/// A run of the script over one seed.
#[derive(Clone, Debug)]
pub(crate) struct Preview {
    pub seed: u32,
    pub key: Option<u32>,
    pub steps: u64,
    pub error: Option<String>,
    /// Executed instructions (at most the trace limit), when traced.
    pub trace: Option<Vec<TraceRow>>,
    pub trace_truncated: bool,
}

/// Run `bytes` over `seed` (4 bytes, hex) with an optional trace of at most
/// `trace_limit` rows (clamped to 1..=[`MAX_TRACE_ROWS`]) and a step budget
/// (default the VM's, at most ten times it). A seed that is not 4 bytes is an
/// `Err`; a run that fails is a `Preview` with `error`.
pub(crate) fn preview(
    bytes: &[u8],
    seed: &str,
    trace: bool,
    trace_limit: Option<usize>,
    max_steps: Option<u64>,
) -> Result<Preview, String> {
    let s = parse_hex(seed).map_err(|e| format!("seed: {e}"))?;
    let s: [u8; 4] = s.as_slice().try_into().map_err(|_| {
        format!(
            "the seed must be exactly 4 bytes (the ECU's 67 11 <seed> reply), got {}",
            s.len()
        )
    })?;
    let seed = u32::from_be_bytes(s);
    let limit = trace_limit
        .unwrap_or(DEFAULT_TRACE_ROWS)
        .clamp(1, MAX_TRACE_ROWS);
    let budget = max_steps.unwrap_or(vm::MAX_STEPS).clamp(1, MAX_STEP_BUDGET);
    let mut rows: Vec<TraceRow> = Vec::new();
    let mut seen = 0u64;
    let mut record = |st: &Step| {
        seen += 1;
        if rows.len() < limit {
            rows.push(TraceRow {
                offset: st.offset,
                op: st.op.to_string(),
                register: st.register,
                carry: st.carry,
                loops: st.loop_depth,
            });
        }
    };
    let cb: Option<&mut dyn FnMut(&Step)> = if trace { Some(&mut record) } else { None };
    let (key, steps, error) = match vm::run_with(bytes, seed, budget, cb) {
        Ok(o) => (Some(o.key), o.steps, None),
        Err(e) => (None, seen, Some(e.to_string())),
    };
    let truncated = trace && seen > rows.len() as u64;
    Ok(Preview {
        seed,
        key,
        steps,
        error,
        trace: trace.then_some(rows),
        trace_truncated: truncated,
    })
}

// ---------------------------------------------------------------------------
// Arm / disarm: the `sa2` seed/key handler for live sessions (Bench tier)
// ---------------------------------------------------------------------------

/// Whether a script is armed, and which.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ArmStatus {
    pub armed: Option<ArmedSa2>,
    /// May a script be armed on this channel (alpha / bench)?
    pub live_allowed: bool,
    /// A plugin resolver answers `sa2` first, so the armed script is unused.
    pub shadowed_by_plugin: bool,
}

pub(crate) fn status(keys: &SeedKeys, channel: &BuildChannel) -> ArmStatus {
    ArmStatus {
        armed: keys.armed_sa2(),
        live_allowed: bench_channel(channel),
        shadowed_by_plugin: keys.plugin(SA2_ID).is_some(),
    }
}

/// Arm `script` (hex or assembler) as seed/key id `sa2` for this run, or
/// disarm with no script. The Bench-tier gate is HERE, not in the UI:
/// refused on a stable channel, refused without `ack` (an armed script
/// answers SecurityAccess on real modules, and wrong keys count against
/// their attempt limit), and refused when the script does not decode or
/// fails on any probe seed. Disarming needs neither. Returns the new status
/// and the script's lints.
pub(crate) fn arm(
    script: Option<&str>,
    label: &str,
    ack: bool,
    keys: &SeedKeys,
    channel: &BuildChannel,
) -> Result<(ArmStatus, Vec<String>), String> {
    let Some(script) = script.filter(|s| !s.trim().is_empty()) else {
        keys.arm_sa2(None);
        return Ok((status(keys, channel), Vec::new()));
    };
    if !bench_channel(channel) {
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
        label: label.trim().to_string(),
    }));
    let lints = program.lints().iter().map(ToString::to_string).collect();
    Ok((status(keys, channel), lints))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // Synthetic scripts written for these tests (never an OEM script).
    // eor 0xFFFFFFFF; finish — key = NOT seed.
    const NOT_HEX: &str = "87 FF FF FF FF 4C";
    // loop 3 { rsl; bcc +5; eor 0x5A5A5A5A }; add 0x100; finish
    // (the opentune-container synthetic ODX fixture's script).
    const LOOPY: &str = "68 03 81 4A 05 87 5A 5A 5A 5A 49 93 00 00 01 00 4C";

    fn bench() -> BuildChannel {
        BuildChannel::new(BuildChannel::BENCH)
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "ot-n3-sa2-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn inspect_lists_rows_labels_and_lints() {
        let v = inspect(LOOPY).unwrap();
        assert_eq!(v.bytes.len(), 17);
        assert!(v.decodes);
        assert_eq!(v.input, "hex");
        assert_eq!(v.rows[0].text, "loop 3");
        assert_eq!(v.rows[1].depth, 1, "loop body is indented");
        assert_eq!(v.rows[2].text, "bcc L000A");
        assert_eq!(v.rows[4].label.as_deref(), Some("L000A"));
        assert!(v.lints.is_empty());
        assert!(v.source.unwrap().contains("loop 3"));
        // Assembler text is accepted too.
        let a = inspect("eor 0xFFFFFFFF\nfinish").unwrap();
        assert_eq!((a.input, hexs(&a.bytes).as_str()), ("assembler", NOT_HEX));
    }

    #[test]
    fn preview_runs_and_traces_within_bounds() {
        let bytes = parse_hex(LOOPY).unwrap();
        let p = preview(&bytes, "00000001", true, None, None).unwrap();
        assert_eq!(p.seed, 1);
        assert_eq!(p.key, Some(vm::run(&bytes, 1).unwrap()));
        let trace = p.trace.unwrap();
        assert_eq!(trace.len() as u64, p.steps);
        assert_eq!(trace.last().unwrap().op, "finish");
        assert!(!p.trace_truncated);
        // No trace unless asked.
        let p = preview(
            &parse_hex(NOT_HEX).unwrap(),
            "00 00 00 01",
            false,
            None,
            None,
        )
        .unwrap();
        assert_eq!(p.key, Some(0xFFFF_FFFE));
        assert!(p.trace.is_none());
        // loop 255 { loop 255 { rsl; next }; next }; finish — 130k+ steps.
        let big = inspect("loop 255\nloop 255\nrsl\nnext\nnext\nfinish")
            .unwrap()
            .bytes;
        let p = preview(&big, "12345678", true, Some(10), None).unwrap();
        assert_eq!(p.trace.as_ref().unwrap().len(), 10);
        assert!(p.trace_truncated && p.error.is_none());
        // A tiny budget stops the run with the VM's error, keeping the trace.
        let p = preview(&big, "12345678", true, Some(5000), Some(100)).unwrap();
        assert!(p.key.is_none());
        assert!(p.error.unwrap().contains("step budget"));
        assert_eq!(p.trace.unwrap().len(), 100);
        // The trace limit is clamped.
        let p = preview(&big, "12345678", true, Some(1_000_000), None).unwrap();
        assert_eq!(p.trace.unwrap().len(), MAX_TRACE_ROWS);
        // A seed of the wrong size is refused before anything runs.
        let e = preview(&big, "0102", false, None, None).unwrap_err();
        assert!(e.contains("4 bytes"), "{e}");
    }

    #[test]
    fn bad_scripts_are_reported_not_fatal() {
        // Unknown opcode after one good instruction: rows up to it + error.
        let v = inspect("81 FF").unwrap();
        assert!(!v.decodes);
        assert_eq!(v.rows.len(), 1);
        assert!(v.error.unwrap().contains("0xFF"));
        // A lint (no finish) does not stop anything.
        let v = inspect("81").unwrap();
        assert!(v.lints[0].contains("finish"));
        let p = preview(&v.bytes, "00000001", false, None, None).unwrap();
        assert_eq!(p.key, Some(2));
        assert!(inspect(&"81 ".repeat(MAX_SCRIPT + 1)).is_err());
        assert!(inspect("  ").is_err());
    }

    #[test]
    fn load_file_and_container() {
        let dir = tmp("load");
        let raw = dir.join("script.bin");
        std::fs::write(&raw, [0x87, 0xFF, 0xFF, 0xFF, 0xFF, 0x4C]).unwrap();
        let v = load(&Source::File(raw.clone())).unwrap();
        assert_eq!(v.source, "file");
        assert!(v.candidates[0].decodes);
        assert_eq!(v.preferred(), Some(0));
        assert_eq!(v.candidates[0].hex, NOT_HEX);
        assert!(v.origin.contains("raw bytes"));
        let asm = dir.join("script.sa2");
        std::fs::write(&asm, "; synthetic\neor 0xFFFFFFFF\nfinish\n").unwrap();
        assert_eq!(load(&Source::File(asm)).unwrap().candidates[0].hex, NOT_HEX);

        // The opentune-container synthetic ODX (a SECURITY block, method SA2).
        let odx = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../crates/opentune-container/tests/fixtures/synthetic-sa2.odx");
        if odx.is_file() {
            let v = load(&Source::Container {
                path: odx,
                frf_key: None,
            })
            .unwrap();
            assert_eq!(v.source, "container");
            assert_eq!(v.container_kind.as_deref(), Some("Odx"));
            let c = &v.candidates[0];
            assert_eq!(
                (c.field.as_str(), c.session.as_deref()),
                ("FW-SIGNATURE", Some("SES_SYNTHETIC"))
            );
            assert_eq!(c.hex, LOOPY);
            assert!(c.decodes);
        }

        // Anything else is read as an encrypted FRF, which needs the key file.
        let e = load(&Source::Container {
            path: raw,
            frf_key: None,
        })
        .unwrap_err();
        assert!(e.contains("FRF key"), "{e}");
        assert!(load(&Source::File(dir.join("missing"))).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn candidates_flag_fields_that_are_not_scripts() {
        let c = candidate(1, "FW-CHECKSUM", None, &[0xDE, 0xAD]);
        assert!(!c.decodes && c.error.is_some());
        let big = vec![0x81u8; MAX_SCRIPT + 1];
        let c = candidate(2, "FW-SIGNATURE", None, &big);
        assert!(c.truncated && !c.decodes);
        assert_eq!(c.hex.split(' ').count(), PREVIEW_BYTES);
        let l = Loaded {
            source: "container",
            origin: "x".into(),
            container_kind: None,
            candidates: vec![c, candidate(3, "FW-SIGNATURE", None, &[0x4C])],
            note: None,
        };
        assert_eq!(l.preferred(), Some(1), "the first that decodes");
    }

    #[test]
    fn arming_is_bench_tier_acknowledged_and_checked() {
        let keys = SeedKeys::default();
        let stable = BuildChannel::new(BuildChannel::STABLE);
        assert!(!status(&keys, &stable).live_allowed);
        let e = arm(Some(NOT_HEX), "", true, &keys, &stable).unwrap_err();
        assert!(e.contains("Bench"), "{e}");
        let e = arm(Some(NOT_HEX), "", false, &keys, &bench()).unwrap_err();
        assert!(e.contains("confirmation"), "{e}");
        // Scripts that do not decode, or fail when run, are refused.
        assert!(arm(Some("81 FF"), "", true, &keys, &bench()).is_err());
        assert!(arm(Some("68 00 49 4C"), "", true, &keys, &bench()).is_err());
        assert!(keys.armed_sa2().is_none(), "nothing armed by a refusal");

        let (st, lints) = arm(Some(NOT_HEX), " my.odx #0 ", true, &keys, &bench()).unwrap();
        assert!(lints.is_empty());
        let a = st.armed.unwrap();
        assert_eq!(a.label, "my.odx #0");
        assert_eq!(hexs(&a.script), NOT_HEX);
        assert!(!st.shadowed_by_plugin);
        let h = keys.get("sa2").unwrap();
        assert_eq!(
            h.compute(&[0, 0, 0, 1], 0x11).unwrap(),
            [0xFF, 0xFF, 0xFF, 0xFE]
        );
        // A clone (what an adapter operation holds) sees the armed script.
        assert!(keys.clone().get("sa2").is_some());
        // Disarming needs neither the channel nor an acknowledgement.
        let (st, _) = arm(None, "", false, &keys, &stable).unwrap();
        assert!(st.armed.is_none());
        assert!(keys.get("sa2").is_none());
    }
}
