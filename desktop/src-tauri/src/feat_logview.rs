//! # Log viewer + map overlay (backend)
//!
//! Offline commands behind the "Log viewer" panel, over [`opentune_logview`]:
//!
//! - [`log_open`] — parse a CSV log (OpenTune Live log or generic wide CSV),
//!   return its channels, per-channel statistics and every detection note.
//! - [`log_series`] — min/max-decimated plot data for a time window.
//! - [`log_sample_at`] — exact values of every channel at one time (cursor).
//! - [`log_replay`] — every row of the chosen channels on one time axis, for
//!   playing the log back through the Drive view.
//! - [`log_def_maps`] — the maps of a definition with their axis names/units,
//!   so the panel can label and pre-select the X/Y channels.
//! - [`log_map_hits`] — bin the log over a map's breakpoints: per-cell hits,
//!   time-in-cell and the value range of a chosen channel.
//!
//! The last parsed log is cached (keyed by path, size and modification time),
//! so zooming and panning do not re-read the file. Unit checks between a
//! channel and a map axis are reported; a mismatch is refused unless the
//! caller explicitly accepts it. Nothing is ever converted.

use crate::AppData;
use opentune_core::ecudef::{AxisDef, EcuDef, MapDef};
use opentune_logview::{
    overlay, series, units, BinMode, Log, OverlayInput, ParseOptions, TimeUnit, UnitCheck,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Parsed-log cache
// ---------------------------------------------------------------------------

struct Cached {
    path: PathBuf,
    stamp: (u64, Option<std::time::SystemTime>),
    unit: Option<TimeUnit>,
    column: Option<String>,
    log: Arc<Log>,
}

static CACHE: Mutex<Option<Cached>> = Mutex::new(None);

fn stamp(path: &Path) -> Result<(u64, Option<std::time::SystemTime>), String> {
    let m =
        std::fs::metadata(path).map_err(|e| format!("cannot open '{}': {e}", path.display()))?;
    Ok((m.len(), m.modified().ok()))
}

fn parse_unit(s: Option<&str>) -> Result<Option<TimeUnit>, String> {
    match s.map(str::trim) {
        None | Some("") | Some("auto") => Ok(None),
        Some(u) => TimeUnit::parse(u)
            .map(Some)
            .ok_or_else(|| format!("unknown time unit '{u}' (use s, ms, us, min, clock or index)")),
    }
}

/// Load `path`, reusing the cache when the file is unchanged. `overrides` is
/// `Some` from [`log_open`] (explicit choice, may be "auto"), `None` from the
/// other commands (reuse whatever the panel last opened the file with).
fn load(
    path: &str,
    overrides: Option<(Option<TimeUnit>, Option<String>)>,
) -> Result<Arc<Log>, String> {
    let p = PathBuf::from(path);
    let st = stamp(&p)?;
    let mut guard = CACHE.lock().map_err(|_| "log cache poisoned")?;
    let (unit, column) = match (&overrides, guard.as_ref()) {
        (Some(o), _) => o.clone(),
        (None, Some(c)) if c.path == p => (c.unit, c.column.clone()),
        _ => (None, None),
    };
    if let Some(c) = guard.as_ref() {
        if c.path == p && c.stamp == st && c.unit == unit && c.column == column {
            return Ok(c.log.clone());
        }
    }
    let opts = ParseOptions {
        time_unit: unit,
        time_column: column.clone(),
    };
    let log = Arc::new(opentune_logview::parse_file(&p, &opts).map_err(|e| e.to_string())?);
    *guard = Some(Cached {
        path: p,
        stamp: st,
        unit,
        column,
        log: log.clone(),
    });
    Ok(log)
}

fn time_label(log: &Log) -> &'static str {
    if log.time_unit == TimeUnit::Index {
        "samples"
    } else {
        "s"
    }
}

