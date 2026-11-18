//! # Aftermarket ECU panel (TunerStudio-compatible) — backend
//!
//! A thin shell over `opentune-tsini` (INI definitions, `.msq` tunes) and
//! `opentune-tsproto` (serial protocol, verified write + burn), following the
//! API in docs/AFTERMARKET-ECUS.md and the CLI (`opentune ts`) as the
//! reference use.
//!
//! **Offline:** load an INI (with `#set` conditions), load / save / diff a
//! `.msq`, read constants, tables and curves of the panel's *working tune* and
//! edit them. Edits are validated: a value outside the INI's limits, an
//! out-of-range bit option or a value the storage type cannot hold is refused,
//! never clamped.
//!
//! **Online** (`port` = a serial device or `sim` for the simulated ECU):
//! connect + signature check, read the ECU's tune, plan a write (byte ranges
//! and the per-constant diff for the confirm dialog), write with an explicit
//! confirmation (the ECU's current tune is re-read and saved as a `.msq` +
//! raw `.pages.bin` backup **first**; every page is read back and verified
//! before anything is burned), and poll realtime channels.
//!
//! The serial port is exclusive hardware like a vehicle adapter, so every
//! online operation runs through the app-wide [`AdapterHub`] busy slot: a TS
//! read, write or poll and a vehicle panel can never run at the same time.
//! The opened port lives in [`TsState`] between operations (an operation takes
//! it out, so a poll can never overlap a write) and is released by
//! `ts_disconnect` or by loading another INI.

use crate::adapter::AdapterHub;
use opentune_tsini::value::range_issues;
use opentune_tsini::{
    decode_channels, load_ini, normalize_signature, AxisRef, ChannelKind, Constant, Ini,
    IniOptions, Kind, Msq, MsqLoadOptions, MsqMeta, MsqReport, Num, Tune, Value as TsValue,
    WarnKind,
};
use opentune_tsproto::{
    plan_write, write_tune, Client, ClientOptions, Framing, Port, SerialPort, SimEcu, SimOptions,
    WriteAuth, WriteOptions,
};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// A port the panel can keep between operations.
pub type BoxPort = Box<dyn Port + Send>;

/// Default serial speed (Speeduino, rusEFI and MS3 all use 115200).
pub const DEFAULT_BAUD: u32 = 115_200;

/// An open ECU connection.
struct Conn {
    port: BoxPort,
    label: String,
    sim: bool,
    framing: Option<Framing>,
    since: Instant,
}

impl Conn {
    fn opts(&self) -> ClientOptions {
        ClientOptions {
            framing: self.framing,
            retries: 2,
            honor_delays: !self.sim,
            ..ClientOptions::default()
        }
    }
}

#[derive(Default)]
struct Inner {
    ini_path: Option<PathBuf>,
    /// The user's `#set` / `#unset` conditions.
    set: BTreeSet<String>,
    unset: BTreeSet<String>,
    ini: Option<Arc<Ini>>,
    /// The tune the panel edits (from a `.msq`, the ECU or blank).
    working: Option<Tune>,
    working_src: String,
    /// The tune last read from the ECU: the base of a write plan and what the
    /// ECU must still hold when the write starts.
    ecu: Option<Tune>,
    conn: Option<Conn>,
}

/// Tauri-managed state of the aftermarket-ECU panel.
#[derive(Default)]
pub struct TsState {
    inner: Mutex<Inner>,
}

