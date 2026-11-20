//! Presentation helpers for a front end's tune editor: a value as one line of
//! text ([`fmt_value`]), how two values differ ([`describe_change`],
//! [`diff_rows`]), a UI value parsed for a constant ([`parse_value`]) and the
//! JSON shapes of an INI summary, a constant's metadata and value, a table /
//! curve axis and an `.msq` load report. Pure: no I/O, no state.

use crate::{
    AxisRef, ChannelKind, Constant, Ini, Kind, MsqReport, Num, Tune, Value as TsValue, WarnKind,
};
use serde_json::{json, Value};
use std::path::Path;

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
pub const MAX_OPTIONS: usize = 256;

/// A constant's metadata for an editor: class, page, type, units, shape,
/// bit options, limits and digits.
pub fn const_meta(c: &Constant) -> Value {
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

/// What an INI loaded from `path` defines: signature, framing, pages,
/// constants, tables, curves, channels, setting groups, conditions and the
/// warning counts (with the first invalid lines).
pub fn ini_summary(ini: &Ini, path: &Path) -> Value {
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

/// An `.msq` load report: applied, unknown, skipped, out-of-range, notes.
pub fn report_json(r: &MsqReport) -> Value {
    json!({
        "applied": r.applied,
        "unknown": r.unknown,
        "skipped": r.skipped.iter().map(|(n, w)| json!({ "name": n, "why": w })).collect::<Vec<_>>(),
        "out_of_range": r.out_of_range,
        "notes": r.notes,
    })
}

/// A constant's value read from a tune: `{ name, value, display, error }`.
pub fn value_json(c: &Constant, v: &Result<TsValue, crate::Error>) -> Value {
    match v {
        Ok(v) => json!({ "name": c.name, "value": v, "display": fmt_value(c, v), "error": null }),
        Err(e) => json!({ "name": c.name, "value": null, "display": null, "error": e.to_string() }),
    }
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

/// A table / curve axis of a tune: its constant, channel, units, digits and
/// values (or why they cannot be read).
pub fn axis_json(ini: &Ini, tune: &Tune, a: Option<&AxisRef>) -> Value {
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
