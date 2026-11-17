//! # opentune-ecuflash
//!
//! Import **EcuFlash / RomRaider** XML ROM definitions into OpenTune
//! [`MapDef`]s. These are the open, community-maintained definition formats
//! behind EcuFlash and RomRaider — a large library of **Subaru / Mitsubishi**
//! (and other) map definitions. Importing the *format* (not the tools' code)
//! lets OpenTune reuse that library through the same map pipeline as the
//! TunerPro XDF and ASAM A2L importers.
//!
//! ## Two dialects, one output
//!
//! The two tools share a family resemblance but differ in where storage and
//! scaling live, so the importer **auto-detects** the dialect and parses both
//! into the same [`EcuflashImport`] / [`MapDef`] output.
//!
//! * **EcuFlash** — a `<scaling name storagetype endian toexpr units .../>`
//!   carries both the raw→physical equation *and* the storage type; a
//!   `<table type="1D|2D|3D" address scaling="name">` references it, and axis
//!   sub-tables give their element count via `elements`.
//! * **RomRaider** (`ecu_defs.xml`) — the **storage type and endian live on the
//!   `<table>`** (and on each axis sub-table), dimensions are `sizex`/`sizey`,
//!   and the raw→physical equation lives in a child `<scaling expression="..">`
//!   element (variable `x`). A `<scalingbase name expression .../>` defines a
//!   shared scaling that a child `<scaling base="name"/>` references. Addresses
//!   use `storageaddress` (we also accept `address`).
//!
//! Detection: a `<scalingbase>`, a `<scaling expression=…>`, or a `<table>`
//! carrying `storagetype`/`sizex` means RomRaider; otherwise EcuFlash.
//!
//! ## Scaling fidelity
//!
//! A table's cell scaling resolves to the raw→physical equation (`toexpr` for
//! EcuFlash, `expression` for RomRaider), which is fitted to a linear
//! `factor`/`offset` with [`opentune_xdf::math::linear_from_equation`] (the same
//! fitter the XDF importer uses; the variable is normalized to uppercase `X`
//! first). A non-linear equation imports **pre-scaled** (factor 1) with a note,
//! never a silent wrong scale.
//!
//! ## Base-definition inheritance
//!
//! The single-document [`parse_ecuflash`] does **not** resolve inheritance: the
//! base lives in a separate document or another `<rom>`, so it only records the
//! referenced base id and warns. When a base-def store is available, use
//! [`parse_ecuflash_with_bases`] (or the in-memory convenience
//! [`parse_ecuflash_with_base_map`]) instead: it resolves EcuFlash `<include>`
//! and RomRaider `<rom base=>` **recursively** (base-of-base first) and merges
//! **base first, child overrides** by scaling `name`, table `name`, and romid
//! xmlid, and inherits a RomRaider `<table base="name">`'s missing fields from
//! the named base table. Missing bases and inheritance cycles are surfaced as
//! warnings, never looped.
//!
//! ## What it does not do (reported, never silent)
//!
//! * In the single-document [`parse_ecuflash`], an inherited table (no body of
//!   its own) is skipped rather than emitted broken, and the referenced base id
//!   is recorded in [`EcuflashImport::warnings`].
//! * Non-linear equations, unknown `storagetype`, a missing `scaling` /
//!   `scalingbase` reference, or more than one `<rom>` in the file are each
//!   surfaced as a warning rather than dropped quietly.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use opentune_core::ecudef::{AxisDef, DataType, Endian, MapDef, Scaling};
use opentune_xdf::math::{linear_from_equation, LinearFit};
use roxmltree::{Document, Node};
use std::collections::HashMap;

/// Error parsing an EcuFlash/RomRaider document.
#[derive(Debug)]
pub enum Error {
    /// The XML did not parse.
    Xml(String),
    /// No `<rom>` element was found.
    NoRom,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Xml(s) => write!(f, "XML parse error: {s}"),
            Error::NoRom => write!(f, "no <rom> element in document"),
        }
    }
}
impl std::error::Error for Error {}

/// Crate result type.
pub type Result<T> = std::result::Result<T, Error>;

/// Which definition dialect a document is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// EcuFlash: storage + equation on `<scaling>`, axes count via `elements`.
    EcuFlash,
    /// RomRaider: storage on `<table>`, `sizex`/`sizey` dims, `<scalingbase>`.
    RomRaider,
}

/// The `<romid>` identity block of an EcuFlash/RomRaider ROM definition.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RomId {
    /// `xmlid` — the definition's unique id (also the inheritance target).
    pub xmlid: String,
    /// `make` (e.g. `SUBARU`).
    pub make: String,
    /// `model`.
    pub model: String,
    /// `submodel`.
    pub submodel: String,
    /// `market`.
    pub market: String,
    /// `year`.
    pub year: String,
    /// `transmission`.
    pub transmission: String,
    /// `ecuid` — the ECU's reported calibration id.
    pub ecuid: String,
    /// `internalidstring` — the id string stored in the ROM.
    pub internalidstring: String,
    /// `internalidaddress` — where `internalidstring` lives (if numeric).
    pub internalidaddress: Option<u32>,
    /// `memmodel`.
    pub memmodel: String,
    /// `flashmethod`.
    pub flashmethod: String,
    /// `checksummodule` — EcuFlash's checksum handler name (informational).
    pub checksummodule: String,
}

/// Result of importing one EcuFlash/RomRaider `<rom>`.
#[derive(Debug, Clone, Default)]
pub struct EcuflashImport {
    /// The ROM identity.
    pub rom_id: RomId,
    /// Base definition ids this ROM inherits from (EcuFlash `<include>` or
    /// RomRaider `<rom base=>`); not merged — see crate docs.
    pub includes: Vec<String>,
    /// The imported maps, one per top-level `<table>`.
    pub maps: Vec<MapDef>,
    /// Fidelity / import-loss warnings (non-linear scalings, unknown types,
    /// unmerged includes, …). Empty means nothing lossy was detected.
    pub warnings: Vec<String>,
}

/// A resolved cell/axis scaling: storage type + linear scaling + metadata.
#[derive(Debug, Clone, Default)]
struct ScalingDef {
    data_type: DataType,
    endian: Endian,
    scaling: Scaling,
    unit: String,
    min: Option<f64>,
    max: Option<f64>,
    toexpr_nonlinear: bool,
}

fn parse_u32_hex(s: &str) -> Option<u32> {
    let t = s.trim();
    let t = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")).unwrap_or(t);
    u32::from_str_radix(t, 16).ok()
}

fn storage_to_type(s: &str) -> Option<DataType> {
    Some(match s.trim().to_ascii_lowercase().as_str() {
        "uint8" | "int8u" => DataType::U8,
        "int8" | "int8s" => DataType::S8,
        "uint16" | "int16u" => DataType::U16,
        "int16" | "int16s" => DataType::S16,
        "uint32" | "int32u" => DataType::U32,
        "int32" | "int32s" => DataType::S32,
        "float" => DataType::F32,
        _ => return None,
    })
}

fn endian_of(s: Option<&str>) -> Endian {
    match s.map(|v| v.trim().to_ascii_lowercase()) {
        Some(ref v) if v == "little" || v == "lsb" => Endian::Little,
        _ => Endian::Big, // EcuFlash/RomRaider default is big-endian (Renesas SH)
    }
}

/// The raw→physical equation attribute, whichever dialect spells it:
/// EcuFlash `toexpr`, RomRaider `expression`.
fn equation_attr<'a>(n: Node<'a, '_>) -> Option<&'a str> {
    n.attribute("toexpr").or_else(|| n.attribute("expression"))
}

/// Fit a raw→physical equation to a linear scaling. Returns the scaling and
/// whether it was non-linear (imported pre-scaled, flag surfaced in notes).
fn fit_equation(eq: Option<&str>) -> (Scaling, bool) {
    match eq {
        None => (Scaling::default(), false),
        Some(eq) => {
            // The fitter samples the variable as uppercase `X`; both dialects
            // write it lowercase `x` — normalize before fitting.
            let normalized = eq.replace('x', "X");
            match linear_from_equation(&normalized) {
                LinearFit::Linear { factor, offset } => (Scaling { factor, offset }, false),
                LinearFit::NonLinear | LinearFit::Unparsable => (Scaling::default(), true),
            }
        }
    }
}

