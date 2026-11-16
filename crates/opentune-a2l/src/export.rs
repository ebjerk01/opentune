//! **OpenTune → ASAM MCD-2 MC (A2L)** export.
//!
//! The inverse of [`crate::parse_a2l`]: given a set of [`MapDef`]s, emit a
//! valid A2L document (`ASAP2_VERSION` + `PROJECT`/`MODULE`) that this crate's
//! own importer — and other ASAM tooling — can read back. The output
//! round-trips through [`crate::parse_a2l`]: each map's address, dimensions,
//! element type, byte order, linear scaling, unit and limits are recovered, and
//! so are its axes (stored, inline and shared/reference).
//!
//! ## What is emitted per map
//!
//! * a dedicated `RECORD_LAYOUT` with a `FNC_VALUES` of the map's [`DataType`]
//!   and a `BYTE_ORDER` for its [`Endian`] (so endianness round-trips per map,
//!   independent of the module default);
//! * a `COMPU_METHOD` of type `LINEAR` whose `COEFFS_LINEAR a b` encode the
//!   map's `factor`/`offset` exactly;
//! * a `CHARACTERISTIC` (`VALUE`/`CURVE`/`MAP`/`VAL_BLK`) with the `ECU_ADDRESS`,
//!   dimensions, `LOWER/UPPER_LIMIT`, the record-layout and compu-method
//!   references, and an `AXIS_DESCR` per present axis;
//! * for a stored axis, an `AXIS_PTS` block (with its own deposit record layout
//!   and compu method) so the axis address/type/scaling survive.
//!
//! ## Honesty / limitations
//!
//! * OpenTune scaling is strictly linear, so the emitted `COMPU_METHOD` is always
//!   affine. A map imported *pre-scaled* from a non-linear source keeps its
//!   original conversion only in its `notes` (written to the `LongIdentifier`);
//!   it is **not** reconstructed into a non-linear `COMPU_METHOD`.
//! * A2L `CHARACTERISTIC` names are identifiers, so a map name containing spaces
//!   or punctuation is sanitised to a valid identifier (and uniquified if two
//!   collide). The original human name is preserved in the `LongIdentifier`.
//! * A2L requires numeric `LOWER_LIMIT`/`UPPER_LIMIT`; a map with no `min`/`max`
//!   is exported with `0`/`0`, so such a map re-imports with `Some(0.0)` limits
//!   rather than `None`.
//! * A 2-D map must carry both axes to round-trip its shape; a map that has only
//!   one of its two axes defined is exported as a `VAL_BLK` (dimensions preserved
//!   via `MATRIX_DIM`, the lone axis dropped with a comment) rather than
//!   fabricating the missing one.

use opentune_core::ecudef::{AxisDef, DataType, Endian, MapDef};

/// The A2L deposit keyword for an OpenTune [`DataType`] (inverse of the
/// importer's `datatype_of`).
fn datatype_keyword(dt: DataType) -> &'static str {
    match dt {
        DataType::U8 => "UBYTE",
        DataType::S8 => "SBYTE",
        DataType::U16 => "UWORD",
        DataType::S16 => "SWORD",
        DataType::U32 => "ULONG",
        DataType::S32 => "SLONG",
        DataType::F32 => "FLOAT32_IEEE",
    }
}

/// `MSB_FIRST` for big-endian, `MSB_LAST` for little-endian.
fn byte_order_keyword(e: Endian) -> &'static str {
    match e {
        Endian::Big => "MSB_FIRST",
        Endian::Little => "MSB_LAST",
    }
}

/// Shortest round-trippable decimal for an `f64` (Rust's `Display`).
fn num(v: f64) -> String {
    format!("{v}")
}