/// Summary payload of a parsed log (also used by tests).
fn summary(path: &str, log: &Log) -> Value {
    let stats = series::stats(log, None, None);
    let dt = log.median_dt();
    json!({
        "path": path,
        "format": log.format,
        "delimiter": match log.delimiter { '\t' => "tab".to_string(), d => d.to_string() },
        "decimal_comma": log.decimal_comma,
        "header": log.header,
        "units_row": log.units_row,
        "time": {
            "column": log.time_column,
            "unit": log.time_unit,
            "source": log.time_source,
            "label": time_label(log),
            "epoch_start": log.epoch_start,
        },
        "rows": log.len(),
        "t0": log.t.first(),
        "t1": log.t.last(),
        "duration": log.duration(),
        "median_dt": dt,
        "rate_hz": (dt > 0.0 && log.time_unit != TimeUnit::Index).then(|| 1.0 / dt),
        "channels": stats,
        "notes": log.notes,
    })
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Open a CSV log. `time_unit` (`auto`/`s`/`ms`/`us`/`min`/`clock`/`index`)
/// and `time_column` override detection.
#[tauri::command]
pub async fn log_open(
    path: String,
    time_unit: Option<String>,
    time_column: Option<String>,
) -> Result<Value, String> {
    let unit = parse_unit(time_unit.as_deref())?;
    let column = time_column.filter(|c| !c.trim().is_empty());
    let log = load(&path, Some((unit, column)))?;
    Ok(summary(&path, &log))
}

/// Plot data for `channels` over `t0..=t1` in at most `buckets` min/max
/// buckets (default 1000), plus the statistics of that window.
#[tauri::command]
pub async fn log_series(
    path: String,
    channels: Vec<String>,
    t0: Option<f64>,
    t1: Option<f64>,
    buckets: Option<usize>,
) -> Result<Value, String> {
    let log = load(&path, None)?;
    let names: Vec<&str> = channels.iter().map(String::as_str).collect();
    let set =
        series::series(&log, &names, t0, t1, buckets.unwrap_or(1000)).map_err(|e| e.to_string())?;
    let window = series::stats(&log, Some(set.t0), Some(set.t1));
    let mut v = serde_json::to_value(&set).map_err(|e| e.to_string())?;
    v["stats"] = serde_json::to_value(window).map_err(|e| e.to_string())?;
    Ok(v)
}

/// Every channel's value at the sample nearest to `t`.
#[tauri::command]
pub async fn log_sample_at(path: String, t: f64) -> Result<Value, String> {
    let log = load(&path, None)?;
    let (idx, ts, vals) = series::sample_at(&log, t).ok_or("the log is empty")?;
    Ok(json!({
        "index": idx,
        "t": ts,
        "values": log.channels.iter().zip(vals).map(|(c, v)| json!({
            "name": c.name, "unit": c.unit, "value": v,
        })).collect::<Vec<_>>(),
    }))
}

/// Default row cap of [`log_replay`]: an hour at 50 Hz.
const REPLAY_ROWS: usize = 180_000;

/// Frames for replaying the log at `path` through the Drive view: one shared
/// time axis and each of `channels` (every channel when empty or absent) per
/// row, at most `max_rows` rows (longer logs keep the latest valid value of
/// each block, see [`opentune_logview::series::replay`]). Offline.
#[tauri::command]
pub async fn log_replay(
    path: String,
    channels: Option<Vec<String>>,
    max_rows: Option<usize>,
) -> Result<Value, String> {
    let log = load(&path, None)?;
    replay_value(&path, &log, &channels.unwrap_or_default(), max_rows)
}

fn replay_value(
    path: &str,
    log: &Log,
    channels: &[String],
    max_rows: Option<usize>,
) -> Result<Value, String> {
    let names: Vec<&str> = channels.iter().map(String::as_str).collect();
    let r =
        series::replay(log, &names, max_rows.unwrap_or(REPLAY_ROWS)).map_err(|e| e.to_string())?;
    let mut v = serde_json::to_value(&r).map_err(|e| e.to_string())?;
    v["path"] = json!(path);
    v["time_label"] = json!(time_label(log));
    v["median_dt"] = json!(log.median_dt() * r.stride as f64);
    v["epoch_start"] = json!(log.epoch_start);
    Ok(v)
}

fn find_def(
    ecudef_id: Option<&str>,
    ecudef_path: Option<&str>,
    state: &AppData,
) -> Result<EcuDef, String> {
    if let Some(p) = ecudef_path.filter(|p| !p.trim().is_empty()) {
        let text = std::fs::read_to_string(p).map_err(|e| format!("read '{p}': {e}"))?;
        return EcuDef::from_json(&text).map_err(|e| format!("definition '{p}': {e}"));
    }
    let id = ecudef_id
        .filter(|i| !i.is_empty())
        .ok_or("choose a definition")?;
    crate::state::load_defs(&state.ecudefs)
        .iter()
        .find(|d| d.id == id)
        .cloned()
        .ok_or_else(|| format!("definition '{id}' not found"))
}

fn axis_kind(a: &Option<AxisDef>) -> &'static str {
    match a {
        None => "none",
        Some(a) if !a.values.is_empty() => "inline",
        Some(a) if a.address.is_some() => "stored",
        Some(a) if a.axis_ref.is_some() => "shared",
        Some(_) => "none",
    }
}

