//! Dashboard tests: the pure decisions and the polling job end to end on a
//! SYNTHETIC OBD ECU ([`SimEcu`]) behind the real hub adapter.

use super::model::{self, Level, Preset, PressureUnit};
use super::poll::{self, lock, Board, Ctl, Phase, Run};
use super::DashboardView;
use crate::host::{DiagTarget, FeedEvent, LiveFeed, NativeView};
use crate::views::coding::simkit::{Bus, Fixture};
use opentune_core::obd::sim::{MultiMode, SimEcu};
use opentune_core::obd::{self, ObdValue, Readout};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn engine() -> SimEcu {
    let mut e = SimEcu::demo(7);
    e.vin = None;
    e
}

fn bus_with(ecu: SimEcu) -> Bus {
    let ecu = Arc::new(Mutex::new(ecu));
    Bus::default().script(
        0x7E0,
        0x7E8,
        Box::new(move |req| {
            use opentune_core::RawDiag;
            ecu.lock().unwrap().request(req)
        }),
    )
}

fn sup(pids: &[u8]) -> BTreeSet<u8> {
    pids.iter().copied().collect()
}

#[test]
fn presets_pick_known_supported_values_without_the_gauges() {
    let all = model::selection(Preset::Diesel, &[], None);
    assert_eq!(all.len(), model::DIESEL.len());
    let s = sup(&[0x04, 0x05, 0x0C]);
    assert_eq!(
        model::selection(Preset::Diesel, &[], Some(&s)),
        vec!["engine_load", "coolant_temp"]
    );
    let custom = vec![
        "rpm".to_string(),
        "coolant_temp".into(),
        "nope".into(),
        "coolant_temp".into(),
        "boost_kpa".into(),
    ];
    assert_eq!(
        model::selection(Preset::Custom, &custom, None),
        vec!["coolant_temp"]
    );
    let every = model::selection(Preset::All, &[], Some(&s));
    assert_eq!(every, vec!["engine_load", "coolant_temp"]);
}

#[test]
fn polls_request_supported_pids_plus_gauges_and_drive_keys() {
    // Nothing discovered yet: every known PID of the keys.
    let p = model::pids_for(&["maf".to_string()], None);
    assert!(p.contains(&0x10) && p.contains(&0x0C) && p.contains(&0x0B));
    assert!(p.windows(2).all(|w| w[0] < w[1]), "sorted, unique");
    // Discovered: unsupported PIDs are dropped, boost falls back to 0x87.
    let s = sup(&[0x0C, 0x05, 0x87, 0x33]);
    let p = model::pids_for(&["maf".to_string(), "coolant_temp".into()], Some(&s));
    assert_eq!(p, vec![0x05, 0x0C, 0x87]);
    // No baro: no boost at all.
    let s = sup(&[0x0C, 0x0B]);
    assert_eq!(model::pids_for(&[], Some(&s)), vec![0x0C]);
}

#[test]
fn levels_units_and_numbers() {
    let ect = obd::channel("coolant_temp");
    assert_eq!(model::level(ect, Some(90.0)), Level::Normal);
    assert_eq!(model::level(ect, Some(105.0)), Level::Warn);
    assert_eq!(model::level(ect, Some(120.0)), Level::Crit);
    assert_eq!(model::level(ect, None), Level::Normal);
    assert_eq!(model::level(obd::channel("rpm"), Some(9e9)), Level::Normal);
    assert_eq!(
        model::shown("kPa", Some(150.0), PressureUnit::Bar),
        (Some(1.5), "bar".to_string(), 2)
    );
    assert_eq!(
        model::shown("kPa", Some(150.0), PressureUnit::KPa),
        (Some(150.0), "kPa".to_string(), 0)
    );
    assert_eq!(model::fmt_num(Some(-0.004), 2), "0.00");
    assert_eq!(model::fmt_num(Some(-1.5), 1), "-1.5");
    assert_eq!(model::fmt_num(None, 1), "\u{2014}");
}

