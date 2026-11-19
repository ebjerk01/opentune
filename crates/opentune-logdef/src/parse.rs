//! Parsing of both RomRaider-style `logger.xml` dialects into the shared
//! [`LogDef`] model.
//!
//! The entry point [`parse`] dispatches on the root element: `<logger>` is the
//! canonical RomRaider format (also emitted by FastECU), `<ecus>` is the
//! EcuFlash / FastECU "logprotocols" format. Unsupported constructs are
//! recorded in [`LogDef::warnings`], never silently dropped.

use roxmltree::{Document, Node};

use crate::convert::{fit_equation, parse_address, storage_type};
use crate::model::{
    Addressing, AltAddress, Conversion, ConversionFit, ConvertFactor, EcuGroup, LogDef, LogParam,
    ProtocolGroup, ProtocolKind, SourceFormat, SsmParamInfo, ValueKind,
};
use crate::{Error, Result};
use opentune_core::ecudef::{DataType, Endian};

/// Parse a RomRaider-style `logger.xml` document from a string.
///
/// Real RomRaider files start with `<!DOCTYPE logger SYSTEM "logger.dtd">`;
/// the declaration is accepted (the external DTD is never fetched, and the
/// XML reader bounds internal entity expansion).
pub fn parse(xml: &str) -> Result<LogDef> {
    let opts = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..roxmltree::ParsingOptions::default()
    };
    let doc = Document::parse_with_options(xml, opts).map_err(|e| Error::Xml(e.to_string()))?;
    let root = doc.root_element();
    match root.tag_name().name() {
        "logger" => parse_logger(root),
        "ecus" => parse_ecus(root),
        other => Err(Error::UnknownRoot(other.to_string())),
    }
}

/// Classify a protocol id (+ optional `mode`) into a [`ProtocolKind`].
fn classify(proto_id: &str, mode: Option<&str>, warnings: &mut Vec<String>) -> ProtocolKind {
    if proto_id.eq_ignore_ascii_case("SSM") {
        return ProtocolKind::SubaruSsm;
    }
    if unmodeled_protocol(proto_id) {
        return ProtocolKind::Other {
            id: proto_id.to_string(),
        };
    }
    match mode.map(str::trim) {
        Some("1") | Some("01") => ProtocolKind::Obd2Mode01,
        Some("22") => ProtocolKind::UdsMode22,
        Some("23") => ProtocolKind::EcuMode23,
        Some(m) if !m.is_empty() => {
            warnings.push(format!(
                "protocol '{proto_id}': unrecognised mode '{m}', treated as direct memory read"
            ));
            ProtocolKind::Other {
                id: proto_id.to_string(),
            }
        }
        _ => {
            if proto_id.to_ascii_uppercase().contains("OBD") {
                ProtocolKind::Obd2Mode01
            } else {
                // Direct address-read protocols (MUT_DMA, CDBG, …) read ECU RAM.
                ProtocolKind::EcuMode23
            }
        }
    }
}

/// RomRaider protocols whose addressing OpenTune does not model: BMW DS2
/// (`group`/`subgroup` telegrams) and Nissan Consult (NCS). Their parameters
/// keep [`Addressing::Unknown`] so nothing reads them as plain memory.
pub(crate) fn unmodeled_protocol(id: &str) -> bool {
    ["DS2", "NCS"].iter().any(|p| id.eq_ignore_ascii_case(p))
}

/// Build an [`Addressing`] from a protocol kind and a raw address + length/bit.
fn make_addressing(
    kind: &ProtocolKind,
    addr: Option<u32>,
    length: Option<u8>,
    bit: Option<u8>,
    storagetype: Option<DataType>,
    ctx: &str,
    warnings: &mut Vec<String>,
) -> Addressing {
    if matches!(kind, ProtocolKind::Other { id } if unmodeled_protocol(id)) {
        // Reported once per protocol by the caller.
        return Addressing::Unknown;
    }
    let Some(addr) = addr else {
        warnings.push(format!("{ctx}: missing or unparsable address"));
        return Addressing::Unknown;
    };
    if bit.is_some() && matches!(kind, ProtocolKind::Obd2Mode01 | ProtocolKind::UdsMode22) {
        warnings.push(format!(
            "{ctx}: a bit inside an OBD PID / UDS DID is not modeled; parameter left unaddressed"
        ));
        return Addressing::Unknown;
    }
    match kind {
        ProtocolKind::Obd2Mode01 => Addressing::ObdPid { pid: addr as u8 },
        ProtocolKind::UdsMode22 => Addressing::UdsDid { did: addr as u16 },
        _ => {
            let length = length
                .or_else(|| storagetype.map(|t| t.byte_len() as u8))
                .unwrap_or(1);
            Addressing::Memory {
                address: addr,
                length,
                bit,
            }
        }
    }
}