/// A node's data address: EcuFlash `address`, RomRaider `storageaddress`.
fn node_address(n: Node) -> Option<u32> {
    n.attribute("address")
        .or_else(|| n.attribute("storageaddress"))
        .and_then(parse_u32_hex)
}

fn child_text(node: Node, tag: &str) -> String {
    node.children()
        .find(|c| c.is_element() && c.tag_name().name() == tag)
        .and_then(|c| c.text())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Detect the dialect: a `<scalingbase>`, a `<scaling>` with `expression=`, or a
/// `<table>` carrying `storagetype`/`sizex` means RomRaider; else EcuFlash.
fn detect_dialect(doc: &Document) -> Dialect {
    for n in doc.descendants().filter(|n| n.is_element()) {
        match n.tag_name().name() {
            "scalingbase" => return Dialect::RomRaider,
            "scaling" if n.has_attribute("expression") => return Dialect::RomRaider,
            "table" if n.has_attribute("storagetype") || n.has_attribute("sizex") => {
                return Dialect::RomRaider
            }
            _ => {}
        }
    }
    Dialect::EcuFlash
}

fn parse_romid(rom: Node) -> RomId {
    let r = rom
        .children()
        .find(|c| c.is_element() && c.tag_name().name() == "romid");
    let Some(r) = r else {
        return RomId::default();
    };
    RomId {
        xmlid: child_text(r, "xmlid"),
        make: child_text(r, "make"),
        model: child_text(r, "model"),
        submodel: child_text(r, "submodel"),
        market: child_text(r, "market"),
        year: child_text(r, "year"),
        transmission: child_text(r, "transmission"),
        ecuid: child_text(r, "ecuid"),
        internalidstring: child_text(r, "internalidstring"),
        internalidaddress: parse_u32_hex(&child_text(r, "internalidaddress")),
        memmodel: child_text(r, "memmodel"),
        flashmethod: child_text(r, "flashmethod"),
        checksummodule: child_text(r, "checksummodule"),
    }
}

/// Parse a `<scaling>` / `<scalingbase>` node's own attributes into a
/// [`ScalingDef`], pushing a warning for an unknown storage type or non-linear
/// equation. Used for the named-scaling table (both dialects) and for inline
/// scalings.
fn parse_scaling_node(n: Node, warnings: &mut Vec<String>) -> ScalingDef {
    let name = n.attribute("name").unwrap_or("<unnamed>");
    let data_type = match n.attribute("storagetype") {
        Some(s) => match storage_to_type(s) {
            Some(t) => t,
            None => {
                warnings.push(format!(
                    "scaling `{name}`: unknown storagetype `{s}`, defaulting to uint8"
                ));
                DataType::U8
            }
        },
        None => DataType::U8,
    };
    let eq = equation_attr(n);
    let (scaling, nonlinear) = fit_equation(eq);
    if nonlinear {
        warnings.push(format!(
            "scaling `{name}`: non-linear equation `{}` imported pre-scaled (factor 1)",
            eq.unwrap_or("")
        ));
    }
    ScalingDef {
        data_type,
        endian: endian_of(n.attribute("endian")),
        scaling,
        unit: n.attribute("units").unwrap_or("").to_string(),
        min: n.attribute("min").and_then(|v| v.trim().parse().ok()),
        max: n.attribute("max").and_then(|v| v.trim().parse().ok()),
        toexpr_nonlinear: nonlinear,
    }
}

/// Override a [`ScalingDef`]'s storage from a RomRaider `<table>`/axis node,
/// where `storagetype`/`endian` live on the table rather than the scaling.
fn apply_storage_from_table(sd: &mut ScalingDef, node: Node, warnings: &mut Vec<String>) {
    if let Some(s) = node.attribute("storagetype") {
        match storage_to_type(s) {
            Some(t) => sd.data_type = t,
            None => {
                warnings.push(format!(
                    "table `{}`: unknown storagetype `{s}`, defaulting to uint8",
                    node.attribute("name").unwrap_or("?")
                ));
                sd.data_type = DataType::U8;
            }
        }
    }
    sd.endian = endian_of(node.attribute("endian"));
}

/// Resolve a RomRaider `<scaling>` child element: either `base="name"` (look up
/// a shared `<scalingbase>` / named scaling) or an inline `expression=`.
fn resolve_scaling_element(
    sc: Node,
    scalings: &HashMap<String, ScalingDef>,
    warnings: &mut Vec<String>,
    owner: &str,
) -> ScalingDef {
    if let Some(base) = sc.attribute("base") {
        if let Some(sd) = scalings.get(base) {
            return sd.clone();
        }
        warnings.push(format!(
            "table `{owner}`: scalingbase `{base}` not defined in this file (likely an unmerged base def)"
        ));
        return ScalingDef::default();
    }
    parse_scaling_node(sc, warnings)
}

/// Resolve the scaling + storage of a RomRaider `<table>` or axis sub-table:
/// equation/units from the first child `<scaling>` (or a `scaling="name"`
/// attribute), storage type/endian overridden from the node's own attributes.
fn resolve_romraider(
    node: Node,
    scalings: &HashMap<String, ScalingDef>,
    warnings: &mut Vec<String>,
) -> ScalingDef {
    let owner = node.attribute("name").unwrap_or("?");
    let scaling_children: Vec<Node> = node
        .children()
        .filter(|c| c.is_element() && c.tag_name().name() == "scaling")
        .collect();

    let mut sd = if let Some(first) = scaling_children.first() {
        resolve_scaling_element(*first, scalings, warnings, owner)
    } else if let Some(name) = node.attribute("scaling") {
        scalings.get(name).cloned().unwrap_or_else(|| {
            warnings.push(format!(
                "table `{owner}`: scaling `{name}` not defined in this file (likely an unmerged base def)"
            ));
            ScalingDef::default()
        })
    } else {
        ScalingDef::default()
    };

    if scaling_children.len() > 1 {
        warnings.push(format!(
            "table `{owner}`: {} alternate unit scalings; used the first (`{}`)",
            scaling_children.len(),
            sd.unit
        ));
    }

    apply_storage_from_table(&mut sd, node, warnings);
    sd
}

/// Resolve the scaling an EcuFlash table/axis refers to (`scaling="name"`), or
/// parse an inline scaling from the node's own attributes.
fn resolve_ecuflash(
    node: Node,
    scalings: &HashMap<String, ScalingDef>,
    warnings: &mut Vec<String>,
) -> ScalingDef {
    if let Some(name) = node.attribute("scaling") {
        if let Some(sd) = scalings.get(name) {
            return sd.clone();
        }
        warnings.push(format!(
            "table `{}`: scaling `{name}` not defined in this file (likely in an included base def)",
            node.attribute("name").unwrap_or("?")
        ));
    }
    if node.attribute("storagetype").is_some() || node.attribute("toexpr").is_some() {
        return parse_scaling_node(node, warnings);
    }
    ScalingDef::default()
}

fn resolve_scaling(
    dialect: Dialect,
    node: Node,
    scalings: &HashMap<String, ScalingDef>,
    warnings: &mut Vec<String>,
) -> ScalingDef {
    match dialect {
        Dialect::EcuFlash => resolve_ecuflash(node, scalings, warnings),
        Dialect::RomRaider => resolve_romraider(node, scalings, warnings),
    }
}

fn is_axis(ty: &str) -> bool {
    ty.to_ascii_lowercase().contains("axis")
}

fn axis_is_x(ty: &str) -> bool {
    ty.to_ascii_lowercase().contains("x axis")
}

/// Collect inline static-axis breakpoint values from `<data>` children (text may
/// hold several whitespace/comma-separated numbers).
fn collect_axis_values(axis: Node) -> Vec<f64> {
    axis.children()
        .filter(|c| c.is_element() && c.tag_name().name() == "data")
        .filter_map(|c| c.text())
        .flat_map(|t| t.split([',', ' ', '\t', '\n', '\r']))
        .filter_map(|t| {
            let t = t.trim();
            if t.is_empty() {
                None
            } else {
                t.parse::<f64>().ok()
            }
        })
        .collect()
}

/// Build an [`AxisDef`] from an axis sub-table node, resolving its scaling.
/// `forced_count` carries a RomRaider parent `sizex`/`sizey` (EcuFlash passes
/// `None` and the count comes from the axis's own `elements`).
fn parse_axis(
    dialect: Dialect,
    axis: Node,
    scalings: &HashMap<String, ScalingDef>,
    warnings: &mut Vec<String>,
    forced_count: Option<u32>,
) -> AxisDef {
    let name = axis.attribute("name").unwrap_or("").to_string();
    let sd = resolve_scaling(dialect, axis, scalings, warnings);
    let address = node_address(axis);
    let values = collect_axis_values(axis);
    let from_values = (!values.is_empty()).then_some(values.len() as u32);
    let count = forced_count
        .or_else(|| axis.attribute("elements").and_then(|v| v.trim().parse::<u32>().ok()))
        .or(from_values)
        .unwrap_or(0);
    AxisDef {
        name,
        unit: sd.unit.clone(),
        count,
        values,
        address,
        data_type: sd.data_type,
        endian: sd.endian,
        scaling: sd.scaling,
        axis_ref: None,
    }
}

/// Build the X/Y [`AxisDef`]s from a table's axis sub-tables. `size_x`/`size_y`
/// carry a RomRaider parent `sizex`/`sizey` used as the forced axis count
/// (EcuFlash passes `None`, so each axis counts via its own `elements`).
fn build_axes(
    dialect: Dialect,
    table: Node,
    scalings: &HashMap<String, ScalingDef>,
    warnings: &mut Vec<String>,
    size_x: Option<u32>,
    size_y: Option<u32>,
) -> (Option<AxisDef>, Option<AxisDef>) {
    let axis_nodes: Vec<Node> = table
        .children()
        .filter(|c| {
            c.is_element()
                && c.tag_name().name() == "table"
                && is_axis(c.attribute("type").unwrap_or(""))
        })
        .collect();
    let mut x_axis = None;
    let mut y_axis = None;
    for a in &axis_nodes {
        let at = a.attribute("type").unwrap_or("");
        let is_x = axis_is_x(at);
        let forced = match dialect {
            Dialect::RomRaider if is_x => size_x,
            Dialect::RomRaider => size_y,
            Dialect::EcuFlash => None,
        };
        let built = parse_axis(dialect, *a, scalings, warnings, forced);
        if is_x {
            x_axis = Some(built);
        } else {
            y_axis = Some(built);
        }
    }
    (x_axis, y_axis)
}

/// Build a [`MapDef`] from a `<table>` that carries its own body (no `base=`
/// inheritance). Shared by the single-document and base-merging paths.
fn build_plain_map(
    dialect: Dialect,
    table: Node,
    scalings: &HashMap<String, ScalingDef>,
    warnings: &mut Vec<String>,
) -> Option<MapDef> {
    let name = table.attribute("name")?.to_string();
    let ty = table.attribute("type").unwrap_or("").to_string();

    let cell = resolve_scaling(dialect, table, scalings, warnings);

    // RomRaider dimensions live on the table; EcuFlash reads them off the axes.
    let size_x = table.attribute("sizex").and_then(|v| v.trim().parse::<u32>().ok());
    let size_y = table.attribute("sizey").and_then(|v| v.trim().parse::<u32>().ok());

    let (x_axis, y_axis) = build_axes(dialect, table, scalings, warnings, size_x, size_y);

    // Dimensions. 3D = X*Y map, 2D = 1-axis curve, 1D = scalar. RomRaider trusts
    // sizex/sizey (falling back to an axis count); EcuFlash uses axis counts.
    let axis_cols = x_axis.as_ref().map(|a| a.count).filter(|c| *c > 0);
    let axis_rows = y_axis.as_ref().map(|a| a.count).filter(|c| *c > 0);
    let (cols, rows) = match dialect {
        Dialect::RomRaider => (
            size_x.or(axis_cols).unwrap_or(1),
            size_y.or(axis_rows).unwrap_or(1),
        ),
        Dialect::EcuFlash => (axis_cols.unwrap_or(1), axis_rows.unwrap_or(1)),
    };

    let mut notes = String::new();
    if let Some(cat) = table.attribute("category") {
        notes = format!("category: {cat}");
    }
    if cell.toexpr_nonlinear {
        if !notes.is_empty() {
            notes.push_str("; ");
        }
        notes.push_str("non-linear scaling imported pre-scaled (factor 1) — verify");
    }

    let address = match node_address(table) {
        Some(a) => a,
        None => {
            warnings.push(format!("table `{name}` ({ty}): no/invalid address, skipped"));
            return None;
        }
    };

    Some(MapDef {
        name,
        notes,
        address,
        data_type: cell.data_type,
        endian: cell.endian,
        rows,
        cols,
        scaling: cell.scaling,
        unit: cell.unit,
        x_axis,
        y_axis,
        min: cell.min,
        max: cell.max,
    })
}

/// Parse one top-level `<table>` into a [`MapDef`] in the single-document path.
/// A RomRaider `<table base="Other Table">` has no body of its own here (the base
/// is not resolved), so it is reported and skipped rather than emitted broken —
/// use [`parse_ecuflash_with_bases`] to merge it.
fn parse_table(
    dialect: Dialect,
    table: Node,
    scalings: &HashMap<String, ScalingDef>,
    warnings: &mut Vec<String>,
) -> Option<MapDef> {
    if let Some(base) = table.attribute("base") {
        let name = table.attribute("name").unwrap_or("?");
        warnings.push(format!(
            "table `{name}` inherits from base table `{base}` (not merged); skipped"
        ));
        return None;
    }
    build_plain_map(dialect, table, scalings, warnings)
}

/// Parse an EcuFlash / RomRaider XML definition into an [`EcuflashImport`].
///
/// The dialect is auto-detected; see the crate docs. Only the **first** `<rom>`
/// in the file is imported (others are warned about), and inherited definitions
/// are reported rather than merged.
pub fn parse_ecuflash(src: &str) -> Result<EcuflashImport> {
    let doc = Document::parse(src).map_err(|e| Error::Xml(e.to_string()))?;
    let dialect = detect_dialect(&doc);

    // All <rom> elements anywhere in the document.
    let roms: Vec<Node> = doc
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "rom")
        .collect();
    let rom = *roms.first().ok_or(Error::NoRom)?;

    let mut warnings = Vec::new();
    if roms.len() > 1 {
        warnings.push(format!(
            "{} <rom> blocks in file; imported the first (`{}`)",
            roms.len(),
            rom.children()
                .find(|c| c.is_element() && c.tag_name().name() == "romid")
                .map(|r| child_text(r, "xmlid"))
                .unwrap_or_default()
        ));
    }

    // Reusable scalings (search the whole file; base scalings sit outside the
    // chosen <rom>). EcuFlash: <scaling name=…>. RomRaider: <scalingbase name=…>.
    let mut scalings: HashMap<String, ScalingDef> = HashMap::new();
    for s in doc.descendants().filter(|n| {
        n.is_element()
            && matches!(n.tag_name().name(), "scaling" | "scalingbase")
            && n.has_attribute("name")
    }) {
        if let Some(name) = s.attribute("name") {
            scalings.insert(name.to_string(), parse_scaling_node(s, &mut warnings));
        }
    }

    let rom_id = parse_romid(rom);

    // Inheritance: EcuFlash <include>base</include>, RomRaider <rom base="…">.
    let mut includes: Vec<String> = rom
        .children()
        .filter(|c| c.is_element() && c.tag_name().name() == "include")
        .filter_map(|c| c.text())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    if let Some(base) = rom.attribute("base") {
        includes.push(base.to_string());
    }
    if !includes.is_empty() {
        warnings.push(format!(
            "inherits from base def(s) {includes:?}; tables/scalings only in the base are NOT merged — load the base file too"
        ));
    }

    // Top-level <table> children of the <rom> are the maps; nested tables are axes.
    let maps: Vec<MapDef> = rom
        .children()
        .filter(|c| c.is_element() && c.tag_name().name() == "table")
        .filter_map(|t| parse_table(dialect, t, &scalings, &mut warnings))
        .collect();

    Ok(EcuflashImport {
        rom_id,
        includes,
        maps,
        warnings,
    })
}

