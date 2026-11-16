//! Parser for the **modern XML** TunerPro XDF format (TunerPro v5).
//!
//! Shape handled:
//! ```xml
//! <XDFFORMAT version="1.60">
//!   <XDFHEADER>
//!     <deftitle>...</deftitle>
//!     <BASEOFFSET offset="0" subtract="0"/>
//!     <DEFAULTS ... lsbfirst="0"/>
//!   </XDFHEADER>
//!   <XDFTABLE uniqueid="0x..">
//!     <title>...</title>
//!     <description>...</description>
//!     <XDFAXIS id="x"> ... </XDFAXIS>
//!     <XDFAXIS id="y"> ... </XDFAXIS>
//!     <XDFAXIS id="z">
//!       <EMBEDDEDDATA mmedaddress="0x288" mmedelementsizebits="16"
//!                     mmedrowcount="16" mmedcolcount="11" mmedtypeflags="0x02"/>
//!       <units>deg</units>
//!       <MATH equation="(45*X)/127-20"/>
//!       <min>..</min><max>..</max>
//!     </XDFAXIS>
//!   </XDFTABLE>
//! </XDFFORMAT>
//! ```

use crate::math::{linear_from_equation, LinearFit};
use crate::{
    datatype_from, endian_from, parse_u32, typeflags, Error, Result, XdfConstant, XdfImport,
};
use opentune_core::ecudef::{AxisDef, MapDef, Scaling};
use roxmltree::{Document, Node};

/// Parse a modern XML XDF document.
pub fn parse(src: &str) -> Result<XdfImport> {
    let doc = Document::parse(src).map_err(|e| Error::Xml(e.to_string()))?;
    let root = doc.root_element();

    // The root is normally <XDFFORMAT>; tolerate the document element being the
    // table itself or a wrapper by searching descendants for tables regardless.
    let header = descendant(root, "XDFHEADER");

    let title = header
        .and_then(|h| child_text(h, "deftitle").or_else(|| child_text(h, "DefTitle")))
        .filter(|s| !s.is_empty());

    // BASEOFFSET: magnitude + subtract flag.
    let (base_offset, base_subtract) = header
        .and_then(|h| child(h, "BASEOFFSET"))
        .map(|b| {
            let off = attr(b, "offset").and_then(parse_u32).unwrap_or(0);
            let sub = attr(b, "subtract")
                .and_then(parse_u32)
                .map(|v| v != 0)
                .unwrap_or(false);
            (off, sub)
        })
        .unwrap_or((0, false));

    // Default endianness when an EMBEDDEDDATA omits a type-flags LSB bit.
    let default_lsb = header
        .and_then(|h| child(h, "DEFAULTS"))
        .and_then(|d| attr(d, "lsbfirst"))
        .and_then(parse_u32)
        .map(|v| v != 0)
        .unwrap_or(false);

    let mut maps = Vec::new();
    let mut warnings = Vec::new();
    for table in root.descendants().filter(|n| has_tag(*n, "XDFTABLE")) {
        if let Some(m) = parse_table(
            table,
            base_offset,
            base_subtract,
            default_lsb,
            &mut warnings,
        ) {
            maps.push(m);
        }
    }

    // Scalar constants are a distinct object class from tables; keep them in
    // their own list (see `XdfConstant`) rather than folding them into `maps`.
    let mut constants = Vec::new();
    for c in root.descendants().filter(|n| has_tag(*n, "XDFCONSTANT")) {
        if let Some(k) = parse_constant(c, base_offset, base_subtract, default_lsb, &mut warnings) {
            constants.push(k);
        }
    }

    if maps.is_empty() && constants.is_empty() {
        return Err(Error::Empty);
    }
    Ok(XdfImport {
        title,
        base_offset,
        maps,
        constants,
        warnings,
    })
}