fn readout() -> Readout {
    let mut ro = Readout::default();
    ro.values
        .insert("coolant_temp".into(), Some(ObdValue::Scalar(88.0)));
    ro.values.insert(
        "egt_b1".into(),
        Some(ObdValue::Array(vec![Some(400.0), None, Some(410.5)])),
    );
    ro.values.insert("maf".into(), None);
    ro.errors
        .insert("maf".into(), "not supported by ECU".into());
    ro
}

#[test]
fn feed_frames_split_multi_sensor_values() {
    let FeedEvent::Frame { source, values, .. } = model::feed_frame(5, &readout()) else {
        panic!()
    };
    assert_eq!(source, "dashboard");
    let keys: Vec<&str> = values.iter().map(|v| v.key.as_str()).collect();
    assert_eq!(keys, vec!["coolant_temp", "egt_b1#0", "egt_b1#2", "maf"]);
    assert_eq!(values[2].name, "Exhaust gas temperature bank 1 S3");
    assert_eq!(values[2].value, Some(410.5));
    assert_eq!(values[3].value, None);
    assert!(matches!(
        model::feed_error("adapter busy: Live log"),
        FeedEvent::Error { busy: true, .. }
    ));
}

#[test]
fn csv_rows_match_the_core_live_log_lines() {
    let ro = readout();
    let rows = model::csv_rows(1_700_000_000_123, &ro);
    let joined: Vec<String> = rows
        .iter()
        .map(|r| {
            r.iter()
                .map(|c| crate::widgets::csv::field(c).into_owned())
                .collect::<Vec<_>>()
                .join(",")
        })
        .collect();
    assert_eq!(joined, obd::live_log_lines(1_700_000_000_123, &ro));
    assert_eq!(model::csv_header().len(), 8);
}