// ---- Base-definition merging --------------------------------------------------

/// The xmlid of a `<rom>`'s `<romid>` (empty if absent).
fn rom_xmlid(rom: Node) -> String {
    rom.children()
        .find(|c| c.is_element() && c.tag_name().name() == "romid")
        .map(|r| child_text(r, "xmlid"))
        .unwrap_or_default()
}

/// The base ids a `<rom>` inherits: EcuFlash `<include>` text + RomRaider `base=`.
fn rom_base_ids(rom: Node) -> Vec<String> {
    let mut ids: Vec<String> = rom
        .children()
        .filter(|c| c.is_element() && c.tag_name().name() == "include")
        .filter_map(|c| c.text())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    if let Some(base) = rom.attribute("base") {
        let b = base.trim();
        if !b.is_empty() {
            ids.push(b.to_string());
        }
    }
    ids
}

/// Pick the `<rom>` matching `want` (by xmlid, falling back to the first), or the
/// first `<rom>` when `want` is `None`.
fn find_rom<'a, 'i>(doc: &'a Document<'i>, want: Option<&str>) -> Option<Node<'a, 'i>> {
    let roms: Vec<Node> = doc
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "rom")
        .collect();
    match want {
        Some(id) => roms
            .iter()
            .copied()
            .find(|r| rom_xmlid(*r) == id)
            .or_else(|| roms.first().copied()),
        None => roms.first().copied(),
    }
}

