//! # opentune-ecuflash
//!
//! Import **EcuFlash / RomRaider** XML ROM definitions into OpenTune
//! [`MapDef`]s. These are the open, community-maintained definition formats
//! behind EcuFlash and RomRaider — a large library of **Subaru / Mitsubishi**
//! (and other) map definitions. Importing the *format* (not the tools' code)
//! lets OpenTune reuse that library through the same map pipeline as the
//! TunerPro XDF and ASAM A2L importers.
//!
//! ## What it parses
//!
//! One `<rom>` definition: its `<romid>` identity, the reusable `<scaling>`
//! elements, and the `<table>` elements (1D scalar / 2D curve / 3D map) with
//! their axis sub-tables. A table's `scaling="..."` reference resolves to a
//! `<scaling>` that supplies the `storagetype`, `endian`, `units`, bounds and
//! the `toexpr` raw→physical equation — which is fitted to a linear
//! `factor`/`offset` with [`opentune_xdf::math::linear_from_equation`] (the same
//! fitter the XDF importer uses; a non-linear `toexpr` imports pre-scaled with a
//! note, never a silent wrong scale).
//!
//! ## What it does not do (v1, reported, never silent)
//!
//! * `<include>` base-definition inheritance is **not merged** (the base file is
//!   a separate document); the referenced base id is recorded in
//!   [`EcuflashImport::warnings`] so the maintainer can merge it.
//! * Non-linear `toexpr`, unknown `storagetype`, a missing `scaling` reference,
//!   or more than one `<rom>` in the file are each surfaced as a warning rather
//!   than dropped quietly.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use opentune_core::ecudef::{AxisDef, DataType, Endian, MapDef, Scaling};
use opentune_xdf::math::{linear_from_equation, LinearFit};
use roxmltree::{Document, Node};

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

/// The `<romid>` identity block of an EcuFlash/RomRaider ROM definition.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RomId {
    /// `xmlid` — the definition's unique id (also the `<include>` target).
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
    /// Base definition ids this ROM `<include>`s (not merged — see crate docs).
    pub includes: Vec<String>,
    /// The imported maps, one per top-level `<table>`.
    pub maps: Vec<MapDef>,
    /// Fidelity / import-loss warnings (non-linear scalings, unknown types,
    /// unmerged includes, …). Empty means nothing lossy was detected.
    pub warnings: Vec<String>,
}

/// A resolved `<scaling>` definition.
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

fn storage_to_type(s: &str) -> Option<(DataType, &'static str)> {
    // Returns the data type, and "" or a warning hint for unusual widths.
    Some(match s.trim().to_ascii_lowercase().as_str() {
        "uint8" | "int8u" => (DataType::U8, ""),
        "int8" | "int8s" => (DataType::S8, ""),
        "uint16" | "int16u" => (DataType::U16, ""),
        "int16" | "int16s" => (DataType::S16, ""),
        "uint32" | "int32u" => (DataType::U32, ""),
        "int32" | "int32s" => (DataType::S32, ""),
        "float" => (DataType::F32, ""),
        _ => return None,
    })
}

fn endian_of(s: Option<&str>) -> Endian {
    match s.map(|v| v.trim().to_ascii_lowercase()) {
        Some(ref v) if v == "little" || v == "lsb" => Endian::Little,
        _ => Endian::Big, // EcuFlash default is big-endian (Renesas SH)
    }
}

/// Fit a `toexpr` (EcuFlash uses lowercase `x`) to a linear scaling. Returns the
/// scaling and whether it was non-linear (imported pre-scaled, flag in notes).
fn fit_toexpr(toexpr: Option<&str>) -> (Scaling, bool) {
    match toexpr {
        None => (Scaling::default(), false),
        Some(eq) => {
            // The fitter samples the variable as uppercase `X`; EcuFlash uses `x`.
            let normalized = eq.replace('x', "X");
            match linear_from_equation(&normalized) {
                LinearFit::Linear { factor, offset } => (Scaling { factor, offset }, false),
                LinearFit::NonLinear | LinearFit::Unparsable => (Scaling::default(), true),
            }
        }
    }
}