/// Find `GetLogParam("…")` dependency references inside an equation.
fn extract_dependencies(expr: &str) -> Vec<String> {
    let mut deps = Vec::new();
    let needle = "GetLogParam";
    let mut rest = expr;
    while let Some(pos) = rest.find(needle) {
        rest = &rest[pos + needle.len()..];
        // Expect `(` then a quoted string.
        if let Some(open) = rest.find('"') {
            if let Some(close_rel) = rest[open + 1..].find('"') {
                let name = &rest[open + 1..open + 1 + close_rel];
                if !name.is_empty() && !deps.iter().any(|d| d == name) {
                    deps.push(name.to_string());
                }
                rest = &rest[open + 1 + close_rel + 1..];
            }
        }
    }
    deps
}

/// Parse an `endian` attribute, defaulting to big-endian.
fn parse_endian(s: Option<&str>) -> Endian {
    match s.map(|v| v.trim().to_ascii_lowercase()) {
        Some(v) if v == "little" || v == "intel" => Endian::Little,
        _ => Endian::Big,
    }
}

/// Parse an optional numeric attribute.
fn parse_f64(s: Option<&str>) -> Option<f64> {
    s.and_then(|v| v.trim().parse::<f64>().ok())
}

/// Build a [`Conversion`] from a `<conversion>` element.
fn conversion_from_node(node: Node) -> (Conversion, Option<DataType>, Option<Endian>, Vec<String>) {
    let expr = node.attribute("expr").unwrap_or("x");
    let (expr, fit) = fit_equation(expr);
    let deps = match &fit {
        ConversionFit::Expression { refs } => refs.clone(),
        _ => extract_dependencies(node.attribute("expr").unwrap_or("")),
    };
    let st = node.attribute("storagetype").and_then(storage_type);
    let endian = node.attribute("endian").map(|e| parse_endian(Some(e)));
    let conv = Conversion {
        units: node.attribute("units").unwrap_or("").to_string(),
        expr,
        format: node.attribute("format").map(str::to_string),
        fit,
        gauge_min: parse_f64(node.attribute("gauge_min")),
        gauge_max: parse_f64(node.attribute("gauge_max")),
        gauge_step: parse_f64(node.attribute("gauge_step")),
    };
    (conv, st, endian, deps)
}

// ---------------------------------------------------------------------------
// Format A: canonical RomRaider <logger>
// ---------------------------------------------------------------------------

fn parse_logger(root: Node) -> Result<LogDef> {
    let mut warnings = Vec::new();
    let mut protocols = Vec::new();

    let Some(protocols_node) = root.children().find(|n| n.has_tag_name("protocols")) else {
        return Err(Error::Empty);
    };

    for proto in protocols_node
        .children()
        .filter(|n| n.has_tag_name("protocol"))
    {
        let id = proto.attribute("id").unwrap_or("").to_string();
        let baud = proto
            .attribute("baud")
            .and_then(|b| b.trim().parse::<u32>().ok());
        let mut std_params = Vec::new();
        let mut ecu_groups: Vec<EcuGroup> = Vec::new();
        if unmodeled_protocol(&id) {
            warnings.push(format!(
                "protocol '{id}': its addressing (DS2 group/subgroup telegrams, Consult) is not \
                 modeled and OpenTune has no live transport for it; its parameters are kept \
                 unaddressed"
            ));
        }

        // Standard parameters: <parameters mode?>.
        for params in proto.children().filter(|n| n.has_tag_name("parameters")) {
            let mode = params.attribute("mode");
            let kind = classify(&id, mode, &mut warnings);
            for p in params.children().filter(|n| n.has_tag_name("parameter")) {
                if let Some(param) =
                    logger_parameter(p, &kind, &mut warnings).map(|x| with_kind(x, p))
                {
                    std_params.push(param);
                }
            }
        }

        // Switches: single-bit flags addressed in memory.
        for switches in proto.children().filter(|n| n.has_tag_name("switches")) {
            let kind = classify(&id, None, &mut warnings);
            for sw in switches.children().filter(|n| n.has_tag_name("switch")) {
                if let Some(param) =
                    logger_switch(sw, &kind, &mut warnings).map(|x| with_ssm(x, sw))
                {
                    std_params.push(param);
                }
            }
        }

        if proto.children().any(|n| n.has_tag_name("dtcodes")) {
            warnings.push(format!(
                "protocol '{id}': <dtcodes> are diagnostic trouble codes, not modeled as live-data parameters in v1"
            ));
        }

        // Per-ECU parameters: <ecuparams><ecuparam><ecu><address> + shared <conversions>.
        for ecuparams in proto.children().filter(|n| n.has_tag_name("ecuparams")) {
            for ep in ecuparams.children().filter(|n| n.has_tag_name("ecuparam")) {
                logger_ecuparam(ep, &id, &mut ecu_groups, &mut warnings);
            }
        }

        let mut ecus = Vec::new();
        if !std_params.is_empty() {
            ecus.push(EcuGroup {
                id: None,
                name: None,
                ecu_type: None,
                params: std_params,
            });
        }
        ecus.extend(ecu_groups);

        protocols.push(ProtocolGroup {
            kind: classify(&id, None, &mut warnings),
            id,
            baud,
            ecus,
        });
    }

    Ok(LogDef {
        format: SourceFormat::RomRaiderLogger,
        version: root.attribute("version").map(str::to_string),
        convert_factors: Vec::new(),
        protocols,
        warnings,
    })
}