fn parse_table(
    table: Node,
    base_offset: u32,
    base_subtract: bool,
    default_lsb: bool,
    warnings: &mut Vec<String>,
) -> Option<MapDef> {
    let mut notes: Vec<String> = Vec::new();

    let name = child_text(table, "title")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            attr(table, "uniqueid")
                .map(|id| format!("table_{id}"))
                .unwrap_or_else(|| "unnamed table".to_string())
        });

    if let Some(d) = child_text(table, "description").filter(|s| !s.is_empty()) {
        notes.push(d);
    }

    // Collect the axes by id.
    let mut z = None;
    let mut x = None;
    let mut y = None;
    for ax in table.children().filter(|n| has_tag(*n, "XDFAXIS")) {
        match attr(ax, "id").map(|s| s.to_ascii_lowercase()).as_deref() {
            Some("z") => z = Some(ax),
            Some("x") => x = Some(ax),
            Some("y") => y = Some(ax),
            _ => {}
        }
    }

    // The z axis carries the table data. Without it we cannot place the map.
    let zed = z?;
    let emb = child(zed, "EMBEDDEDDATA")?;

    let mmedaddr = attr(emb, "mmedaddress").and_then(parse_u32)?;
    let size_bits = attr(emb, "mmedelementsizebits")
        .and_then(parse_u32)
        .unwrap_or(0);
    let rows = attr(emb, "mmedrowcount")
        .and_then(parse_u32)
        .unwrap_or(1)
        .max(1);
    let cols = attr(emb, "mmedcolcount")
        .and_then(parse_u32)
        .unwrap_or(1)
        .max(1);
    let flags_present = attr(emb, "mmedtypeflags").is_some();
    let flags = attr(emb, "mmedtypeflags").and_then(parse_u32).unwrap_or(0);

    let (data_type, exact) = datatype_from(size_bits, flags);
    if size_bits == 0 {
        notes.push("element size missing in XDF; defaulted to u8".into());
    } else if !exact {
        notes.push(format!(
            "non-standard element size {size_bits} bits; approximated"
        ));
    }
    let endian = if flags_present {
        endian_from(flags)
    } else {
        notes.push("endianness not in type flags; used file default".into());
        if default_lsb {
            opentune_core::ecudef::Endian::Little
        } else {
            opentune_core::ecudef::Endian::Big
        }
    };

    // Swap-XY (0x04): the data is stored transposed relative to OpenTune's
    // row-major (column-fastest) convention. OpenTune has no per-map storage
    // orientation and no round-trippable way to record the flag (an export would
    // drop it and silently re-corrupt), so we DO NOT transpose here — a blind
    // transpose of an asymmetric map is itself dangerous. Instead we flag the
    // table so a human can verify its orientation. See the README.
    if flags & typeflags::SWAP_XY != 0 {
        notes.push(
            "swap-XY storage flag (0x04) set: rows/cols may be transposed vs this import; \
             orientation UNVERIFIED and not applied"
                .into(),
        );
        warnings.push(format!(
            "table \"{name}\": swap-XY storage flag (mmedtypeflags 0x04) is set but OpenTune \
             does not model storage orientation; imported WITHOUT transposing \
             ({rows}x{cols}) — verify row/column orientation before trusting cell positions"
        ));
    }

    // Exotic strides: flag but do not try to reshape.
    for a in ["mmedmajorstridebits", "mmedminorstridebits"] {
        if let Some(v) = attr(emb, a).and_then(|s| s.trim().parse::<i64>().ok()) {
            // A stride of 0 (auto) or the natural element size is normal; a
            // negative or odd stride means a non-contiguous / reversed layout.
            if v < 0 {
                notes.push(format!("{a}={v} (reversed/strided layout not modeled)"));
            }
        }
    }

    // Checked rebasing: a subtract that goes below the base, or an add that
    // overflows u32, is a malformed mapping. Refuse the table rather than emit a
    // wrapped giant address.
    let address = match resolve_addr(mmedaddr, base_offset, base_subtract) {
        Some(a) => a,
        None => {
            warnings.push(format!(
                "table \"{name}\": address 0x{mmedaddr:X} could not be rebased against base \
                 offset 0x{base_offset:X} ({}); table skipped",
                if base_subtract {
                    "subtract underflow"
                } else {
                    "add overflow"
                }
            ));
            return None;
        }
    };

    // Scaling + unit from the z axis.
    let (scaling, nonlinear) = scaling_from_axis(zed);
    if let Some(eq) = nonlinear {
        notes.push(format!("non-linear MATH imported pre-scaled: {eq}"));
    }
    let unit = child_text(zed, "units").unwrap_or_default();

    let min = child_text(zed, "min").and_then(|s| s.trim().parse::<f64>().ok());
    let max = child_text(zed, "max").and_then(|s| s.trim().parse::<f64>().ok());

    let x_axis = x.map(|ax| parse_axis(ax, cols, base_offset, base_subtract, default_lsb));
    let y_axis = y.map(|ax| parse_axis(ax, rows, base_offset, base_subtract, default_lsb));
    for (id, parsed) in [("x", &x_axis), ("y", &y_axis)] {
        let Some(p) = parsed else { continue };
        if !p.dropped_labels.is_empty() {
            let joined = p.dropped_labels.join(", ");
            notes.push(format!(
                "{id}-axis has {} categorical label(s) not representable in a numeric axis: {joined}",
                p.dropped_labels.len()
            ));
            warnings.push(format!(
                "table \"{name}\": {id}-axis dropped {} categorical label(s) ({joined}) — \
                 OpenTune's axis model stores only numeric breakpoints",
                p.dropped_labels.len()
            ));
        }
        if let Some(issue) = &p.issue {
            notes.push(format!("{id}-axis: {issue}"));
            warnings.push(format!("table \"{name}\": {id}-axis {issue}"));
        }
    }
    let x_axis = x_axis.and_then(|p| p.axis);
    let y_axis = y_axis.and_then(|p| p.axis);

    Some(MapDef {
        name,
        notes: notes.join("; "),
        address,
        data_type,
        endian,
        rows,
        cols,
        scaling,
        unit,
        x_axis,
        y_axis,
        min,
        max,
    })
}