fn axis_info(a: &Option<AxisDef>, count: u32) -> Value {
    json!({
        "name": a.as_ref().map(|a| a.name.clone()).unwrap_or_default(),
        "unit": a.as_ref().map(|a| a.unit.clone()).unwrap_or_default(),
        "count": count,
        "kind": axis_kind(a),
    })
}

/// The maps of a definition (bundled `ecudef_id` or a `.json` file at
/// `ecudef_path`) with their axis names, units and storage kind.
#[tauri::command]
pub fn log_def_maps(
    ecudef_id: Option<String>,
    ecudef_path: Option<String>,
    state: tauri::State<AppData>,
) -> Result<Value, String> {
    let def = find_def(ecudef_id.as_deref(), ecudef_path.as_deref(), &state)?;
    def_maps_for_ui(&def, ecudef_id.is_none())
}

/// [`def_maps_value`], plus each map's full `MapDef` as `def` when the
/// definition came from a file: it is not in the Maps panel's list, so "Open
/// in Maps" opens such a map there as an ad-hoc map.
fn def_maps_for_ui(def: &EcuDef, from_file: bool) -> Result<Value, String> {
    let mut v = def_maps_value(def);
    if from_file {
        if let Some(maps) = v["maps"].as_array_mut() {
            for (j, m) in maps.iter_mut().zip(&def.maps) {
                j["def"] = serde_json::to_value(m).map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(v)
}

fn def_maps_value(def: &EcuDef) -> Value {
    json!({
        "id": def.id,
        "name": def.name,
        "maps": def.maps.iter().map(|m| json!({
            "name": m.name,
            "rows": m.rows,
            "cols": m.cols,
            "unit": m.unit,
            "x_axis": axis_info(&m.x_axis, m.cols),
            "y_axis": axis_info(&m.y_axis, m.rows),
            "needs_image": axis_kind(&m.x_axis) == "stored" || axis_kind(&m.y_axis) == "stored",
        })).collect::<Vec<_>>(),
    })
}

/// What the overlay was asked for.
#[derive(Debug, Clone, Default)]
struct OverlayReq {
    x_channel: String,
    y_channel: Option<String>,
    value_channel: Option<String>,
    mode: BinMode,
    accept_units: bool,
    t0: Option<f64>,
    t1: Option<f64>,
}

/// Bin the log at `path` over the breakpoints of `map_name`. Stored axes are
/// read from `image_path`; without an image only inline axes work, and the
/// error says so. With an image the map's cell values are returned too.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn log_map_hits(
    path: String,
    x_channel: String,
    y_channel: Option<String>,
    ecudef_id: Option<String>,
    ecudef_path: Option<String>,
    map_name: String,
    image_path: Option<String>,
    mode: Option<String>,
    value_channel: Option<String>,
    accept_units: Option<bool>,
    t0: Option<f64>,
    t1: Option<f64>,
    state: tauri::State<'_, AppData>,
) -> Result<Value, String> {
    let def = find_def(ecudef_id.as_deref(), ecudef_path.as_deref(), &state)?;
    let log = load(&path, None)?;
    let img = match image_path.as_deref().filter(|p| !p.trim().is_empty()) {
        Some(p) => Some(std::fs::read(p).map_err(|e| format!("read image '{p}': {e}"))?),
        None => None,
    };
    let mode = match mode.as_deref() {
        None => BinMode::Nearest,
        Some(m) => {
            BinMode::parse(m).ok_or_else(|| format!("unknown mode '{m}' (nearest or bilinear)"))?
        }
    };
    let blank = |s: Option<String>| s.filter(|s| !s.trim().is_empty());
    let req = OverlayReq {
        x_channel,
        y_channel: blank(y_channel),
        value_channel: blank(value_channel),
        mode,
        accept_units: accept_units.unwrap_or(false),
        t0,
        t1,
    };
    overlay_value(&log, &def, &map_name, img.as_deref(), &req)
}

// ---------------------------------------------------------------------------
// Overlay core (no Tauri state; unit-tested below)
// ---------------------------------------------------------------------------

/// Physical breakpoints of one axis. Refuses (never guesses) when they cannot
/// be known: stored axis without an image, shared axis, or no axis at all.
fn axis_values(
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

fn unit_value(channel: &str, channel_unit: &str, axis: &Option<AxisDef>) -> (Value, UnitCheck) {
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

fn overlay_value(
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
    fn replay_frames_share_one_time_axis() {
        let log = log_of(LOG);
        let v = replay_value("x.csv", &log, &["iq".into()], None).unwrap();
        assert_eq!(v["path"], "x.csv");
        assert_eq!(v["rows"], 5);
        assert_eq!(v["stride"], 1);
        assert_eq!(v["time_label"], "s");
        assert_eq!(v["t"], json!([0.0, 0.1, 0.2, 0.3, 0.4]));
        assert_eq!(v["series"][0]["name"], "iq");
        assert_eq!(v["series"][0]["unit"], "mg/stroke");
        assert_eq!(v["series"][0]["v"], json!([5.0, 6.0, 30.0, 50.0, 52.0]));
        let all = replay_value("x.csv", &log, &[], Some(2)).unwrap();
        assert_eq!(all["series"].as_array().unwrap().len(), 3);
        assert_eq!(all["stride"], 3);
        assert_eq!(all["t"], json!([0.2, 0.4]));
        assert!(replay_value("x.csv", &log, &["nope".into()], None).is_err());
    }

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
    fn demo_sample_log_overlays_the_demo_map() {
        let def = EcuDef::from_json(
            &std::fs::read_to_string(repo("samples/opentune-demo-edc16.json")).unwrap(),
        )
        .unwrap();
        let img = std::fs::read(repo("samples/opentune-demo-edc16.bin")).unwrap();
        let path = repo("samples/opentune-demo-log.csv");
        let log = opentune_logview::parse_file(&path, &ParseOptions::default()).unwrap();
        let s = summary(path.to_str().unwrap(), &log);
        assert_eq!(s["rows"], 1201);
        assert_eq!(s["time"]["source"], "header");
        let map = &def.maps[0].name;
        let r = OverlayReq {
            x_channel: "air mass".into(),
            y_channel: Some("engine speed".into()),
            value_channel: Some("injection quantity".into()),
            ..Default::default()
        };
        let v = overlay_value(&log, &def, map, Some(&img), &r).unwrap();
        assert_eq!(v["samples_used"], 1201);
        assert!(v["outside"]["y_high"].as_u64().unwrap() > 0); // the 4500 rpm pull
        assert!(v["warnings"].as_array().unwrap().is_empty());
        let maps = def_maps_value(&def);
        assert_eq!(maps["maps"][0]["needs_image"], true);
        assert_eq!(maps["maps"][0]["x_axis"]["unit"], "mg/stroke");
        // "Open in Maps": a bundled definition is opened by id, a file's maps
        // travel as their MapDef.
        assert!(def_maps_for_ui(&def, false).unwrap()["maps"][0]
            .get("def")
            .is_none());
        let file = def_maps_for_ui(&def, true).unwrap();
        let back: opentune_core::ecudef::MapDef =
            serde_json::from_value(file["maps"][0]["def"].clone()).unwrap();
        assert_eq!(back, def.maps[0]);
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

    #[test]
    fn cache_follows_overrides() {
        let dir = std::env::temp_dir().join(format!("opentune-logview-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("log.csv");
        std::fs::write(&p, "t,a\n0,1\n100,2\n").unwrap();
        let ps = p.to_str().unwrap();
        let auto = load(ps, Some((None, None))).unwrap();
        assert_eq!(auto.time_unit, TimeUnit::Milliseconds);
        let secs = load(ps, Some((Some(TimeUnit::Seconds), None))).unwrap();
        assert_eq!(secs.t, vec![0.0, 100.0]);
        // A follow-up call without overrides keeps the last choice.
        assert_eq!(load(ps, None).unwrap().time_unit, TimeUnit::Seconds);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