/// Parse one standard `<parameter>` (address- or depends-based).
fn logger_parameter(p: Node, kind: &ProtocolKind, warnings: &mut Vec<String>) -> Option<LogParam> {
    let id = p.attribute("id")?.to_string();
    let name = p.attribute("name").unwrap_or(&id).to_string();
    let desc = p.attribute("desc").unwrap_or("").to_string();
    let enabled = p
        .attribute("enabled")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"));

    let mut depends_on = Vec::new();
    let mut addressing = Addressing::Unknown;

    // <depends><ref parameter=.. ecuparam=..>: flag, do not resolve.
    if let Some(depends) = p.children().find(|n| n.has_tag_name("depends")) {
        for r in depends.children().filter(|n| n.has_tag_name("ref")) {
            if let Some(dep) = r.attribute("parameter").or_else(|| r.attribute("ecuparam")) {
                depends_on.push(dep.to_string());
            }
        }
    }

    // <address length? bit?>HEX, or one <address> per byte.
    let addresses: Vec<Node> = p.children().filter(|n| n.has_tag_name("address")).collect();
    if !addresses.is_empty() {
        let (addr, length, bit) =
            address_fragments(&addresses, &format!("parameter '{id}'"), warnings);
        let (conversions, st, endian) = logger_conversions(p, &mut depends_on, warnings, &id);
        let st = st.or_else(|| obd_width(kind, length));
        addressing = make_addressing(
            kind,
            addr,
            length,
            bit,
            st,
            &format!("parameter '{id}'"),
            warnings,
        );
        return Some(finish_param(
            id,
            name,
            desc,
            kind.clone(),
            addressing,
            st,
            endian,
            conversions,
            depends_on,
            None,
            enabled,
        ));
    }

    // depends-only parameter: no address of its own.
    let (conversions, st, endian) = logger_conversions(p, &mut depends_on, warnings, &id);
    Some(finish_param(
        id,
        name,
        desc,
        kind.clone(),
        addressing,
        st,
        endian,
        conversions,
        depends_on,
        None,
        enabled,
    ))
}

/// Collect `<conversions><conversion>` for a parameter/ecuparam, returning the
/// conversions plus any storagetype/endian they declared and merging equation
/// dependencies into `depends_on`.
fn logger_conversions(
    owner: Node,
    depends_on: &mut Vec<String>,
    warnings: &mut Vec<String>,
    id: &str,
) -> (Vec<Conversion>, Option<DataType>, Option<Endian>) {
    let mut conversions = Vec::new();
    let mut st = None;
    let mut endian = None;
    if let Some(convs) = owner.children().find(|n| n.has_tag_name("conversions")) {
        for c in convs.children().filter(|n| n.has_tag_name("conversion")) {
            let labels = c.children().filter(|n| n.has_tag_name("replace")).count();
            if labels > 0 {
                warnings.push(format!(
                    "parameter '{id}': {labels} <replace> value label(s) in the '{}' conversion \
                     are not modeled; the number is logged",
                    c.attribute("units").unwrap_or("")
                ));
            }
            let (conv, cst, cend, deps) = conversion_from_node(c);
            st = st.or(cst);
            endian = endian.or(cend);
            for d in deps {
                if !depends_on.contains(&d) {
                    depends_on.push(d);
                }
            }
            conversions.push(conv);
        }
    }
    (conversions, st, endian)
}

