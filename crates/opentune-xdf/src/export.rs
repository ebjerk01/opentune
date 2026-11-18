//! **OpenTune → TunerPro XDF** export (modern XML format).
//!
//! The inverse of [`crate::xml`]: given a set of [`MapDef`]s, emit a modern
//! `<XDFFORMAT>` document that TunerPro (and this crate's own importer) can read
//! back. The output round-trips through [`crate::parse_xdf`]: address,
//! dimensions, element type, byte order, linear scaling, unit, min/max and the
//! x/y axes all survive.
//!
//! ## Honesty / limitations
//!
//! * OpenTune scaling is strictly linear, so the emitted `<MATH>` is always
//!   affine (`X`, `X*f`, `X*f+o`). A map that was imported *pre-scaled* from a
//!   non-linear source keeps its original equation only in its `notes`, which
//!   are written to `<description>` — the `<MATH>` is identity. Such a map is
//!   not reconstructed into its original non-linear form.
//! * `BASEOFFSET` is written as `0 / subtract=0` and addresses are emitted
//!   directly, so a decoder does not need the base to locate the data.

use crate::typeflags;
use opentune_core::ecudef::{AxisDef, DataType, Endian, MapDef};

/// Element size in bits for a stored data type.
fn size_bits(dt: DataType) -> u32 {
    dt.byte_len() * 8
}

/// Build the TunerPro `mmedtypeflags` word for a data type + byte order.
fn type_flags(dt: DataType, endian: Endian) -> u32 {
    let mut f = 0u32;
    if matches!(dt, DataType::S8 | DataType::S16 | DataType::S32) {
        f |= typeflags::SIGNED;
    }
    if matches!(endian, Endian::Little) {
        f |= typeflags::LSB_FIRST;
    }
    if matches!(dt, DataType::F32) {
        f |= typeflags::FLOAT;
    }
    f
}

/// Shortest round-trippable decimal for an `f64` (Rust's `Display` already does
/// this).
fn num(v: f64) -> String {
    format!("{v}")
}

