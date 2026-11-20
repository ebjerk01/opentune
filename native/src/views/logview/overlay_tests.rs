//! Map overlay tests on a SYNTHETIC definition (inline axes) and log.

use super::overlay::{self, compute, guess_channel, heat_color, metric_range, DefSrc, Metric};
use super::{LogContext, LogView};
use opentune_core::ecudef::EcuDef;
use opentune_hub::logoverlay::OverlayReq;
use opentune_logview::{parse, BinMode, Log, ParseOptions};

fn def() -> EcuDef {
    EcuDef::from_json(
        r#"{"id":"synth","name":"Synthetic","maker":"x","family":"x",
        "protocol":"iso14230_kline","diag_layer":"kwp2000","bitrate":10400,
        "flash_size":64,"regions":[],"full_read":"obd",
        "maps":[
          {"name":"limit","address":0,"rows":2,"cols":3,"unit":"mg",
           "x_axis":{"name":"air mass","unit":"mg/stroke","values":[10,20,40]},
           "y_axis":{"name":"engine speed","unit":"rpm","values":[1000,2000]}},
          {"name":"stored","address":0,"rows":1,"cols":3,
           "x_axis":{"name":"air mass","unit":"mg/stroke","address":16}},
          {"name":"single","address":0}
        ]}"#,
    )
    .unwrap()
}

const LOG: &str = "Time [s],air mass [mg/stroke],engine speed [1/min],iq [mg/stroke]\n\
0.0,10,1000,5\n\
0.1,10,1000,6\n\
0.2,19,1900,30\n\
0.3,90,2500,50\n\
0.4,,2000,52\n";

fn log() -> Log {
    parse(LOG, &ParseOptions::default()).unwrap()
}

fn req(x: &str, y: &str) -> OverlayReq {
    OverlayReq {
        x_channel: x.into(),
        y_channel: Some(y.into()),
        value_channel: Some("iq".into()),
        ..Default::default()
    }
}

#[test]
fn hits_time_values_and_outside_are_typed() {
    let h = compute(
        &log(),
        &def(),
        "limit",
        None,
        &req("air mass", "engine speed"),
    )
    .unwrap();
    assert_eq!((h.rows, h.cols), (2, 3));
    assert_eq!(h.hits[0][0], 2.0);
    assert_eq!(h.hits[1][1], 1.0, "19 mg / 1900 rpm -> 20 / 2000");
    assert_eq!(h.hits[1][2], 1.0, "clamped to the edge");
    assert_eq!(h.samples_used, 4);
    assert_eq!(h.samples_skipped, 1);
    assert_eq!(h.outside.x_high, 1);
    assert_eq!(h.outside.y_high, 1);
    assert_eq!(h.map.x_axis.values, vec![10.0, 20.0, 40.0]);
    assert_eq!(h.units.y.as_ref().unwrap().check, "match", "1/min == rpm");
    assert_eq!(h.value_channel.as_ref().unwrap().name, "iq");
    let v = h.values.as_ref().unwrap();
    assert_eq!(v.mean[0][0], Some(5.5));
    assert_eq!(v.max[1][2], Some(50.0));
    assert!(h.map.cells.is_none(), "no image, no map values");
    assert_eq!(overlay::metric(&h, Metric::Value, 0, 0), Some(5.5));
    assert_eq!(metric_range(&h, Metric::Hits), (0.0, 2.0));
    assert_eq!(metric_range(&h, Metric::Value), (5.5, 50.0));
    // Bilinear: fractional hits that still sum to the samples used.
    let mut r = req("air mass", "engine speed");
    r.mode = BinMode::Bilinear;
    let b = compute(&log(), &def(), "limit", None, &r).unwrap();
    let total: f64 = b.hits.iter().flatten().sum();
    assert!((total - 4.0).abs() < 1e-9);
    assert_eq!(b.mode, "bilinear");
}

