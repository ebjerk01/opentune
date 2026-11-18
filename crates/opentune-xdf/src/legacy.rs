//! Parser for the **legacy text** TunerPro XDF format (`XDF\n1.11…`).
//!
//! This is the older key/value encoding, still common in community archives:
//! ```text
//! %%HEADER%%
//!     001035 BaseOffset       =0
//!     001005 DefTitle         ="Daewoo Lanos"
//! %%END%%
//! %%TABLE%%
//!     040005 Title            ="Spark Advance"
//!     040100 Address          =0x288
//!     040200 ZEq              =(45*x)/127-20,TH|0|0|0|0|
//!     040300 Rows             =0x10
//!     040305 Cols             =0xB
//!     040320 XUnits           ="MAP"
//!     040330 ZUnits           ="deg"
//!     040350 XLabels          =00,10,20,30,40,50,60,70,80,90,105
//!     040360 YLabels          =400,800,1200,...
//!     040230 RangeLow         =0.0000
//!     040240 RangeHigh        =255.0000
//! %%END%%
//! ```
//!
//! Important limitation: the legacy format records a value's **display** type
//! (`OutType`) but not a reliable per-cell **storage** size for a table, so the
//! element type is defaulted to `u8`/big-endian and that fact is flagged in the
//! map's `notes`. Addresses, dimensions, scaling (`ZEq`), units and inline
//! axis labels are recovered faithfully.

use crate::math::{linear_from_equation, LinearFit};
use crate::report::{kinds, ObjectKind, ObjectReport, Tracker};
use crate::{parse_i64, parse_u32, Error, Result, XdfImport};
use opentune_core::ecudef::{AxisDef, DataType, Endian, MapDef, Scaling};
use std::collections::HashMap;

/// A parsed `%%...%%` block as a map from field name (e.g. `"Address"`) to its
/// raw value string.
type Block = HashMap<String, String>;

/// Parse a legacy text XDF document.
pub fn parse(src: &str) -> Result<XdfImport> {
    let mut header: Option<Block> = None;
    let mut tables: Vec<Block> = Vec::new();
    let mut flags = 0usize;

    let mut lines = src.lines().peekable();
    while let Some(line) = lines.next() {
        let marker = line.trim();
        let kind = match marker {
            "%%HEADER%%" => Some(false),
            "%%TABLE%%" => Some(true),
            "%%FLAG%%" => {
                flags += 1;
                None
            }
            _ => None,
        };
        let Some(is_table) = kind else {
            continue;
        };
        let mut block = Block::new();
        for body in lines.by_ref() {
            if body.trim() == "%%END%%" {
                break;
            }
            if let Some((k, v)) = parse_field(body) {
                block.insert(k, v);
            }
        }
        if is_table {
            tables.push(block);
        } else if header.is_none() {
            header = Some(block);
        }
    }

    let title = header
        .as_ref()
        .and_then(|h| h.get("DefTitle"))
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());

    // Signed: a negative BaseOffset subtracts (it used to saturate to 0).
    let base: i64 = header
        .as_ref()
        .and_then(|h| h.get("BaseOffset"))
        .and_then(|s| parse_i64(s))
        .unwrap_or(0);

    let mut maps = Vec::new();
    let mut reports: Vec<ObjectReport> = Vec::new();
    for t in &tables {
        let (m, r) = parse_table(t, base);
        maps.extend(m);
        reports.push(r);
    }
    for i in 0..flags {
        let mut tr = Tracker::new();
        tr.refuse(
            kinds::UNSUPPORTED_OBJECT,
            "legacy %%FLAG%% bit flag — not imported as a writable map",
        );
        reports.push(tr.finish(format!("flag_{i}"), ObjectKind::Flag));
    }

    if reports.is_empty() {
        return Err(Error::Empty);
    }
    let warnings = crate::warning_lines(&reports);
    Ok(XdfImport {
        title,
        base_offset: base.unsigned_abs().min(u64::from(u32::MAX)) as u32,
        base_subtract: base < 0,
        maps,
        warnings,
        reports,
        ..Default::default()
    })
}

/// Parse one body line `\t040100 Address          =0x288` into (`Address`,
/// `0x288`). Quotes around a value are stripped.
fn parse_field(line: &str) -> Option<(String, String)> {
    let (left, right) = line.split_once('=')?;
    // left = "\t040100 Address          " → last whitespace token is the name.
    let name = left.split_whitespace().next_back()?.to_string();
    let mut val = right.trim().to_string();
    if val.len() >= 2 && val.starts_with('"') && val.ends_with('"') {
        val = val[1..val.len() - 1].to_string();
    }
    Some((name, val))
}

