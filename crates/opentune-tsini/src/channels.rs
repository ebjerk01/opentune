//! Decode the realtime output-channel block (`ochGetCommand` response).
use crate::expr::{Expr, ExprError};
use crate::model::{ChannelKind, Ini, Num};
use crate::value::{eval_num, read_raw};
use serde::Serialize;
use std::collections::HashMap;

/// One channel's reading.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChannelSample {
    pub name: String,
    /// The stored raw value (`None` for expression channels).
    pub raw: Option<f64>,
    /// The value in user units (`None` when it could not be computed).
    pub value: Option<f64>,
    pub units: String,
    /// Why `value` is missing, or other remarks.
    pub note: String,
}

/// Decode every output channel from `block`, then evaluate `{ }` channels
/// (in dependency order). `vars` resolves other names expressions use
/// (constants of the current tune, `timeNow`, ...). `wanted` limits and orders
/// the result; an unknown wanted name yields a sample with a note.
pub fn decode_channels(
    ini: &Ini,
    block: &[u8],
    vars: &dyn Fn(&str) -> Option<f64>,
    wanted: Option<&[String]>,
) -> Vec<ChannelSample> {
    let e = ini.endianness;
    let mut values: HashMap<String, f64> = HashMap::new();
    let mut samples: Vec<ChannelSample> = Vec::with_capacity(ini.output_channels.len());
    for ch in &ini.output_channels {
        let mut s = ChannelSample {
            name: ch.name.clone(),
            raw: None,
            value: None,
            units: ch.units.clone(),
            note: String::new(),
        };
        match &ch.kind {
            ChannelKind::Scalar {
                ty,
                offset,
                scale,
                translate,
            } => match block.get(*offset..).and_then(|b| read_raw(b, *ty, e)) {
                Some(raw) => {
                    s.raw = Some(raw);
                    let sc = |n: &Num| eval_num(n, vars);
                    match (sc(scale), sc(translate)) {
                        (Ok(k), Ok(t)) => s.value = Some((raw + t) * k),
                        (Err(err), _) | (_, Err(err)) => s.note = format!("scale: {err}"),
                    }
                }
                None => s.note = format!("offset {offset} beyond the {}-byte block", block.len()),
            },
            ChannelKind::Bits { ty, offset, lo, hi } => {
                match block.get(*offset..).and_then(|b| read_raw(b, *ty, e)) {
                    Some(raw) => {
                        let w = raw as i64 as u64;
                        let v = (w >> lo) & ((1u64 << (hi - lo + 1)) - 1);
                        s.raw = Some(v as f64);
                        s.value = Some(v as f64);
                    }
                    None => {
                        s.note = format!("offset {offset} beyond the {}-byte block", block.len())
                    }
                }
            }
            ChannelKind::Array { len, .. } => {
                s.note = format!("array channel ({len} values) not decoded");
            }
            ChannelKind::Expr { .. } => {}
        }
        if let Some(v) = s.value {
            values.insert(ch.name.clone(), v);
        }
        samples.push(s);
    }

    // Expressions: repeat while progress is made, so a channel may use one
    // defined later in the file.
    let parsed: Vec<(usize, Result<Expr, ExprError>)> = ini
        .output_channels
        .iter()
        .enumerate()
        .filter_map(|(i, ch)| match &ch.kind {
            ChannelKind::Expr { expr } => Some((i, Expr::parse(expr))),
            _ => None,
        })
        .collect();
    let mut pending: Vec<usize> = (0..parsed.len()).collect();
    loop {
        let before = pending.len();
        let mut still = Vec::new();
        for k in pending {
            let (i, ex) = &parsed[k];
            let r = match ex {
                Ok(x) => x.eval(&|n| values.get(n).copied().or_else(|| vars(n))),
                Err(err) => Err(err.clone()),
            };
            match r {
                Ok(v) => {
                    values.insert(samples[*i].name.clone(), v);
                    samples[*i].value = Some(v);
                    samples[*i].note.clear();
                }
                Err(err) => {
                    samples[*i].note = err.to_string();
                    still.push(k);
                }
            }
        }
        pending = still;
        if pending.is_empty() || pending.len() == before {
            break;
        }
    }

    match wanted {
        None => samples,
        Some(names) => names
            .iter()
            .map(|n| {
                samples
                    .iter()
                    .find(|s| &s.name == n)
                    .cloned()
                    .unwrap_or_else(|| ChannelSample {
                        name: n.clone(),
                        raw: None,
                        value: None,
                        units: String::new(),
                        note: "not an output channel in this INI".into(),
                    })
            })
            .collect(),
    }
}