impl TsState {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn e2s(e: impl std::fmt::Display) -> String {
    e.to_string()
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

fn fmt_num(v: f64, digits: Option<usize>) -> String {
    match digits {
        Some(d) => format!("{v:.d$}", d = d.min(9)),
        None => format!("{v}"),
    }
}

/// A value as one line of text (arrays summarised as their range).
pub fn fmt_value(c: &Constant, v: &TsValue) -> String {
    let units = if c.units.is_empty() {
        String::new()
    } else {
        format!(" {}", c.units)
    };
    match v {
        TsValue::Scalar(x) => format!("{}{units}", fmt_num(*x, c.digits_lit())),
        TsValue::Bits(i) => format!("{i} \"{}\"", c.bit_label(*i as usize)),
        TsValue::Text(t) => format!("{t:?}"),
        TsValue::Array(a) => {
            let (cols, rows) = match c.kind {
                Kind::Array { cols, rows } => (cols, rows),
                _ => (a.len(), 1),
            };
            let lo = a.iter().copied().fold(f64::INFINITY, f64::min);
            let hi = a.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            format!(
                "[{cols}x{rows}] {} .. {}{units}",
                fmt_num(lo, c.digits_lit()),
                fmt_num(hi, c.digits_lit())
            )
        }
    }
}

/// How two values of one constant differ, or `None` if equal.
pub fn describe_change(c: &Constant, a: &TsValue, b: &TsValue) -> Option<String> {
    if a == b {
        return None;
    }
    match (a, b) {
        (TsValue::Array(x), TsValue::Array(y)) if x.len() == y.len() => {
            let changed: Vec<f64> = x
                .iter()
                .zip(y)
                .filter(|(p, q)| p != q)
                .map(|(p, q)| (q - p).abs())
                .collect();
            let max = changed.iter().copied().fold(0.0, f64::max);
            Some(format!(
                "{} of {} cells changed (max |change| {}{})",
                changed.len(),
                x.len(),
                fmt_num(max, c.digits_lit()),
                if c.units.is_empty() {
                    String::new()
                } else {
                    format!(" {}", c.units)
                }
            ))
        }
        _ => Some(format!("{} -> {}", fmt_value(c, a), fmt_value(c, b))),
    }
}

/// `[{ name, change }]` for every constant whose value differs.
pub fn diff_rows(ini: &Ini, a: &Tune, b: &Tune) -> Vec<Value> {
    let mut out = Vec::new();
    for c in ini.constants.iter().chain(&ini.pc_variables) {
        if let (Ok(x), Ok(y)) = (a.get(ini, &c.name), b.get(ini, &c.name)) {
            if let Some(d) = describe_change(c, &x, &y) {
                out.push(json!({ "name": c.name, "page": c.page, "change": d }));
            }
        }
    }
    out
}

fn num_json(n: &Option<Num>) -> Value {
    match n {
        Some(Num::Lit(x)) => json!(x),
        Some(Num::Expr(e)) => json!({ "expr": e }),
        None => Value::Null,
    }
}

/// Most bit-field options listed for one constant.
const MAX_OPTIONS: usize = 256;

fn const_meta(c: &Constant) -> Value {
    let (class, cols, rows, options, len) = match &c.kind {
        Kind::Scalar => ("scalar", None, None, None, None),
        Kind::Array { cols, rows } => ("array", Some(*cols), Some(*rows), None, None),
        Kind::Bits { .. } => {
            let n = c.bit_slots().min(MAX_OPTIONS);
            let opts: Vec<String> = (0..n).map(|i| c.bit_label(i)).collect();
            ("bits", None, None, Some(opts), None)
        }
        Kind::String { len } => ("string", None, None, None, Some(*len)),
    };
    json!({
        "name": c.name,
        "page": c.page,
        "pc": c.page.is_none(),
        "class": class,
        "type": format!("{:?}", c.ty),
        "units": c.units,
        "cols": cols,
        "rows": rows,
        "options": options,
        "len": len,
        "min": num_json(&c.min),
        "max": num_json(&c.max),
        "digits": c.digits_lit(),
    })
}

fn ini_summary(ini: &Ini, path: &Path) -> Value {
    let count = |k: WarnKind| ini.warnings_of(k).count();
    let invalid: Vec<Value> = ini
        .warnings_of(WarnKind::Invalid)
        .take(20)
        .map(|w| json!({ "line": w.line, "section": w.section, "message": w.message }))
        .collect();
    json!({
        "path": path.to_string_lossy(),
        "signature": ini.signature,
        "framing": if ini.comms.uses_envelope() { "envelope" } else { "plain" },
        "pages": ini.pages.iter().map(|p| json!({
            "page": p.index + 1,
            "size": p.size,
            "burnable": p.burn.is_some(),
        })).collect::<Vec<_>>(),
        "constants": ini.constants.len(),
        "pc_variables": ini.pc_variables.len(),
        "tables": ini.tables.iter().map(|t| json!({ "id": t.id, "title": t.title, "z": t.z_bins })).collect::<Vec<_>>(),
        "curves": ini.curves.iter().map(|c| json!({ "id": c.id, "title": c.title })).collect::<Vec<_>>(),
        "channels": ini.output_channels.iter().map(|c| json!({
            "name": c.name,
            "units": c.units,
            "class": match c.kind {
                ChannelKind::Scalar { .. } => "scalar",
                ChannelKind::Bits { .. } => "bits",
                ChannelKind::Array { .. } => "array",
                ChannelKind::Expr { .. } => "expr",
            },
        })).collect::<Vec<_>>(),
        "setting_groups": ini.setting_groups.iter().map(|g| json!({
            "name": g.name,
            "label": g.label,
            "options": g.options.iter().map(|(n, l)| json!({ "name": n, "label": l })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "conditions": ini.conditions,
        "warnings": {
            "invalid": count(WarnKind::Invalid),
            "reference": count(WarnKind::Reference),
            "ignored": count(WarnKind::Ignored),
            "first_invalid": invalid,
        },
    })
}

fn report_json(r: &MsqReport) -> Value {
    json!({
        "applied": r.applied,
        "unknown": r.unknown,
        "skipped": r.skipped.iter().map(|(n, w)| json!({ "name": n, "why": w })).collect::<Vec<_>>(),
        "out_of_range": r.out_of_range,
        "notes": r.notes,
    })
}

// ---------------------------------------------------------------------------
// Offline: INI, .msq, values
// ---------------------------------------------------------------------------

fn options(set: &BTreeSet<String>, unset: &BTreeSet<String>) -> IniOptions {
    let mut o = IniOptions {
        set: set.clone(),
        unset: unset.clone(),
    };
    for u in unset {
        o.set.remove(u);
    }
    o
}

/// Load an INI: resets the working tune to blank, forgets the ECU tune and
/// closes any connection (its signature check was against the old INI).
pub fn load_ini_into(
    state: &TsState,
    path: &str,
    set: Vec<String>,
    unset: Vec<String>,
) -> Result<Value, String> {
    let (set, unset): (BTreeSet<String>, BTreeSet<String>) =
        (set.into_iter().collect(), unset.into_iter().collect());
    let p = PathBuf::from(path);
    let ini = load_ini(&p, &options(&set, &unset)).map_err(|e| format!("load INI {path}: {e}"))?;
    let summary = ini_summary(&ini, &p);
    let mut g = state.lock();
    *g = Inner {
        working: Some(Tune::blank(&ini)),
        working_src: "blank".into(),
        ini: Some(Arc::new(ini)),
        ini_path: Some(p),
        set,
        unset,
        ..Inner::default()
    };
    Ok(summary)
}

/// Load a `.msq` as the working tune. Its saved settings become INI
/// conditions (the INI is re-read with them when they add any); bytes the
/// tune does not mention keep the ECU's values when an ECU tune was read.
pub fn load_msq_into(state: &TsState, path: &str, force: bool) -> Result<Value, String> {
    let msq = Msq::load(Path::new(path)).map_err(|e| format!("read tune {path}: {e}"))?;
    let mut g = state.lock();
    let mut ini = g
        .ini
        .clone()
        .ok_or_else(|| "load an INI definition first".to_string())?;
    let mut reloaded = Vec::new();
    let extra: BTreeSet<String> = msq
        .settings_conditions()
        .into_iter()
        .filter(|c| !g.unset.contains(c) && !ini.conditions.contains(c))
        .collect();
    if !extra.is_empty() {
        let ini_path = g.ini_path.clone().ok_or("INI path unknown")?;
        let set: BTreeSet<String> = g.set.union(&extra).cloned().collect();
        let new = load_ini(&ini_path, &options(&set, &g.unset)).map_err(e2s)?;
        reloaded = extra.into_iter().collect();
        // An ECU tune / connection made for the old layout stays only if the
        // pages still fit.
        if let Some(t) = &g.ecu {
            if Tune::from_pages(&new, t.pages.clone()).is_err() {
                g.ecu = None;
            }
        }
        ini = Arc::new(new);
        g.ini = Some(ini.clone());
    }
    let base = g.ecu.clone().unwrap_or_else(|| Tune::blank(&ini));
    let (tune, rep) = msq
        .to_tune(
            &ini,
            base,
            MsqLoadOptions {
                force_signature: force,
            },
        )
        .map_err(e2s)?;
    g.working = Some(tune);
    g.working_src = Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(json!({
        "path": path,
        "signature": msq.signature(),
        "ini_signature": ini.signature,
        "entries": msq.entries().count(),
        "report": report_json(&rep),
        "conditions_added": reloaded,
        "base": if g.ecu.is_some() { "ecu" } else { "blank" },
    }))
}

fn meta(comment: &str) -> MsqMeta {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    MsqMeta {
        author: format!("OpenTune {}", env!("CARGO_PKG_VERSION")),
        comment: comment.into(),
        write_date: format!("{secs} (Unix time)"),
        firmware_info: String::new(),
    }
}

/// Write `tune` as a `.msq`; returns `(entries, skipped)`.
fn save_tune(ini: &Ini, tune: &Tune, path: &Path, comment: &str) -> Result<Value, String> {
    let (m, skipped) = Msq::from_tune(ini, tune, &meta(comment));
    std::fs::write(path, m.to_xml())
        .map_err(|e| crate::common::write_error(&path.to_string_lossy(), &e))?;
    Ok(json!({
        "path": path.to_string_lossy(),
        "entries": m.entries().count(),
        "skipped": skipped.iter().map(|(n, w)| json!({ "name": n, "why": w })).collect::<Vec<_>>(),
    }))
}

/// Save the working tune as a `.msq`.
pub fn save_msq_from(state: &TsState, path: &str) -> Result<Value, String> {
    let g = state.lock();
    let ini = g.ini.clone().ok_or("load an INI definition first")?;
    let tune = g.working.as_ref().ok_or("no working tune")?;
    save_tune(&ini, tune, Path::new(path), "saved by OpenTune")
}

/// Diff the working tune against a `.msq` file.
pub fn diff_msq_with(state: &TsState, path: &str, force: bool) -> Result<Value, String> {
    let msq = Msq::load(Path::new(path)).map_err(|e| format!("read tune {path}: {e}"))?;
    let g = state.lock();
    let ini = g.ini.clone().ok_or("load an INI definition first")?;
    let working = g.working.as_ref().ok_or("no working tune")?;
    let (other, rep) = msq
        .to_tune(
            &ini,
            Tune::blank(&ini),
            MsqLoadOptions {
                force_signature: force,
            },
        )
        .map_err(e2s)?;
    let rows = diff_rows(&ini, &other, working);
    Ok(json!({
        "path": path,
        "rows": rows,
        "count": rows.len(),
        "report": report_json(&rep),
        "direction": "file -> working tune",
    }))
}

fn value_json(c: &Constant, v: &Result<TsValue, opentune_tsini::Error>) -> Value {
    match v {
        Ok(v) => json!({ "name": c.name, "value": v, "display": fmt_value(c, v), "error": null }),
        Err(e) => json!({ "name": c.name, "value": null, "display": null, "error": e.to_string() }),
    }
}

/// Every non-array constant (and PC variable) of the working tune with its
/// metadata and value, for the scalar / bits editor.
pub fn values_of(state: &TsState) -> Result<Value, String> {
    let g = state.lock();
    let ini = g.ini.clone().ok_or("load an INI definition first")?;
    let tune = g.working.as_ref().ok_or("no working tune")?;
    let rows: Vec<Value> = ini
        .constants
        .iter()
        .chain(&ini.pc_variables)
        .filter(|c| !matches!(c.kind, Kind::Array { .. }))
        .map(|c| {
            let mut v = value_json(c, &tune.get(&ini, &c.name));
            v["meta"] = const_meta(c);
            v
        })
        .collect();
    Ok(json!({ "source": g.working_src, "values": rows }))
}

/// One constant: `{ meta, value, display, error }`.
pub fn get_value(state: &TsState, name: &str) -> Result<Value, String> {
    let g = state.lock();
    let ini = g.ini.clone().ok_or("load an INI definition first")?;
    let c = ini
        .constant(name)
        .ok_or_else(|| format!("'{name}' is not defined in the INI"))?;
    let tune = g.working.as_ref().ok_or("no working tune")?;
    let mut v = value_json(c, &tune.get(&ini, name));
    v["meta"] = const_meta(c);
    Ok(v)
}

/// Parse a UI value for constant `c`: a number (scalar), an option index or
/// label (bits), an array of numbers (array, row-major), a string (string).
pub fn parse_value(c: &Constant, v: &Value) -> Result<TsValue, String> {
    let name = &c.name;
    match &c.kind {
        Kind::Scalar => v
            .as_f64()
            .filter(|x| x.is_finite())
            .map(TsValue::Scalar)
            .ok_or_else(|| format!("{name} needs a number")),
        Kind::Bits { .. } => {
            let slots = c.bit_slots();
            let idx = match (v.as_u64(), v.as_str()) {
                (Some(i), _) => i as usize,
                (None, Some(label)) => (0..slots.min(MAX_OPTIONS))
                    .find(|&i| c.bit_label(i) == label)
                    .ok_or_else(|| format!("{name}: '{label}' is not one of its options"))?,
                _ => return Err(format!("{name} needs an option index or label")),
            };
            if idx >= slots {
                return Err(format!("{name}: option {idx} does not fit its bits"));
            }
            if c.bit_label(idx) == "INVALID" {
                return Err(format!("{name}: option {idx} is marked INVALID in the INI"));
            }
            Ok(TsValue::Bits(idx as u32))
        }
        Kind::Array { .. } => {
            let arr = v
                .as_array()
                .ok_or_else(|| format!("{name} needs a list of numbers"))?;
            if arr.len() != c.elements() {
                return Err(format!(
                    "{name} has {} cells, got {}",
                    c.elements(),
                    arr.len()
                ));
            }
            arr.iter()
                .map(|x| x.as_f64().filter(|f| f.is_finite()))
                .collect::<Option<Vec<f64>>>()
                .map(TsValue::Array)
                .ok_or_else(|| format!("{name}: every cell must be a number"))
        }
        Kind::String { len } => {
            let s = v.as_str().ok_or_else(|| format!("{name} needs text"))?;
            if !s.is_ascii() || s.len() > *len {
                return Err(format!("{name}: at most {len} ASCII characters"));
            }
            Ok(TsValue::Text(s.to_owned()))
        }
    }
}

/// Set a constant of the working tune. Refuses (never clamps) a value outside
/// the INI's limits or one the storage type cannot hold.
pub fn set_value(state: &TsState, name: &str, value: &Value) -> Result<Value, String> {
    let mut g = state.lock();
    let ini = g.ini.clone().ok_or("load an INI definition first")?;
    let c = ini
        .constant(name)
        .ok_or_else(|| format!("'{name}' is not defined in the INI"))?;
    let v = parse_value(c, value)?;
    let issues = range_issues(c, &v);
    if let Some(first) = issues.first() {
        return Err(format!("refused: {first} (the INI's limits)"));
    }
    let tune = g.working.as_mut().ok_or("no working tune")?;
    tune.set(&ini, name, &v)
        .map_err(|e| format!("refused: {e}"))?;
    let mut out = value_json(c, &tune.get(&ini, name));
    out["meta"] = const_meta(c);
    Ok(out)
}

fn axis_json(ini: &Ini, tune: &Tune, a: Option<&AxisRef>) -> Value {
    let Some(a) = a else { return Value::Null };
    let c = ini.constant(&a.constant);
    let (values, error) = match tune.get(ini, &a.constant) {
        Ok(TsValue::Array(v)) => (Some(v), None),
        Ok(other) => (None, Some(format!("not an array: {other:?}"))),
        Err(e) => (None, Some(e.to_string())),
    };
    json!({
        "name": a.constant,
        "channel": a.channel,
        "units": c.map(|c| c.units.clone()),
        "digits": c.and_then(Constant::digits_lit),
        "values": values,
        "error": error,
    })
}

/// A `[TableEditor]` table of the working tune: axes and a `rows x cols`
/// grid (`rows` = y bins, `cols` = x bins).
pub fn table_of(state: &TsState, id: &str) -> Result<Value, String> {
    let g = state.lock();
    let ini = g.ini.clone().ok_or("load an INI definition first")?;
    let tune = g.working.as_ref().ok_or("no working tune")?;
    let t = ini.table(id).ok_or_else(|| format!("no table '{id}'"))?;
    let z = t
        .z_bins
        .as_deref()
        .ok_or_else(|| format!("table '{id}' has no zBins"))?;
    let zc = ini
        .constant(z)
        .ok_or_else(|| format!("table '{id}': zBins '{z}' is not defined"))?;
    let Kind::Array { cols, rows } = zc.kind else {
        return Err(format!("table '{id}': '{z}' is not an array"));
    };
    let cells = match tune.get(&ini, z).map_err(e2s)? {
        TsValue::Array(v) => v
            .chunks(cols.max(1))
            .map(<[f64]>::to_vec)
            .collect::<Vec<_>>(),
        _ => return Err(format!("'{z}' is not an array")),
    };
    Ok(json!({
        "id": t.id,
        "title": t.title,
        "x": axis_json(&ini, tune, t.x_bins.as_ref()),
        "y": axis_json(&ini, tune, t.y_bins.as_ref()),
        "z": const_meta(zc),
        "cols": cols,
        "rows": rows,
        "cells": cells,
        "help": t.topic_help,
    }))
}

/// A `[CurveEditor]` curve of the working tune: the x axis and every y line.
pub fn curve_of(state: &TsState, id: &str) -> Result<Value, String> {
    let g = state.lock();
    let ini = g.ini.clone().ok_or("load an INI definition first")?;
    let tune = g.working.as_ref().ok_or("no working tune")?;
    let c = ini.curve(id).ok_or_else(|| format!("no curve '{id}'"))?;
    Ok(json!({
        "id": c.id,
        "title": c.title,
        "labels": c.column_labels,
        "x": axis_json(&ini, tune, c.x_bins.as_ref()),
        "ys": c.y_bins.iter().map(|y| {
            let mut a = axis_json(&ini, tune, Some(y));
            a["meta"] = ini.constant(&y.constant).map(const_meta).unwrap_or(Value::Null);
            a
        }).collect::<Vec<_>>(),
    }))
}

// ---------------------------------------------------------------------------
// Online
// ---------------------------------------------------------------------------

/// Busy-slot names other panels see while a TS operation runs.
pub const OP_PREFIX: &str = "aftermarket ECU";

fn parse_framing(f: Option<&str>) -> Result<Option<Framing>, String> {
    match f.map(str::trim).filter(|s| !s.is_empty()) {
        None | Some("auto") => Ok(None),
        Some("envelope") => Ok(Some(Framing::Envelope)),
        Some("plain") => Ok(Some(Framing::Plain)),
        Some(o) => Err(format!("unknown framing '{o}' (auto, envelope or plain)")),
    }
}

/// Take the connection and INI out of the state for one operation.
fn take_conn(state: &TsState) -> Result<(Conn, Arc<Ini>), String> {
    let mut g = state.lock();
    let ini = g.ini.clone().ok_or("load an INI definition first")?;
    let conn = g.conn.take().ok_or("not connected — press Connect first")?;
    Ok((conn, ini))
}

/// Run `f` with a client over the connection on the adapter hub, then put
/// the port back (also after an error).
fn with_client<R, F>(hub: &AdapterHub, state: &Arc<TsState>, what: &str, f: F) -> Result<R, String>
where
    R: Send + 'static,
    F: FnOnce(&mut Client<'_, BoxPort>, &Ini, &TsState, Duration) -> Result<R, String>
        + Send
        + 'static,
{
    let st = state.clone();
    hub.run(&format!("{OP_PREFIX}: {what}"), move |_s| {
        let (mut conn, ini) = take_conn(&st)?;
        let since = conn.since.elapsed();
        let opts = conn.opts();
        let mut client = Client::new(conn.port, &ini, opts);
        let r = f(&mut client, &ini, &st, since);
        conn.port = client.into_port();
        let mut g = st.lock();
        // Keep the port only if the INI it was checked against is still loaded.
        if g.ini.as_ref().is_some_and(|i| Arc::ptr_eq(i, &ini)) {
            g.conn = Some(conn);
        }
        r
    })
}

/// Open `port` (`sim` = the simulated ECU, holding the working tune) and
/// check the ECU's signature against the INI's.
pub fn connect(
    hub: &AdapterHub,
    state: &Arc<TsState>,
    port: &str,
    baud: Option<u32>,
    framing: Option<&str>,
    force_signature: bool,
) -> Result<Value, String> {
    let framing = parse_framing(framing)?;
    let port = port.trim().to_string();
    if port.is_empty() {
        return Err("choose a serial port (or 'sim')".into());
    }
    let st = state.clone();
    hub.run(&format!("{OP_PREFIX}: connect"), move |s| {
        // Release any cached vehicle adapter: it may hold this serial port.
        s.invalidate();
        let (ini, working) = {
            let mut g = st.lock();
            g.conn = None; // close a previous connection first
            let ini = g.ini.clone().ok_or("load an INI definition first")?;
            (ini, g.working.clone())
        };
        let sim = port.eq_ignore_ascii_case("sim");
        let (boxed, label): (BoxPort, String) = if sim {
            // The simulated ECU starts out holding the working tune.
            let mut ecu = SimEcu::new(&ini, SimOptions::default());
            if let Some(t) = &working {
                ecu.load_tune(t);
            }
            (Box::new(ecu), "sim (simulated ECU, no hardware)".into())
        } else {
            let baud = baud.unwrap_or(DEFAULT_BAUD);
            let settle = Duration::from_millis(ini.comms.delay_after_port_open_ms.unwrap_or(0));
            let p =
                SerialPort::open(&port, baud, settle).map_err(|e| format!("open {port}: {e}"))?;
            (Box::new(p), format!("{port} @ {baud} baud"))
        };
        let conn = Conn {
            port: boxed,
            label,
            sim,
            framing,
            since: Instant::now(),
        };
        attach(&st, &ini, conn, force_signature)
    })
}

/// Check the signature over a freshly opened connection and keep it. A
/// mismatch (unless forced) drops the port: nothing is kept open.
fn attach(st: &TsState, ini: &Ini, mut conn: Conn, force: bool) -> Result<Value, String> {
    let opts = conn.opts();
    let mut client = Client::new(conn.port, ini, opts);
    let used = client.framing();
    let check = client.check_signature(force).map_err(e2s)?;
    conn.port = client.into_port();
    let out = json!({
        "label": conn.label,
        "sim": conn.sim,
        "framing": if used == Framing::Envelope { "envelope" } else { "plain" },
        "signature": {
            "ecu": check.ecu,
            "ini": check.ini.as_deref().map(normalize_signature),
            "matches": check.matches,
        },
    });
    st.lock().conn = Some(conn);
    Ok(out)
}

/// Close the connection (releases the serial port).
pub fn disconnect(hub: &AdapterHub, state: &Arc<TsState>) -> Result<Value, String> {
    let st = state.clone();
    hub.run(&format!("{OP_PREFIX}: disconnect"), move |_s| {
        let was = st.lock().conn.take().map(|c| c.label);
        Ok(json!({ "closed": was }))
    })
}

/// Read every page; the result becomes the ECU tune and the working tune.
pub fn read_ecu(hub: &AdapterHub, state: &Arc<TsState>) -> Result<Value, String> {
    with_client(hub, state, "read tune", |client, ini, st, _| {
        let t0 = Instant::now();
        let tune = client.read_tune(&mut |_, _, _| {}).map_err(e2s)?;
        let mut g = st.lock();
        g.ecu = Some(tune.clone());
        g.working = Some(tune);
        g.working_src = "ECU".into();
        Ok(json!({
            "pages": ini.pages.len(),
            "bytes": ini.total_page_bytes(),
            "elapsed_s": t0.elapsed().as_secs_f64(),
        }))
    })
}

/// What a write of the working tune would change on the ECU (offline: uses
/// the tune last read from the ECU).
pub fn plan(state: &TsState) -> Result<Value, String> {
    let g = state.lock();
    let ini = g.ini.clone().ok_or("load an INI definition first")?;
    let ecu = g
        .ecu
        .as_ref()
        .ok_or("read the ECU's tune first (Read from ECU) — the plan compares against it")?;
    let working = g.working.as_ref().ok_or("no working tune")?;
    let p = plan_write(ecu, working);
    let pages: Vec<Value> = p
        .pages
        .iter()
        .map(|c| {
            json!({
                "page": c.page + 1,
                "ranges": c.ranges,
                "changed_bytes": c.changed_bytes,
                "burnable": ini.pages.get(c.page).is_some_and(|d| d.burn.is_some()),
            })
        })
        .collect();
    Ok(json!({
        "empty": p.is_empty(),
        "changed_bytes": p.changed_bytes(),
        "pages": pages,
        "constants": diff_rows(&ini, ecu, working),
        "connected": g.conn.as_ref().map(|c| c.label.clone()),
        "signature": ini.signature,
    }))
}

/// Write the working tune. Refuses without `confirmed` and without a backup
/// path; re-reads the ECU and refuses if it no longer holds the tune the plan
/// was made against; saves the backup (`.msq` + raw `.pages.bin`) before
/// writing; verifies every page before burning (when `burn`).
pub fn write(
    hub: &AdapterHub,
    state: &Arc<TsState>,
    confirmed: bool,
    burn: bool,
    backup_path: Option<String>,
) -> Result<Value, String> {
    if !confirmed {
        return Err(
            "refusing to write without an explicit confirmation — review the planned changes first"
                .into(),
        );
    }
    let backup = backup_path
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .ok_or("choose where to save the backup of the ECU's current tune before writing")?;
    {
        let g = state.lock();
        if g.ecu.is_none() {
            return Err("read the ECU's tune first (Read from ECU)".into());
        }
        if g.working.is_none() {
            return Err("no working tune".into());
        }
    }
    with_client(hub, state, "write tune", move |client, ini, st, _| {
        let (planned, target) = {
            let g = st.lock();
            (g.ecu.clone(), g.working.clone())
        };
        let (Some(planned), Some(target)) = (planned, target) else {
            return Err("read the ECU's tune first".into());
        };
        // 1. What does the ECU hold right now?
        let current = client.read_tune(&mut |_, _, _| {}).map_err(e2s)?;
        if current.pages != planned.pages {
            st.lock().ecu = Some(current);
            return Err(
                "the ECU's tune changed since it was read — nothing written. Review the new plan \
                 and confirm again."
                    .into(),
            );
        }
        let plan = plan_write(&current, &target);
        if plan.is_empty() {
            return Ok(json!({ "written": false, "reason": "the ECU already holds this tune" }));
        }
        // 2. Backup first: .msq plus the raw pages (lossless).
        let saved = save_tune(ini, &current, &backup, "backup before write")?;
        let raw = backup.with_extension("pages.bin");
        std::fs::write(&raw, current.pages.concat())
            .map_err(|e| crate::common::write_error(&raw.to_string_lossy(), &e))?;
        // 3. Write changed ranges, verify every page, then burn.
        let auth = WriteAuth::user_confirmed();
        let res = write_tune(
            client,
            &auth,
            &current,
            &target,
            WriteOptions {
                burn,
                check_crc: true,
            },
            &mut |_, _, _, _| {},
        );
        let mut g = st.lock();
        match res {
            Ok(rep) => {
                g.ecu = Some(target);
                let pages = |v: &[usize]| v.iter().map(|p| p + 1).collect::<Vec<_>>();
                Ok(json!({
                    "written": true,
                    "bytes_written": rep.bytes_written,
                    "verified_pages": pages(&rep.pages),
                    "burned": pages(&rep.burned),
                    "not_burnable": pages(&rep.not_burnable),
                    "crc_checked": pages(&rep.crc_checked),
                    "burn": burn,
                    "backup": saved["path"],
                    "raw_backup": raw.to_string_lossy(),
                }))
            }
            Err(e) => {
                // The ECU's RAM may now differ from both tunes: re-read first.
                g.ecu = None;
                Err(format!(
                    "write aborted: {e}. The backup {} holds the ECU's previous tune; read the \
                     ECU again before retrying.",
                    backup.display()
                ))
            }
        }
    })
}

/// Read the realtime block and decode `channels` (all non-array channels
/// when empty).
pub fn poll(
    hub: &AdapterHub,
    state: &Arc<TsState>,
    channels: Vec<String>,
) -> Result<Value, String> {
    with_client(hub, state, "poll", move |client, ini, st, since| {
        let wanted: Vec<String> = if channels.is_empty() {
            ini.output_channels
                .iter()
                .filter(|c| !matches!(c.kind, ChannelKind::Array { .. }))
                .map(|c| c.name.clone())
                .collect()
        } else {
            channels
        };
        let block = client.read_realtime().map_err(e2s)?;
        let tune = st.lock().working.clone();
        let t_s = since.as_secs_f64();
        let vars = |n: &str| {
            if n == "timeNow" {
                return Some(t_s);
            }
            tune.as_ref().and_then(|t| t.var(ini, n))
        };
        let samples = decode_channels(ini, &block, &vars, Some(&wanted));
        Ok(json!({ "samples": samples, "t_s": t_s }))
    })
}

/// Connection and tune status for the panel header.
pub fn status_of(state: &TsState) -> Value {
    let g = state.lock();
    json!({
        "ini": g.ini_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
        "signature": g.ini.as_ref().and_then(|i| i.signature.clone()),
        "working": g.working.as_ref().map(|_| g.working_src.clone()),
        "ecu_read": g.ecu.is_some(),
        "connected": g.conn.as_ref().map(|c| c.label.clone()),
        "sim": g.conn.as_ref().is_some_and(|c| c.sim),
    })
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

/// Load an INI definition (`set` / `unset` = `#set` conditions).
#[tauri::command]
pub async fn ts_load_ini(
    path: String,
    set: Option<Vec<String>>,
    unset: Option<Vec<String>>,
    state: tauri::State<'_, Arc<TsState>>,
) -> Result<Value, String> {
    load_ini_into(
        &state,
        &path,
        set.unwrap_or_default(),
        unset.unwrap_or_default(),
    )
}

/// Load a `.msq` as the working tune.
#[tauri::command]
pub async fn ts_load_msq(
    path: String,
    force: Option<bool>,
    state: tauri::State<'_, Arc<TsState>>,
) -> Result<Value, String> {
    load_msq_into(&state, &path, force.unwrap_or(false))
}

/// Save the working tune as a `.msq`.
#[tauri::command]
pub async fn ts_save_msq(
    path: String,
    state: tauri::State<'_, Arc<TsState>>,
) -> Result<Value, String> {
    save_msq_from(&state, &path)
}

/// Per-constant diff: a `.msq` file -> the working tune.
#[tauri::command]
pub async fn ts_diff_msq(
    path: String,
    force: Option<bool>,
    state: tauri::State<'_, Arc<TsState>>,
) -> Result<Value, String> {
    diff_msq_with(&state, &path, force.unwrap_or(false))
}

/// Scalars, bit fields and strings of the working tune.
#[tauri::command]
pub async fn ts_values(state: tauri::State<'_, Arc<TsState>>) -> Result<Value, String> {
    values_of(&state)
}

/// One constant of the working tune.
#[tauri::command]
pub async fn ts_get(name: String, state: tauri::State<'_, Arc<TsState>>) -> Result<Value, String> {
    get_value(&state, &name)
}

/// Set one constant of the working tune (validated against the INI).
#[tauri::command]
pub async fn ts_set(
    name: String,
    value: Value,
    state: tauri::State<'_, Arc<TsState>>,
) -> Result<Value, String> {
    set_value(&state, &name, &value)
}

/// A table of the working tune.
#[tauri::command]
pub async fn ts_table(id: String, state: tauri::State<'_, Arc<TsState>>) -> Result<Value, String> {
    table_of(&state, &id)
}

/// A curve of the working tune.
#[tauri::command]
pub async fn ts_curve(id: String, state: tauri::State<'_, Arc<TsState>>) -> Result<Value, String> {
    curve_of(&state, &id)
}

/// Panel status (INI, working tune, connection).
#[tauri::command]
pub async fn ts_status(state: tauri::State<'_, Arc<TsState>>) -> Result<Value, String> {
    Ok(status_of(&state))
}

/// Connect to `port` (a serial device or `sim`) and check the signature.
#[tauri::command]
pub async fn ts_connect(
    port: String,
    baud: Option<u32>,
    framing: Option<String>,
    force_signature: Option<bool>,
    hub: tauri::State<'_, AdapterHub>,
    state: tauri::State<'_, Arc<TsState>>,
) -> Result<Value, String> {
    connect(
        &hub,
        &state,
        &port,
        baud,
        framing.as_deref(),
        force_signature.unwrap_or(false),
    )
}

/// Close the ECU connection.
#[tauri::command]
pub async fn ts_disconnect(
    hub: tauri::State<'_, AdapterHub>,
    state: tauri::State<'_, Arc<TsState>>,
) -> Result<Value, String> {
    disconnect(&hub, &state)
}

/// Read the ECU's tune (becomes the working tune and the write base).
#[tauri::command]
pub async fn ts_read(
    hub: tauri::State<'_, AdapterHub>,
    state: tauri::State<'_, Arc<TsState>>,
) -> Result<Value, String> {
    read_ecu(&hub, &state)
}

/// The write plan for the confirm dialog.
#[tauri::command]
pub async fn ts_plan_write(state: tauri::State<'_, Arc<TsState>>) -> Result<Value, String> {
    plan(&state)
}

/// Write the working tune (needs `confirmed` and a backup path).
#[tauri::command]
pub async fn ts_write(
    confirmed: bool,
    burn: bool,
    backup_path: Option<String>,
    hub: tauri::State<'_, AdapterHub>,
    state: tauri::State<'_, Arc<TsState>>,
) -> Result<Value, String> {
    write(&hub, &state, confirmed, burn, backup_path)
}

/// Poll realtime channels.
#[tauri::command]
pub async fn ts_poll(
    channels: Option<Vec<String>>,
    hub: tauri::State<'_, AdapterHub>,
    state: tauri::State<'_, Arc<TsState>>,
) -> Result<Value, String> {
    poll(&hub, &state, channels.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::testkit::SimBus;

    fn demo_ini() -> String {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../samples/ts/opentune-demo.ini"
        )
        .to_string()
    }

    fn tmp(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("opentune-ts-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name).to_string_lossy().into_owned()
    }

    fn setup() -> (AdapterHub, Arc<TsState>) {
        let hub = AdapterHub::spawn(|| SimBus::default().opener());
        let st = Arc::new(TsState::default());
        let s = load_ini_into(&st, &demo_ini(), vec![], vec![]).unwrap();
        assert_eq!(s["signature"], "opentune demo 1");
        assert_eq!(s["framing"], "envelope");
        (hub, st)
    }

    #[test]
    fn offline_edit_validates_against_the_ini() {
        let (_hub, st) = setup();
        let v = set_value(&st, "reqFuel", &json!(12.3)).unwrap();
        assert_eq!(v["display"], "12.3 ms");
        // Outside the INI's limits (0..25.5): refused, value unchanged.
        let e = set_value(&st, "reqFuel", &json!(30)).unwrap_err();
        assert!(e.contains("refused") && e.contains("25.5"), "{e}");
        assert_eq!(get_value(&st, "reqFuel").unwrap()["display"], "12.3 ms");
        // Bits by label or index; INVALID and out-of-range options refused.
        let v = set_value(&st, "injLayout", &json!("Sequential")).unwrap();
        assert_eq!(v["value"], json!({ "type": "bits", "value": 3 }));
        assert!(set_value(&st, "injLayout", &json!(2))
            .unwrap_err()
            .contains("INVALID"));
        assert!(set_value(&st, "injLayout", &json!(4)).is_err());
        assert!(set_value(&st, "injLayout", &json!("Nope")).is_err());
        // Arrays need every cell, strings fit their length.
        assert!(set_value(&st, "wueRates", &json!([100, 100])).is_err());
        set_value(&st, "wueRates", &json!([180, 160, 140, 120, 110, 100])).unwrap();
        assert!(set_value(&st, "tuneName", &json!("x".repeat(17))).is_err());
        set_value(&st, "tuneName", &json!("street")).unwrap();
        assert!(set_value(&st, "nope", &json!(1)).is_err());
        let vals = values_of(&st).unwrap();
        let rows = vals["values"].as_array().unwrap();
        assert!(rows.iter().all(|r| r["meta"]["class"] != "array"));
        assert!(rows.iter().any(|r| r["name"] == "crankRPM"));
    }

    #[test]
    fn tables_curves_and_msq_round_trip_with_diff() {
        let (_hub, st) = setup();
        let mut ve = vec![50.0; 48];
        ve[9] = 77.0; // row 1, col 1
        set_value(&st, "veTable", &json!(ve)).unwrap();
        set_value(
            &st,
            "rpmBins",
            &json!([500, 1000, 2000, 3000, 4000, 5000, 6000, 7000]),
        )
        .unwrap();
        let t = table_of(&st, "veTbl").unwrap();
        assert_eq!((t["rows"].as_u64(), t["cols"].as_u64()), (Some(6), Some(8)));
        assert_eq!(t["cells"][1][1], 77.0);
        assert_eq!(t["x"]["name"], "rpmBins");
        assert_eq!(t["x"]["channel"], "rpm");
        assert_eq!(t["x"]["values"][2], 2000.0);
        let c = curve_of(&st, "wueCurve").unwrap();
        assert_eq!(c["x"]["name"], "wueBins");
        assert_eq!(c["ys"][0]["name"], "wueRates");

        let path = tmp("roundtrip.msq");
        let saved = save_msq_from(&st, &path).unwrap();
        assert!(saved["entries"].as_u64().unwrap() > 10);
        // Diff against the saved file: identical.
        assert_eq!(diff_msq_with(&st, &path, false).unwrap()["count"], 0);
        set_value(&st, "crankRPM", &json!(400)).unwrap();
        let d = diff_msq_with(&st, &path, false).unwrap();
        assert_eq!(d["count"], 1);
        assert_eq!(d["rows"][0]["name"], "crankRPM");
        // Loading it back restores the saved value.
        let r = load_msq_into(&st, &path, false).unwrap();
        assert!(r["report"]["applied"].as_u64().unwrap() > 10);
        assert_eq!(
            table_of(&st, "veTbl").unwrap()["cells"][1][1],
            77.0,
            "msq round trip"
        );
        assert_ne!(get_value(&st, "crankRPM").unwrap()["display"], "400 rpm");
    }

    #[test]
    fn simulated_ecu_connect_read_plan_write_verify_poll() {
        let (hub, st) = setup();
        // Offline only until connected.
        assert!(read_ecu(&hub, &st).unwrap_err().contains("not connected"));
        set_value(&st, "reqFuel", &json!(8.0)).unwrap();
        let c = connect(&hub, &st, "sim", None, None, false).unwrap();
        assert_eq!(c["signature"]["matches"], true);
        assert_eq!(c["sim"], true);
        // The plan needs an ECU read first.
        assert!(plan(&st).is_err());
        read_ecu(&hub, &st).unwrap();
        assert_eq!(get_value(&st, "reqFuel").unwrap()["display"], "8.0 ms");
        assert_eq!(plan(&st).unwrap()["empty"], true);

        set_value(&st, "reqFuel", &json!(9.5)).unwrap();
        let mut ve = vec![60.0; 48];
        ve[0] = 61.0;
        set_value(&st, "veTable", &json!(ve)).unwrap();
        let p = plan(&st).unwrap();
        assert_eq!(p["empty"], false);
        let names: Vec<&str> = p["constants"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["name"].as_str().unwrap())
            .collect();
        assert!(
            names.contains(&"reqFuel") && names.contains(&"veTable"),
            "{names:?}"
        );
        assert_eq!(p["pages"].as_array().unwrap().len(), 2);

        // Refused without confirmation, and without a backup path.
        let backup = tmp("before-write.msq");
        let _ = std::fs::remove_file(&backup);
        assert!(write(&hub, &st, false, true, Some(backup.clone()))
            .unwrap_err()
            .contains("confirmation"));
        assert!(write(&hub, &st, true, true, None)
            .unwrap_err()
            .contains("backup"));
        assert!(!Path::new(&backup).exists(), "nothing written or saved");

        let w = write(&hub, &st, true, true, Some(backup.clone())).unwrap();
        assert_eq!(w["written"], true, "{w}");
        assert_eq!(w["verified_pages"], json!([1, 2]));
        assert_eq!(w["burned"], json!([1, 2]));
        assert!(Path::new(&backup).exists());
        assert!(Path::new(&backup).with_extension("pages.bin").exists());
        // Read back: the ECU now holds the edited tune; the plan is empty.
        read_ecu(&hub, &st).unwrap();
        assert_eq!(get_value(&st, "reqFuel").unwrap()["display"], "9.5 ms");
        assert_eq!(plan(&st).unwrap()["empty"], true);
        // The backup holds the old value.
        let d = diff_msq_with(&st, &backup, false).unwrap();
        assert!(d["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["name"] == "reqFuel"));

        // Realtime poll.
        let r = poll(&hub, &st, vec!["rpm".into(), "coolant".into()]).unwrap();
        let s = r["samples"].as_array().unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0]["name"], "rpm");
        assert!(s[0]["value"].is_number());
        // A TS operation holds the shared busy slot like any adapter op.
        assert!(hub.current().is_none());
        disconnect(&hub, &st).unwrap();
        assert!(poll(&hub, &st, vec![]).is_err());
    }

    #[test]
    fn write_refuses_when_the_ecu_changed_since_it_was_read() {
        let (hub, st) = setup();
        connect(&hub, &st, "sim", None, None, false).unwrap();
        read_ecu(&hub, &st).unwrap();
        // Pretend the read was of a different tune: the fresh read differs.
        st.lock().ecu.as_mut().unwrap().pages[0][0] ^= 0xFF;
        set_value(&st, "crankRPM", &json!(300)).unwrap();
        let e = write(&hub, &st, true, true, Some(tmp("changed.msq"))).unwrap_err();
        assert!(e.contains("changed since it was read"), "{e}");
    }

    #[test]
    fn signature_mismatch_is_refused_unless_forced_and_bad_input_is_rejected() {
        let (hub, st) = setup();
        let ini = st.lock().ini.clone().unwrap();
        let other = |sig: &str| Conn {
            port: Box::new(SimEcu::new(
                &ini,
                SimOptions {
                    signature: Some(sig.into()),
                    ..SimOptions::default()
                },
            )),
            label: "sim".into(),
            sim: true,
            framing: None,
            since: Instant::now(),
        };
        let e = attach(&st, &ini, other("speeduino 202501"), false).unwrap_err();
        assert!(e.contains("does not match"), "{e}");
        assert_eq!(status_of(&st)["connected"], Value::Null, "port dropped");
        let forced = attach(&st, &ini, other("speeduino 202501"), true).unwrap();
        assert_eq!(forced["signature"]["matches"], false);
        assert_eq!(forced["signature"]["ini"], "opentune demo 1");
        disconnect(&hub, &st).unwrap();
        // Load the MegaSquirt-like demo INI for the check, then connect the
        // sim built from it: matches. A wrong framing name is refused early.
        assert!(connect(&hub, &st, "sim", None, Some("bogus"), false).is_err());
        assert!(connect(&hub, &st, " ", None, None, false).is_err());
        let ms = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../samples/ts/opentune-demo-ms.ini"
        );
        load_ini_into(&st, ms, vec![], vec![]).unwrap();
        let c = connect(&hub, &st, "sim", None, None, false).unwrap();
        assert_eq!(c["framing"], "plain");
        // Loading another INI drops the connection.
        load_ini_into(&st, &demo_ini(), vec![], vec![]).unwrap();
        assert_eq!(status_of(&st)["connected"], Value::Null);
    }
}