fn parse_table(t: &Block, base: i64) -> (Option<MapDef>, ObjectReport) {
    let mut notes: Vec<String> = Vec::new();
    let mut tr = Tracker::new();

    let name = t
        .get("Title")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| t.get("UniqueID").map(|id| format!("table_{id}")))
        .unwrap_or_else(|| "unnamed table".to_string());

    let Some(raw_addr) = t.get("Address").and_then(|s| parse_u32(s)) else {
        tr.refuse(kinds::ADDRESS_MISSING, "no (valid) Address");
        return (None, tr.finish(name, ObjectKind::Table));
    };
    let address = match u32::try_from(i64::from(raw_addr) + base) {
        Ok(a) => a,
        Err(_) => {
            tr.refuse(
                kinds::ADDRESS_REBASE,
                format!("address 0x{raw_addr:X} could not be rebased against BaseOffset {base}"),
            );
            return (None, tr.finish(name, ObjectKind::Table));
        }
    };
    let rows = t.get("Rows").and_then(|s| parse_u32(s)).unwrap_or(1).max(1);
    let cols = t.get("Cols").and_then(|s| parse_u32(s)).unwrap_or(1).max(1);

    if let Some(d) = t.get("Desc").map(|s| s.trim()).filter(|s| !s.is_empty()) {
        notes.push(d.to_string());
    }

    // Legacy format: storage type is not reliably recorded; default u8/big.
    notes.push("legacy XDF: cell type defaulted to u8/big-endian (not stored by format)".into());
    tr.warn(
        kinds::LEGACY_CELL_TYPE,
        "legacy text XDF does not record the cell storage type; u8/big-endian assumed",
    );

    // Scaling from ZEq (strip the trailing ",TH|..." flag block).
    let (scaling, nonlinear) = match t.get("ZEq") {
        None => (Scaling::default(), None),
        Some(raw) => {
            let eq = raw.split_once(',').map(|(e, _)| e).unwrap_or(raw).trim();
            match linear_from_equation(eq) {
                LinearFit::Linear { factor, offset } => (Scaling { factor, offset }, None),
                LinearFit::NonLinear | LinearFit::Unparsable => {
                    (Scaling::default(), Some(eq.to_string()))
                }
            }
        }
    };
    if let Some(eq) = nonlinear {
        notes.push(format!("non-linear ZEq imported pre-scaled: {eq}"));
        tr.warn(
            kinds::MATH_NONLINEAR,
            format!("non-linear ZEq `{eq}` — imported pre-scaled (factor 1, offset 0)"),
        );
    }

    let unit = t.get("ZUnits").cloned().unwrap_or_default();

    let min = t.get("RangeLow").and_then(|s| s.trim().parse::<f64>().ok());
    let max = t
        .get("RangeHigh")
        .and_then(|s| s.trim().parse::<f64>().ok());

    let x_axis = axis_from_labels(t.get("XLabels"), t.get("XUnits"), t.get("XEq"), cols, "x");
    let y_axis = axis_from_labels(t.get("YLabels"), t.get("YUnits"), t.get("YEq"), rows, "y");
    for (id, ax, dim) in [("x", &x_axis, cols), ("y", &y_axis, rows)] {
        if let Some(a) = ax {
            if !a.values.is_empty() && a.values.len() as u32 != dim {
                tr.warn(
                    kinds::AXIS_COUNT_MISMATCH,
                    format!(
                        "{id}-axis has {} label(s) for a dimension of {dim}",
                        a.values.len()
                    ),
                );
            }
        }
    }

    let map = MapDef {
        name: name.clone(),
        notes: notes.join("; "),
        address,
        data_type: DataType::U8,
        endian: Endian::Big,
        rows,
        cols,
        scaling,
        unit,
        x_axis,
        y_axis,
        min,
        max,
    };
    (Some(map), tr.finish(name, ObjectKind::Table))
}

