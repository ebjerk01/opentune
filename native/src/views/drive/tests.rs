//! Drive view tests: the ported `driveview-core.js` decisions, the live
//! state over feed events, and the view's gating and replay loading on
//! synthetic logs.

use super::core::*;
use super::live::LiveState;
use super::{DriveView, Mode};
use crate::host::{BuildChannel, DiagTarget, FeedEvent, FeedValue, HostShared};
use crate::views::coding::simkit::Fixture;
use opentune_logview::{parse, ParseOptions};
use std::time::{Duration, Instant};

fn ch(key: &str, name: &str, unit: &str) -> Chan {
    Chan {
        key: key.into(),
        name: name.into(),
        unit: unit.into(),
    }
}

#[test]
fn units_and_numbers() {
    assert_eq!(norm_unit(" [1/min] "), "rpm");
    assert_eq!(norm_unit("°C"), "degc");
    assert_eq!(norm_unit("mbar"), "hpa");
    assert_eq!(norm_unit("mg/stroke"), "mg/stroke");
    assert_eq!(
        display(Some(150.0), "kPa", PUnit::Bar),
        (Some(1.5), "bar".into(), 2)
    );
    assert_eq!(
        display(Some(1.5), "bar", PUnit::KPa),
        (Some(150.0), "kPa".into(), 0)
    );
    assert_eq!(display(Some(812.4), "rpm", PUnit::Bar).2, 0);
    assert_eq!(display(Some(3.21), "mg", PUnit::Bar).2, 2);
    assert_eq!(fmt_num(Some(-0.01), 1, false), "0.0");
    assert_eq!(fmt_num(Some(2.0), 0, true), "+2");
    assert_eq!(fmt_num(Some(0.0), 0, true), "0");
    assert_eq!(fmt_num(Some(-3.0), 0, true), "-3");
    assert_eq!(fmt_num(None, 0, false), "\u{2014}");
}

#[test]
fn default_tiles_follow_roles_ids_names_and_units() {
    let dash = vec![
        ch("engine_load", "Calculated engine load", "%"),
        ch("rpm", "Engine speed", "rpm"),
        ch("coolant_temp", "Coolant temperature", "°C"),
        ch("speed", "Vehicle speed", "km/h"),
        ch("boost_kpa", "Boost pressure (MAP − baro)", "kPa"),
        ch("maf", "Mass air flow", "g/s"),
    ];
    assert_eq!(
        pick_default(&dash),
        vec!["rpm", "speed", "coolant_temp", "boost_kpa", "engine_load"]
    );
    // A generic log: header names; a "Speed" column in rpm is not speed,
    // a commanded boost is not boost.
    let log = vec![
        ch("Speed", "Speed", "rpm"),
        ch("Boost target", "Boost target", "kPa"),
        ch("Boost actual", "Boost actual", "hPa"),
        ch("Vehicle speed", "Vehicle speed", "km/h"),
        ch("IAT", "IAT", "degC"),
        ch("Engine speed", "Engine speed", "1/min"),
    ];
    assert_eq!(
        pick_default(&log),
        vec!["Engine speed", "Vehicle speed", "IAT", "Boost actual"]
    );
    assert!(
        role_of(&log[0]).is_none(),
        "a Speed column in rpm is no speed"
    );
    assert_eq!(role_of(&log[5]).unwrap().role, "rpm");
    // A saved selection survives when its channels exist.
    assert_eq!(selection_for(&log, &["IAT".into()]), vec!["IAT"]);
    assert_eq!(selection_for(&log, &["gone".into()]).len(), 4);
    // Optional GPS tiles and signed speed error.
    let gps = ch("speed_error_kmh", "Speed error", "km/h");
    let r = role_of(&gps).unwrap();
    assert!(r.optional && r.signed);
}