/// Collect named scalings from a document into `scalings`. Later inserts override
/// earlier, so call it base-first then for the child.
fn collect_scalings(
    doc: &Document,
    scalings: &mut HashMap<String, ScalingDef>,
    warnings: &mut Vec<String>,
) {
    for s in doc.descendants().filter(|n| {
        n.is_element()
            && matches!(n.tag_name().name(), "scaling" | "scalingbase")
            && n.has_attribute("name")
    }) {
        if let Some(name) = s.attribute("name") {
            scalings.insert(name.to_string(), parse_scaling_node(s, warnings));
        }
    }
}

/// Does this node define its own axis sub-tables?
fn has_axis_children(node: Node) -> bool {
    node.children().any(|c| {
        c.is_element()
            && c.tag_name().name() == "table"
            && is_axis(c.attribute("type").unwrap_or(""))
    })
}

/// Overlay a child `<table base="…">`'s explicitly-specified fields onto the
/// [`MapDef`] built from its base table. Fields the child does not specify are
/// inherited from the base (storage, dims, cell scaling, axes).
fn overlay_child_fields(
    mut base_map: MapDef,
    dialect: Dialect,
    node: Node,
    scalings: &HashMap<String, ScalingDef>,
    warnings: &mut Vec<String>,
) -> MapDef {
    if let Some(n) = node.attribute("name") {
        base_map.name = n.to_string();
    }
    if let Some(a) = node_address(node) {
        base_map.address = a;
    }
    if let Some(s) = node.attribute("storagetype") {
        match storage_to_type(s) {
            Some(t) => base_map.data_type = t,
            None => {
                warnings.push(format!(
                    "table `{}`: unknown storagetype `{s}`, defaulting to uint8",
                    base_map.name
                ));
                base_map.data_type = DataType::U8;
            }
        }
    }
    if let Some(e) = node.attribute("endian") {
        base_map.endian = endian_of(Some(e));
    }
    if let Some(sx) = node.attribute("sizex").and_then(|v| v.trim().parse::<u32>().ok()) {
        base_map.cols = sx;
    }
    if let Some(sy) = node.attribute("sizey").and_then(|v| v.trim().parse::<u32>().ok()) {
        base_map.rows = sy;
    }

    // Cell scaling: only override when the child provides its own (a `<scaling>`
    // child or a `scaling="name"` ref). Storage/endian were handled above, so we
    // take only the equation-derived fields from the resolved scaling here.
    let has_scaling = node.attribute("scaling").is_some()
        || node
            .children()
            .any(|c| c.is_element() && c.tag_name().name() == "scaling");
    if has_scaling {
        let cell = resolve_scaling(dialect, node, scalings, warnings);
        base_map.scaling = cell.scaling;
        base_map.unit = cell.unit.clone();
        base_map.min = cell.min;
        base_map.max = cell.max;
        if cell.toexpr_nonlinear && !base_map.notes.contains("non-linear") {
            if !base_map.notes.is_empty() {
                base_map.notes.push_str("; ");
            }
            base_map
                .notes
                .push_str("non-linear scaling imported pre-scaled (factor 1) — verify");
        }
    }

    if has_axis_children(node) {
        let size_x = node
            .attribute("sizex")
            .and_then(|v| v.trim().parse::<u32>().ok())
            .or(Some(base_map.cols));
        let size_y = node
            .attribute("sizey")
            .and_then(|v| v.trim().parse::<u32>().ok())
            .or(Some(base_map.rows));
        let (x, y) = build_axes(dialect, node, scalings, warnings, size_x, size_y);
        if x.is_some() {
            base_map.x_axis = x;
        }
        if y.is_some() {
            base_map.y_axis = y;
        }
    }

    if let Some(cat) = node.attribute("category") {
        let note = format!("category: {cat}");
        if !base_map.notes.contains(&note) {
            if !base_map.notes.is_empty() {
                base_map.notes.push_str("; ");
            }
            base_map.notes.push_str(&note);
        }
    }
    base_map
}

/// Build a [`MapDef`] in the base-merging path, resolving a RomRaider
/// `<table base="name">` by inheriting the named base table's fields from `scope`
/// (the merged, possibly-inherited table set) and overlaying the child's own
/// fields. Cycles and missing bases are reported, never looped.
fn build_merged_map<'a, 'i>(
    dialect: Dialect,
    node: Node<'a, 'i>,
    scalings: &HashMap<String, ScalingDef>,
    scope: &HashMap<String, Node<'a, 'i>>,
    warnings: &mut Vec<String>,
    visiting: &mut Vec<String>,
) -> Option<MapDef> {
    let name = node.attribute("name")?.to_string();
    let Some(base_name) = node.attribute("base") else {
        return build_plain_map(dialect, node, scalings, warnings);
    };

    if visiting.iter().any(|v| v == base_name) {
        warnings.push(format!(
            "table `{name}`: base-table cycle via `{base_name}`; emitted from own fields only"
        ));
        return build_plain_map(dialect, node, scalings, warnings);
    }
    match scope.get(base_name) {
        None => {
            warnings.push(format!(
                "table `{name}`: base table `{base_name}` not found in merged scope; emitted from own fields only"
            ));
            build_plain_map(dialect, node, scalings, warnings)
        }
        Some(base_node) => {
            visiting.push(base_name.to_string());
            let base_map = build_merged_map(dialect, *base_node, scalings, scope, warnings, visiting);
            visiting.pop();
            match base_map {
                Some(bm) => Some(overlay_child_fields(bm, dialect, node, scalings, warnings)),
                None => {
                    warnings.push(format!(
                        "table `{name}`: base table `{base_name}` could not be built; emitted from own fields only"
                    ));
                    build_plain_map(dialect, node, scalings, warnings)
                }
            }
        }
    }
}

