//! **RomRaider logger definitions** → OpenTune logger definitions, and back.
//!
//! [`import_romraider_logger`] reads a RomRaider `logger.xml` (the open format
//! described by RomRaider's `logger.dtd`: `<protocols><protocol id="SSM">` with
//! `<parameters>`, `<switches>`, `<ecuparams>`, `<depends>` and
//! `<conversions>`) and produces one checked [`LogDef`] for one protocol:
//! its standard parameters and switches plus, with
//! [`RomRaiderImportOptions::ecu`], the `<ecuparam>` addresses of that ECU
//! (ROM) id. Every parameter gets a [`ParamStatus`]:
//!
//! * **linear** / **expression** — a scaling OpenTune evaluates exactly
//!   (`x*0.25`, `(x-128)*100/128`, `14.7/(1+x)`);
//! * **calculated** — computed from other imported parameters each round
//!   (`(P12*60)/P8`, `[P7:psi]-[P24:psi]`);
//! * **switch** — a single bit; **text** — an identification string;
//! * **refused** — kept in the file but **disabled**, with the reason: an
//!   expression outside the safe grammar, a reference to a parameter that is
//!   not imported (often a per-ECU `<ecuparam>`: pass the ECU id), a unit
//!   conversion the referenced parameter does not have, no address, or an
//!   address outside the protocol's range. A refused parameter never logs a
//!   number: its conversion is marked unusable, so a forced read shows the raw
//!   value with a note.
//!
//! Alternative unit conversions OpenTune cannot evaluate are dropped from the
//! parameter (listed in its detail). The result passes [`LogDef::validate`].
//!
//! [`export_romraider_logger`] writes the representable subset of a
//! [`LogDef`] back as `logger.xml` (memory, PID and DID addressing, switches,
//! per-ECU parameters, `<depends>` and conversions), listing what it left
//! out. Importing an export reproduces the definition.
//!
//! This is an independent implementation of the published file format; no
//! RomRaider code is used. RomRaider's own definition files carry no
//! redistribution licence, so none ship with OpenTune: import the file you
//! have.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use serde::Serialize;

use crate::derived;
use crate::expr::LogExpr;
use crate::model::{
    Addressing, Conversion, ConversionFit, EcuGroup, LogDef, LogParam, ProtocolGroup, ProtocolKind,
    SourceFormat, ValueKind,
};
use opentune_core::ecudef::{DataType, Endian};

/// The highest address a 3-byte SSM `A8` read can carry.
const SSM_MAX_ADDRESS: u32 = 0x00FF_FFFF;

/// What to import from a `logger.xml`.
#[derive(Debug, Clone, Default)]
pub struct RomRaiderImportOptions {
    /// Protocol id (`SSM`, `OBD`, …; case-insensitive). Default: the first
    /// protocol in the file.
    pub protocol: Option<String>,
    /// ECU (ROM) id whose `<ecuparam>` addresses to include, e.g.
    /// `2F12785206`. Default: none (standard parameters only).
    pub ecu: Option<String>,
}

/// How one imported parameter will log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamStatus {
    /// Read and scaled linearly (or logged raw).
    Linear,
    /// Read and scaled by an expression in the safe grammar.
    Expression,
    /// Computed from other imported parameters.
    Calculated,
    /// A single bit (`<switch>` or `<address bit>`).
    Switch,
    /// Decoded as text / BCD / hex.
    Text,
    /// Disabled: cannot be logged correctly (see the detail).
    Refused,
}

impl ParamStatus {
    /// Short name, as in the report.
    pub fn as_str(self) -> &'static str {
        match self {
            ParamStatus::Linear => "linear",
            ParamStatus::Expression => "expression",
            ParamStatus::Calculated => "calculated",
            ParamStatus::Switch => "switch",
            ParamStatus::Text => "text",
            ParamStatus::Refused => "refused",
        }
    }
}

/// One parameter's import outcome.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ImportedParam {
    /// Parameter id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// How it will log.
    pub status: ParamStatus,
    /// Why it was refused, what it is calculated from, or which conversions
    /// were dropped (empty when there is nothing to say).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

/// The result of [`import_romraider_logger`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RomRaiderImport {
    /// The OpenTune logger definition (one protocol).
    pub def: LogDef,
    /// The imported protocol id.
    pub protocol: String,
    /// The ECU id whose `<ecuparam>`s were included, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ecu: Option<String>,
    /// One entry per imported parameter, in definition order.
    pub params: Vec<ImportedParam>,
    /// File-level notes: other protocols, per-ECU parameters left out,
    /// constructs not modeled.
    pub notes: Vec<String>,
}