/// Result of parsing one `<XDFAXIS id="x|y">`: the axis itself (when one could
/// be built) plus any fidelity losses the caller should surface.
#[derive(Default)]
struct ParsedAxis {
    /// The imported axis, if there was enough to build one.
    axis: Option<AxisDef>,
    /// Non-numeric `<LABEL value=..>` strings (gear/mode names on a categorical
    /// axis) that the numeric [`AxisDef`] model cannot store, in index order.
    dropped_labels: Vec<String>,
    /// A problem worth a warning note, e.g. a stored-axis address that could not
    /// be rebased.
    issue: Option<String>,
}

/// Build an [`AxisDef`] from an `<XDFAXIS id="x|y">`.
///
/// Priority: explicit numeric `<LABEL>` values (inline axis) → embedded stored
/// axis (address + type) → a bare count/unit descriptor. Non-numeric labels
/// (categorical gear/mode axes) cannot be stored in the numeric axis model, so
/// they are collected into [`ParsedAxis::dropped_labels`] for the caller to
/// report rather than silently discarded.
fn parse_axis(
    ax: Node,
    fallback_count: u32,
    base_offset: u32,
    base_subtract: bool,
    default_lsb: bool,
) -> ParsedAxis {
    let unit = child_text(ax, "units").unwrap_or_default();
    let name = child_text(ax, "title")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| attr(ax, "id").unwrap_or("").to_string());

    let (scaling, _) = scaling_from_axis(ax);

    // Inline labels: numeric ones become breakpoints; non-numeric ones are
    // categorical labels the model can't hold and are reported as a loss.
    let mut values: Vec<f64> = Vec::new();
    let mut dropped_labels: Vec<String> = Vec::new();
    for label in ax.children().filter(|n| has_tag(*n, "LABEL")) {
        if let Some(raw) = attr(label, "value") {
            let t = raw.trim();
            match t.parse::<f64>() {
                Ok(v) => values.push(v),
                Err(_) if !t.is_empty() => dropped_labels.push(t.to_string()),
                Err(_) => {}
            }
        }
    }

    let index_count = child_text(ax, "indexcount").and_then(|s| parse_u32(&s));

    // Embedded stored axis.
    let emb = child(ax, "EMBEDDEDDATA");
    let emb_addr = emb
        .and_then(|e| attr(e, "mmedaddress"))
        .and_then(parse_u32)
        .filter(|a| *a != 0);

    // When there are only categorical labels (no numeric ones), still size the
    // axis by the label count so the dimension is preserved.
    let label_count = if !values.is_empty() {
        Some(values.len() as u32)
    } else if !dropped_labels.is_empty() {
        Some(dropped_labels.len() as u32)
    } else {
        None
    };

    let count = index_count.or(label_count).unwrap_or(fallback_count).max(1);

    if !values.is_empty() {
        return ParsedAxis {
            axis: Some(AxisDef {
                name,
                unit,
                count,
                values,
                scaling,
                ..Default::default()
            }),
            dropped_labels,
            issue: None,
        };
    }

    if let (Some(e), Some(addr)) = (emb, emb_addr) {
        match resolve_addr(addr, base_offset, base_subtract) {
            Some(resolved) => {
                let size_bits = attr(e, "mmedelementsizebits")
                    .and_then(parse_u32)
                    .unwrap_or(0);
                let flags_present = attr(e, "mmedtypeflags").is_some();
                let flags = attr(e, "mmedtypeflags").and_then(parse_u32).unwrap_or(0);
                let (dt, _) = datatype_from(size_bits, flags);
                let endian = if flags_present {
                    endian_from(flags)
                } else if default_lsb {
                    opentune_core::ecudef::Endian::Little
                } else {
                    opentune_core::ecudef::Endian::Big
                };
                return ParsedAxis {
                    axis: Some(AxisDef {
                        name,
                        unit,
                        count,
                        address: Some(resolved),
                        data_type: dt,
                        endian,
                        scaling,
                        ..Default::default()
                    }),
                    dropped_labels,
                    issue: None,
                };
            }
            None => {
                // Fall through to a count/unit descriptor, but flag the bad
                // rebasing instead of emitting a wrapped address.
                let issue = Some(format!(
                    "stored-axis address 0x{addr:X} could not be rebased against base offset \
                     0x{base_offset:X} ({}); address dropped",
                    if base_subtract {
                        "subtract underflow"
                    } else {
                        "add overflow"
                    }
                ));
                return ParsedAxis {
                    axis: Some(AxisDef {
                        name,
                        unit,
                        count,
                        scaling,
                        ..Default::default()
                    }),
                    dropped_labels,
                    issue,
                };
            }
        }
    }

    // Nothing but a count/unit (or categorical labels) — still useful as a
    // labelled dimension.
    if count > 1 || !unit.is_empty() || !dropped_labels.is_empty() {
        return ParsedAxis {
            axis: Some(AxisDef {
                name,
                unit,
                count,
                scaling,
                ..Default::default()
            }),
            dropped_labels,
            issue: None,
        };
    }
    ParsedAxis {
        axis: None,
        dropped_labels,
        issue: None,
    }
}

