//! Map overlay of a log: place a parsed log's samples on a calibration map's
//! breakpoints (per-cell hits, time-in-cell and a value channel's range).
//!
//! The binning itself is [`opentune_logview::overlay::map_hits`]; this module
//! adds what it needs from the definition and the image: the axis
//! breakpoints ([`axis_values`], stored axes read through [`crate::maps`]),
//! the unit check between a channel and a map axis ([`unit_value`], compare,
//! never convert) and the payload the desktop's `log_map_hits` returns
//! ([`overlay_value`]). Breakpoints that cannot be known are refused, never
//! guessed.
//!
//! It lives here rather than in `opentune-logview` because it reads axes and
//! cells with this crate's map codecs.

use opentune_core::ecudef::{AxisDef, EcuDef, MapDef};
use opentune_logview::{overlay, series, units, BinMode, Log, OverlayInput, TimeUnit, UnitCheck};
use serde_json::{json, Value};

/// The time axis label of a log: `samples` for an index axis, else `s`.
pub fn time_label(log: &Log) -> &'static str {
    if log.time_unit == TimeUnit::Index {
        "samples"
    } else {
        "s"
    }
}

/// What the overlay was asked for.
#[derive(Debug, Clone, Default)]
pub struct OverlayReq {
    pub x_channel: String,
    pub y_channel: Option<String>,
    pub value_channel: Option<String>,
    pub mode: BinMode,
    pub accept_units: bool,
    pub t0: Option<f64>,
    pub t1: Option<f64>,
}

/// Physical breakpoints of one axis. Refuses (never guesses) when they cannot
/// be known: stored axis without an image, shared axis, or no axis at all.
pub fn axis_values(
    img: Option<&[u8]>,
    axis: &Option<AxisDef>,
    count: u32,
    which: &str,
    map: &MapDef,
) -> Result<Vec<f64>, String> {
    let n = count.max(1) as usize;
    let Some(a) = axis else {
        return if which == "Y" && count <= 1 {
            Ok(Vec::new())
        } else {
            Err(format!(
                "map '{}' has no {which} axis, so log samples cannot be placed on it",
                map.name
            ))
        };
    };
    let label = if a.name.is_empty() {
        which.to_string()
    } else {
        format!("{which} ({})", a.name)
    };
    if !a.values.is_empty() {
        if a.values.len() != n {
            return Err(format!(
                "the {label} axis lists {} breakpoints but the map has {n} along it",
                a.values.len()
            ));
        }
        return Ok(a.values.clone());
    }
    if let Some(addr) = a.address {
        let img = img.ok_or_else(|| {
            format!(
                "the {label} axis is stored in the image at 0x{addr:08X}; choose the image (.bin) this definition belongs to"
            )
        })?;
        let w = a.data_type.byte_len() as usize;
        return (0..n)
            .map(|i| {
                crate::maps::read_elem(img, addr as usize + i * w, a.data_type, a.endian)
                    .map(|raw| a.scaling.to_physical(raw))
                    .ok_or_else(|| {
                        format!("the {label} axis at 0x{addr:08X} lies outside the opened image")
                    })
            })
            .collect();
    }
    if let Some(r) = &a.axis_ref {
        return Err(format!(
            "the {label} axis reuses the shared axis '{r}', which the overlay cannot resolve; give the map its own axis"
        ));
    }
    Err(format!("the {label} axis has no breakpoints"))
}

/// The unit check between a log channel and a map axis, as the payload's
/// `units.x` / `units.y` object plus the verdict.
pub fn unit_value(channel: &str, channel_unit: &str, axis: &Option<AxisDef>) -> (Value, UnitCheck) {
    let axis_unit = axis.as_ref().map(|a| a.unit.as_str()).unwrap_or("");
    let check = units::check(channel_unit, axis_unit);
    (
        json!({
            "channel": channel,
            "channel_unit": channel_unit,
            "axis": axis.as_ref().map(|a| a.name.clone()).unwrap_or_default(),
            "axis_unit": axis_unit,
            "check": check,
        }),
        check,
    )
}