impl RomRaiderImport {
    /// Number of parameters with `status`.
    pub fn count(&self, status: ParamStatus) -> usize {
        self.params.iter().filter(|p| p.status == status).count()
    }
}

/// Import a RomRaider `logger.xml`. Errors when the document is not a
/// RomRaider `<logger>`, the protocol or ECU id is not in it, or the protocol
/// has no OpenTune transport (DS2, Consult).
pub fn import_romraider_logger(
    xml: &str,
    opts: &RomRaiderImportOptions,
) -> Result<RomRaiderImport, String> {
    let src = crate::parse(xml).map_err(|e| e.to_string())?;
    if src.format != SourceFormat::RomRaiderLogger {
        return Err(
            "not a RomRaider <logger> document (an EcuFlash <ecus> logprotocols file \
                    is read directly by `log --def`)"
                .into(),
        );
    }
    let ids: Vec<&str> = src.protocols.iter().map(|p| p.id.as_str()).collect();
    let proto = match &opts.protocol {
        Some(want) => src
            .protocols
            .iter()
            .find(|p| p.id.eq_ignore_ascii_case(want))
            .ok_or_else(|| {
                format!(
                    "protocol '{want}' is not in the file (it has: {})",
                    ids.join(", ")
                )
            })?,
        None => src
            .protocols
            .first()
            .ok_or("the file defines no protocol")?,
    };
    if let ProtocolKind::Other { id } = &proto.kind {
        return Err(format!(
            "protocol '{id}' cannot be imported: OpenTune has no live transport for it and does \
             not model its addressing (supported: SSM, OBD)"
        ));
    }

    let mut notes = Vec::new();
    let others: Vec<&str> = ids.iter().copied().filter(|i| *i != proto.id).collect();
    if !others.is_empty() {
        notes.push(format!(
            "other protocol(s) not imported: {} (choose with --protocol)",
            others.join(", ")
        ));
    }

    // Standard parameters + the chosen ECU's per-ECU parameters.
    let mut std_params: Vec<LogParam> = Vec::new();
    let mut ecu_params: Vec<LogParam> = Vec::new();
    let mut ecu_ids: HashSet<&str> = HashSet::new();
    let mut other_ecu_param_ids: HashSet<&str> = HashSet::new();
    for g in &proto.ecus {
        match &g.id {
            None => std_params.extend(g.params.iter().cloned()),
            Some(gid) => {
                let ids_here: Vec<&str> = gid.split(',').map(str::trim).collect();
                ecu_ids.extend(ids_here.iter().copied());
                let wanted = opts
                    .ecu
                    .as_deref()
                    .is_some_and(|e| ids_here.iter().any(|i| i.eq_ignore_ascii_case(e)));
                if wanted {
                    for p in &g.params {
                        if ecu_params.iter().any(|q| q.id == p.id) {
                            notes.push(format!(
                                "ecuparam '{}' lists the ECU twice; the first address is used",
                                p.id
                            ));
                        } else {
                            ecu_params.push(p.clone());
                        }
                    }
                } else {
                    other_ecu_param_ids.extend(g.params.iter().map(|p| p.id.as_str()));
                }
            }
        }
    }
    if let Some(e) = &opts.ecu {
        if ecu_params.is_empty() {
            return Err(format!(
                "ECU id '{e}' has no <ecuparam> in protocol '{}' ({} ECU id(s) are defined)",
                proto.id,
                ecu_ids.len()
            ));
        }
    } else if !ecu_ids.is_empty() {
        let n = other_ecu_param_ids.len();
        notes.push(format!(
            "{n} per-ECU parameter(s) across {} ECU id(s) not imported (pass the ECU/ROM id \
             with --ecu)",
            ecu_ids.len()
        ));
    }
    for w in &src.warnings {
        notes.push(w.clone());
    }

    if proto.kind == ProtocolKind::Obd2Mode01 && xml.contains("ecubyteindex") {
        notes.push(
            "OBD: the supported-PID capability bits (ecubyteindex/ecubit) are not used; a PID \
             the vehicle lacks answers with a negative response, noted per sample"
                .into(),
        );
    }

    // Classify, then refuse calculated parameters whose inputs are missing,
    // to a fixed point.
    let mut all: Vec<(LogParam, bool)> = std_params
        .into_iter()
        .map(|p| (p, false))
        .chain(ecu_params.into_iter().map(|p| (p, true)))
        .collect();
    let mut report: Vec<(ParamStatus, String)> = Vec::with_capacity(all.len());
    for (p, _) in &mut all {
        report.push(classify_param(p, &proto.kind));
    }
    loop {
        let usable: HashMap<String, Vec<String>> = all
            .iter()
            .zip(&report)
            .filter(|(_, (st, _))| *st != ParamStatus::Refused)
            .map(|((p, _), _)| {
                (
                    p.id.clone(),
                    p.conversions.iter().map(|c| c.units.clone()).collect(),
                )
            })
            .collect();
        let mut changed = false;
        for ((p, _), entry) in all.iter_mut().zip(report.iter_mut()) {
            if entry.0 != ParamStatus::Calculated {
                continue;
            }
            if let Some((why, detail)) = missing_input(p, &usable, &other_ecu_param_ids) {
                refuse(p, &why);
                *entry = (ParamStatus::Refused, detail);
                changed = true;
            }
        }
        if !changed {
            // Calculated parameters that do not bottom out in read ones are
            // in (or behind) a cycle.
            let mut grounded: HashSet<String> = all
                .iter()
                .zip(&report)
                .filter(|(_, (st, _))| {
                    !matches!(st, ParamStatus::Refused | ParamStatus::Calculated)
                })
                .map(|((p, _), _)| p.id.clone())
                .collect();
            loop {
                let before = grounded.len();
                for ((p, _), (st, _)) in all.iter().zip(&report) {
                    if *st == ParamStatus::Calculated
                        && derived::references(p).iter().all(|r| grounded.contains(r))
                    {
                        grounded.insert(p.id.clone());
                    }
                }
                if grounded.len() == before {
                    break;
                }
            }
            for ((p, _), entry) in all.iter_mut().zip(report.iter_mut()) {
                if entry.0 == ParamStatus::Calculated && !grounded.contains(&p.id) {
                    let why = "circular dependency between calculated parameters".to_string();
                    refuse(p, &why);
                    *entry = (ParamStatus::Refused, why);
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }

    let mut groups = Vec::new();
    let (std_part, ecu_part): (Vec<_>, Vec<_>) = all.iter().partition(|(_, per_ecu)| !per_ecu);
    if !std_part.is_empty() {
        groups.push(EcuGroup {
            id: None,
            name: None,
            ecu_type: None,
            params: std_part.into_iter().map(|(p, _)| p.clone()).collect(),
        });
    }
    if !ecu_part.is_empty() {
        groups.push(EcuGroup {
            id: opts.ecu.clone(),
            name: None,
            ecu_type: None,
            params: ecu_part.into_iter().map(|(p, _)| p.clone()).collect(),
        });
    }
    let params: Vec<ImportedParam> = all
        .iter()
        .zip(report)
        .map(|((p, _), (status, detail))| ImportedParam {
            id: p.id.clone(),
            name: p.name.clone(),
            status,
            detail,
        })
        .collect();

    let def = LogDef {
        format: SourceFormat::RomRaiderLogger,
        version: src.version.clone(),
        convert_factors: Vec::new(),
        protocols: vec![ProtocolGroup {
            id: proto.id.clone(),
            kind: proto.kind.clone(),
            baud: proto.baud,
            ecus: groups,
        }],
        warnings: Vec::new(),
    };
    let errs = def.validate();
    if !errs.is_empty() {
        return Err(format!(
            "the imported definition does not validate (please report): {}",
            errs.join("; ")
        ));
    }
    Ok(RomRaiderImport {
        def,
        protocol: proto.id.clone(),
        ecu: opts.ecu.clone(),
        params,
        notes,
    })
}

/// Disable `p` and mark its conversions unusable with `why`.
fn refuse(p: &mut LogParam, why: &str) {
    p.enabled = Some(false);
    let fit = ConversionFit::NonLinear {
        reason: format!("refused on import: {why}"),
    };
    if let Some(c) = p.conversion.as_mut() {
        c.fit = fit.clone();
    }
    if let Some(c) = p.conversions.first_mut() {
        c.fit = fit;
    }
}

/// First classification of one parameter (may refuse it; drops alternative
/// conversions OpenTune cannot evaluate).
fn classify_param(p: &mut LogParam, kind: &ProtocolKind) -> (ParamStatus, String) {
    let mut detail = Vec::new();
    // Alternative conversions that cannot be evaluated are dropped.
    if p.conversions.len() > 1 {
        let first = p.conversions[0].clone();
        let mut kept = vec![first];
        for c in p.conversions.drain(..).skip(1) {
            if matches!(c.fit, ConversionFit::NonLinear { .. }) {
                detail.push(format!(
                    "conversion '{}' dropped ({})",
                    c.units,
                    reason_of(&c)
                ));
            } else {
                kept.push(c);
            }
        }
        p.conversions = kept;
    }
    let refused = |p: &mut LogParam, why: String| {
        refuse(p, &why);
        (ParamStatus::Refused, why)
    };
    let primary = p.conversion.clone();
    if let Some(c) = &primary {
        if let ConversionFit::NonLinear { .. } = c.fit {
            return refused(p, format!("conversion '{}': {}", c.expr, reason_of(c)));
        }
    }
    match &p.addressing {
        Addressing::Unknown if derived::is_calculated(p) => {
            if !derived::is_computable(p) {
                return refused(
                    p,
                    "calculated, but its expression also uses x and it has no address".into(),
                );
            }
        }
        Addressing::Unknown if !p.depends_on.is_empty() => {
            return refused(
                p,
                format!(
                    "depends on {} but its conversion does not reference them, so it cannot be \
                     calculated",
                    p.depends_on.join(", ")
                ),
            );
        }
        Addressing::Unknown => return refused(p, "no usable address".into()),
        Addressing::Memory {
            address, length, ..
        } if *kind == ProtocolKind::SubaruSsm
            && u64::from(*address) + u64::from(*length).max(1) - 1 > u64::from(SSM_MAX_ADDRESS) =>
        {
            return refused(
                p,
                format!("address 0x{address:X} is past the 3-byte SSM range"),
            );
        }
        _ => {}
    }
    let status = if !p.value_kind.is_number() {
        ParamStatus::Text
    } else if derived::is_calculated(p) {
        detail.insert(0, format!("from {}", derived::references(p).join(", ")));
        ParamStatus::Calculated
    } else if matches!(p.addressing, Addressing::Memory { bit: Some(_), .. }) {
        ParamStatus::Switch
    } else if matches!(
        primary.as_ref().map(|c| &c.fit),
        Some(ConversionFit::Expression { .. })
    ) {
        ParamStatus::Expression
    } else {
        ParamStatus::Linear
    };
    (status, detail.join("; "))
}

fn reason_of(c: &Conversion) -> String {
    match &c.fit {
        ConversionFit::NonLinear { reason } => reason.clone(),
        _ => String::new(),
    }
}

/// Why a calculated parameter cannot be computed from `usable` parameters
/// (id → conversion units), if it cannot: the reason stored in the
/// definition, and the report detail (which may add a hint from the source
/// file, such as the `--ecu` that would supply a per-ECU input).
fn missing_input(
    p: &LogParam,
    usable: &HashMap<String, Vec<String>>,
    other_ecu: &HashSet<&str>,
) -> Option<(String, String)> {
    let expr = LogExpr::parse(&p.conversion.as_ref()?.expr).ok()?;
    for r in expr.refs() {
        let Some(units) = usable.get(&r.id) else {
            let why = format!("depends on {}, which is not imported or was refused", r.id);
            let detail = if other_ecu.contains(r.id.as_str()) {
                format!(
                    "depends on {}, a per-ECU parameter not imported (pass its ECU id with --ecu)",
                    r.id
                )
            } else {
                why.clone()
            };
            return Some((why, detail));
        };
        if let Some(u) = &r.units {
            if !units.iter().any(|have| have.eq_ignore_ascii_case(u)) {
                let why = format!(
                    "depends on {} in '{u}', but {} has no usable '{u}' conversion",
                    r.id, r.id
                );
                return Some((why.clone(), why));
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

/// Escape an XML attribute or text value.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            '\t' => out.push_str("&#9;"),
            c => out.push(c),
        }
    }
    out
}

fn storage_name(t: DataType) -> &'static str {
    match t {
        DataType::U8 => "uint8",
        DataType::S8 => "int8",
        DataType::U16 => "uint16",
        DataType::S16 => "int16",
        DataType::U32 => "uint32",
        DataType::S32 => "int32",
        DataType::F32 => "float",
    }
}

/// `ecubyteindex`/`ecubit`/`target`/`enabled` attributes of a parameter.
fn common_attrs(p: &LogParam) -> String {
    let mut a = String::new();
    if let Some(s) = p.ssm {
        if let Some(v) = s.ecu_byte_index {
            let _ = write!(a, " ecubyteindex=\"{v}\"");
        }
        if let Some(v) = s.ecu_bit {
            let _ = write!(a, " ecubit=\"{v}\"");
        }
        if let Some(v) = s.target {
            let _ = write!(a, " target=\"{v}\"");
        }
    }
    if let Some(e) = p.enabled {
        let _ = write!(a, " enabled=\"{e}\"");
    }
    a
}

fn write_conversions(out: &mut String, p: &LogParam, indent: &str) {
    if p.conversions.is_empty() {
        return;
    }
    let _ = writeln!(out, "{indent}<conversions>");
    for (n, c) in p.conversions.iter().enumerate() {
        let _ = write!(
            out,
            "{indent}  <conversion units=\"{}\" expr=\"{}\" format=\"{}\"",
            esc(&c.units),
            esc(&crate::convert::denormalize_equation(&c.expr)),
            esc(c.format.as_deref().unwrap_or("0")),
        );
        if n == 0 {
            match (p.value_kind, p.storagetype) {
                (ValueKind::Number, Some(t)) => {
                    let _ = write!(out, " storagetype=\"{}\"", storage_name(t));
                }
                (ValueKind::Number, None) => {}
                (k, _) => {
                    let name = if k == ValueKind::Text {
                        "string"
                    } else {
                        k.as_str()
                    };
                    let _ = write!(out, " storagetype=\"{name}\"");
                }
            }
            if p.endian == Endian::Little {
                out.push_str(" endian=\"little\"");
            }
        }
        for (attr, v) in [
            ("gauge_min", c.gauge_min),
            ("gauge_max", c.gauge_max),
            ("gauge_step", c.gauge_step),
        ] {
            if let Some(v) = v {
                let _ = write!(out, " {attr}=\"{v}\"");
            }
        }
        out.push_str("/>\n");
    }
    let _ = writeln!(out, "{indent}</conversions>");
}

fn address_xml(a: &Addressing) -> Option<String> {
    match a {
        Addressing::Memory {
            address,
            length,
            bit,
        } => {
            let bit = bit.map(|b| format!(" bit=\"{b}\"")).unwrap_or_default();
            Some(format!(
                "<address length=\"{length}\"{bit}>0x{address:06X}</address>"
            ))
        }
        Addressing::ObdPid { pid } => Some(format!("<address>0x{pid:02X}</address>")),
        Addressing::UdsDid { did } => Some(format!("<address>0x{did:04X}</address>")),
        Addressing::Unknown => None,
    }
}

/// A `<switch>` round-trips exactly: one bit, one byte, no conversions.
fn is_switch(p: &LogParam) -> bool {
    matches!(
        p.addressing,
        Addressing::Memory {
            length: 1,
            bit: Some(_),
            ..
        }
    ) && p.conversions.is_empty()
        && p.storagetype == Some(DataType::U8)
        && p.value_kind.is_number()
        && p.depends_on.is_empty()
        && p.enabled.is_none()
}

/// Write the representable subset of `def` as a RomRaider `logger.xml`.
/// Returns the document and one note per thing left out (protocols with no
/// RomRaider form, `<alt>` addresses, EcuFlash unit-conversion groups, …).
pub fn export_romraider_logger(def: &LogDef) -> (String, Vec<String>) {
    let mut notes = Vec::new();
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<!-- Exported by OpenTune (opentune export-romraider-logger). -->\n");
    match &def.version {
        Some(v) => {
            let _ = writeln!(out, "<logger version=\"{}\">", esc(v));
        }
        None => out.push_str("<logger>\n"),
    }
    out.push_str("  <protocols>\n");
    if !def.convert_factors.is_empty() {
        notes.push(format!(
            "{} EcuFlash <convert_factor> unit conversion(s) have no RomRaider form; left out",
            def.convert_factors.len()
        ));
    }
    for proto in &def.protocols {
        let mode = match &proto.kind {
            ProtocolKind::SubaruSsm => None,
            ProtocolKind::Obd2Mode01 if proto.id.to_ascii_uppercase().contains("OBD") => None,
            ProtocolKind::Obd2Mode01 => Some("1"),
            ProtocolKind::UdsMode22 => Some("22"),
            ProtocolKind::EcuMode23 => Some("23"),
            ProtocolKind::Other { id } => {
                notes.push(format!(
                    "protocol '{id}' has no exportable addressing; left out"
                ));
                continue;
            }
        };
        if mode.is_some() {
            notes.push(format!(
                "protocol '{}': written with mode=\"{}\" (an OpenTune/FastECU extension RomRaider \
                 itself does not read)",
                proto.id,
                mode.unwrap_or_default()
            ));
        }
        let baud = proto
            .baud
            .map(|b| format!(" baud=\"{b}\""))
            .unwrap_or_default();
        let _ = writeln!(out, "    <protocol id=\"{}\"{baud}>", esc(&proto.id));
        let mut params = String::new();
        let mut switches = String::new();
        // Per-ECU parameters: id -> (first param, [(ecu id, address)]).
        let mut ecuparams: Vec<(&LogParam, Vec<(String, String)>)> = Vec::new();
        for g in &proto.ecus {
            for p in &g.params {
                if !p.alternates.is_empty() {
                    notes.push(format!(
                        "{}: {} <alt> address(es) left out",
                        p.id,
                        p.alternates.len()
                    ));
                }
                if p.convert_group.is_some() {
                    notes.push(format!("{}: unit-conversion group left out", p.id));
                }
                if let Some(gid) = &g.id {
                    if !matches!(p.addressing, Addressing::Memory { .. }) {
                        notes.push(format!(
                            "{}: per-ECU parameter without a memory address; left out",
                            p.id
                        ));
                        continue;
                    }
                    let xml = address_xml(&p.addressing).unwrap_or_default();
                    match ecuparams.iter_mut().find(|(q, _)| q.id == p.id) {
                        Some((_, ecus)) => ecus.push((gid.clone(), xml)),
                        None => ecuparams.push((p, vec![(gid.clone(), xml)])),
                    }
                    continue;
                }
                if is_switch(p) {
                    if let Addressing::Memory {
                        address,
                        bit: Some(b),
                        ..
                    } = p.addressing
                    {
                        let _ = writeln!(
                            switches,
                            "        <switch id=\"{}\" name=\"{}\" desc=\"{}\" byte=\"0x{address:06X}\" \
                             bit=\"{b}\"{}/>",
                            esc(&p.id),
                            esc(&p.name),
                            esc(&p.description),
                            common_attrs(p)
                        );
                    }
                    continue;
                }
                let _ = writeln!(
                    params,
                    "        <parameter id=\"{}\" name=\"{}\" desc=\"{}\"{}>",
                    esc(&p.id),
                    esc(&p.name),
                    esc(&p.description),
                    common_attrs(p)
                );
                if !p.depends_on.is_empty() {
                    params.push_str("          <depends>\n");
                    for d in &p.depends_on {
                        let _ = writeln!(params, "            <ref parameter=\"{}\"/>", esc(d));
                    }
                    params.push_str("          </depends>\n");
                }
                if let Some(a) = address_xml(&p.addressing) {
                    let _ = writeln!(params, "          {a}");
                }
                write_conversions(&mut params, p, "          ");
                params.push_str("        </parameter>\n");
            }
        }
        if !params.is_empty() {
            match mode {
                Some(m) => {
                    let _ = writeln!(out, "      <parameters mode=\"{m}\">");
                }
                None => out.push_str("      <parameters>\n"),
            }
            out.push_str(&params);
            out.push_str("      </parameters>\n");
        }
        if !switches.is_empty() {
            out.push_str("      <switches>\n");
            out.push_str(&switches);
            out.push_str("      </switches>\n");
        }
        if !ecuparams.is_empty() {
            out.push_str("      <ecuparams>\n");
            for (p, ecus) in ecuparams {
                let _ = writeln!(
                    out,
                    "        <ecuparam id=\"{}\" name=\"{}\" desc=\"{}\"{}>",
                    esc(&p.id),
                    esc(&p.name),
                    esc(&p.description),
                    common_attrs(p)
                );
                for (gid, addr) in ecus {
                    let _ = writeln!(out, "          <ecu id=\"{}\">{addr}</ecu>", esc(&gid));
                }
                write_conversions(&mut out, p, "          ");
                out.push_str("        </ecuparam>\n");
            }
            out.push_str("      </ecuparams>\n");
        }
        out.push_str("    </protocol>\n");
    }
    out.push_str("  </protocols>\n</logger>\n");
    (out, notes)
}