/// Wrap a string as an A2L double-quoted literal, doubling embedded quotes.
fn a2l_str(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Sanitise an arbitrary map name into a valid A2L identifier (keep
/// `[A-Za-z0-9_.]`, map everything else to `_`).
fn sanitize_ident(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        out.push_str("map");
    }
    out
}

/// Export ECU map definitions as an A2L (ASAM MCD-2 MC) document string.
///
/// `project`/`module` name the emitted `PROJECT`/`MODULE` (sanitised; defaulted
/// when `None`). The result parses back through [`crate::parse_a2l`].
pub fn export_a2l(maps: &[MapDef], project: Option<&str>, module: Option<&str>) -> String {
    let project = sanitize_ident(project.unwrap_or("OpenTuneExport"));
    let module = sanitize_ident(module.unwrap_or("OpenTune"));

    // Uniquify sanitised characteristic names so two maps never collide.
    let mut seen: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let idents: Vec<String> = maps
        .iter()
        .map(|m| {
            let base = sanitize_ident(&m.name);
            let n = seen.entry(base.clone()).or_insert(0);
            let id = if *n == 0 {
                base.clone()
            } else {
                format!("{base}_{n}")
            };
            *n += 1;
            id
        })
        .collect();

    let mut record_layouts = String::new();
    let mut compu_methods = String::new();
    let mut axis_pts = String::new();
    let mut characteristics = String::new();

    for (i, m) in maps.iter().enumerate() {
        emit_map(
            i,
            &idents[i],
            m,
            &mut record_layouts,
            &mut compu_methods,
            &mut axis_pts,
            &mut characteristics,
        );
    }

    let mut out = String::new();
    out.push_str("ASAP2_VERSION 1 71\n");
    out.push_str(&format!("/begin PROJECT {project} \"OpenTune export\"\n"));
    out.push_str(&format!("  /begin MODULE {module} \"\"\n"));
    out.push_str("    /begin MOD_COMMON \"\"\n");
    out.push_str("      BYTE_ORDER MSB_FIRST\n");
    out.push_str("    /end MOD_COMMON\n\n");
    out.push_str(&record_layouts);
    out.push_str(&compu_methods);
    out.push_str(&axis_pts);
    out.push_str(&characteristics);
    out.push_str("  /end MODULE\n");
    out.push_str("/end PROJECT\n");
    out
}

/// Which `CHARACTERISTIC` type best represents a map's shape, without
/// fabricating axes that were not defined.
enum Kind {
    Value,
    /// A 1-D curve carrying its single X axis.
    Curve,
    /// A 2-D map carrying both axes.
    Map,
    /// A multi-element block whose dimensions ride in `MATRIX_DIM`.
    ValBlk,
}

fn classify(m: &MapDef) -> Kind {
    if m.rows <= 1 && m.cols <= 1 {
        Kind::Value
    } else if m.rows <= 1 {
        if m.x_axis.is_some() {
            Kind::Curve
        } else {
            Kind::ValBlk
        }
    } else if m.x_axis.is_some() && m.y_axis.is_some() {
        Kind::Map
    } else {
        Kind::ValBlk
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_map(
    i: usize,
    ident: &str,
    m: &MapDef,
    record_layouts: &mut String,
    compu_methods: &mut String,
    axis_pts: &mut String,
    characteristics: &mut String,
) {
    let rl = format!("RL_{i}");
    let cm = format!("CM_{i}");
    let kind = classify(m);

    // Value record layout + compu method.
    record_layouts.push_str(&format!(
        "    /begin RECORD_LAYOUT {rl}\n      FNC_VALUES 1 {dt} COLUMN_DIR DIRECT\n      BYTE_ORDER {bo}\n    /end RECORD_LAYOUT\n",
        dt = datatype_keyword(m.data_type),
        bo = byte_order_keyword(m.endian),
    ));
    compu_methods.push_str(&linear_compu_method(
        &cm,
        m.scaling.factor,
        m.scaling.offset,
        &m.unit,
    ));

    let kind_kw = match kind {
        Kind::Value => "VALUE",
        Kind::Curve => "CURVE",
        Kind::Map => "MAP",
        Kind::ValBlk => "VAL_BLK",
    };
    let long = if m.notes.is_empty() {
        m.name.clone()
    } else {
        format!("{} | {}", m.name, m.notes)
    };

    characteristics.push_str(&format!(
        "    /begin CHARACTERISTIC {ident} {long} {kind_kw} 0x{addr:X} {rl} 0 {cm} {lo} {hi}\n",
        long = a2l_str(&long),
        addr = m.address,
        lo = num(m.min.unwrap_or(0.0)),
        hi = num(m.max.unwrap_or(0.0)),
    ));

    match kind {
        Kind::ValBlk => {
            if m.x_axis.is_some() || m.y_axis.is_some() {
                characteristics.push_str(
                    "      /* note: a defined axis was dropped; this map had only one of two axes */\n",
                );
            }
            characteristics.push_str(&format!(
                "      MATRIX_DIM {cols} {rows} 1\n",
                cols = m.cols.max(1),
                rows = m.rows.max(1),
            ));
        }
        Kind::Value => {}
        Kind::Curve => {
            if let Some(x) = &m.x_axis {
                characteristics.push_str(&emit_axis_descr(
                    i,
                    'x',
                    x,
                    m.cols.max(1),
                    compu_methods,
                    record_layouts,
                    axis_pts,
                ));
            }
        }
        Kind::Map => {
            if let Some(x) = &m.x_axis {
                characteristics.push_str(&emit_axis_descr(
                    i,
                    'x',
                    x,
                    m.cols.max(1),
                    compu_methods,
                    record_layouts,
                    axis_pts,
                ));
            }
            if let Some(y) = &m.y_axis {
                characteristics.push_str(&emit_axis_descr(
                    i,
                    'y',
                    y,
                    m.rows.max(1),
                    compu_methods,
                    record_layouts,
                    axis_pts,
                ));
            }
        }
    }

    characteristics.push_str("    /end CHARACTERISTIC\n");
}

/// A `LINEAR` compu method encoding `physical = factor*raw + offset`.
fn linear_compu_method(name: &str, factor: f64, offset: f64, unit: &str) -> String {
    format!(
        "    /begin COMPU_METHOD {name} \"\" LINEAR \"%.6\" {unit}\n      COEFFS_LINEAR {f} {o}\n    /end COMPU_METHOD\n",
        unit = a2l_str(unit),
        f = num(factor),
        o = num(offset),
    )
}

/// Emit one `AXIS_DESCR` (returned as a string) and, for a stored axis, append
/// the supporting `COMPU_METHOD`/`RECORD_LAYOUT`/`AXIS_PTS` blocks.
#[allow(clippy::too_many_arguments)]
fn emit_axis_descr(
    i: usize,
    which: char,
    ax: &AxisDef,
    fallback_count: u32,
    compu_methods: &mut String,
    record_layouts: &mut String,
    axis_pts: &mut String,
) -> String {
    let count = ax.count.max(fallback_count).max(1);
    let input = if ax.name.is_empty() {
        "NO_INPUT_QUANTITY".to_string()
    } else {
        sanitize_ident(&ax.name)
    };
    // Each axis gets its own compu method so its unit + scaling round-trip.
    let cm = format!("CM_{i}_{which}");
    compu_methods.push_str(&linear_compu_method(
        &cm,
        ax.scaling.factor,
        ax.scaling.offset,
        &ax.unit,
    ));

    let mut out = String::new();
    out.push_str("      /begin AXIS_DESCR\n");

    if !ax.values.is_empty() {
        // Inline physical breakpoints → FIX_AXIS + FIX_AXIS_PAR_LIST.
        out.push_str(&format!(
            "        FIX_AXIS {input} {cm} {count} 0 0\n        /begin FIX_AXIS_PAR_LIST\n          {vals}\n        /end FIX_AXIS_PAR_LIST\n",
            vals = ax
                .values
                .iter()
                .map(|v| num(*v))
                .collect::<Vec<_>>()
                .join(" "),
        ));
    } else if let Some(addr) = ax.address {
        // Stored axis → COM_AXIS referencing a dedicated AXIS_PTS block.
        let ap = format!("AX_{i}_{which}");
        let arl = format!("RL_{i}_{which}");
        let apx = if which == 'y' {
            "AXIS_PTS_Y"
        } else {
            "AXIS_PTS_X"
        };
        record_layouts.push_str(&format!(
            "    /begin RECORD_LAYOUT {arl}\n      {apx} 1 {dt} INDEX_INCR DIRECT\n      BYTE_ORDER {bo}\n    /end RECORD_LAYOUT\n",
            dt = datatype_keyword(ax.data_type),
            bo = byte_order_keyword(ax.endian),
        ));
        axis_pts.push_str(&format!(
            "    /begin AXIS_PTS {ap} \"\" 0x{addr:X} {input} {arl} 0 {cm} {count} 0 0\n    /end AXIS_PTS\n",
        ));
        out.push_str(&format!(
            "        COM_AXIS {input} {cm} {count} 0 0\n        AXIS_PTS_REF {ap}\n",
        ));
    } else if let Some(axis_ref) = &ax.axis_ref {
        // Shared/reference axis → COM_AXIS pointing at an external name.
        out.push_str(&format!(
            "        COM_AXIS {input} {cm} {count} 0 0\n        AXIS_PTS_REF {r}\n",
            r = sanitize_ident(axis_ref),
        ));
    } else {
        // Dimension only (count, no breakpoints): a COM_AXIS with an unresolved
        // reference preserves the point count on re-import.
        out.push_str(&format!("        COM_AXIS {input} {cm} {count} 0 0\n"));
    }

    out.push_str("      /end AXIS_DESCR\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_a2l;
    use opentune_core::ecudef::Scaling;

    fn sample_maps() -> Vec<MapDef> {
        vec![
            // A 2-D MAP: stored X axis, inline Y axis.
            MapDef {
                name: "inj_q_limiter".into(),
                notes: String::new(),
                address: 0x1D30BC,
                data_type: DataType::U16,
                endian: Endian::Big,
                rows: 16,
                cols: 13,
                scaling: Scaling {
                    factor: 0.01,
                    offset: 0.0,
                },
                unit: "mg/stroke".into(),
                x_axis: Some(AxisDef {
                    name: "air_mass".into(),
                    unit: "mg".into(),
                    count: 13,
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
                    count: 16,
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
            // A scalar VALUE with a negative offset.
            MapDef {
                name: "spark_advance".into(),
                notes: String::new(),
                address: 0x2000,
                data_type: DataType::S16,
                endian: Endian::Little,
                rows: 1,
                cols: 1,
                scaling: Scaling {
                    factor: 0.75,
                    offset: -20.0,
                },
                unit: "deg".into(),
                x_axis: None,
                y_axis: None,
                min: Some(-30.0),
                max: Some(60.0),
            },
            // A 1-D CURVE with a shared/reference X axis.
            MapDef {
                name: "injector_lin".into(),
                notes: String::new(),
                address: 0x3000,
                data_type: DataType::U8,
                endian: Endian::Big,
                rows: 1,
                cols: 12,
                scaling: Scaling {
                    factor: 1.0,
                    offset: 0.0,
                },
                unit: "us".into(),
                x_axis: Some(AxisDef {
                    name: "voltage".into(),
                    unit: "V".into(),
                    count: 12,
                    axis_ref: Some("shared_voltage".into()),
                    ..Default::default()
                }),
                y_axis: None,
                min: Some(0.0),
                max: Some(255.0),
            },
            // A VAL_BLK (no axes): dimensions ride in MATRIX_DIM.
            MapDef {
                name: "cylinder_trims".into(),
                notes: String::new(),
                address: 0x4000,
                data_type: DataType::S8,
                endian: Endian::Little,
                rows: 3,
                cols: 4,
                scaling: Scaling {
                    factor: 0.1,
                    offset: 0.0,
                },
                unit: "%".into(),
                x_axis: None,
                y_axis: None,
                min: Some(-12.8),
                max: Some(12.7),
            },
        ]
    }

    #[test]
    fn round_trips_through_importer() {
        let maps = sample_maps();
        let a2l = export_a2l(&maps, Some("Fleet"), Some("Mod1"));
        let back = parse_a2l(&a2l).expect("re-import");
        assert_eq!(back.project.as_deref(), Some("Fleet"));
        assert_eq!(back.module.as_deref(), Some("Mod1"));
        assert_eq!(back.maps.len(), maps.len(), "map count");

        for (a, b) in maps.iter().zip(back.maps.iter()) {
            assert_eq!(a.name, b.name, "name");
            assert_eq!(a.address, b.address, "address of {}", a.name);
            assert_eq!(a.rows, b.rows, "rows of {}", a.name);
            assert_eq!(a.cols, b.cols, "cols of {}", a.name);
            assert_eq!(a.data_type, b.data_type, "data_type of {}", a.name);
            assert_eq!(a.endian, b.endian, "endian of {}", a.name);
            assert_eq!(a.unit, b.unit, "unit of {}", a.name);
            assert_eq!(a.min, b.min, "min of {}", a.name);
            assert_eq!(a.max, b.max, "max of {}", a.name);
            assert!(
                (a.scaling.factor - b.scaling.factor).abs() < 1e-9,
                "factor of {}: {} != {}",
                a.name,
                a.scaling.factor,
                b.scaling.factor
            );
            assert!(
                (a.scaling.offset - b.scaling.offset).abs() < 1e-9,
                "offset of {}: {} != {}",
                a.name,
                a.scaling.offset,
                b.scaling.offset
            );
        }

        // Map 0: the stored X axis survived with address + type + unit.
        let x = back.maps[0].x_axis.as_ref().expect("x axis");
        assert_eq!(x.address, Some(0x1C0000), "x addr");
        assert_eq!(x.count, 13, "x count");
        assert_eq!(x.data_type, DataType::U16, "x type");
        assert_eq!(x.endian, Endian::Big, "x endian");
        assert_eq!(x.unit, "mg", "x unit");
        // Map 0: the inline Y axis labels survived.
        let y = back.maps[0].y_axis.as_ref().expect("y axis");
        assert_eq!(y.values, vec![800.0, 1200.0, 1600.0], "y values");
        assert_eq!(y.count, 16, "y count");

        // Map 2: the shared/reference X axis kept its reference name + count.
        let cx = back.maps[2].x_axis.as_ref().expect("curve x axis");
        assert_eq!(cx.axis_ref.as_deref(), Some("shared_voltage"), "x ref");
        assert_eq!(cx.count, 12, "curve x count");
    }

    #[test]
    fn negative_offset_coeffs_round_trip() {
        let a2l = export_a2l(&sample_maps()[1..2], None, None);
        let back = parse_a2l(&a2l).expect("re-import");
        let m = &back.maps[0];
        assert!((m.scaling.factor - 0.75).abs() < 1e-12);
        assert!((m.scaling.offset + 20.0).abs() < 1e-12);
    }

    #[test]
    fn name_with_spaces_is_sanitised_but_valid() {
        let maps = vec![MapDef {
            name: "Injection Quantity".into(),
            address: 0x100,
            rows: 1,
            cols: 1,
            min: Some(0.0),
            max: Some(1.0),
            ..Default::default()
        }];
        let a2l = export_a2l(&maps, None, None);
        let back = parse_a2l(&a2l).expect("re-import sanitised name");
        assert_eq!(back.maps[0].name, "Injection_Quantity");
        // The original human name is preserved in the notes (LongIdentifier).
        assert!(back.maps[0].notes.contains("Injection Quantity"));
    }

    #[test]
    fn duplicate_names_are_uniquified() {
        let mk = || MapDef {
            name: "dup".into(),
            address: 0x10,
            rows: 1,
            cols: 1,
            min: Some(0.0),
            max: Some(1.0),
            ..Default::default()
        };
        let maps = vec![mk(), mk()];
        let a2l = export_a2l(&maps, None, None);
        let back = parse_a2l(&a2l).expect("re-import");
        assert_eq!(back.maps.len(), 2);
        assert_eq!(back.maps[0].name, "dup");
        assert_eq!(back.maps[1].name, "dup_1");
    }
}