/// Parse a `<switch>` as a single-bit flag parameter.
fn logger_switch(sw: Node, kind: &ProtocolKind, warnings: &mut Vec<String>) -> Option<LogParam> {
    let id = sw.attribute("id")?.to_string();
    let name = sw.attribute("name").unwrap_or(&id).to_string();
    let desc = sw.attribute("desc").unwrap_or("").to_string();
    let addr = sw.attribute("byte").and_then(parse_address);
    let bit = sw
        .attribute("bit")
        .and_then(|b| b.trim().parse::<u8>().ok());
    let addressing = make_addressing(
        kind,
        addr,
        Some(1),
        bit,
        Some(DataType::U8),
        &format!("switch '{id}'"),
        warnings,
    );
    Some(finish_param(
        id,
        name,
        desc,
        kind.clone(),
        addressing,
        Some(DataType::U8),
        None,
        Vec::new(),
        Vec::new(),
        None,
        None,
    ))
}

/// Parse an `<ecuparam>` into per-ECU parameters.
fn logger_ecuparam(
    ep: Node,
    proto_id: &str,
    groups: &mut Vec<EcuGroup>,
    warnings: &mut Vec<String>,
) {
    let Some(id) = ep.attribute("id").map(str::to_string) else {
        return;
    };
    let name = ep.attribute("name").unwrap_or(&id).to_string();
    let desc = ep.attribute("desc").unwrap_or("").to_string();
    let enabled = ep
        .attribute("enabled")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    let kind = classify(proto_id, None, warnings);
    let mut depends_on = Vec::new();
    let (conversions, st, endian) = logger_conversions(ep, &mut depends_on, warnings, &id);
    if !unmodeled_protocol(proto_id)
        && ["group", "subgroup", "groupsize"]
            .iter()
            .any(|a| ep.attribute(*a).is_some())
    {
        warnings.push(format!(
            "ecuparam '{id}': group/subgroup/groupsize (DS2 telegram) attributes are not modeled"
        ));
    }

    for ecu in ep.children().filter(|n| n.has_tag_name("ecu")) {
        let Some(ecu_id) = ecu.attribute("id").map(str::to_string) else {
            continue;
        };
        let addresses: Vec<Node> = ecu
            .children()
            .filter(|n| n.has_tag_name("address"))
            .collect();
        let ctx = format!("ecuparam '{id}' / ecu '{ecu_id}'");
        let (addr, length, bit) = if addresses.is_empty() {
            (None, None, None)
        } else {
            address_fragments(&addresses, &ctx, warnings)
        };
        let addressing = make_addressing(&kind, addr, length, bit, st, &ctx, warnings);
        let param = with_kind(
            finish_param(
                id.clone(),
                name.clone(),
                desc.clone(),
                kind.clone(),
                addressing,
                st,
                endian,
                conversions.clone(),
                depends_on.clone(),
                None,
                enabled,
            ),
            ep,
        );
        match groups
            .iter_mut()
            .find(|g| g.id.as_deref() == Some(ecu_id.as_str()))
        {
            Some(g) => g.params.push(param),
            None => groups.push(EcuGroup {
                id: Some(ecu_id),
                name: None,
                ecu_type: None,
                params: vec![param],
            }),
        }
    }
}

/// The address, length and bit of a parameter's `<address>` element(s): the
/// first one's `length`/`bit`, or, for the older one-`<address>`-per-byte
/// style, consecutive fragments read as one value of that many bytes. Gappy
/// fragments keep only the first (with a warning).
fn address_fragments(
    addresses: &[Node],
    ctx: &str,
    warnings: &mut Vec<String>,
) -> (Option<u32>, Option<u8>, Option<u8>) {
    let first = addresses[0];
    let addr = first.text().and_then(parse_address);
    let mut length = first
        .attribute("length")
        .and_then(|l| l.trim().parse::<u8>().ok());
    let fragments: Vec<Option<u32>> = addresses
        .iter()
        .map(|a| a.text().and_then(parse_address))
        .collect();
    let contiguous = addresses.len() > 1
        && length.is_none()
        && addresses.len() <= 8
        && fragments.iter().enumerate().all(|(i, a)| {
            matches!((a, addr), (Some(a), Some(b)) if u64::from(*a) == u64::from(b) + i as u64)
        });
    if contiguous {
        length = Some(addresses.len() as u8);
    }
    if addresses.len() > 1 && !contiguous {
        warnings.push(format!(
            "{ctx}: {} multi-byte <address> fragments; only the first is modeled",
            addresses.len()
        ));
    }
    let bit = first
        .attribute("bit")
        .and_then(|b| b.trim().parse::<u8>().ok());
    (addr, length, bit)
}

