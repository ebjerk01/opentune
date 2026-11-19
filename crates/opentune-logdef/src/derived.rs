//! **Calculated parameters**: RomRaider `<depends>` parameters such as
//! *Engine Load (Calculated)* `(P12*60)/P8` or *Manifold Relative Pressure*
//! `[P7:psi]-[P24:psi]` have no address of their own; their value is an
//! expression over other parameters read in the same round.
//!
//! [`crate::live::selected_params`] pulls every parameter a selected
//! calculated one references into the selection (so selecting only `P200`
//! also reads `P8` and `P12`), and [`fill`] evaluates the calculated channels
//! after each round's reads, with the safe evaluator in [`crate::expr`]. A
//! reference is the other parameter's value in its primary conversion, or with
//! `[id:units]` in the conversion with those units. A missing input, a failed
//! read or a division by zero leaves the sample empty with a note — never a
//! stale or guessed number.

use std::borrow::Borrow;

use crate::expr::{LogExpr, ParamRef};
use crate::live::{conversion_input, Sample};
use crate::model::{Addressing, Conversion, ConversionFit, LogParam, ProtocolGroup};

/// Deepest chain of calculated-on-calculated parameters followed.
const MAX_CHAIN: usize = 16;

/// The parameter ids `p`'s primary conversion references (empty for an
/// ordinary channel).
pub fn references(p: &LogParam) -> &[String] {
    match &p.conversion {
        Some(Conversion {
            fit: ConversionFit::Expression { refs },
            ..
        }) => refs,
        _ => &[],
    }
}

/// Whether `p` is a calculated parameter: its primary conversion references
/// other parameters.
pub fn is_calculated(p: &LogParam) -> bool {
    !references(p).is_empty()
}

/// Whether `p` can be computed by [`fill`]: it is calculated and, when it has
/// no address of its own, its expression does not need a raw `x`.
pub fn is_computable(p: &LogParam) -> bool {
    if !is_calculated(p) {
        return false;
    }
    if p.addressing != Addressing::Unknown {
        return true;
    }
    p.conversion
        .as_ref()
        .and_then(|c| LogExpr::parse(&c.expr).ok())
        .is_some_and(|e| !e.uses_x())
}

/// `picked` (in order) plus, before each calculated parameter, the parameters
/// it references that are not already there, looked up in its own protocol.
pub(crate) fn with_dependencies<'d>(
    picked: Vec<(&'d ProtocolGroup, &'d LogParam)>,
) -> Vec<&'d LogParam> {
    fn add<'d>(
        proto: &'d ProtocolGroup,
        p: &'d LogParam,
        out: &mut Vec<&'d LogParam>,
        depth: usize,
    ) {
        if out.iter().any(|q| std::ptr::eq(*q, p)) {
            return;
        }
        if depth < MAX_CHAIN {
            for id in references(p) {
                let dep = proto
                    .ecus
                    .iter()
                    .flat_map(|g| g.params.iter())
                    .find(|q| &q.id == id);
                if let Some(dep) = dep {
                    if !std::ptr::eq(dep, p) {
                        add(proto, dep, out, depth + 1);
                    }
                }
            }
        }
        if !out.iter().any(|q| std::ptr::eq(*q, p)) {
            out.push(p);
        }
    }
    let mut out = Vec::with_capacity(picked.len());
    for (proto, p) in picked {
        add(proto, p, &mut out, 0);
    }
    out
}

/// The value of reference `r` from sample `j`.
fn ref_value<P: Borrow<LogParam>>(
    r: &ParamRef,
    j: usize,
    params: &[P],
    samples: &[Sample],
) -> Result<f64, String> {
    let p = params[j].borrow();
    let s = &samples[j];
    let primary_units = p.conversion.as_ref().map(|c| c.units.as_str());
    let wanted = match &r.units {
        None => None,
        Some(u) if primary_units == Some(u.as_str()) => None,
        Some(u) => Some(u),
    };
    let Some(units) = wanted else {
        return s
            .value
            .ok_or_else(|| format!("{} has no value this round", r.id));
    };
    let conv = p
        .conversions
        .iter()
        .find(|c| &c.units == units)
        .or_else(|| {
            p.conversions
                .iter()
                .find(|c| c.units.eq_ignore_ascii_case(units))
        })
        .ok_or_else(|| format!("{} has no '{units}' conversion", r.id))?;
    let raw = s
        .raw
        .ok_or_else(|| format!("{} has no value this round", r.id))?;
    let x = conversion_input(p, raw);
    match &conv.fit {
        ConversionFit::Linear { factor, offset } => Ok(x * factor + offset),
        ConversionFit::Expression { refs } if refs.is_empty() => LogExpr::parse(&conv.expr)
            .and_then(|e| e.eval_x(x))
            .map_err(|e| format!("{r}: {e}")),
        _ => Err(format!("{r}: that conversion cannot be evaluated here")),
    }
}

/// Evaluate the calculated channels of one round in place. `params` and
/// `samples` are aligned (one sample per parameter, same order).
pub(crate) fn fill<P: Borrow<LogParam>>(params: &[P], samples: &mut [Sample]) {
    let calc: Vec<usize> = (0..params.len())
        .filter(|&i| is_calculated(params[i].borrow()))
        .collect();
    if calc.is_empty() || samples.len() != params.len() {
        return;
    }
    let index_of = |id: &str| params.iter().position(|p| p.borrow().id == id);
    let mut done: Vec<bool> = (0..params.len())
        .map(|i| !is_calculated(params[i].borrow()))
        .collect();
    for _ in 0..=calc.len() {
        let mut progress = false;
        for &i in &calc {
            if done[i] {
                continue;
            }
            let p = params[i].borrow();
            let pending = references(p)
                .iter()
                .any(|id| index_of(id).is_some_and(|j| j != i && !done[j]));
            if pending {
                continue;
            }
            evaluate_one(i, params, samples, &index_of);
            done[i] = true;
            progress = true;
        }
        if !progress {
            break;
        }
    }
    for &i in &calc {
        if !done[i] {
            let s = &mut samples[i];
            s.value = None;
            s.note = "calculated: circular dependency between calculated parameters".into();
        }
    }
}

fn evaluate_one<P: Borrow<LogParam>>(
    i: usize,
    params: &[P],
    samples: &mut [Sample],
    index_of: &dyn Fn(&str) -> Option<usize>,
) {
    let p = params[i].borrow();
    let Some(conv) = p.conversion.as_ref() else {
        return;
    };
    let addressed = p.addressing != Addressing::Unknown;
    let x = if addressed {
        match samples[i].raw {
            Some(raw) => Some(conversion_input(p, raw)),
            // The read failed: its note already says why.
            None => return,
        }
    } else {
        None
    };
    let result = LogExpr::parse(&conv.expr)
        .map_err(|e| e.to_string())
        .and_then(|e| {
            e.eval(x, &mut |r| match index_of(&r.id) {
                Some(j) if j != i => ref_value(r, j, params, samples),
                Some(_) => Err(format!("{} references itself", r.id)),
                None => Err(format!("{} is not logged in this session", r.id)),
            })
            .map_err(|e| e.to_string())
        });
    let s = &mut samples[i];
    match result {
        Ok(v) => {
            s.value = Some(v);
            if !conv.units.is_empty() {
                s.units = conv.units.clone();
            }
            if !addressed {
                s.note.clear();
            }
        }
        Err(why) => {
            s.value = None;
            s.note = format!("calculated: {why}");
        }
    }
}