fn wait_polls(board: &Mutex<Board>, n: u64) {
    let t0 = Instant::now();
    while lock(board).polls < n {
        assert!(t0.elapsed() < Duration::from_secs(10), "stuck");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn run_for(
    bus: &Bus,
    session: Arc<Mutex<Option<obd::ObdSession>>>,
    polls: usize,
) -> (Board, Vec<FeedEvent>) {
    let feed = LiveFeed::default();
    let rx = feed.subscribe();
    let board = Arc::new(Mutex::new(Board::default()));
    let run = Run {
        adapter: bus.access("sim"),
        target: DiagTarget::ENGINE,
        ctl: Arc::new(Ctl::new(50, model::selection(Preset::Diesel, &[], None))),
        board: board.clone(),
        feed,
        rec: Default::default(),
        session,
    };
    let b2 = board.clone();
    let out = poll::run_with(&run, &move || lock(&b2).polls as usize >= polls);
    assert!(out.is_ok(), "{out:?}");
    let b = lock(&board).clone();
    (b, rx.try_iter().collect())
}

#[test]
fn polling_discovers_then_reads_only_on_the_read_path() {
    let bus = bus_with(engine());
    let session = Arc::new(Mutex::new(None));
    let (b, events) = run_for(&bus, session.clone(), 3);
    assert_eq!(b.phase, Phase::Stopped);
    assert!(b.polls >= 3);
    let s = b.supported.clone().expect("discovered");
    assert!(s.contains(&0x0C) && s.contains(&0x05));
    assert_eq!(b.multi_pid, Some(true));
    assert!(model::scalar(b.values.get("rpm").unwrap()).is_some());
    assert!(b
        .tracks
        .get("coolant_temp")
        .is_some_and(|t| t.last.is_some()));
    // Frames reached the feed, then the stop.
    assert!(events
        .iter()
        .any(|e| matches!(e, FeedEvent::Frame { source, .. } if source == "dashboard")));
    assert!(matches!(events.last(), Some(FeedEvent::Stop { .. })));
    // Only read services went out (mode 01 / 09, UDS 22 for the VIN).
    let wire = bus.wire();
    assert!(!wire.is_empty());
    for (_, req) in &wire {
        assert!(crate::ops::refuse_ecu_changing(req).is_ok(), "{req:02X?}");
        assert!(matches!(req[0], 0x01 | 0x09 | 0x22), "{req:02X?}");
    }
    // The learned session is cached: a second run does not discover again.
    assert!(lock(&session)
        .as_ref()
        .is_some_and(|s| s.supported().is_some()));
    bus.clear_wire();
    let (b2, _) = run_for(&bus, session, 1);
    assert!(b2.supported.is_some());
    assert!(
        !bus.wire().iter().any(|(_, r)| r == &vec![0x01, 0x00]),
        "no rediscovery"
    );
}

#[test]
fn single_pid_ecus_are_learned() {
    let mut e = engine();
    e.multi = MultiMode::Reject;
    let bus = bus_with(e);
    let (b, _) = run_for(&bus, Arc::new(Mutex::new(None)), 2);
    assert_eq!(b.multi_pid, Some(false));
    assert!(model::scalar(b.values.get("rpm").unwrap()).is_some());
}

#[test]
fn an_absent_ecu_fails_cleanly_and_says_so_on_the_feed() {
    // Nothing on 7E0: discovery fails, the job ends, the feed hears it.
    let bus = Bus::default();
    let feed = LiveFeed::default();
    let rx = feed.subscribe();
    let board = Arc::new(Mutex::new(Board::default()));
    let run = Run {
        adapter: bus.access("sim"),
        target: DiagTarget::ENGINE,
        ctl: Arc::new(Ctl::new(50, vec![])),
        board: board.clone(),
        feed,
        rec: Default::default(),
        session: Default::default(),
    };
    assert!(poll::run_with(&run, &|| false).is_err());
    assert!(matches!(lock(&board).phase, Phase::Failed(_)));
    let ev: Vec<FeedEvent> = rx.try_iter().collect();
    assert!(matches!(ev[0], FeedEvent::Error { .. }));
    assert!(matches!(ev.last(), Some(FeedEvent::Stop { .. })));
}

#[test]
fn the_view_polls_the_app_target_until_stopped() {
    let bus = bus_with(engine());
    let mut fx = Fixture::new("dash-view");
    let mut v = DashboardView::default();
    // No adapter: refused before anything is sent.
    assert!(fx.host(DiagTarget::ENGINE, |h| v.start(h)).is_err());
    fx.adapter = Some(bus.access("sim"));
    fx.host(DiagTarget::ENGINE, |h| v.start(h)).unwrap();
    assert_eq!(v.count().as_deref(), Some("live"));
    wait_polls(&v.board, 2);
    v.stop();
    let t0 = Instant::now();
    while v.running() {
        assert!(t0.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(5));
    }
    v.poll_job();
    assert!(v.count().is_none());
    assert_eq!(lock(&v.board).phase, Phase::Stopped);
    assert!(bus.wire().iter().all(|(tx, _)| *tx == 0x7E0));
}

/// The preset, selection, unit and interval are remembered in egui's
/// persisted memory: loaded on the first frame, saved when they change.
#[test]
fn preferences_are_remembered_between_runs() {
    use eframe::egui;
    let ctx = egui::Context::default();
    let frame = |ctx: &egui::Context, v: &mut DashboardView| {
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| v.sync_prefs(ui));
        // Headless: nothing uploads the font atlas.
        out.textures_delta.clear();
    };
    // First run: defaults, then the user changes them.
    let mut a = DashboardView::default();
    frame(&ctx, &mut a);
    a.preset = Preset::Custom;
    a.custom = vec!["rpm".into(), "speed".into()];
    a.punit = PressureUnit::KPa;
    a.interval_ms = 1000;
    frame(&ctx, &mut a);
    // Next run (a new view over the same memory) starts with them.
    let mut b = DashboardView::default();
    frame(&ctx, &mut b);
    assert_eq!(b.preset, Preset::Custom);
    assert_eq!(b.custom, ["rpm", "speed"]);
    assert_eq!(b.punit, PressureUnit::KPa);
    assert_eq!(b.interval_ms, 1000);
    // An interval the panel does not offer is not taken over.
    ctx.data_mut(|d| {
        d.insert_persisted(
            egui::Id::unique("opentune-dashboard-prefs"),
            super::DashPrefs {
                preset: Preset::Petrol,
                custom: Vec::new(),
                punit: PressureUnit::Bar,
                interval_ms: 7,
            },
        )
    });
    let mut c = DashboardView::default();
    frame(&ctx, &mut c);
    assert_eq!(c.preset, Preset::Petrol);
    assert_eq!(c.interval_ms, 500);
}