/// Bin `log` over the breakpoints of `map_name` in `def` (stored axes and the
/// map's cells are read from `img`). A unit mismatch is refused unless
/// `req.accept_units`; nothing is converted.
pub fn overlay_value(
    log: &Log,
    def: &EcuDef,
    map_name: &str,
    img: Option<&[u8]>,
    req: &OverlayReq,
) -> Result<Value, String> {
    let map = def
        .maps
        .iter()
        .find(|m| m.name == map_name)
        .ok_or_else(|| format!("map '{map_name}' not found in {}", def.id))?;
    if map.cols <= 1 && map.rows <= 1 {
        return Err(format!(
            "map '{}' is a single value; there is nothing to overlay",
            map.name
        ));
    }
    let x_axis = axis_values(img, &map.x_axis, map.cols, "X", map)?;
    let y_axis = if map.rows > 1 {
        axis_values(img, &map.y_axis, map.rows, "Y", map)?
    } else {
        Vec::new()
    };

    let xc = log.require(&req.x_channel).map_err(|e| e.to_string())?;
    let yc = if map.rows > 1 {
        let name = req
            .y_channel
            .as_deref()
            .ok_or("this map has rows: choose a Y channel")?;
        Some(log.require(name).map_err(|e| e.to_string())?)
    } else {
        None
    };
    let vc = match req.value_channel.as_deref() {
        Some(n) => Some(log.require(n).map_err(|e| e.to_string())?),
        None => None,
    };

    // Units: compare, never convert.
    let mut warnings: Vec<String> = Vec::new();
    let (xu, xcheck) = unit_value(&xc.name, &xc.unit, &map.x_axis);
    let (yu, ycheck) = match yc {
        Some(c) => {
            let (v, k) = unit_value(&c.name, &c.unit, &map.y_axis);
            (v, Some(k))
        }
        None => (Value::Null, None),
    };
    let mut mismatches = Vec::new();
    for (which, u, check) in [("X", &xu, Some(xcheck)), ("Y", &yu, ycheck)] {
        match check {
            Some(UnitCheck::Mismatch) => mismatches.push(format!(
                "{which}: channel '{}' is in '{}' but the map's {which} axis ({}) is in '{}'",
                u["channel"].as_str().unwrap_or(""),
                u["channel_unit"].as_str().unwrap_or(""),
                u["axis"].as_str().unwrap_or(""),
                u["axis_unit"].as_str().unwrap_or(""),
            )),
            Some(UnitCheck::Unknown) => warnings.push(format!(
                "{which}: unit not known on both sides (channel '{}', axis '{}'); values are used as logged",
                u["channel_unit"].as_str().unwrap_or(""),
                u["axis_unit"].as_str().unwrap_or(""),
            )),
            _ => {}
        }
    }
    if !mismatches.is_empty() {
        if !req.accept_units {
            return Err(format!(
                "unit mismatch — {}. OpenTune does not convert units: pick the matching channel, or explicitly accept the values as logged.",
                mismatches.join("; ")
            ));
        }
        warnings.extend(
            mismatches
                .into_iter()
                .map(|m| format!("{m} (accepted as logged, not converted)")),
        );
    }
    if req.y_channel.is_some() && map.rows <= 1 {
        warnings.push("this map is a 1-row curve; the Y channel is not used".into());
    }

    let range = series::index_range(log, req.t0, req.t1);
    let hits = overlay::map_hits(&OverlayInput {
        t: &log.t,
        x: &xc.values,
        y: yc.map(|c| c.values.as_slice()),
        value: vc.map(|c| c.values.as_slice()),
        x_axis: &x_axis,
        y_axis: &y_axis,
        mode: req.mode,
        range: Some(range),
    })
    .map_err(|e| e.to_string())?;

    let cells = img.map(|b| crate::maps::decode_map_value(b, map, Some(def.flash_size / 1024)));
    let mut v = serde_json::to_value(&hits).map_err(|e| e.to_string())?;
    v["map"] = json!({
        "name": map.name,
        "unit": map.unit,
        "rows": map.rows,
        "cols": map.cols,
        "x_axis": { "name": map.x_axis.as_ref().map(|a| a.name.clone()), "unit": map.x_axis.as_ref().map(|a| a.unit.clone()), "values": x_axis },
        "y_axis": { "name": map.y_axis.as_ref().map(|a| a.name.clone()), "unit": map.y_axis.as_ref().map(|a| a.unit.clone()), "values": y_axis },
        "cells": cells.as_ref().and_then(|c| c.as_ref().ok()).map(|c| c["cells"].clone()),
        "cells_error": cells.as_ref().and_then(|c| c.as_ref().err()),
    });
    v["units"] = json!({ "x": xu, "y": yu });
    v["value_channel"] = match vc {
        Some(c) => json!({ "name": c.name, "unit": c.unit }),
        None => Value::Null,
    };
    v["time_label"] = json!(time_label(log));
    v["range"] = json!({
        "t0": log.t.get(range.0),
        "t1": range.1.checked_sub(1).and_then(|i| log.t.get(i)),
    });
    v["warnings"] = json!(warnings);
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentune_core::ecudef::{DataType, Endian};
    use opentune_logview::ParseOptions;
    use std::path::PathBuf;

    fn repo(p: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(p)
    }

    fn edc16u34() -> EcuDef {
        EcuDef::from_json(&std::fs::read_to_string(repo("ecudefs/bosch-edc16u34.json")).unwrap())
            .unwrap()
    }

    const SMOKE: &str = "injection quantity limiter (smoke / air-mass limiter)";

    /// A synthetic 2 MiB image holding only the smoke limiter's two stored
    /// axes: air mass 10..130 mg/stroke (13 points), engine speed
    /// 750..4500 rpm (16 points, 250 rpm apart).
    fn synth_image(def: &EcuDef) -> (Vec<u8>, Vec<f64>, Vec<f64>) {
        let map = def.maps.iter().find(|m| m.name == SMOKE).unwrap();
        let mut img = vec![0u8; def.flash_size as usize];
        let xs: Vec<f64> = (0..13).map(|i| 10.0 + 10.0 * i as f64).collect();
        let ys: Vec<f64> = (0..16).map(|i| 750.0 + 250.0 * i as f64).collect();
        for (axis, vals) in [(&map.x_axis, &xs), (&map.y_axis, &ys)] {
            let a = axis.as_ref().unwrap();
            assert_eq!(a.data_type, DataType::U16);
            assert_eq!(a.endian, Endian::Big);
            let base = a.address.unwrap() as usize;
            for (i, v) in vals.iter().enumerate() {
                let raw = ((v - a.scaling.offset) / a.scaling.factor).round() as u16;
                img[base + 2 * i..base + 2 * i + 2].copy_from_slice(&raw.to_be_bytes());
            }
        }
        (img, xs, ys)
    }

    fn log_of(csv: &str) -> Log {
        opentune_logview::parse(csv, &ParseOptions::default()).unwrap()
    }

    fn req(x: &str, y: &str) -> OverlayReq {
        OverlayReq {
            x_channel: x.into(),
            y_channel: Some(y.into()),
            value_channel: Some("iq".into()),
            ..Default::default()
        }
    }

    const LOG: &str = "Time [s],air mass [mg/stroke],engine speed [1/min],iq [mg/stroke]\n\
        0.0,10,750,5\n\
        0.1,10,750,6\n\
        0.2,54,1260,30\n\
        0.3,130,4500,50\n\
        0.4,200,5000,52\n";

    #[test]
    fn edc16u34_smoke_limiter_overlay() {
        let def = edc16u34();
        let (img, xs, ys) = synth_image(&def);
        let log = log_of(LOG);
        let v = overlay_value(
            &log,
            &def,
            SMOKE,
            Some(&img),
            &req("air mass", "engine speed"),
        )
        .unwrap();
        assert_eq!(v["rows"], 16);
        assert_eq!(v["cols"], 13);
        assert_eq!(v["map"]["x_axis"]["values"], json!(xs));
        assert_eq!(v["map"]["y_axis"]["values"], json!(ys));
        assert_eq!(v["units"]["x"]["check"], "match");
        assert_eq!(v["units"]["y"]["check"], "match"); // 1/min == rpm
        let hits = &v["hits"];
        assert_eq!(hits[0][0], 2.0); // two idle samples on the first cell
        assert_eq!(hits[2][4], 1.0); // 54 mg -> 50 (col 4), 1260 rpm -> 1250 (row 2)
        assert_eq!(hits[15][12], 2.0); // last breakpoint + one clamped beyond it
        assert_eq!(v["samples_used"], 5);
        assert_eq!(v["outside"]["x_high"], 1);
        assert_eq!(v["outside"]["y_high"], 1);
        assert_eq!(v["values"]["max"][15][12], 52.0);
        assert_eq!(v["values"]["mean"][0][0], 5.5);
        // Map cells decode from the (all-zero) image.
        assert_eq!(v["map"]["cells"].as_array().unwrap().len(), 16);
    }

    #[test]
    fn stored_axes_without_image_are_refused() {
        let def = edc16u34();
        let err = overlay_value(
            &log_of(LOG),
            &def,
            SMOKE,
            None,
            &req("air mass", "engine speed"),
        )
        .unwrap_err();
        assert!(err.contains("stored in the image"), "{err}");
    }

    #[test]
    fn swapped_channels_are_a_reported_unit_mismatch() {
        let def = edc16u34();
        let (img, _, _) = synth_image(&def);
        let log = log_of(LOG);
        let err = overlay_value(
            &log,
            &def,
            SMOKE,
            Some(&img),
            &req("engine speed", "air mass"),
        )
        .unwrap_err();
        assert!(err.contains("unit mismatch"), "{err}");
        let mut r = req("engine speed", "air mass");
        r.accept_units = true;
        let v = overlay_value(&log, &def, SMOKE, Some(&img), &r).unwrap();
        assert!(v["warnings"][0].as_str().unwrap().contains("not converted"));
    }

    #[test]
    fn bilinear_and_time_window() {
        let def = edc16u34();
        let (img, _, _) = synth_image(&def);
        let log = log_of(LOG);
        let mut r = req("air mass", "engine speed");
        r.mode = BinMode::Bilinear;
        r.t0 = Some(0.15);
        r.t1 = Some(0.25);
        let v = overlay_value(&log, &def, SMOKE, Some(&img), &r).unwrap();
        assert_eq!(v["samples_total"], 1);
        let total: f64 = v["hits"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|r| r.as_array().unwrap().iter().map(|x| x.as_f64().unwrap()))
            .sum();
        assert!((total - 1.0).abs() < 1e-9);
    }

    #[test]
    fn shared_axis_is_refused_not_guessed() {
        let mut def = edc16u34();
        let m = def.maps.iter_mut().find(|m| m.name == SMOKE).unwrap();
        let x = m.x_axis.as_mut().unwrap();
        x.address = None;
        x.axis_ref = Some("common rpm".into());
        let (img, _, _) = synth_image(&edc16u34());
        let err = overlay_value(
            &log_of(LOG),
            &def,
            SMOKE,
            Some(&img),
            &req("air mass", "engine speed"),
        )
        .unwrap_err();
        assert!(err.contains("shared axis"), "{err}");
    }
}