#[test]
fn thresholds_need_matching_units() {
    let ect = thresholds_for(&ch("coolant_temp", "Coolant temperature", "°C"));
    assert_eq!((ect.warn, ect.crit), (Some(105.0), Some(115.0)));
    assert_eq!(level(Some(110.0), &ect), Level::Warn);
    assert_eq!(level(Some(115.0), &ect), Level::Crit);
    assert_eq!(level(Some(80.0), &ect), Level::Normal);
    // By role from a log name, in the table's unit.
    let by_role = thresholds_for(&ch("ECT", "ECT", "degC"));
    assert_eq!(by_role.crit, Some(115.0));
    // A Fahrenheit coolant column is never compared with °C limits.
    let f = thresholds_for(&ch("Coolant", "Coolant", "°F"));
    assert!(f.unit_mismatch && f.crit.is_none());
    assert_eq!(
        thresholds_for(&ch("x", "Unknown", "V")),
        Thresholds::default()
    );
    // Bar range: the table's, else what was seen.
    assert_eq!(bar_range(&ect, &Peak::default()), Some((-40.0, 130.0)));
    let mut p = Peak::default();
    p.add(Some(-5.0), 0.0);
    p.add(Some(12.0), 1.0);
    assert_eq!(bar_range(&Thresholds::default(), &p), Some((-5.0, 12.0)));
    assert_eq!(frac(Some(200.0), Some((0.0, 100.0))), Some(1.0));
    assert_eq!(frac(None, Some((0.0, 100.0))), None);
}

#[test]
fn link_and_tile_states() {
    assert_eq!(stale_after(0.1), STALE_MIN_S);
    assert_eq!(stale_after(2.0), 6.0);
    assert_eq!(link_state(true, false, Some(1.0), 1.0, 2.5), Link::Stopped);
    assert_eq!(link_state(false, true, Some(1.0), 1.0, 2.5), Link::Lost);
    assert_eq!(link_state(false, false, None, 1.0, 2.5), Link::Waiting);
    assert_eq!(link_state(false, false, Some(1.0), 5.0, 2.5), Link::Lost);
    assert_eq!(link_state(false, false, Some(4.0), 5.0, 2.5), Link::Up);
    assert_eq!(tile_state(Link::Lost, Some(1.0), 2.0, 2.5), TileState::Lost);
    assert_eq!(tile_state(Link::Lost, None, 2.0, 2.5), TileState::NoData);
    assert_eq!(tile_state(Link::Up, Some(1.0), 9.0, 2.5), TileState::Stale);
    assert_eq!(
        tile_state(Link::Replay, Some(1.0), 2.0, 2.5),
        TileState::Live
    );
    assert_eq!(fmt_age(4.7), "4 s ago");
    assert_eq!(fmt_age(125.0), "2 min ago");
    assert_eq!(fmt_age(7300.0), "2 h ago");
}

#[test]
fn history_and_sparklines() {
    let mut h = Hist::default();
    for i in 0..100 {
        h.push(i as f64, if i == 50 { None } else { Some(i as f64) }, 30.0);
    }
    assert_eq!(h.0.front().unwrap().0, 69.0);
    let pts: Vec<_> = h.0.iter().copied().collect();
    let s = spark(&pts, 69.0, 99.0);
    assert_eq!(s.len(), 1);
    assert_eq!(s[0][0], [0.0, 0.0]);
    assert_eq!(*s[0].last().unwrap(), [1.0, 1.0]);
    // Gaps break the line; a flat series sits mid-height.
    let gap = vec![(0.0, Some(1.0)), (1.0, None), (2.0, Some(1.0))];
    let s = spark(&gap, 0.0, 2.0);
    assert_eq!(s.len(), 2);
    assert_eq!(s[0][0][1], 0.5);
    assert!(spark(&[(0.0, None)], 0.0, 1.0).is_empty());
}

const LOG: &str = "Time [s],Engine speed [rpm],Coolant [degC]\n\
0.0,800,80\n\
0.5,,81\n\
1.0,1500,82\n\
1.5,4500,83\n\
2.0,900,84\n";

fn prepared() -> Prepared {
    let log = parse(LOG, &ParseOptions::default()).unwrap();
    prepare(&log, REPLAY_ROWS).unwrap()
}