/// Build an inline axis from a comma-separated `*Labels` list.
fn axis_from_labels(
    labels: Option<&String>,
    units: Option<&String>,
    eq: Option<&String>,
    fallback_count: u32,
    id: &str,
) -> Option<AxisDef> {
    let unit = units.cloned().unwrap_or_default();
    let values: Vec<f64> = labels
        .map(|s| {
            s.split(',')
                .filter_map(|p| p.trim().parse::<f64>().ok())
                .collect()
        })
        .unwrap_or_default();

    // Optional linear transform of the labels.
    let scaling = match eq {
        Some(raw) => {
            let e = raw.split_once(',').map(|(e, _)| e).unwrap_or(raw).trim();
            match linear_from_equation(e) {
                LinearFit::Linear { factor, offset } => Scaling { factor, offset },
                _ => Scaling::default(),
            }
        }
        None => Scaling::default(),
    };

    if values.is_empty() && unit.is_empty() {
        return None;
    }
    let count = if values.is_empty() {
        fallback_count
    } else {
        values.len() as u32
    };
    Some(AxisDef {
        name: id.to_string(),
        unit,
        count,
        values,
        scaling,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "XDF\n1.110000\n\n%%HEADER%%\n\t001035 BaseOffset       =0\n\t001005 DefTitle         =\"Test Def\"\n%%END%%\n\n%%TABLE%%\n\t000002 UniqueID         =0x1A29\n\t040005 Title            =\"Spark Advance\"\n\t040010 Desc             =\"main table\"\n\t040100 Address          =0x288\n\t040200 ZEq              =(45*x)/127-20,TH|0|0|0|0|\n\t040300 Rows             =0x10\n\t040305 Cols             =0xB\n\t040320 XUnits           =\"MAP\"\n\t040330 ZUnits           =\"deg\"\n\t040350 XLabels          =0,10,20,30,40,50,60,70,80,90,105\n\t040360 YLabels          =400,800,1200,1600,2000,2400,2800,3200,3600,4000,4400,4800,5200,5600,6000,6400\n\t040230 RangeLow         =0.0000\n\t040240 RangeHigh        =255.0000\n%%END%%\n";

    #[test]
    fn parses_legacy_table() {
        let imp = parse(SAMPLE).expect("parse");
        assert_eq!(imp.title.as_deref(), Some("Test Def"));
        assert_eq!(imp.maps.len(), 1);
        let m = &imp.maps[0];
        assert_eq!(m.name, "Spark Advance");
        assert_eq!(m.address, 0x288);
        assert_eq!(m.rows, 16);
        assert_eq!(m.cols, 11);
        assert_eq!(m.data_type, DataType::U8);
        assert_eq!(m.endian, Endian::Big);
        assert_eq!(m.unit, "deg");
        assert_eq!(m.min, Some(0.0));
        assert_eq!(m.max, Some(255.0));
        assert!((m.scaling.factor - 45.0 / 127.0).abs() < 1e-9);
        assert!((m.scaling.offset + 20.0).abs() < 1e-9);
        assert!(m.notes.contains("legacy XDF"));
        // Every legacy table is warned: its storage type is an assumption.
        assert_eq!(imp.reports[0].status, crate::Status::Warned);
        assert_eq!(imp.reports[0].issues[0].kind, kinds::LEGACY_CELL_TYPE);
        assert!(imp.clone().into_strict().is_err());

        let x = m.x_axis.as_ref().expect("x axis");
        assert_eq!(x.unit, "MAP");
        assert_eq!(x.count, 11);
        assert_eq!(x.values.len(), 11);
        let y = m.y_axis.as_ref().expect("y axis");
        assert_eq!(y.count, 16);
        assert_eq!(y.values.first().copied(), Some(400.0));
    }

    #[test]
    fn negative_base_offset_subtracts_and_underflow_refuses() {
        let s = SAMPLE.replace("BaseOffset       =0", "BaseOffset       =-0x100");
        let imp = parse(&s).expect("parse");
        assert_eq!(imp.maps[0].address, 0x188);
        assert!(imp.base_subtract);

        let s = SAMPLE.replace("BaseOffset       =0", "BaseOffset       =-0x1000");
        let imp = parse(&s).expect("parse");
        assert!(imp.maps.is_empty(), "0x288 - 0x1000 must not wrap");
        assert_eq!(imp.reports[0].status, crate::Status::Refused);
    }

    #[test]
    fn field_with_quotes_and_hex() {
        let (k, v) = parse_field("\t040100 Address          =0x288").unwrap();
        assert_eq!(k, "Address");
        assert_eq!(v, "0x288");
        let (k, v) = parse_field("\t040005 Title            =\"Hello\"").unwrap();
        assert_eq!(k, "Title");
        assert_eq!(v, "Hello");
    }
}