/// Walk the base-definition chain depth-first (post-order, so base-most first),
/// appending each resolved base's `(xmlid, source)` to `ordered`. Missing bases,
/// unparsable bases, and inheritance cycles are recorded as warnings and skipped.
fn resolve_base_chain<F: Fn(&str) -> Option<String>>(
    base_ids: &[String],
    resolve: &F,
    visiting: &mut Vec<String>,
    done: &mut std::collections::HashSet<String>,
    ordered: &mut Vec<(String, String)>,
    warnings: &mut Vec<String>,
) {
    for id in base_ids {
        if done.contains(id) {
            continue;
        }
        if visiting.iter().any(|v| v == id) {
            warnings.push(format!(
                "base def `{id}`: inheritance cycle detected; not merged"
            ));
            continue;
        }
        let Some(src) = resolve(id) else {
            warnings.push(format!("base def `{id}` not found via resolver; not merged"));
            continue;
        };
        // Parse transiently to find this base's own base ids (the Document is
        // dropped before `src` is moved into `ordered`).
        let child_ids: Vec<String> = match Document::parse(&src) {
            Err(e) => {
                warnings.push(format!("base def `{id}`: XML parse error: {e}; not merged"));
                continue;
            }
            Ok(doc) => match find_rom(&doc, Some(id)) {
                Some(rom) => rom_base_ids(rom),
                None => {
                    warnings.push(format!("base def `{id}`: no <rom> element; not merged"));
                    continue;
                }
            },
        };
        visiting.push(id.clone());
        resolve_base_chain(&child_ids, resolve, visiting, done, ordered, warnings);
        visiting.pop();
        done.insert(id.clone());
        ordered.push((id.clone(), src));
    }
}

/// Parse an EcuFlash / RomRaider definition, **merging its base definitions**.
///
/// `resolve(xmlid)` returns the XML source of a base def (the target of an
/// EcuFlash `<include>xmlid</include>` or a RomRaider `<rom base="xmlid">`),
/// letting the caller supply the base-def store (a `--base-dir`, a bundled
/// library, …). The chain is resolved recursively (base-of-base first) and merged
/// **base first, child overrides** by scaling `name`, table `name`, and romid
/// xmlid; a RomRaider `<table base="name">` inherits the named base table's
/// missing fields from the (possibly inherited) merged scope. Missing bases and
/// inheritance cycles are surfaced as warnings, never looped.
///
/// The single-document [`parse_ecuflash`] is unchanged; prefer this entry when a
/// base-def store is available.
pub fn parse_ecuflash_with_bases<F>(src: &str, resolve: F) -> Result<EcuflashImport>
where
    F: Fn(&str) -> Option<String>,
{
    let main_doc = Document::parse(src).map_err(|e| Error::Xml(e.to_string()))?;

    let main_roms: Vec<Node> = main_doc
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "rom")
        .collect();
    let main_rom = *main_roms.first().ok_or(Error::NoRom)?;

    let mut warnings = Vec::new();
    if main_roms.len() > 1 {
        warnings.push(format!(
            "{} <rom> blocks in file; imported the first (`{}`)",
            main_roms.len(),
            rom_xmlid(main_rom)
        ));
    }

    let main_base_ids = rom_base_ids(main_rom);

    // Resolve the base-def chain, base-most first.
    let mut ordered: Vec<(String, String)> = Vec::new();
    let mut visiting: Vec<String> = Vec::new();
    let mut done = std::collections::HashSet::new();
    let main_id = rom_xmlid(main_rom);
    if !main_id.is_empty() {
        // Seed so a base that re-includes this ROM is caught as a cycle.
        visiting.push(main_id);
    }
    resolve_base_chain(
        &main_base_ids,
        &resolve,
        &mut visiting,
        &mut done,
        &mut ordered,
        &mut warnings,
    );

    // Parse every resolved base document (base-first); each parsed cleanly during
    // chain resolution, so they are kept alive here for borrowing.
    let base_docs: Vec<Document> = ordered
        .iter()
        .filter_map(|(_, s)| Document::parse(s).ok())
        .collect();

    // Dialect: RomRaider if the main doc or any base looks RomRaider.
    let dialect = if detect_dialect(&main_doc) == Dialect::RomRaider
        || base_docs.iter().any(|d| detect_dialect(d) == Dialect::RomRaider)
    {
        Dialect::RomRaider
    } else {
        Dialect::EcuFlash
    };

    // Combined scalings: base-first, child (main) overrides by name.
    let mut scalings: HashMap<String, ScalingDef> = HashMap::new();
    for d in &base_docs {
        collect_scalings(d, &mut scalings, &mut warnings);
    }
    collect_scalings(&main_doc, &mut scalings, &mut warnings);

    // The selected `<rom>` per document (base-first), then the main rom last.
    let mut selected: Vec<Node> = Vec::new();
    for (i, (id, _)) in ordered.iter().enumerate() {
        if let Some(d) = base_docs.get(i) {
            if let Some(r) = find_rom(d, Some(id)) {
                selected.push(r);
            }
        }
    }
    selected.push(main_rom);

    // Merge top-level `<table>`s by name: base-first, child overrides, order kept.
    let mut table_order: Vec<String> = Vec::new();
    let mut table_node: HashMap<String, Node> = HashMap::new();
    for rom in &selected {
        for t in rom
            .children()
            .filter(|c| c.is_element() && c.tag_name().name() == "table")
        {
            if let Some(name) = t.attribute("name") {
                if !table_node.contains_key(name) {
                    table_order.push(name.to_string());
                }
                table_node.insert(name.to_string(), t);
            }
        }
    }

    let mut maps = Vec::new();
    for name in &table_order {
        if let Some(node) = table_node.get(name) {
            let mut visiting_tables: Vec<String> = Vec::new();
            if let Some(m) = build_merged_map(
                dialect,
                *node,
                &scalings,
                &table_node,
                &mut warnings,
                &mut visiting_tables,
            ) {
                maps.push(m);
            }
        }
    }

    Ok(EcuflashImport {
        rom_id: parse_romid(main_rom),
        includes: main_base_ids,
        maps,
        warnings,
    })
}