#[test]
fn replay_samples_folds_and_windows() {
    let d = prepared();
    assert_eq!(d.t, vec![0.0, 0.5, 1.0, 1.5, 2.0]);
    assert_eq!((d.t0, d.t1, d.rate, d.index), (0.0, 2.0, 1.0, false));
    assert!((d.dt - 0.5).abs() < 1e-9);
    let rpm = d.chan("Engine speed").unwrap();
    assert_eq!(rpm.chan.unit, "rpm");
    // The latest valid sample at or before x.
    assert_eq!(sample(&d, rpm, 0.7), Some((800.0, 0.0)));
    assert_eq!(sample(&d, rpm, 1.2), Some((1500.0, 1.0)));
    assert_eq!(sample(&d, rpm, -1.0), None);
    assert_eq!(idx_at(&d.t, 1.0), Some(2));
    // A jump over the 4500 spike still holds it.
    let mut p = Peak::default();
    fold(&d, rpm, &mut p, 0.2, 2.0);
    assert_eq!((p.min, p.max, p.n), (Some(900.0), Some(4500.0), 3));
    let w = window(&d, rpm, 0.0, 2.0, 2);
    assert_eq!(w.first().unwrap().0, 0.0);
    assert_eq!(w.last().unwrap().0, 2.0);
    // Defaults from the header names.
    assert_eq!(pick_default(&d.list()), vec!["Engine speed", "Coolant"]);
    // An index log replays at INDEX_HZ.
    let idx = parse("a,b\n1,2\n3,4\n", &ParseOptions::default()).unwrap();
    let di = prepare(&idx, 10).unwrap();
    assert!(di.index);
    assert_eq!(di.rate, INDEX_HZ);
    assert_eq!(fmt_clock(3.4, true), "#3");
    assert_eq!(fmt_clock(3725.0, false), "1:02:05");
    assert_eq!(fmt_clock(65.0, false), "1:05");
}

#[test]
fn the_replay_clock() {
    let mut c = Clock::new(10.0, 20.0, 1.0);
    assert_eq!(c.pos_at(5000.0), 10.0, "paused");
    c.play(0.0);
    c.set_speed(4.0, 0.0);
    let s = c.tick(1000.0);
    assert_eq!((s.from, s.to, s.ended), (10.0, 14.0, false));
    c.set_speed(3.0, 1000.0);
    assert_eq!(c.speed, 1.0, "unknown speeds fall back to 1x");
    c.pause(2000.0);
    assert_eq!(c.pos, 15.0);
    assert!(!c.playing);
    c.seek(99.0, 2000.0);
    assert_eq!(c.pos, 20.0);
    c.play(3000.0);
    assert_eq!(c.pos, 10.0, "play again from the start");
    c.set_speed(8.0, 3000.0);
    let s = c.tick(10_000.0);
    assert!(s.ended && !c.playing);
    assert_eq!(c.pos, 20.0);
}

fn frame(src: &str, vals: &[(&str, Option<f64>)]) -> FeedEvent {
    FeedEvent::Frame {
        source: src.into(),
        t_ms: 0,
        values: vals
            .iter()
            .map(|(k, v)| FeedValue {
                key: k.to_string(),
                name: k.to_string(),
                unit: "rpm".into(),
                value: *v,
            })
            .collect(),
    }
}

#[test]
fn live_state_follows_one_source() {
    let mut l = LiveState::default();
    assert_eq!(l.link(0.0), Link::Waiting);
    l.apply(&frame("dashboard", &[("rpm", Some(800.0))]), 1.0);
    assert!(l.fresh);
    assert_eq!(l.link(1.2), Link::Up);
    l.apply(
        &frame("dashboard", &[("rpm", Some(900.0)), ("x", None)]),
        1.2,
    );
    assert_eq!(l.chans.len(), 2);
    assert_eq!(l.chan("rpm").unwrap().last, Some((900.0, 1.2)));
    assert_eq!(l.chan("rpm").unwrap().peak.max, Some(900.0));
    assert!(l.interval < 0.5);
    // A second source is ignored while the first is fresh...
    l.apply(&frame("livelog", &[("y", Some(1.0))]), 1.3);
    assert!(l.chan("y").is_none());
    // ... errors: busy ones do not count, two others mean lost.
    let err = |busy| FeedEvent::Error {
        source: "dashboard".into(),
        message: "timeout".into(),
        busy,
    };
    l.apply(&err(true), 1.7);
    l.apply(&err(false), 1.8);
    assert_eq!(l.link(1.8), Link::Up);
    l.apply(&err(false), 1.9);
    assert_eq!(l.link(1.9), Link::Lost);
    l.apply(
        &FeedEvent::Stop {
            source: "dashboard".into(),
        },
        2.0,
    );
    assert_eq!(l.link(2.0), Link::Stopped);
    // ... and taken over once the first stopped.
    l.apply(&frame("livelog", &[("y", Some(1.0))]), 2.1);
    assert_eq!(l.src.as_deref(), Some("livelog"));
    assert!(l.chan("rpm").is_none() && l.chan("y").is_some());
    l.reset_peaks();
    assert_eq!(l.chan("y").unwrap().peak, Peak::default());
}

