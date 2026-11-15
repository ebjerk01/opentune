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
use crate::{datatype_from, endian_from, parse_u32, Error, Result, XdfImport};
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
    for table in root.descendants().filter(|n| has_tag(*n, "XDFTABLE")) {
        if let Some(m) = parse_table(table, base_offset, base_subtract, default_lsb) {
            maps.push(m);
        }
    }

    if maps.is_empty() {
        return Err(Error::Empty);
    }
    Ok(XdfImport {
        title,
        base_offset,
        maps,
    })
}

fn parse_table(
    table: Node,
    base_offset: u32,
    base_subtract: bool,
    default_lsb: bool,
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

    let address = resolve_addr(mmedaddr, base_offset, base_subtract);

    // Scaling + unit from the z axis.
    let (scaling, nonlinear) = scaling_from_axis(zed);
    if let Some(eq) = nonlinear {
        notes.push(format!("non-linear MATH imported pre-scaled: {eq}"));
    }
    let unit = child_text(zed, "units").unwrap_or_default();

    let min = child_text(zed, "min").and_then(|s| s.trim().parse::<f64>().ok());
    let max = child_text(zed, "max").and_then(|s| s.trim().parse::<f64>().ok());

    let x_axis = x.and_then(|ax| parse_axis(ax, cols, base_offset, base_subtract, default_lsb));
    let y_axis = y.and_then(|ax| parse_axis(ax, rows, base_offset, base_subtract, default_lsb));

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

/// Build an [`AxisDef`] from an `<XDFAXIS id="x|y">`.
///
/// Priority: explicit `<LABEL>` values (inline axis) → embedded stored axis
/// (address + type) → a bare count/unit descriptor.
fn parse_axis(
    ax: Node,
    fallback_count: u32,
    base_offset: u32,
    base_subtract: bool,
    default_lsb: bool,
) -> Option<AxisDef> {
    let unit = child_text(ax, "units").unwrap_or_default();
    let name = child_text(ax, "title")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| attr(ax, "id").unwrap_or("").to_string());

    let (scaling, _) = scaling_from_axis(ax);

    // Inline labels.
    let mut values: Vec<f64> = Vec::new();
    for label in ax.children().filter(|n| has_tag(*n, "LABEL")) {
        if let Some(v) = attr(label, "value").and_then(|s| s.trim().parse::<f64>().ok()) {
            values.push(v);
        }
    }

    let index_count = child_text(ax, "indexcount").and_then(|s| parse_u32(&s));

    // Embedded stored axis.
    let emb = child(ax, "EMBEDDEDDATA");
    let emb_addr = emb
        .and_then(|e| attr(e, "mmedaddress"))
        .and_then(parse_u32)
        .filter(|a| *a != 0);

    let count = index_count
        .or(if values.is_empty() {
            None
        } else {
            Some(values.len() as u32)
        })
        .unwrap_or(fallback_count)
        .max(1);

    if !values.is_empty() {
        return Some(AxisDef {
            name,
            unit,
            count,
            values,
            scaling,
            ..Default::default()
        });
    }

    if let (Some(e), Some(addr)) = (emb, emb_addr) {
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
        return Some(AxisDef {
            name,
            unit,
            count,
            address: Some(resolve_addr(addr, base_offset, base_subtract)),
            data_type: dt,
            endian,
            scaling,
            ..Default::default()
        });
    }

    // Nothing but a count/unit — still useful as a labelled dimension.
    if count > 1 || !unit.is_empty() {
        return Some(AxisDef {
            name,
            unit,
            count,
            scaling,
            ..Default::default()
        });
    }
    None
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

fn resolve_addr(mmed: u32, base: u32, subtract: bool) -> u32 {
    if subtract {
        mmed.wrapping_sub(base)
    } else {
        mmed.wrapping_add(base)
    }
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
}