/// For an OBD-II PID with a declared `length` and no `storagetype`: the
/// unsigned type of that width, so a one-byte value of a multi-value PID is
/// read from its own byte only.
fn obd_width(kind: &ProtocolKind, length: Option<u8>) -> Option<DataType> {
    if *kind != ProtocolKind::Obd2Mode01 {
        return None;
    }
    match length? {
        1 => Some(DataType::U8),
        2 => Some(DataType::U16),
        4 => Some(DataType::U32),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Format B: EcuFlash / FastECU <ecus><logprotocols>
// ---------------------------------------------------------------------------

fn parse_ecus(root: Node) -> Result<LogDef> {
    let mut warnings = Vec::new();
    let mut convert_factors = Vec::new();

    if let Some(tools) = root.children().find(|n| n.has_tag_name("ecu_tools")) {
        for cf in tools
            .children()
            .filter(|n| n.has_tag_name("convert_factor"))
        {
            let raw = cf.attribute("expr").unwrap_or("x");
            let (expr, fit) = fit_equation(raw);
            convert_factors.push(ConvertFactor {
                kind: cf.attribute("type").unwrap_or("").to_string(),
                name: cf.attribute("name").unwrap_or("").to_string(),
                metric: cf.attribute("metric").unwrap_or("").to_string(),
                expr,
                fit,
            });
        }
    }

    let Some(logprotocols) = root.children().find(|n| n.has_tag_name("logprotocols")) else {
        return Err(Error::Empty);
    };

    let mut protocols = Vec::new();
    for lp in logprotocols
        .children()
        .filter(|n| n.has_tag_name("logprotocol"))
    {
        let id = lp.attribute("type").unwrap_or("").to_string();
        let kind = classify(&id, None, &mut warnings);
        let mut ecus = Vec::new();
        for ecu in lp.children().filter(|n| n.has_tag_name("ecu")) {
            let ecu_id = ecu
                .attribute("id")
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let mut params = Vec::new();
            for p in ecu.children().filter(|n| n.has_tag_name("parameter")) {
                if let Some(param) =
                    ecus_parameter(p, &kind, &mut warnings).map(|x| with_kind(x, p))
                {
                    params.push(param);
                }
            }
            ecus.push(EcuGroup {
                id: ecu_id,
                name: ecu.attribute("name").map(str::to_string),
                ecu_type: ecu.attribute("type").map(str::to_string),
                params,
            });
        }
        protocols.push(ProtocolGroup {
            id,
            kind,
            baud: None,
            ecus,
        });
    }

    Ok(LogDef {
        format: SourceFormat::EcuFlashLogProtocols,
        version: root.attribute("version").map(str::to_string),
        convert_factors,
        protocols,
        warnings,
    })
}

/// Parse one Format-B `<parameter offset=… storagetype=… expr=… metric=…>`.
fn ecus_parameter(p: Node, kind: &ProtocolKind, warnings: &mut Vec<String>) -> Option<LogParam> {
    let id = p.attribute("id")?.to_string();
    let name = p.attribute("name").unwrap_or(&id).to_string();
    let desc = p.attribute("desc").unwrap_or("").to_string();
    let st = p.attribute("storagetype").and_then(|s| {
        let parsed = storage_type(s);
        if parsed.is_none() && ValueKind::from_storage(s).is_none() {
            warnings.push(format!("parameter '{id}': unknown storagetype '{s}'"));
        }
        parsed
    });
    let addr = p.attribute("offset").and_then(parse_address);
    let addressing = make_addressing(
        kind,
        addr,
        None,
        None,
        st,
        &format!("parameter '{id}'"),
        warnings,
    );

    let raw_expr = p.attribute("expr").unwrap_or("x");
    let depends_on = extract_dependencies(raw_expr);
    let (expr, fit) = fit_equation(raw_expr);
    let conversion = Conversion {
        units: p.attribute("metric").unwrap_or("").to_string(),
        expr,
        format: None,
        fit,
        gauge_min: None,
        gauge_max: None,
        gauge_step: None,
    };

    // <alt> alternates.
    let mut alternates = Vec::new();
    for alt in p.children().filter(|n| n.has_tag_name("alt")) {
        let Some(alt_id) = alt.attribute("id").map(str::to_string) else {
            continue;
        };
        let alt_addr = alt.attribute("offset").and_then(parse_address);
        let alt_addressing = make_addressing(
            kind,
            alt_addr,
            None,
            None,
            st,
            &format!("alt '{alt_id}'"),
            warnings,
        );
        alternates.push(AltAddress {
            id: alt_id,
            addressing: alt_addressing,
        });
    }

    let convert_group = p.attribute("type").map(str::to_string);
    Some(finish_param_full(
        id,
        name,
        desc,
        kind.clone(),
        addressing,
        st,
        Endian::Big,
        vec![conversion],
        depends_on,
        alternates,
        convert_group,
        None,
    ))
}

// ---------------------------------------------------------------------------
// Shared finishing
// ---------------------------------------------------------------------------

/// Assemble a [`LogParam`] from a conversions list (deriving units/min/max from
/// the primary conversion). `endian` may be `None` (defaults to big).
#[allow(clippy::too_many_arguments)]
fn finish_param(
    id: String,
    name: String,
    desc: String,
    protocol: ProtocolKind,
    addressing: Addressing,
    storagetype: Option<DataType>,
    endian: Option<Endian>,
    conversions: Vec<Conversion>,
    depends_on: Vec<String>,
    convert_group: Option<String>,
    enabled: Option<bool>,
) -> LogParam {
    finish_param_full(
        id,
        name,
        desc,
        protocol,
        addressing,
        storagetype,
        endian.unwrap_or(Endian::Big),
        conversions,
        depends_on,
        Vec::new(),
        convert_group,
        enabled,
    )
}

#[allow(clippy::too_many_arguments)]
fn finish_param_full(
    id: String,
    name: String,
    desc: String,
    protocol: ProtocolKind,
    addressing: Addressing,
    storagetype: Option<DataType>,
    endian: Endian,
    conversions: Vec<Conversion>,
    depends_on: Vec<String>,
    alternates: Vec<AltAddress>,
    convert_group: Option<String>,
    enabled: Option<bool>,
) -> LogParam {
    let primary = conversions.first().cloned();
    let units = primary
        .as_ref()
        .map(|c| c.units.clone())
        .unwrap_or_default();
    let min = primary.as_ref().and_then(|c| c.gauge_min);
    let max = primary.as_ref().and_then(|c| c.gauge_max);
    LogParam {
        id,
        name,
        description: desc,
        units,
        protocol,
        addressing,
        storagetype,
        endian,
        conversion: primary,
        conversions,
        min,
        max,
        alternates,
        depends_on,
        convert_group,
        enabled,
        value_kind: ValueKind::Number,
        ssm: None,
    }
}

/// The text kind a parameter node's `storagetype` (its own, or one of its
/// `<conversion>`s') implies, if any.
fn node_value_kind(n: Node) -> ValueKind {
    n.descendants()
        .filter(|d| d == &n || d.has_tag_name("conversion"))
        .filter_map(|d| d.attribute("storagetype"))
        .find_map(ValueKind::from_storage)
        .unwrap_or_default()
}

/// `p` with the value kind its definition node implies (and, for SSM, its
/// selection metadata).
fn with_kind(mut p: LogParam, n: Node) -> LogParam {
    p.value_kind = node_value_kind(n);
    with_ssm(p, n)
}

/// For an SSM parameter, the RomRaider `ecubyteindex`/`ecubit` capability
/// flag and `target` module from its definition node, when declared.
fn with_ssm(mut p: LogParam, n: Node) -> LogParam {
    if p.protocol != ProtocolKind::SubaruSsm {
        return p;
    }
    let num = |a: &str| n.attribute(a).and_then(|v| v.trim().parse::<u16>().ok());
    let info = SsmParamInfo {
        ecu_byte_index: num("ecubyteindex"),
        ecu_bit: num("ecubit").and_then(|b| u8::try_from(b).ok()),
        target: num("target").and_then(|t| u8::try_from(t).ok()),
    };
    if info != SsmParamInfo::default() {
        p.ssm = Some(info);
    }
    p
}