#[test]
fn live_mode_only_on_bench_and_only_reads_the_feed() {
    // Stable: no Live mode, no subscription.
    let mut fx = Fixture::new("drive-stable");
    fx.shared = HostShared::new(BuildChannel::new(BuildChannel::STABLE), None);
    let mut v = DriveView {
        mode: Mode::Live,
        ..Default::default()
    };
    let feed = fx.shared.live.clone();
    fx.host(DiagTarget::ENGINE, |h| v.pump_live(h));
    assert!(v.rx.is_none());
    assert_eq!(feed.subscribers(), 0);
    // Bench: subscribes and follows.
    let mut fx = Fixture::new("drive-bench");
    let feed = fx.shared.live.clone();
    fx.host(DiagTarget::ENGINE, |h| v.pump_live(h));
    assert_eq!(feed.subscribers(), 1);
    feed.publish(frame("dashboard", &[("rpm", Some(2000.0))]));
    fx.host(DiagTarget::ENGINE, |h| v.pump_live(h));
    assert_eq!(v.live.chan("rpm").unwrap().last.unwrap().0, 2000.0);
    assert_eq!(v.sel_live, vec!["rpm"]);
    // Leaving Live unsubscribes.
    v.mode = Mode::Replay;
    fx.host(DiagTarget::ENGINE, |h| v.pump_live(h));
    assert!(v.rx.is_none());
    feed.publish(FeedEvent::Stop {
        source: "dashboard".into(),
    });
    assert_eq!(feed.subscribers(), 0);
    assert!(fx.adapter.is_none(), "never needs an adapter");
}

#[test]
fn replay_loads_a_log_in_a_job_and_plays() {
    let mut fx = Fixture::new("drive-replay");
    let path = fx.dir.join("log.csv");
    std::fs::write(&path, LOG).unwrap();
    let mut v = DriveView::default();
    fx.host(DiagTarget::ENGINE, |h| v.open(h, path.clone()));
    let t0 = Instant::now();
    while v.load.is_some() {
        v.poll_load();
        assert!(t0.elapsed() < Duration::from_secs(10), "stuck");
        std::thread::sleep(Duration::from_millis(5));
    }
    let rp = v.rp.as_ref().expect("loaded");
    assert_eq!(rp.data.t.len(), 5);
    assert_eq!(v.sel_replay, vec!["Engine speed", "Coolant"]);
    // Play at 8x for 250 ms of wall time: past the spike, held as max.
    let rp = v.rp.as_mut().unwrap();
    rp.clock.play(0.0);
    rp.clock.set_speed(8.0, 0.0);
    let sel = v.sel_replay.clone();
    let ended = rp.advance(250.0, &sel);
    assert!(ended);
    assert_eq!(rp.peaks["Engine speed"].max, Some(4500.0));
    let tiles = v.tiles(0.0);
    assert_eq!(tiles.len(), 2);
    assert_eq!(tiles[0].v, Some(900.0));
    assert_eq!(tiles[1].level, Level::Normal);
    // A missing file is a note, not a panic.
    fx.host(DiagTarget::ENGINE, |h| v.open(h, fx_missing()));
    let t0 = Instant::now();
    while v.load.is_some() {
        v.poll_load();
        assert!(t0.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(v
        .note
        .as_ref()
        .is_some_and(|n| n.0 == crate::theme::Tone::Crit));
}

fn fx_missing() -> std::path::PathBuf {
    std::env::temp_dir().join("ot-drive-no-such-log.csv")
}