/// Convenience over [`parse_ecuflash_with_bases`]: resolve base defs from an
/// in-memory `xmlid → XML source` map (the merge over a `HashMap`).
pub fn parse_ecuflash_with_base_map(
    src: &str,
    bases: &HashMap<String, String>,
) -> Result<EcuflashImport> {
    parse_ecuflash_with_bases(src, |id| bases.get(id).cloned())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- EcuFlash dialect ---------------------------------------------------

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rom>
  <romid>
    <xmlid>2015 WRX MT</xmlid>
    <internalidaddress>400</internalidaddress>
    <internalidstring>AVCF000L</internalidstring>
    <ecuid>2F12795207</ecuid>
    <make>SUBARU</make>
    <model>WRX</model>
    <submodel>2.0GT</submodel>
    <market>EDM</market>
    <transmission>MT</transmission>
    <year>2015</year>
    <flashmethod>subarucan</flashmethod>
    <memmodel>SH7058</memmodel>
    <checksummodule>subarucan</checksummodule>
  </romid>
  <include>32BITBASE</include>

  <scaling name="Load" units="g/rev" toexpr="x*0.0078125" storagetype="uint16" endian="big" min="0" max="4"/>
  <scaling name="RPM"  units="rpm"   toexpr="x*0.25"      storagetype="uint16" endian="big"/>
  <scaling name="Timing" units="deg" toexpr="x*0.5-64"   storagetype="uint8"  endian="big"/>
  <scaling name="Squared" units="x"  toexpr="x*x"         storagetype="uint8"  endian="big"/>

  <table name="Primary Open Loop Fueling" type="3D" category="Fueling" address="0x30000" scaling="Timing">
    <table name="RPM" type="X Axis" address="0x30100" elements="4" scaling="RPM"/>
    <table name="Load" type="Y Axis" address="0x30200" elements="3" scaling="Load"/>
  </table>

  <table name="Rev Limit" type="1D" category="Limits" address="0x31000" scaling="RPM"/>

  <table name="Bad Scale Map" type="3D" category="Test" address="0x32000" scaling="Squared">
    <table name="RPM" type="X Axis" address="0x32100" elements="2" scaling="RPM"/>
    <table name="Y" type="Y Axis" address="0x32200" elements="2" scaling="RPM"/>
  </table>
</rom>"#;

    #[test]
    fn imports_identity_and_includes() {
        let imp = parse_ecuflash(SAMPLE).unwrap();
        assert_eq!(imp.rom_id.xmlid, "2015 WRX MT");
        assert_eq!(imp.rom_id.make, "SUBARU");
        assert_eq!(imp.rom_id.memmodel, "SH7058");
        assert_eq!(imp.rom_id.internalidaddress, Some(0x400));
        assert_eq!(imp.includes, vec!["32BITBASE".to_string()]);
        assert!(imp.warnings.iter().any(|w| w.contains("inherits from base")));
    }

    #[test]
    fn imports_3d_map_with_axes_and_scaling() {
        let imp = parse_ecuflash(SAMPLE).unwrap();
        let m = imp.maps.iter().find(|m| m.name == "Primary Open Loop Fueling").unwrap();
        assert_eq!(m.address, 0x30000);
        assert_eq!(m.data_type, DataType::U8); // Timing scaling = uint8
        assert_eq!((m.cols, m.rows), (4, 3)); // X=4, Y=3
        assert!((m.scaling.factor - 0.5).abs() < 1e-9 && (m.scaling.offset + 64.0).abs() < 1e-9);
        assert_eq!(m.unit, "deg");
        let x = m.x_axis.as_ref().unwrap();
        assert_eq!((x.address, x.count, x.data_type), (Some(0x30100), 4, DataType::U16));
        assert!((x.scaling.factor - 0.25).abs() < 1e-9);
        let y = m.y_axis.as_ref().unwrap();
        assert_eq!((y.address, y.count), (Some(0x30200), 3));
        assert!((y.scaling.factor - 0.0078125).abs() < 1e-12);
        assert_eq!(y.unit, "g/rev");
    }

    #[test]
    fn imports_1d_scalar() {
        let imp = parse_ecuflash(SAMPLE).unwrap();
        let m = imp.maps.iter().find(|m| m.name == "Rev Limit").unwrap();
        assert_eq!((m.rows, m.cols), (1, 1));
        assert_eq!(m.data_type, DataType::U16);
        assert!(m.x_axis.is_none() && m.y_axis.is_none());
    }

    #[test]
    fn nonlinear_scaling_is_flagged_not_wrong() {
        let imp = parse_ecuflash(SAMPLE).unwrap();
        let m = imp.maps.iter().find(|m| m.name == "Bad Scale Map").unwrap();
        // x*x cannot be linear: imported pre-scaled (factor 1) + noted + warned.
        assert_eq!(m.scaling.factor, 1.0);
        assert!(m.notes.contains("non-linear"));
        assert!(imp.warnings.iter().any(|w| w.contains("Squared") && w.contains("non-linear")));
    }

    #[test]
    fn missing_address_is_skipped_with_warning() {
        let src = r#"<rom><romid><xmlid>t</xmlid></romid>
          <table name="NoAddr" type="1D"/></rom>"#;
        let imp = parse_ecuflash(src).unwrap();
        assert!(imp.maps.is_empty());
        assert!(imp.warnings.iter().any(|w| w.contains("no/invalid address")));
    }

    #[test]
    fn empty_or_bad_xml() {
        assert!(matches!(parse_ecuflash("<notarom/>"), Err(Error::NoRom)));
        assert!(matches!(parse_ecuflash("<<<"), Err(Error::Xml(_))));
    }

    #[test]
    fn detects_ecuflash_dialect() {
        let doc = Document::parse(SAMPLE).unwrap();
        assert_eq!(detect_dialect(&doc), Dialect::EcuFlash);
    }

    // ---- RomRaider dialect --------------------------------------------------

    // Mirrors RomRaider's ecu_defs.xml: <scalingbase> shared scalings, storage on
    // the <table>, sizex/sizey dims, child <scaling> (base ref or expression),
    // storageaddress, <rom base=> and <table base=> inheritance.
    const RR: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<roms>
  <scalingbase name="rpm" units="RPM" expression="x" to_byte="x" format="#"/>
  <scalingbase name="boost" units="bar" expression="x*0.001333224" to_byte="x/0.001333224" format="0.000" min="0" max="3"/>
  <scalingbase name="wgdc" units="Wastegate Duty (%)" expression="x*x" to_byte="x" format="0.0"/>
  <rom base="32BITBASE">
    <romid>
      <xmlid>A2WC522S</xmlid>
      <internalidaddress>2000</internalidaddress>
      <internalidstring>A2WC522S</internalidstring>
      <ecuid>2F12795606</ecuid>
      <make>Subaru</make>
      <model>Outback</model>
      <submodel>XT</submodel>
      <memmodel>SH7058</memmodel>
      <flashmethod>sti05</flashmethod>
    </romid>

    <table type="3D" name="Target Boost" category="Boost Control - Target" storagetype="uint16" endian="big" sizex="3" sizey="2" storageaddress="0xC11D4">
      <scaling base="boost"/>
      <scaling base="rpm"/>
      <table type="X Axis" name="Throttle Plate Opening Angle" storagetype="float" endian="little" storageaddress="0xC1184">
        <scaling units="%" expression="x/.84" to_byte="x*.84" format="0.0"/>
      </table>
      <table type="Y Axis" name="Engine Speed" storagetype="float" endian="little" storageaddress="0xC11A4">
        <scaling base="rpm"/>
      </table>
      <description>Desired boost targets.</description>
    </table>

    <table type="2D" name="Rev Limit" category="Limits" storagetype="uint16" endian="big" sizex="1" storageaddress="0xD000">
      <scaling base="rpm"/>
    </table>

    <table type="3D" name="Initial Wastegate Duty" category="Boost Control - Wastegate" storagetype="uint8" endian="big" sizex="2" sizey="2" storageaddress="0xE000">
      <scaling base="wgdc"/>
      <table type="X Axis" name="Throttle" storagetype="float" endian="little" storageaddress="0xE100">
        <scaling units="%" expression="x/.84" to_byte="x*.84" format="0.0"/>
      </table>
      <table type="Y Axis" name="Engine Speed" storagetype="float" endian="little" storageaddress="0xE200">
        <scaling base="rpm"/>
      </table>
    </table>

    <table base="Target Boost" name="Target Boost B">
      <description>Inherited Target Boost</description>
    </table>
  </rom>
</roms>"##;

    #[test]
    fn detects_romraider_dialect() {
        let doc = Document::parse(RR).unwrap();
        assert_eq!(detect_dialect(&doc), Dialect::RomRaider);
    }

    #[test]
    fn romraider_identity_and_base() {
        let imp = parse_ecuflash(RR).unwrap();
        assert_eq!(imp.rom_id.xmlid, "A2WC522S");
        assert_eq!(imp.rom_id.make, "Subaru");
        assert_eq!(imp.rom_id.memmodel, "SH7058");
        assert_eq!(imp.rom_id.internalidaddress, Some(0x2000));
        // <rom base="32BITBASE"> surfaces as an unmerged-base include + warning.
        assert_eq!(imp.includes, vec!["32BITBASE".to_string()]);
        assert!(imp.warnings.iter().any(|w| w.contains("inherits from base")));
    }

    #[test]
    fn romraider_3d_storage_on_table_sizes_and_axes() {
        let imp = parse_ecuflash(RR).unwrap();
        let m = imp.maps.iter().find(|m| m.name == "Target Boost").unwrap();
        // storageaddress + storagetype/endian live on the <table>.
        assert_eq!(m.address, 0xC11D4);
        assert_eq!(m.data_type, DataType::U16);
        assert_eq!(m.endian, Endian::Big);
        // Dimensions come from sizex/sizey.
        assert_eq!((m.cols, m.rows), (3, 2));
        // Cell scaling via <scaling base="boost"> -> scalingbase expression.
        assert!((m.scaling.factor - 0.001333224).abs() < 1e-12);
        assert_eq!(m.unit, "bar");
        assert_eq!((m.min, m.max), (Some(0.0), Some(3.0)));
        // X axis: storage on the axis sub-table, count from parent sizex, scaling
        // from an inline expression=.
        let x = m.x_axis.as_ref().unwrap();
        assert_eq!(x.address, Some(0xC1184));
        assert_eq!(x.count, 3);
        assert_eq!((x.data_type, x.endian), (DataType::F32, Endian::Little));
        assert!((x.scaling.factor - (1.0 / 0.84)).abs() < 1e-9);
        assert_eq!(x.unit, "%");
        // Y axis: count from parent sizey, scaling via base ref.
        let y = m.y_axis.as_ref().unwrap();
        assert_eq!((y.address, y.count), (Some(0xC11A4), 2));
        assert_eq!(y.data_type, DataType::F32);
        assert!((y.scaling.factor - 1.0).abs() < 1e-12);
        assert_eq!(y.unit, "RPM");
    }

    #[test]
    fn romraider_multiple_scalings_uses_first_and_warns() {
        let imp = parse_ecuflash(RR).unwrap();
        // Target Boost carries two <scaling> children (boost + rpm): the first wins.
        let m = imp.maps.iter().find(|m| m.name == "Target Boost").unwrap();
        assert_eq!(m.unit, "bar");
        assert!(imp
            .warnings
            .iter()
            .any(|w| w.contains("Target Boost") && w.contains("alternate unit scalings")));
    }

    #[test]
    fn romraider_2d_scalar_from_sizex() {
        let imp = parse_ecuflash(RR).unwrap();
        let m = imp.maps.iter().find(|m| m.name == "Rev Limit").unwrap();
        assert_eq!((m.cols, m.rows), (1, 1));
        assert_eq!(m.data_type, DataType::U16);
        assert_eq!(m.unit, "RPM");
        assert!((m.scaling.factor - 1.0).abs() < 1e-12);
        assert!(m.x_axis.is_none() && m.y_axis.is_none());
    }

    #[test]
    fn romraider_nonlinear_scalingbase_is_flagged() {
        let imp = parse_ecuflash(RR).unwrap();
        let m = imp.maps.iter().find(|m| m.name == "Initial Wastegate Duty").unwrap();
        // wgdc expression x*x is non-linear -> pre-scaled factor 1, noted + warned.
        assert_eq!(m.scaling.factor, 1.0);
        assert!(m.notes.contains("non-linear"));
        assert!(imp
            .warnings
            .iter()
            .any(|w| w.contains("wgdc") && w.contains("non-linear")));
    }

    #[test]
    fn romraider_inherited_table_is_skipped() {
        let imp = parse_ecuflash(RR).unwrap();
        assert!(imp.maps.iter().all(|m| m.name != "Target Boost B"));
        assert!(imp
            .warnings
            .iter()
            .any(|w| w.contains("Target Boost B") && w.contains("inherits from base table")));
    }

    #[test]
    fn romraider_static_axis_values_and_count() {
        // A Static X Axis carries inline breakpoints and no sizex on the parent.
        let src = r#"<roms>
          <scalingbase name="deg" units="C" expression="x" to_byte="x"/>
          <rom>
            <romid><xmlid>T</xmlid></romid>
            <table type="2D" name="Temp Comp" storagetype="uint8" endian="big" storageaddress="0x1000">
              <scaling base="deg"/>
              <table type="Static X Axis" name="Coolant">
                <data>-40</data><data>0</data><data>40 80</data>
              </table>
            </table>
          </rom>
        </roms>"#;
        let imp = parse_ecuflash(src).unwrap();
        let m = imp.maps.iter().find(|m| m.name == "Temp Comp").unwrap();
        let x = m.x_axis.as_ref().unwrap();
        assert_eq!(x.count, 4);
        assert_eq!(x.values, vec![-40.0, 0.0, 40.0, 80.0]);
        // sizex absent -> cols fall back to the static axis count.
        assert_eq!(m.cols, 4);
    }

    // ---- Base-definition merging --------------------------------------------

    // An EcuFlash base def: it supplies the RPM/Load scalings and a table of its
    // own. A child `<include>`s it and references those base scalings.
    const BASE_32BIT: &str = r#"<rom>
      <romid><xmlid>32BITBASE</xmlid></romid>
      <scaling name="RPM"  units="rpm"   toexpr="x*0.25"      storagetype="uint16" endian="big"/>
      <scaling name="Load" units="g/rev" toexpr="x*0.0078125" storagetype="uint16" endian="big"/>
      <table name="Base Rev Limit" type="1D" category="Limits" address="0x31000" scaling="RPM"/>
    </rom>"#;

    const CHILD_INCLUDE: &str = r#"<rom>
      <romid><xmlid>CHILD</xmlid></romid>
      <include>32BITBASE</include>
      <table name="Child Map" type="3D" category="Fueling" address="0x30000" scaling="Load">
        <table name="RPM"  type="X Axis" address="0x30100" elements="4" scaling="RPM"/>
        <table name="Load" type="Y Axis" address="0x30200" elements="3" scaling="Load"/>
      </table>
    </rom>"#;

    #[test]
    fn merges_base_scalings_and_tables() {
        let mut bases = HashMap::new();
        bases.insert("32BITBASE".to_string(), BASE_32BIT.to_string());
        let imp = parse_ecuflash_with_base_map(CHILD_INCLUDE, &bases).unwrap();

        assert_eq!(imp.rom_id.xmlid, "CHILD");
        assert_eq!(imp.includes, vec!["32BITBASE".to_string()]);

        // A table that lives ONLY in the base def is merged in.
        let base_tbl = imp.maps.iter().find(|m| m.name == "Base Rev Limit").unwrap();
        assert_eq!(base_tbl.data_type, DataType::U16);
        assert!((base_tbl.scaling.factor - 0.25).abs() < 1e-9);

        // The child's own table resolves scalings that live only in the base.
        let cm = imp.maps.iter().find(|m| m.name == "Child Map").unwrap();
        assert_eq!(cm.unit, "g/rev");
        assert!((cm.scaling.factor - 0.0078125).abs() < 1e-12);
        assert_eq!((cm.cols, cm.rows), (4, 3));
        let x = cm.x_axis.as_ref().unwrap();
        assert!((x.scaling.factor - 0.25).abs() < 1e-9); // RPM, from the base

        // Nothing went unresolved.
        assert!(!imp.warnings.iter().any(|w| w.contains("not defined")));
        assert!(!imp.warnings.iter().any(|w| w.contains("not found")));
    }

    #[test]
    fn child_override_wins_over_base() {
        // Child redefines the RPM scaling and the Base Rev Limit table; both of
        // its versions must win over the base's.
        let child = r#"<rom>
          <romid><xmlid>CHILD</xmlid></romid>
          <include>32BITBASE</include>
          <scaling name="RPM" units="rpm" toexpr="x*0.5" storagetype="uint16" endian="big"/>
          <table name="Base Rev Limit" type="1D" category="Limits" address="0x39000" scaling="RPM"/>
          <table name="Child Map" type="2D" address="0x30000" scaling="RPM">
            <table name="RPM" type="X Axis" address="0x30100" elements="4" scaling="RPM"/>
          </table>
        </rom>"#;
        let mut bases = HashMap::new();
        bases.insert("32BITBASE".to_string(), BASE_32BIT.to_string());
        let imp = parse_ecuflash_with_base_map(child, &bases).unwrap();

        // Table override by name: the child's address, not the base's 0x31000.
        let rl = imp.maps.iter().find(|m| m.name == "Base Rev Limit").unwrap();
        assert_eq!(rl.address, 0x39000);
        // Exactly one map with that name (base version was overridden, not kept).
        assert_eq!(imp.maps.iter().filter(|m| m.name == "Base Rev Limit").count(), 1);
        // Scaling override by name: child RPM factor 0.5, not the base's 0.25.
        assert!((rl.scaling.factor - 0.5).abs() < 1e-9);
        let cm = imp.maps.iter().find(|m| m.name == "Child Map").unwrap();
        assert!((cm.x_axis.as_ref().unwrap().scaling.factor - 0.5).abs() < 1e-9);
    }

    #[test]
    fn three_level_base_chain_is_resolved() {
        // C defines a scaling + a table; B includes C; the child A includes B and
        // uses C's scaling. Base-of-base must resolve (post-order, base-most first).
        let c = r#"<rom>
          <romid><xmlid>C</xmlid></romid>
          <scaling name="TEMP" units="C" toexpr="x*0.1-40" storagetype="uint8" endian="big"/>
          <table name="C Only" type="1D" address="0xC000" scaling="TEMP"/>
        </rom>"#;
        let b = r#"<rom>
          <romid><xmlid>B</xmlid></romid>
          <include>C</include>
          <scaling name="PRES" units="kPa" toexpr="x*0.75" storagetype="uint16" endian="big"/>
        </rom>"#;
        let a = r#"<rom>
          <romid><xmlid>A</xmlid></romid>
          <include>B</include>
          <table name="A Map" type="3D" address="0xA000" scaling="PRES">
            <table name="T" type="X Axis" address="0xA100" elements="2" scaling="TEMP"/>
            <table name="P" type="Y Axis" address="0xA200" elements="2" scaling="PRES"/>
          </table>
        </rom>"#;
        let mut bases = HashMap::new();
        bases.insert("B".to_string(), b.to_string());
        bases.insert("C".to_string(), c.to_string());
        let imp = parse_ecuflash_with_base_map(a, &bases).unwrap();

        // Table from the deepest base (C) appears.
        assert!(imp.maps.iter().any(|m| m.name == "C Only"));
        let am = imp.maps.iter().find(|m| m.name == "A Map").unwrap();
        assert_eq!(am.unit, "kPa"); // PRES, from B
        assert!((am.scaling.factor - 0.75).abs() < 1e-9);
        let x = am.x_axis.as_ref().unwrap();
        assert!((x.scaling.factor - 0.1).abs() < 1e-9); // TEMP, from C
        assert!(!imp.warnings.iter().any(|w| w.contains("not defined")));
        assert!(!imp.warnings.iter().any(|w| w.contains("not found")));
    }

    #[test]
    fn missing_base_still_warns_and_does_not_fail() {
        let child = r#"<rom>
          <romid><xmlid>CHILD</xmlid></romid>
          <include>NOPE</include>
          <table name="Local" type="1D" address="0x1000"/>
        </rom>"#;
        // Empty store -> the base cannot be resolved.
        let imp = parse_ecuflash_with_base_map(child, &HashMap::new()).unwrap();
        assert!(imp
            .warnings
            .iter()
            .any(|w| w.contains("NOPE") && w.contains("not found via resolver")));
        // The child's own content still imports.
        assert!(imp.maps.iter().any(|m| m.name == "Local"));
        assert_eq!(imp.includes, vec!["NOPE".to_string()]);
    }

    #[test]
    fn inheritance_cycle_is_detected_not_looped() {
        // X includes Y, Y includes X; the child includes X.
        let x = r#"<rom><romid><xmlid>X</xmlid></romid><include>Y</include></rom>"#;
        let y = r#"<rom><romid><xmlid>Y</xmlid></romid><include>X</include></rom>"#;
        let child = r#"<rom><romid><xmlid>CH</xmlid></romid><include>X</include>
          <table name="Local" type="1D" address="0x1000"/></rom>"#;
        let mut bases = HashMap::new();
        bases.insert("X".to_string(), x.to_string());
        bases.insert("Y".to_string(), y.to_string());
        // Must terminate (no stack overflow) and warn.
        let imp = parse_ecuflash_with_base_map(child, &bases).unwrap();
        assert!(imp.warnings.iter().any(|w| w.contains("cycle")));
        assert!(imp.maps.iter().any(|m| m.name == "Local"));
    }

    #[test]
    fn romraider_table_base_inherits_missing_fields() {
        // RomRaider `<table base="name">`: the child overrides only the address
        // and must inherit storage, dims, cell scaling and axes from the base.
        let rr = r##"<roms>
          <scalingbase name="boost" units="bar" expression="x*0.001333224" to_byte="x" format="0.000" min="0" max="3"/>
          <scalingbase name="rpm" units="RPM" expression="x" to_byte="x"/>
          <rom>
            <romid><xmlid>T</xmlid></romid>
            <table type="3D" name="Target Boost" storagetype="uint16" endian="big" sizex="3" sizey="2" storageaddress="0xC11D4">
              <scaling base="boost"/>
              <table type="X Axis" name="X" storagetype="float" endian="little" storageaddress="0xC1184"><scaling base="rpm"/></table>
              <table type="Y Axis" name="Y" storagetype="float" endian="little" storageaddress="0xC11A4"><scaling base="rpm"/></table>
            </table>
            <table base="Target Boost" name="Target Boost B" storageaddress="0xBEEF">
              <description>Inherited</description>
            </table>
          </rom>
        </roms>"##;
        let imp = parse_ecuflash_with_base_map(rr, &HashMap::new()).unwrap();

        // The inherited table is emitted (not skipped) with merged fields.
        let b = imp.maps.iter().find(|m| m.name == "Target Boost B").unwrap();
        assert_eq!(b.address, 0xBEEF); // child override
        assert_eq!(b.data_type, DataType::U16); // inherited storage
        assert_eq!(b.endian, Endian::Big);
        assert_eq!((b.cols, b.rows), (3, 2)); // inherited dims
        assert_eq!(b.unit, "bar"); // inherited cell scaling
        assert!((b.scaling.factor - 0.001333224).abs() < 1e-12);
        assert!(b.x_axis.is_some() && b.y_axis.is_some()); // inherited axes
        assert_eq!(b.x_axis.as_ref().unwrap().count, 3);

        // The base itself is still present, and no "skipped" warning was emitted.
        assert!(imp.maps.iter().any(|m| m.name == "Target Boost"));
        assert!(!imp
            .warnings
            .iter()
            .any(|w| w.contains("Target Boost B") && w.contains("skipped")));
    }

    #[test]
    fn with_bases_matches_single_doc_when_no_includes() {
        // With no inheritance, the merging entry yields the same maps as the
        // single-document parser (same count, names, addresses).
        let single = parse_ecuflash(SAMPLE).unwrap();
        let merged = parse_ecuflash_with_base_map(SAMPLE, &HashMap::new()).unwrap();
        assert_eq!(single.maps.len(), merged.maps.len());
        for m in &single.maps {
            let n = merged.maps.iter().find(|x| x.name == m.name).unwrap();
            assert_eq!((n.address, n.cols, n.rows), (m.address, m.cols, m.rows));
        }
    }

    /// Real EcuFlash/RomRaider defs, when available via
    /// `OPENTUNE_FIXTURES/ecuflash/*.xml` (never shipped). Each must parse and
    /// yield at least one map, with no map carrying an address of 0.
    #[test]
    #[ignore = "needs owner EcuFlash/RomRaider fixtures via OPENTUNE_FIXTURES/ecuflash"]
    fn reference_ecuflash_fixtures() {
        let Ok(dir) = std::env::var("OPENTUNE_FIXTURES") else {
            return;
        };
        let d = std::path::Path::new(&dir).join("ecuflash");
        let Ok(entries) = std::fs::read_dir(&d) else {
            return;
        };
        let mut checked = 0;
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("xml") {
                continue;
            }
            let src = std::fs::read_to_string(&p).unwrap();
            match parse_ecuflash(&src) {
                Ok(imp) => {
                    // A base-only def may legitimately have no tables; just ensure
                    // nothing with a zero address slipped through.
                    assert!(
                        imp.maps.iter().all(|m| m.address != 0),
                        "{}: a map imported with address 0",
                        p.display()
                    );
                    checked += 1;
                }
                Err(e) => panic!("{}: {e}", p.display()),
            }
        }
        assert!(checked >= 1, "no EcuFlash fixtures under OPENTUNE_FIXTURES/ecuflash");
    }
}
