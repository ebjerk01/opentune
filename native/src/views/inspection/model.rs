//! Inspection glue (pure where it can be, unit-tested): which modules to
//! read, one READ operation per module over the core's
//! [`opentune_core::obd::inspect`], the summary and the plain-text report.

use crate::host::{AdapterAccess, DiagTarget, ProgressSink};
use opentune_core::obd::inspect::{obd_ecu_label, MonitorState};
use opentune_core::obd::{self, Inspection, Sections, OBD_ECU_IDS};
use std::fmt::Write as _;
use std::sync::Arc;

/// Adapter operation name.
pub const OP: &str = "Inspection";

/// Which modules a read covers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Scope {
    /// The app's target module only.
    #[default]
    Target,
    /// Every emissions OBD address `7E0..7E7` (the ones that answer).
    AllObd,
}

/// One module to read.
#[derive(Clone, Debug, PartialEq)]
pub struct Module {
    pub target: DiagTarget,
    pub label: String,
}

/// The modules of `scope` (`app` is the app's target).
pub fn modules(scope: Scope, app: DiagTarget) -> Vec<Module> {
    match scope {
        Scope::Target => vec![Module {
            target: app,
            label: label_of(app),
        }],
        Scope::AllObd => OBD_ECU_IDS
            .iter()
            .map(|&(tx, rx)| {
                let t = DiagTarget::can(tx, rx);
                Module {
                    target: t,
                    label: label_of(t),
                }
            })
            .collect(),
    }
}

fn label_of(t: DiagTarget) -> String {
    match OBD_ECU_IDS
        .iter()
        .find(|(tx, rx)| *tx == t.tx && *rx == t.rx)
    {
        Some(_) => format!("{} ({t})", obd_ecu_label(t.tx)),
        None => format!("Module ({t})"),
    }
}

/// One module's result.
#[derive(Clone, Debug, PartialEq)]
pub struct ModuleResult {
    pub module: Module,
    pub ins: Inspection,
}

/// Read `modules`, one read operation each; with [`Scope::AllObd`] only the
/// modules that answered are kept. An adapter error (no link, busy) ends the
/// read; a module that does not answer is not an error.
pub fn read(
    adapter: &Arc<dyn AdapterAccess>,
    mods: Vec<Module>,
    keep_silent: bool,
    sections: Sections,
    p: Option<&ProgressSink>,
) -> Result<Vec<ModuleResult>, String> {
    let n = mods.len().max(1);
    let mut out = Vec::new();
    for (i, m) in mods.into_iter().enumerate() {
        if p.is_some_and(ProgressSink::cancelled) {
            return Err("cancelled".into());
        }
        if let Some(p) = p {
            p.set(i as f32 / n as f32, format!("reading {}", m.label));
        }
        let ins = adapter.read(OP, m.target, move |d| Ok(obd::inspect(d, sections, 0)))?;
        if keep_silent || ins.responded {
            out.push(ModuleResult { module: m, ins });
        }
    }
    Ok(out)
}

/// Totals over every module read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub modules: usize,
    pub mil_on: bool,
    pub stored: usize,
    pub pending: usize,
    pub permanent: usize,
    /// Supported monitors not complete (since the codes were cleared).
    pub incomplete: usize,
}

pub fn summary(rs: &[ModuleResult]) -> Summary {
    let mut s = Summary {
        modules: rs.iter().filter(|r| r.ins.responded).count(),
        ..Default::default()
    };
    for r in rs {
        if let Some(rd) = &r.ins.readiness {
            s.mil_on |= rd.status.mil;
            s.incomplete += rd.status.incomplete().count();
        }
        if let Some(d) = &r.ins.dtcs {
            s.stored += d.stored.len();
            s.pending += d.pending.len();
            s.permanent += d.permanent.len();
        }
    }
    s
}

pub fn state_text(s: MonitorState) -> &'static str {
    match s {
        MonitorState::Complete => "complete",
        MonitorState::Incomplete => "incomplete",
        MonitorState::NotSupported => "n/a",
    }
}

/// A plain-text report of `rs` (for the clipboard). The VIN is left out, so
/// a pasted report does not identify the car.
pub fn report_text(rs: &[ModuleResult]) -> String {
    let s = summary(rs);
    let mut o = String::new();
    let _ = writeln!(o, "OBD inspection");
    let _ = writeln!(
        o,
        "{} module(s) answered; MIL {}; {} stored, {} pending, {} permanent code(s); {} monitor(s) incomplete",
        s.modules,
        if s.mil_on { "ON" } else { "off" },
        s.stored,
        s.pending,
        s.permanent,
        s.incomplete
    );
    for r in rs {
        let _ = writeln!(o, "\n== {} ==", r.module.label);
        let i = &r.ins;
        if !i.responded {
            let _ = writeln!(o, "no answer");
            continue;
        }
        if let Some(rd) = &i.readiness {
            let _ = writeln!(
                o,
                "MIL {}, {} stored code(s), {:?} ignition",
                if rd.status.mil { "ON" } else { "off" },
                rd.status.dtc_count,
                rd.status.ignition
            );
            for m in &rd.status.monitors {
                let this = rd
                    .this_cycle
                    .as_ref()
                    .and_then(|c| c.monitors.iter().find(|x| x.key == m.key))
                    .map(|x| format!(" (this drive cycle: {})", state_text(x.state)))
                    .unwrap_or_default();
                let _ = writeln!(o, "  {:<28} {}{this}", m.name, state_text(m.state));
            }
        }
        if let Some(d) = &i.dtcs {
            for (what, list) in [
                ("stored", &d.stored),
                ("pending", &d.pending),
                ("permanent", &d.permanent),
            ] {
                match d.errors.get(what) {
                    Some(e) => {
                        let _ = writeln!(o, "{what} codes: not read ({e})");
                    }
                    None if list.is_empty() => {
                        let _ = writeln!(o, "{what} codes: none");
                    }
                    None => {
                        let _ = writeln!(o, "{what} codes:");
                        for c in list {
                            let _ = writeln!(o, "  {} {}", c.code, c.description);
                        }
                    }
                }
            }
        }
        if let Some(f) = &i.freeze_frame {
            if f.stored {
                let by = f.dtc.as_ref().map(|d| d.code.as_str()).unwrap_or("?");
                let _ = writeln!(o, "freeze frame (stored by {by}):");
                for v in &f.values {
                    let val = v
                        .value
                        .as_ref()
                        .and_then(|x| x.first())
                        .map(|x| format!("{x}"))
                        .unwrap_or_else(|| v.raw.clone());
                    let _ = writeln!(o, "  {:<36} {val} {}", v.name, v.unit);
                }
            } else {
                let _ = writeln!(o, "freeze frame: none stored");
            }
        }
        if let Some(v) = &i.info {
            if let Some(n) = &v.ecu_name {
                let _ = writeln!(o, "ECU name: {n}");
            }
            for (k, c) in v.calids.iter().enumerate() {
                let cvn = v.cvns.get(k).map(String::as_str).unwrap_or("-");
                let _ = writeln!(o, "CALID {c}  CVN {cvn}");
            }
        }
        for (k, e) in &i.errors {
            let _ = writeln!(o, "{k}: {e}");
        }
    }
    o
}