/// Extract a [`Scaling`] from an axis `<MATH equation="...">`. Returns the
/// scaling and, when the equation is non-linear/unparsable, the original
/// equation string so the caller can note it.
fn scaling_from_axis(ax: Node) -> (Scaling, Option<String>) {
    let eq = child(ax, "MATH").and_then(|m| attr(m, "equation"));
    match eq {
        None => (Scaling::default(), None),
        Some(eq) => match linear_from_equation(eq) {
            LinearFit::Linear { factor, offset } => (Scaling { factor, offset }, None),
            LinearFit::NonLinear | LinearFit::Unparsable => {
                (Scaling::default(), Some(eq.trim().to_string()))
            }
        },
    }
}

/// Resolve a stored `mmedaddress` against the header base offset, with **checked**
/// arithmetic. A `subtract` rebasing of an address below the base (which would
/// otherwise wrap to a huge offset like `0xFFFFFF10`), or an `add` that overflows
/// `u32`, returns `None` so the caller can refuse the malformed mapping instead
/// of emitting a wrapped address. A valid rebasing — including one that lands at
/// exactly `0` (address == base under subtract) — returns `Some`.
fn resolve_addr(mmed: u32, base: u32, subtract: bool) -> Option<u32> {
    if subtract {
        mmed.checked_sub(base)
    } else {
        mmed.checked_add(base)
    }
}

/// Parse an `<XDFCONSTANT>` into an [`XdfConstant`] scalar.
///
/// A constant carries one `<EMBEDDEDDATA>` (address/size/flags), an optional
/// `<MATH>` scaling, `<units>` and range bounds (`<min>/<max>` or
/// `<rangelow>/<rangehigh>`). It is imported only as a plain numeric scalar;
/// nothing here promotes a bitfield/switch to an unrestricted write.
fn parse_constant(
    node: Node,
    base_offset: u32,
    base_subtract: bool,
    default_lsb: bool,
    warnings: &mut Vec<String>,
) -> Option<XdfConstant> {
    let mut notes: Vec<String> = Vec::new();

    let name = child_text(node, "title")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            attr(node, "uniqueid")
                .map(|id| format!("const_{id}"))
                .unwrap_or_else(|| "unnamed constant".to_string())
        });

    if let Some(d) = child_text(node, "description").filter(|s| !s.is_empty()) {
        notes.push(d);
    }

    let emb = child(node, "EMBEDDEDDATA")?;
    let mmedaddr = attr(emb, "mmedaddress").and_then(parse_u32)?;

    let size_bits = attr(emb, "mmedelementsizebits")
        .and_then(parse_u32)
        .unwrap_or(0);
    let flags_present = attr(emb, "mmedtypeflags").is_some();
    let flags = attr(emb, "mmedtypeflags").and_then(parse_u32).unwrap_or(0);

    let (data_type, exact) = datatype_from(size_bits, flags);
    if size_bits == 0 {
        notes.push("element size missing in XDF; defaulted to u8".into());
    } else if !exact {
        notes.push(format!(
            "non-standard element size {size_bits} bits; approximated"
        ));
    }
    let endian = if flags_present {
        endian_from(flags)
    } else if default_lsb {
        opentune_core::ecudef::Endian::Little
    } else {
        notes.push("endianness not in type flags; used file default".into());
        opentune_core::ecudef::Endian::Big
    };

    let address = match resolve_addr(mmedaddr, base_offset, base_subtract) {
        Some(a) => a,
        None => {
            warnings.push(format!(
                "constant \"{name}\": address 0x{mmedaddr:X} could not be rebased against base \
                 offset 0x{base_offset:X} ({}); constant skipped",
                if base_subtract {
                    "subtract underflow"
                } else {
                    "add overflow"
                }
            ));
            return None;
        }
    };

    let (scaling, nonlinear) = scaling_from_axis(node);
    if let Some(eq) = nonlinear {
        notes.push(format!("non-linear MATH imported pre-scaled: {eq}"));
    }
    let unit = child_text(node, "units").unwrap_or_default();

    let min = child_text(node, "min")
        .or_else(|| child_text(node, "rangelow"))
        .and_then(|s| s.trim().parse::<f64>().ok());
    let max = child_text(node, "max")
        .or_else(|| child_text(node, "rangehigh"))
        .and_then(|s| s.trim().parse::<f64>().ok());

    Some(XdfConstant {
        name,
        notes: notes.join("; "),
        address,
        data_type,
        endian,
        scaling,
        unit,
        min,
        max,
    })
}