#[test]
fn refusals_are_errors_never_guesses() {
    let l = log();
    let d = def();
    // Units: swapped channels are refused unless accepted.
    let e = compute(&l, &d, "limit", None, &req("engine speed", "air mass")).unwrap_err();
    assert!(e.contains("unit mismatch"), "{e}");
    let mut r = req("engine speed", "air mass");
    r.accept_units = true;
    let h = compute(&l, &d, "limit", None, &r).unwrap();
    assert!(h.warnings.iter().any(|w| w.contains("not converted")));
    // A stored axis needs the image.
    let e = compute(&l, &d, "stored", None, &req("air mass", "")).unwrap_err();
    assert!(e.contains("stored in the image"), "{e}");
    // ... and reads it from the image when given (raw u8 breakpoints).
    let mut img = vec![0u8; 64];
    img[16..19].copy_from_slice(&[10, 20, 40]);
    let mut r = req("air mass", "");
    r.y_channel = None;
    let h = compute(&l, &d, "stored", Some(&img), &r).unwrap();
    assert_eq!(h.map.x_axis.values, vec![10.0, 20.0, 40.0]);
    assert!(
        h.map.cells.is_some() || h.map.cells_error.is_some(),
        "map values decode from the image (or say why not)"
    );
    assert!(compute(&l, &d, "single", None, &r).is_err());
    assert!(compute(&l, &d, "nope", None, &r).is_err());
}

#[test]
fn channel_guess_and_heat_ramp() {
    let chans: Vec<(String, String)> = log()
        .channels
        .iter()
        .map(|c| (c.name.clone(), c.unit.clone()))
        .collect();
    let d = def();
    let m = &d.maps[0];
    assert_eq!(
        guess_channel(m.x_axis.as_ref(), &chans).as_deref(),
        Some("air mass")
    );
    assert_eq!(
        guess_channel(m.y_axis.as_ref(), &chans).as_deref(),
        Some("engine speed")
    );
    assert_eq!(guess_channel(None, &chans), None);
    assert!(overlay::needs_image(&d.maps[1]));
    assert!(!overlay::needs_image(m));
    assert_eq!(
        overlay::usable_maps(&d).len(),
        2,
        "the single value is not offered"
    );
    assert_eq!(
        heat_color(0.0),
        eframe::egui::Color32::from_rgb(43, 108, 176)
    );
    assert_eq!(
        heat_color(1.0),
        eframe::egui::Color32::from_rgb(197, 48, 48)
    );
    assert_eq!(heat_color(7.0), heat_color(1.0));
    assert_eq!(overlay::fmt(1260.0), "1260");
    assert_eq!(overlay::fmt(0.125), "0.125");
}

#[test]
fn the_overlay_uses_the_app_definition_and_the_visible_window() {
    let d = def();
    let l = log();
    let mut o = overlay::Overlay::default();
    let cx = LogContext {
        def: Some(&d),
        image: None,
    };
    // Nothing chosen yet: an error, not a panic.
    o.run(&l, None, &cx);
    assert!(o.result.as_ref().unwrap().is_err());
    o.set_for_test(DefSrc::App, "limit", "air mass", "engine speed");
    o.run(&l, Some((0.15, 0.35)), &cx);
    assert_eq!(o.result.as_ref().unwrap().as_ref().unwrap().samples_used, 4);
    o.set_window_for_test(true);
    o.run(&l, Some((0.15, 0.35)), &cx);
    let h = o.result.as_ref().unwrap().as_ref().unwrap();
    assert_eq!(h.samples_total, 2, "only the visible range");
}

#[test]
fn the_drive_button_queues_the_open_log() {
    let dir = std::env::temp_dir().join(format!("ot-lv-drive-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("drive.csv");
    std::fs::write(&p, LOG).unwrap();
    let mut v = LogView::default();
    v.install(super::load_file(&p).unwrap());
    assert!(!v.take_show_drive());
    v.send_to_drive();
    assert!(v.take_show_drive());
    assert!(!v.take_show_drive(), "once");
    assert!(v.drive_note.as_ref().unwrap().contains("drive.csv"));
    assert_eq!(crate::views::drive::take_queued_for_test(), Some(p));
    let _ = std::fs::remove_dir_all(&dir);
}