fn child_text(node: Node, tag: &str) -> String {
    node.children()
        .find(|c| c.is_element() && c.tag_name().name() == tag)
        .and_then(|c| c.text())
        .unwrap_or("")
        .trim()
        .to_string()
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

/// Resolve a `<scaling>` node (or a table's own inline scaling attrs) to a
/// [`ScalingDef`], pushing a warning for an unknown storage type.
fn parse_scaling_node(n: Node, warnings: &mut Vec<String>) -> ScalingDef {
    let name = n.attribute("name").unwrap_or("<unnamed>");
    let data_type = match n.attribute("storagetype") {
        Some(s) => match storage_to_type(s) {
            Some((t, _)) => t,
            None => {
                warnings.push(format!(
                    "scaling `{name}`: unknown storagetype `{s}`, defaulting to uint8"
                ));
                DataType::U8
            }
        },
        None => DataType::U8,
    };
    let (scaling, nonlinear) = fit_toexpr(n.attribute("toexpr"));
    if nonlinear {
        warnings.push(format!(
            "scaling `{name}`: non-linear toexpr `{}` imported pre-scaled (factor 1)",
            n.attribute("toexpr").unwrap_or("")
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

/// Build an [`AxisDef`] from an axis sub-table node, resolving its scaling.
fn parse_axis(
    axis: Node,
    scalings: &std::collections::HashMap<String, ScalingDef>,
    warnings: &mut Vec<String>,
) -> AxisDef {
    let name = axis.attribute("name").unwrap_or("").to_string();
    let count = axis
        .attribute("elements")
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(0);
    let sd = resolve_table_scaling(axis, scalings, warnings);
    let address = axis.attribute("address").and_then(parse_u32_hex);
    // A "Static" axis may carry inline breakpoint values as child <data> text.
    let values: Vec<f64> = axis
        .children()
        .filter(|c| c.is_element() && c.tag_name().name() == "data")
        .filter_map(|c| c.text())
        .filter_map(|t| t.trim().parse::<f64>().ok())
        .collect();
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

/// Resolve the scaling a table/axis refers to (by `scaling="name"`), or parse an
/// inline scaling from the node's own attributes.
fn resolve_table_scaling(
    node: Node,
    scalings: &std::collections::HashMap<String, ScalingDef>,
    warnings: &mut Vec<String>,
) -> ScalingDef {
    if let Some(name) = node.attribute("scaling") {
        if let Some(sd) = scalings.get(name) {
            return sd.clone();
        }
        // No matching <scaling> in this file — the base def probably holds it.
        warnings.push(format!(
            "table `{}`: scaling `{name}` not defined in this file (likely in an included base def)",
            node.attribute("name").unwrap_or("?")
        ));
    }
    // Inline storagetype/toexpr on the node itself, if any.
    if node.attribute("storagetype").is_some() || node.attribute("toexpr").is_some() {
        return parse_scaling_node(node, warnings);
    }
    ScalingDef::default()
}

fn is_axis(ty: &str) -> bool {
    let t = ty.to_ascii_lowercase();
    t.contains("axis")
}

fn axis_is_x(ty: &str) -> bool {
    ty.to_ascii_lowercase().contains("x axis")
}

/// Parse one top-level `<table>` into a [`MapDef`].
fn parse_table(
    table: Node,
    scalings: &std::collections::HashMap<String, ScalingDef>,
    warnings: &mut Vec<String>,
) -> Option<MapDef> {
    let name = table.attribute("name")?.to_string();
    let ty = table.attribute("type").unwrap_or("").to_string();
    let address = table.attribute("address").and_then(parse_u32_hex);

    let cell = resolve_table_scaling(table, scalings, warnings);

    // Axis sub-tables.
    let axis_nodes: Vec<Node> = table
        .children()
        .filter(|c| c.is_element() && c.tag_name().name() == "table" && is_axis(c.attribute("type").unwrap_or("")))
        .collect();
    let mut x_axis = None;
    let mut y_axis = None;
    for a in &axis_nodes {
        let at = a.attribute("type").unwrap_or("");
        let built = parse_axis(*a, scalings, warnings);
        if axis_is_x(at) {
            x_axis = Some(built);
        } else {
            y_axis = Some(built);
        }
    }

    // Dimensions. EcuFlash: 3D = X*Y map, 2D = 1-axis curve, 1D = scalar.
    let cols = x_axis.as_ref().map(|a| a.count).filter(|c| *c > 0).unwrap_or(1);
    let rows = y_axis.as_ref().map(|a| a.count).filter(|c| *c > 0).unwrap_or(1);

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

    let address = match address {
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

/// Parse an EcuFlash / RomRaider XML definition into an [`EcuflashImport`].
pub fn parse_ecuflash(src: &str) -> Result<EcuflashImport> {
    let doc = Document::parse(src).map_err(|e| Error::Xml(e.to_string()))?;

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

    // Reusable <scaling> definitions (search the whole file; base scalings may
    // sit outside the chosen <rom>).
    let mut scalings = std::collections::HashMap::new();
    for s in doc
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "scaling")
    {
        if let Some(name) = s.attribute("name") {
            scalings.insert(name.to_string(), parse_scaling_node(s, &mut warnings));
        }
    }

    let rom_id = parse_romid(rom);

    let includes: Vec<String> = rom
        .children()
        .filter(|c| c.is_element() && c.tag_name().name() == "include")
        .filter_map(|c| c.text())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    if !includes.is_empty() {
        warnings.push(format!(
            "inherits from base def(s) {:?}; tables/scalings only in the base are NOT merged — load the base file too",
            includes
        ));
    }

    // Top-level <table> children of the <rom> are the maps; nested tables are axes.
    let maps: Vec<MapDef> = rom
        .children()
        .filter(|c| c.is_element() && c.tag_name().name() == "table")
        .filter_map(|t| parse_table(t, &scalings, &mut warnings))
        .collect();

    Ok(EcuflashImport {
        rom_id,
        includes,
        maps,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