// --- small roxmltree helpers (case-insensitive tag match) ---

fn has_tag(n: Node, tag: &str) -> bool {
    n.is_element() && n.tag_name().name().eq_ignore_ascii_case(tag)
}

fn child<'a, 'input>(n: Node<'a, 'input>, tag: &str) -> Option<Node<'a, 'input>> {
    n.children().find(|c| has_tag(*c, tag))
}

fn descendant<'a, 'input>(n: Node<'a, 'input>, tag: &str) -> Option<Node<'a, 'input>> {
    n.descendants().find(|c| has_tag(*c, tag))
}

fn child_text(n: Node, tag: &str) -> Option<String> {
    child(n, tag).map(|c| c.text().unwrap_or("").trim().to_string())
}

fn attr<'a>(n: Node<'a, '_>, name: &str) -> Option<&'a str> {
    n.attributes()
        .find(|a| a.name().eq_ignore_ascii_case(name))
        .map(|a| a.value())
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentune_core::ecudef::{DataType, Endian};

    // A compact but realistic modern XML XDF: one 16x11 table, big-endian u8,
    // a non-trivial linear z equation, a label x-axis and a stored y-axis.
    const SAMPLE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<XDFFORMAT version="1.60">
  <XDFHEADER>
    <flags>0x1</flags>
    <deftitle>Sample ECU</deftitle>
    <description>synthetic test</description>
    <BASEOFFSET offset="0" subtract="0"/>
    <DEFAULTS datasizeinbits="8" sigdigits="2" outputtype="1" signed="0" lsbfirst="0" float="0"/>
  </XDFHEADER>
  <XDFTABLE uniqueid="0x1A29" flags="0x0">
    <title>Spark Advance</title>
    <description>main spark table</description>
    <XDFAXIS id="x">
      <units>MAP</units>
      <indexcount>11</indexcount>
      <LABEL index="0" value="0"/>
      <LABEL index="1" value="10"/>
      <LABEL index="2" value="20"/>
      <MATH equation="X"/>
    </XDFAXIS>
    <XDFAXIS id="y">
      <units>rpm</units>
      <indexcount>16</indexcount>
      <EMBEDDEDDATA mmedaddress="0x200" mmedelementsizebits="16" mmedtypeflags="0x00"/>
      <MATH equation="X*40"/>
    </XDFAXIS>
    <XDFAXIS id="z">
      <units>deg</units>
      <EMBEDDEDDATA mmedaddress="0x288" mmedelementsizebits="8" mmedrowcount="16" mmedcolcount="11" mmedtypeflags="0x00"/>
      <min>-20</min>
      <max>60</max>
      <MATH equation="(45*X)/127-20"/>
    </XDFAXIS>
  </XDFTABLE>
</XDFFORMAT>"#;

    #[test]
    fn parses_sample_table() {
        let imp = parse(SAMPLE).expect("parse");
        assert_eq!(imp.title.as_deref(), Some("Sample ECU"));
        assert_eq!(imp.base_offset, 0);
        assert_eq!(imp.maps.len(), 1);

        let m = &imp.maps[0];
        assert_eq!(m.name, "Spark Advance");
        assert_eq!(m.address, 0x288);
        assert_eq!(m.rows, 16);
        assert_eq!(m.cols, 11);
        assert_eq!(m.data_type, DataType::U8);
        assert_eq!(m.endian, Endian::Big);
        assert_eq!(m.unit, "deg");
        assert_eq!(m.min, Some(-20.0));
        assert_eq!(m.max, Some(60.0));
        // (45*X)/127-20
        assert!((m.scaling.factor - 45.0 / 127.0).abs() < 1e-9);
        assert!((m.scaling.offset + 20.0).abs() < 1e-9);

        // x axis: inline labels.
        let x = m.x_axis.as_ref().expect("x axis");
        assert_eq!(x.unit, "MAP");
        assert_eq!(x.values, vec![0.0, 10.0, 20.0]);

        // y axis: stored, u16 big-endian, scaled.
        let y = m.y_axis.as_ref().expect("y axis");
        assert_eq!(y.address, Some(0x200));
        assert_eq!(y.data_type, DataType::U16);
        assert_eq!(y.count, 16);
        assert!((y.scaling.factor - 40.0).abs() < 1e-9);
    }

    #[test]
    fn base_offset_subtract_is_applied() {
        let xml = SAMPLE.replace(
            r#"<BASEOFFSET offset="0" subtract="0"/>"#,
            r#"<BASEOFFSET offset="0x100" subtract="1"/>"#,
        );
        let imp = parse(&xml).expect("parse");
        assert_eq!(imp.base_offset, 0x100);
        // 0x288 - 0x100
        assert_eq!(imp.maps[0].address, 0x188);
    }

    #[test]
    fn signed_and_little_endian_flags() {
        let xml = SAMPLE.replace(
            r#"mmedelementsizebits="8" mmedrowcount="16" mmedcolcount="11" mmedtypeflags="0x00""#,
            r#"mmedelementsizebits="16" mmedrowcount="16" mmedcolcount="11" mmedtypeflags="0x03""#,
        );
        let imp = parse(&xml).expect("parse");
        let m = &imp.maps[0];
        assert_eq!(m.data_type, DataType::S16);
        assert_eq!(m.endian, Endian::Little);
    }

    #[test]
    fn nonlinear_math_is_flagged_not_guessed() {
        let xml = SAMPLE.replace("(45*X)/127-20", "X*X+5");
        let imp = parse(&xml).expect("parse");
        let m = &imp.maps[0];
        assert_eq!(m.scaling, Scaling::default());
        assert!(m.notes.contains("non-linear"), "notes: {}", m.notes);
    }

    // --- Task 1: checked address rebasing (no wrapping underflow/overflow) ---

    #[test]
    fn resolve_addr_checked_arithmetic() {
        // Below base under subtract must refuse, never wrap.
        assert_eq!(resolve_addr(0x10, 0x100, true), None);
        // Exactly at the base lands at 0 and is valid.
        assert_eq!(resolve_addr(0x100, 0x100, true), Some(0));
        // Normal subtract.
        assert_eq!(resolve_addr(0x288, 0x100, true), Some(0x188));
        // Add overflow must refuse.
        assert_eq!(resolve_addr(0xFFFF_FFFF, 0x10, false), None);
        // Normal add, including the real DQ BASEOFFSET=0x30000 convention.
        assert_eq!(resolve_addr(0x184FC, 0x30000, false), Some(0x484FC));
    }

    /// Build a minimal single-table XDF with a given BASEOFFSET and z address.
    fn table_with_base(z_addr: &str, offset: &str, subtract: &str) -> String {
        format!(
            r#"<XDFFORMAT version="1.60"><XDFHEADER><deftitle>T</deftitle>
            <BASEOFFSET offset="{offset}" subtract="{subtract}"/></XDFHEADER>
            <XDFTABLE uniqueid="0x1"><title>Tbl</title><XDFAXIS id="z">
            <EMBEDDEDDATA mmedaddress="{z_addr}" mmedelementsizebits="8" mmedrowcount="2" mmedcolcount="2" mmedtypeflags="0x00"/>
            <MATH equation="X"/></XDFAXIS></XDFTABLE></XDFFORMAT>"#
        )
    }

    #[test]
    fn rebasing_below_base_is_refused_not_wrapped() {
        // Address 0x10 below base 0x100 with subtract: the old code produced
        // 0xFFFFFF10; now the table is skipped with a reason, not wrapped. A
        // second, valid table (0x400 - 0x100 = 0x300) still imports — one bad
        // mapping must not drop the rest of the file.
        let xml = r#"<XDFFORMAT version="1.60"><XDFHEADER><deftitle>T</deftitle>
          <BASEOFFSET offset="0x100" subtract="1"/></XDFHEADER>
          <XDFTABLE uniqueid="0x1"><title>Bad</title><XDFAXIS id="z">
            <EMBEDDEDDATA mmedaddress="0x10" mmedelementsizebits="8" mmedrowcount="2" mmedcolcount="2" mmedtypeflags="0x00"/>
            <MATH equation="X"/></XDFAXIS></XDFTABLE>
          <XDFTABLE uniqueid="0x2"><title>Good</title><XDFAXIS id="z">
            <EMBEDDEDDATA mmedaddress="0x400" mmedelementsizebits="8" mmedrowcount="2" mmedcolcount="2" mmedtypeflags="0x00"/>
            <MATH equation="X"/></XDFAXIS></XDFTABLE></XDFFORMAT>"#;
        let imp = parse(xml).expect("parse");
        assert_eq!(imp.maps.len(), 1, "only the valid table imports");
        assert_eq!(imp.maps[0].name, "Good");
        assert_eq!(imp.maps[0].address, 0x300);
        assert!(
            imp.warnings
                .iter()
                .any(|w| w.contains("subtract underflow")),
            "expected an underflow warning, got {:?}",
            imp.warnings
        );
        // No wrapped giant address leaked anywhere.
        assert!(!imp.maps.iter().any(|m| m.address > 0xFF00_0000));
    }

    #[test]
    fn rebasing_at_base_yields_zero_address() {
        let xml = table_with_base("0x100", "0x100", "1");
        let imp = parse(&xml).expect("parse");
        assert_eq!(imp.maps.len(), 1);
        assert_eq!(imp.maps[0].address, 0);
        assert!(imp.warnings.is_empty(), "at-base must be clean");
    }

    #[test]
    fn rebasing_normal_add_base_preserved() {
        // The real DQ F49M path: offset 0x30000, add, 0x184FC -> 0x484FC.
        let xml = table_with_base("0x184FC", "196608", "0");
        let imp = parse(&xml).expect("parse");
        assert_eq!(imp.maps.len(), 1);
        assert_eq!(imp.maps[0].address, 0x484FC);
        assert!(imp.warnings.is_empty());
    }

    // --- Task 2: categorical axis labels are preserved as a reported loss ---

    #[test]
    fn categorical_axis_labels_are_reported_not_silent() {
        // A y-axis of gear-shift names (like the DQ250 shift tables) plus a
        // normal numeric x-axis. The labels can't be numeric breakpoints, but
        // the loss must be surfaced, not swallowed.
        let xml = r#"<XDFFORMAT version="1.60"><XDFHEADER><deftitle>TCU</deftitle>
          <BASEOFFSET offset="0" subtract="0"/></XDFHEADER>
          <XDFTABLE uniqueid="0x2"><title>Shift Times</title>
          <XDFAXIS id="x"><units>%</units><indexcount>2</indexcount>
            <LABEL index="0" value="0"/><LABEL index="1" value="100"/><MATH equation="X"/></XDFAXIS>
          <XDFAXIS id="y"><units>Gear</units><indexcount>5</indexcount>
            <LABEL index="0" value="1-&gt;2"/><LABEL index="1" value="2-&gt;3"/>
            <LABEL index="2" value="3-&gt;4"/><LABEL index="3" value="4-&gt;5"/>
            <LABEL index="4" value="5-&gt;6"/><MATH equation="X"/></XDFAXIS>
          <XDFAXIS id="z"><units>ms</units>
            <EMBEDDEDDATA mmedaddress="0x484fc" mmedelementsizebits="16" mmedrowcount="5" mmedcolcount="2" mmedtypeflags="0x02"/>
            <MATH equation="X"/></XDFAXIS></XDFTABLE></XDFFORMAT>"#;
        let imp = parse(xml).expect("parse");
        assert_eq!(imp.maps.len(), 1);
        let m = &imp.maps[0];
        // Data still imports with its dimensions.
        assert_eq!((m.rows, m.cols), (5, 2));
        // The y dimension is preserved (count 5) even though the labels are text.
        let y = m.y_axis.as_ref().expect("y axis kept as a dimension");
        assert_eq!(y.count, 5);
        assert!(y.values.is_empty(), "text labels are not numeric values");
        // The loss is in the map notes AND the structured warnings.
        assert!(
            m.notes.contains("categorical"),
            "map notes must mention the categorical loss: {}",
            m.notes
        );
        let w = imp
            .warnings
            .iter()
            .find(|w| w.contains("categorical"))
            .expect("a categorical warning");
        assert!(
            w.contains("1->2") && w.contains("5->6"),
            "warning lists labels: {w}"
        );
        // The numeric x-axis is untouched (no false categorical report).
        assert!(!m.notes.contains("x-axis has"));
    }

    // --- Task 3: scalar constants imported as a distinct feature ---

    #[test]
    fn scalar_constants_are_imported_separately() {
        let xml = r#"<XDFFORMAT version="1.60"><XDFHEADER><deftitle>TCU</deftitle>
          <BASEOFFSET offset="0" subtract="0"/></XDFHEADER>
          <XDFCONSTANT uniqueid="0xB"><title>Oil temp disable</title>
            <EMBEDDEDDATA mmedaddress="0x6B8EE" mmedelementsizebits="16" mmedtypeflags="0x00"/>
            <units>degC</units><rangehigh>150.0</rangehigh>
            <MATH equation="X*0.1"/></XDFCONSTANT>
          <XDFTABLE uniqueid="0x3"><title>A Table</title><XDFAXIS id="z">
            <EMBEDDEDDATA mmedaddress="0x1000" mmedelementsizebits="8" mmedrowcount="2" mmedcolcount="2" mmedtypeflags="0x00"/>
            <MATH equation="X"/></XDFAXIS></XDFTABLE></XDFFORMAT>"#;
        let imp = parse(xml).expect("parse");
        // The table stays a map; the constant is NOT in maps.
        assert_eq!(imp.maps.len(), 1);
        assert_eq!(imp.constants.len(), 1);
        let c = &imp.constants[0];
        assert_eq!(c.name, "Oil temp disable");
        assert_eq!(c.address, 0x6B8EE);
        assert_eq!(c.data_type, DataType::U16);
        assert_eq!(c.endian, Endian::Big);
        assert_eq!(c.unit, "degC");
        assert_eq!(c.max, Some(150.0));
        assert!((c.scaling.factor - 0.1).abs() < 1e-12);
    }

    #[test]
    fn constants_only_file_is_not_empty() {
        // A file with only constants (no tables) must still import.
        let xml = r#"<XDFFORMAT version="1.60"><XDFHEADER><deftitle>C</deftitle></XDFHEADER>
          <XDFCONSTANT uniqueid="0x1"><title>K</title>
            <EMBEDDEDDATA mmedaddress="0x40" mmedelementsizebits="8" mmedtypeflags="0x00"/>
            <MATH equation="X"/></XDFCONSTANT></XDFFORMAT>"#;
        let imp = parse(xml).expect("constants-only must parse");
        assert!(imp.maps.is_empty());
        assert_eq!(imp.constants.len(), 1);
    }

    #[test]
    fn constant_address_underflow_is_refused() {
        let xml = r#"<XDFFORMAT version="1.60"><XDFHEADER><deftitle>C</deftitle>
          <BASEOFFSET offset="0x100" subtract="1"/></XDFHEADER>
          <XDFCONSTANT uniqueid="0x1"><title>K</title>
            <EMBEDDEDDATA mmedaddress="0x10" mmedelementsizebits="8" mmedtypeflags="0x00"/>
            <MATH equation="X"/></XDFCONSTANT>
          <XDFTABLE uniqueid="0x3"><title>T</title><XDFAXIS id="z">
            <EMBEDDEDDATA mmedaddress="0x1000" mmedelementsizebits="8" mmedrowcount="2" mmedcolcount="2" mmedtypeflags="0x00"/>
            <MATH equation="X"/></XDFAXIS></XDFTABLE></XDFFORMAT>"#;
        let imp = parse(xml).expect("parse");
        assert!(imp.constants.is_empty(), "underflowing constant skipped");
        assert!(imp
            .warnings
            .iter()
            .any(|w| w.contains("constant") && w.contains("underflow")));
    }

    // --- Task 4: swap-XY (0x04) flag is detected and flagged, never silently
    // transposed (investigate-only outcome; see README) ---

    #[test]
    fn swap_xy_flag_is_flagged_but_not_transposed() {
        // Asymmetric 3x5 table with the 0x04 swap flag on z. The reference
        // RomRaider converter sets swapxy=true (a storage-order transpose), but
        // OpenTune cannot represent that losslessly, so the dimensions MUST be
        // left as-is (not silently flipped to 5x3) and the risk surfaced.
        let xml = r#"<XDFFORMAT version="1.60"><XDFHEADER><deftitle>BMW</deftitle>
          <BASEOFFSET offset="0" subtract="0"/></XDFHEADER>
          <XDFTABLE uniqueid="0x4"><title>Shift map</title><XDFAXIS id="z">
            <EMBEDDEDDATA mmedaddress="0x6F47E" mmedelementsizebits="16" mmedrowcount="3" mmedcolcount="5" mmedtypeflags="0x04"/>
            <MATH equation="X"/></XDFAXIS></XDFTABLE></XDFFORMAT>"#;
        let imp = parse(xml).expect("parse");
        let m = &imp.maps[0];
        // Dimensions untouched: NO silent transpose.
        assert_eq!((m.rows, m.cols), (3, 5));
        // 0x04 has no 0x02 bit -> big-endian, unsigned (matches RomRaider).
        assert_eq!(m.endian, Endian::Big);
        assert_eq!(m.data_type, DataType::U16);
        // The flag is surfaced in both the map notes and the warnings.
        assert!(m.notes.contains("swap-XY"), "notes: {}", m.notes);
        assert!(imp
            .warnings
            .iter()
            .any(|w| w.contains("swap-XY") && w.contains("3x5")));
    }

    #[test]
    fn no_swap_flag_means_no_swap_warning() {
        // The plain SAMPLE (z typeflags 0x00) must not produce a swap warning.
        let imp = parse(SAMPLE).expect("parse");
        assert!(!imp.warnings.iter().any(|w| w.contains("swap-XY")));
        assert!(!imp.maps[0].notes.contains("swap-XY"));
    }
}