/// Build an affine equation string `X`, `X*f`, `X+o`, or `X*f±o`.
fn equation(factor: f64, offset: f64) -> String {
    let base = if (factor - 1.0).abs() < 1e-12 {
        "X".to_string()
    } else {
        format!("X*{}", num(factor))
    };
    if offset.abs() < 1e-12 {
        base
    } else if offset > 0.0 {
        format!("{base}+{}", num(offset))
    } else {
        format!("{base}-{}", num(-offset))
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn push_axis(out: &mut String, id: &str, ax: &AxisDef, fallback_count: u32) {
    let count = ax.count.max(fallback_count).max(1);
    out.push_str(&format!("    <XDFAXIS id=\"{id}\">\n"));
    if !ax.unit.is_empty() {
        out.push_str(&format!("      <units>{}</units>\n", escape(&ax.unit)));
    }
    out.push_str(&format!("      <indexcount>{count}</indexcount>\n"));
    if !ax.values.is_empty() {
        // Inline physical breakpoints.
        for (i, v) in ax.values.iter().enumerate() {
            out.push_str(&format!(
                "      <LABEL index=\"{i}\" value=\"{}\"/>\n",
                num(*v)
            ));
        }
    } else if let Some(addr) = ax.address {
        // Stored axis.
        out.push_str(&format!(
            "      <EMBEDDEDDATA mmedaddress=\"0x{:X}\" mmedelementsizebits=\"{}\" mmedtypeflags=\"0x{:X}\"/>\n",
            addr,
            size_bits(ax.data_type),
            type_flags(ax.data_type, ax.endian)
        ));
        // Declare the source explicitly: embedded (read from the image).
        out.push_str("      <embedinfo type=\"1\"/>\n");
    }
    out.push_str(&format!(
        "      <MATH equation=\"{}\"/>\n",
        escape(&equation(ax.scaling.factor, ax.scaling.offset))
    ));
    out.push_str("    </XDFAXIS>\n");
}

/// Serialize maps as a modern XML XDF document.
pub fn export_xdf(title: Option<&str>, maps: &[MapDef]) -> String {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    out.push_str("<XDFFORMAT version=\"1.60\">\n");
    out.push_str("  <XDFHEADER>\n");
    out.push_str("    <flags>0x1</flags>\n");
    out.push_str(&format!(
        "    <deftitle>{}</deftitle>\n",
        escape(title.unwrap_or("OpenTune export"))
    ));
    out.push_str("    <BASEOFFSET offset=\"0\" subtract=\"0\"/>\n");
    out.push_str("    <DEFAULTS datasizeinbits=\"8\" sigdigits=\"2\" outputtype=\"1\" signed=\"0\" lsbfirst=\"0\" float=\"0\"/>\n");
    out.push_str("  </XDFHEADER>\n");

    for (i, m) in maps.iter().enumerate() {
        out.push_str(&format!(
            "  <XDFTABLE uniqueid=\"0x{:X}\" flags=\"0x0\">\n",
            0x1000 + i
        ));
        out.push_str(&format!("    <title>{}</title>\n", escape(&m.name)));
        if !m.notes.is_empty() {
            out.push_str(&format!(
                "    <description>{}</description>\n",
                escape(&m.notes)
            ));
        }
        if let Some(x) = &m.x_axis {
            push_axis(&mut out, "x", x, m.cols);
        }
        if let Some(y) = &m.y_axis {
            push_axis(&mut out, "y", y, m.rows);
        }
        // z axis carries the table data.
        out.push_str("    <XDFAXIS id=\"z\">\n");
        if !m.unit.is_empty() {
            out.push_str(&format!("      <units>{}</units>\n", escape(&m.unit)));
        }
        out.push_str(&format!(
            "      <EMBEDDEDDATA mmedaddress=\"0x{:X}\" mmedelementsizebits=\"{}\" mmedrowcount=\"{}\" mmedcolcount=\"{}\" mmedtypeflags=\"0x{:X}\"/>\n",
            m.address,
            size_bits(m.data_type),
            m.rows,
            m.cols,
            type_flags(m.data_type, m.endian)
        ));
        if let Some(v) = m.min {
            out.push_str(&format!("      <min>{}</min>\n", num(v)));
        }
        if let Some(v) = m.max {
            out.push_str(&format!("      <max>{}</max>\n", num(v)));
        }
        out.push_str(&format!(
            "      <MATH equation=\"{}\"/>\n",
            escape(&equation(m.scaling.factor, m.scaling.offset))
        ));
        out.push_str("    </XDFAXIS>\n");
        out.push_str("  </XDFTABLE>\n");
    }

    out.push_str("</XDFFORMAT>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_xdf;
    use opentune_core::ecudef::{AxisDef, Scaling};

    fn sample_maps() -> Vec<MapDef> {
        vec![
            MapDef {
                name: "Injection Quantity Limiter".into(),
                notes: String::new(),
                address: 0x1D30BC,
                data_type: DataType::U16,
                endian: Endian::Big,
                rows: 3,
                cols: 13,
                scaling: Scaling {
                    factor: 0.01,
                    offset: 0.0,
                },
                unit: "mg/stroke".into(),
                x_axis: Some(AxisDef {
                    name: "air mass".into(),
                    unit: "mg".into(),
                    count: 13,
                    values: vec![],
                    address: Some(0x1C0000),
                    data_type: DataType::U16,
                    endian: Endian::Big,
                    scaling: Scaling {
                        factor: 1.0,
                        offset: 0.0,
                    },
                    ..Default::default()
                }),
                y_axis: Some(AxisDef {
                    name: "rpm".into(),
                    unit: "rpm".into(),
                    count: 3,
                    values: vec![800.0, 1200.0, 1600.0],
                    scaling: Scaling {
                        factor: 1.0,
                        offset: 0.0,
                    },
                    ..Default::default()
                }),
                min: Some(0.0),
                max: Some(80.0),
            },
            MapDef {
                name: "Spark Advance".into(),
                notes: String::new(),
                address: 0x2000,
                data_type: DataType::S16,
                endian: Endian::Little,
                rows: 1,
                cols: 8,
                scaling: Scaling {
                    factor: 0.75,
                    offset: -20.0,
                },
                unit: "deg".into(),
                x_axis: None,
                y_axis: None,
                min: None,
                max: None,
            },
        ]
    }

    #[test]
    fn round_trips_through_importer() {
        let maps = sample_maps();
        let xml = export_xdf(Some("Fleet"), &maps);
        let back = parse_xdf(&xml).expect("re-import");
        assert_eq!(back.title.as_deref(), Some("Fleet"));
        // Our own export must re-import strictly clean.
        assert!(
            back.strict_failures().is_empty(),
            "{:?}",
            back.strict_failures()
        );
        assert_eq!(back.maps.len(), maps.len());

        for (a, b) in maps.iter().zip(back.maps.iter()) {
            assert_eq!(a.name, b.name, "name");
            assert_eq!(a.address, b.address, "address");
            assert_eq!(a.rows, b.rows, "rows");
            assert_eq!(a.cols, b.cols, "cols");
            assert_eq!(a.data_type, b.data_type, "data_type");
            assert_eq!(a.endian, b.endian, "endian");
            assert_eq!(a.unit, b.unit, "unit");
            assert_eq!(a.min, b.min, "min");
            assert_eq!(a.max, b.max, "max");
            assert!(
                (a.scaling.factor - b.scaling.factor).abs() < 1e-9,
                "factor {} != {}",
                a.scaling.factor,
                b.scaling.factor
            );
            assert!(
                (a.scaling.offset - b.scaling.offset).abs() < 1e-9,
                "offset {} != {}",
                a.scaling.offset,
                b.scaling.offset
            );
        }

        // The stored x-axis survived with its address + type.
        let x = back.maps[0].x_axis.as_ref().expect("x axis");
        assert_eq!(x.address, Some(0x1C0000));
        assert_eq!(x.data_type, DataType::U16);
        // The inline y-axis labels survived.
        let y = back.maps[0].y_axis.as_ref().expect("y axis");
        assert_eq!(y.values, vec![800.0, 1200.0, 1600.0]);
    }

    #[test]
    fn negative_offset_equation_is_well_formed() {
        // Must not emit "X*0.75+-20"; the importer has to read it back exactly.
        let eq = equation(0.75, -20.0);
        assert_eq!(eq, "X*0.75-20");
        assert!(!eq.contains("+-"));
    }

    #[test]
    fn identity_scaling_is_just_x() {
        assert_eq!(equation(1.0, 0.0), "X");
        assert_eq!(equation(1.0, 5.0), "X+5");
        assert_eq!(equation(2.0, 0.0), "X*2");
    }
}
