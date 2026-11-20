//! Live log view tests: a scripted fake adapter (live_start / live_tick /
//! live_stop answer canned engine JSON) for the panel's flow, and the real
//! path through the hub's live-log engine on a simulated bus (udssim with
//! `0x2C`). Synthetic only: no hardware, no ECU writes.

use super::session::{self, Ctl, RowSink, Run};
use super::*;
use crate::host::{
    Ack, AdapterAccess, BackupReceipt, Capabilities, Confirms, DiagJob, DiagTarget, FeedEvent,
    HostAction, HostShared, Jobs, LinkInfo, LiveSpec,
};
use serde_json::{json, Value};
use std::any::Any;
use std::collections::VecDeque;
use std::sync::mpsc::Receiver;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A fake adapter whose live engine answers from a script.
struct Fake {
    start: Result<Value, String>,
    /// Ticks in order; when it runs dry, `idle_tick` repeats.
    ticks: Mutex<VecDeque<Result<Value, String>>>,
    calls: Mutex<Vec<String>>,
    specs: Mutex<Vec<(LiveSpec, DiagTarget)>>,
}

fn tick(ts: u64, rpm: f64, map: f64) -> Value {
    json!({
        "ts": ts, "count": 2,
        "samples": [
            {"paramId": "rpm", "name": "Engine speed", "raw": 1, "value": rpm, "units": "rpm", "note": "", "text": null},
            {"paramId": "map", "name": "MAP", "raw": 2, "value": map, "units": "kPa", "note": "", "text": null}
        ],
        "status": {"mode": "polling", "reason": "fast mode not requested", "dynamic_dids": [], "polled": ["rpm", "map"]},
        "rate": {"ticks": 1, "samples_per_s": 2.0, "requests_per_sample": 2.0},
        "running": true, "ended": null,
        "target": {"tx": "7E0", "rx": "7E8", "assumed": false},
        "link": {"state": "up"}
    })
}

fn start_report() -> Value {
    json!({
        "status": {"mode": "polling", "reason": "fast mode not requested"},
        "channels": 2,
        "params": [{"id": "rpm", "name": "Engine speed", "units": "rpm"}, {"id": "map", "name": "MAP", "units": "kPa"}],
        "ssm": null,
        "target": {"tx": "7E0", "rx": "7E8", "assumed": false},
        "link": {"state": "up"}
    })
}

impl Fake {
    fn new(start: Result<Value, String>, ticks: Vec<Result<Value, String>>) -> Arc<Self> {
        Arc::new(Self {
            start,
            ticks: Mutex::new(ticks.into()),
            calls: Default::default(),
            specs: Default::default(),
        })
    }

    fn calls(&self) -> Vec<String> {
        lock(&self.calls).clone()
    }

    fn count(&self, what: &str) -> usize {
        self.calls().iter().filter(|c| *c == what).count()
    }
}

impl AdapterAccess for Fake {
    fn link(&self) -> LinkInfo {
        LinkInfo {
            driver: "fake".into(),
            connected: true,
            ..Default::default()
        }
    }
    fn read_job(
        &self,
        _op: &str,
        _t: DiagTarget,
        _job: DiagJob,
    ) -> Result<Box<dyn Any + Send>, String> {
        Err("the live log never reads directly".into())
    }
    fn mutate_job(
        &self,
        _op: &str,
        _ack: Ack,
        _b: &BackupReceipt,
        _job: DiagJob,
    ) -> Result<Box<dyn Any + Send>, String> {
        panic!("the live log never mutates")
    }
    fn capabilities(&self) -> Option<Capabilities> {
        Some(Capabilities {
            can: true,
            ..Default::default()
        })
    }
    fn live_start(&self, spec: LiveSpec, target: DiagTarget) -> Result<Value, String> {
        lock(&self.calls).push("start".into());
        lock(&self.specs).push((spec, target));
        std::thread::sleep(Duration::from_millis(10));
        self.start.clone()
    }
    fn live_tick(&self) -> Result<Value, String> {
        lock(&self.calls).push("tick".into());
        std::thread::sleep(Duration::from_millis(5));
        let n = self.count("tick") as u64;
        lock(&self.ticks)
            .pop_front()
            .unwrap_or_else(|| Ok(tick(1_700_000_000_000 + n * 50, 800.0 + n as f64, 100.0)))
    }
    fn live_stop(&self) -> Result<Value, String> {
        lock(&self.calls).push("stop".into());
        Ok(json!({
            "running": false, "status": {"mode": "polling"}, "ended": null,
            "rate": {"ticks": self.count("tick")}, "target": {}, "link": {"state": "up"},
            "events": [], "ssm": null
        }))
    }
}

/// What a `Host` borrows, owned by a test.
struct Rig {
    ctx: egui::Context,
    jobs: Jobs,
    confirms: Confirms,
    actions: Vec<HostAction>,
    status: Option<String>,
    shared: HostShared,
    adapter: Option<Arc<dyn AdapterAccess>>,
}

impl Rig {
    /// A rig over the fake adapter.
    fn fake(fake: &Arc<Fake>) -> Self {
        let a: Arc<dyn AdapterAccess> = fake.clone();
        Self::new(Some(a))
    }

    fn new(adapter: Option<Arc<dyn AdapterAccess>>) -> Self {
        let ctx = egui::Context::default();
        theme::setup(&ctx, theme::ThemeMode::Dark);
        Self {
            jobs: Jobs::new(ctx.clone()),
            ctx,
            confirms: Confirms::default(),
            actions: Vec::new(),
            status: None,
            shared: HostShared::default(),
            adapter,
        }
    }

    fn with_host<R>(&mut self, f: impl FnOnce(&mut Host<'_>) -> R) -> R {
        let keys = opentune_hub::seedkeys::SeedKeys::default();
        let mut host = Host {
            tokens: theme::DARK,
            image_path: None,
            image: None,
            image_edited: false,
            defs: &[],
            def_idx: None,
            map_idx: 0,
            checksums: &[],
            adapter: self.adapter.as_ref(),
            diagnostics: None,
            seedkeys: crate::host::KeyRing::new(&keys),
            target: Ok(DiagTarget::ENGINE),
            jobs: &self.jobs,
            confirms: &mut self.confirms,
            actions: &mut self.actions,
            status: &mut self.status,
            shared: &self.shared,
        };
        f(&mut host)
    }

    /// One frame with the view drawn.
    fn frame(&mut self, v: &mut LivelogView) {
        let ctx = self.ctx.clone();
        let mut out = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(1400.0, 900.0),
                )),
                ..Default::default()
            },
            |ui| {
                self.with_host(|host| v.ui(ui, host));
            },
        );
        // Headless: nothing uploads the font atlas.
        out.textures_delta.clear();
    }

    /// One frame of another view (the Live log is not drawn).
    fn elsewhere(&mut self) {
        let mut out = self.ctx.run_ui(egui::RawInput::default(), |_ui| {});
        out.textures_delta.clear();
    }

    /// Draw frames until `done`.
    fn until(&mut self, v: &mut LivelogView, done: impl Fn(&mut LivelogView) -> bool) {
        let t0 = Instant::now();
        loop {
            self.frame(v);
            if done(v) {
                return;
            }
            assert!(t0.elapsed() < Duration::from_secs(30), "timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Start a session on the view (as the Start button does).
    fn start(&mut self, v: &mut LivelogView) -> Result<(), String> {
        let ctx = self.ctx.clone();
        self.with_host(|h| v.start(h, &ctx))
    }
}

fn wait(what: &str, mut done: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !done() {
        assert!(t0.elapsed() < Duration::from_secs(30), "timed out: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn did_param(id: &str, name: &str, did: u16, factor: f64) -> opentune_logdef::LogParam {
    use opentune_logdef::{Addressing, Conversion, ConversionFit, LogParam, ProtocolKind};
    LogParam {
        id: id.into(),
        name: name.into(),
        description: String::new(),
        units: "u".into(),
        protocol: ProtocolKind::UdsMode22,
        addressing: Addressing::UdsDid { did },
        storagetype: None,
        endian: opentune_core::ecudef::Endian::Big,
        value_kind: Default::default(),
        conversion: Some(Conversion {
            units: "u".into(),
            expr: "X".into(),
            format: None,
            fit: ConversionFit::Linear {
                factor,
                offset: 0.0,
            },
            gauge_min: None,
            gauge_max: None,
            gauge_step: None,
        }),
        conversions: Vec::new(),
        min: None,
        max: None,
        alternates: Vec::new(),
        depends_on: Vec::new(),
        convert_group: None,
        enabled: None,
        ssm: None,
    }
}

/// A JSON logger definition with three UDS DID channels (the hub test's
/// rpm / ect / map on DIDs 1000..1002), written to a temporary file.
fn def_file(tag: &str) -> PathBuf {
    use opentune_logdef::{EcuGroup, LogDef, ProtocolGroup, ProtocolKind, SourceFormat};
    let def = LogDef {
        format: SourceFormat::RomRaiderLogger,
        version: Some("t1".into()),
        convert_factors: Vec::new(),
        protocols: vec![ProtocolGroup {
            id: "uds".into(),
            kind: ProtocolKind::UdsMode22,
            baud: None,
            ecus: vec![EcuGroup {
                id: None,
                name: None,
                ecu_type: None,
                params: vec![
                    did_param("rpm", "Engine speed", 0x1000, 0.25),
                    did_param("ect", "Coolant", 0x1001, 1.0),
                    did_param("map", "MAP", 0x1002, 1.0),
                    // The same id under another variant: listed once.
                    did_param("rpm", "Engine speed", 0x1000, 0.25),
                ],
            }],
        }],
        warnings: Vec::new(),
    };
    let p = std::env::temp_dir().join(format!(
        "ot-native-livelog-{tag}-{}.json",
        std::process::id()
    ));
    std::fs::write(&p, serde_json::to_string(&def).unwrap()).unwrap();
    p
}

/// A view with `def_file(tag)` loaded and rpm + map chosen.
fn loaded(rig: &mut Rig, tag: &str) -> (LivelogView, PathBuf) {
    let mut v = LivelogView::default();
    let p = def_file(tag);
    let path = p.clone();
    rig.with_host(|h| v.open(h, path));
    rig.until(&mut v, |v| v.def.is_some() || v.note.is_some());
    let d = v.def.as_mut().expect("definition loaded");
    assert_eq!(
        d.params.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
        ["rpm", "ect", "map"],
        "one row per id"
    );
    assert_eq!(d.version.as_deref(), Some("t1"));
    d.checked = vec![true, false, true];
    v.opts.interval_ms = 20;
    (v, p)
}

fn drain(rx: &Receiver<FeedEvent>) -> Vec<FeedEvent> {
    rx.try_iter().collect()
}

fn done(v: &mut LivelogView) -> bool {
    v.job.is_none() && lock(&v.board).phase == Phase::Done
}

// ---------------------------------------------------------------------------
// The panel's flow on the fake engine
// ---------------------------------------------------------------------------

#[test]
fn a_session_ticks_publishes_to_the_drive_feed_and_stops_on_stop() {
    let fake = Fake::new(Ok(start_report()), Vec::new());
    let mut rig = Rig::fake(&fake);
    let (mut v, p) = loaded(&mut rig, "flow");
    let feed = rig.shared.live.subscribe();
    rig.start(&mut v).unwrap();
    assert!(rig.start(&mut v).unwrap_err().contains("already running"));
    rig.until(&mut v, |v| lock(&v.board).ticks >= 3);
    rig.frame(&mut v);
    // The spec the engine got: the chosen channels on the app's target.
    let (spec, target) = lock(&fake.specs)[0].clone();
    assert_eq!(spec.ids, ["rpm", "map"]);
    assert_eq!(spec.def_path, p.display().to_string());
    assert_eq!((spec.timeout_ms, spec.fast), (model::TIMEOUT_MS, false));
    assert_eq!(spec.transport.as_deref(), Some("uds"));
    assert_eq!(target, DiagTarget::ENGINE);
    {
        let b = lock(&v.board);
        assert_eq!(b.phase, Phase::Running);
        assert_eq!(b.chans.len(), 2);
        assert!(b.chan("rpm").unwrap().hist.len() >= 3);
        assert_eq!(model::target_text(&b.target), "tx 7E0 / rx 7E8");
        assert_eq!(model::mode_badge(&b.status).unwrap().text, "polling");
    }
    // The first channels are charted once values arrive.
    assert_eq!(v.plotted, ["rpm", "map"]);
    assert_eq!(v.count().as_deref(), Some("live"));
    let evs = drain(&feed);
    assert!(evs.len() >= 3, "{evs:?}");
    match &evs[0] {
        FeedEvent::Frame { source, values, .. } => {
            assert_eq!(source, model::SOURCE);
            assert_eq!(values[0].key, "rpm");
            assert_eq!(values[0].unit, "rpm");
            assert!(values[0].value.is_some());
        }
        e => panic!("{e:?}"),
    }
    v.stop();
    rig.until(&mut v, done);
    assert_eq!(fake.count("start"), 1);
    assert_eq!(fake.count("stop"), 1, "{:?}", fake.calls());
    assert_eq!(fake.calls().last().map(String::as_str), Some("stop"));
    assert_eq!(lock(&v.board).ended.as_deref(), Some("stopped"));
    assert!(drain(&feed)
        .iter()
        .any(|e| matches!(e, FeedEvent::Stop { source } if source == model::SOURCE)));
    assert_eq!(v.count(), None);
    // The last session stays on screen; nothing ticks any more.
    let n = fake.count("tick");
    for _ in 0..5 {
        rig.frame(&mut v);
    }
    assert_eq!(fake.count("tick"), n);
    let _ = std::fs::remove_file(&p);
}

#[test]
fn leaving_the_view_stops_the_session_unless_kept_in_the_background() {
    let fake = Fake::new(Ok(start_report()), Vec::new());
    let mut rig = Rig::fake(&fake);
    let (mut v, p) = loaded(&mut rig, "leave");
    rig.start(&mut v).unwrap();
    rig.until(&mut v, |v| lock(&v.board).ticks >= 1);
    // Other views are drawn: the session ends by itself.
    let t0 = Instant::now();
    while fake.count("stop") == 0 {
        rig.elsewhere();
        assert!(t0.elapsed() < Duration::from_secs(10), "never stopped");
        std::thread::sleep(Duration::from_millis(10));
    }
    rig.until(&mut v, done);
    assert_eq!(
        lock(&v.board).ended.as_deref(),
        Some("left the Live log view")
    );

    // Kept in the background: it goes on logging (e.g. for the Drive view).
    v.keep = true;
    rig.start(&mut v).unwrap();
    rig.until(&mut v, |v| lock(&v.board).ticks >= 1);
    let before = fake.count("tick");
    for _ in 0..30 {
        rig.elsewhere();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(fake.count("tick") > before + 3, "still ticking");
    assert_eq!(fake.count("stop"), 1, "not stopped again");
    // Turning it off while away stops it at once.
    v.keep = false;
    v.ctl.as_ref().unwrap().set_keep(false);
    wait("stop after keep off", || fake.count("stop") == 2);
    rig.until(&mut v, done);
    let _ = std::fs::remove_file(&p);
}

#[test]
fn the_engine_ending_the_session_ends_the_job_with_its_reason() {
    let lost = json!({
        "ts": 1_700_000_000_100u64, "count": 1,
        "samples": [{"paramId": "link", "name": "Link status", "raw": null, "value": null,
            "units": "", "note": "lost: 5 dead ticks", "text": "lost"}],
        "status": {"mode": "polling"}, "rate": {"ticks": 2},
        "running": false, "ended": "connection lost (5 dead ticks)",
        "target": {"tx": "7E0", "rx": "7E8", "assumed": false},
        "link": {"state": "lost", "reconnect": false, "error": "timeout"}
    });
    let fake = Fake::new(
        Ok(start_report()),
        vec![Ok(tick(1_700_000_000_000, 800.0, 99.0)), Ok(lost)],
    );
    let mut rig = Rig::fake(&fake);
    let (mut v, p) = loaded(&mut rig, "ended");
    let feed = rig.shared.live.subscribe();
    rig.start(&mut v).unwrap();
    rig.until(&mut v, done);
    assert_eq!(fake.count("tick"), 2);
    assert_eq!(
        fake.count("stop"),
        1,
        "stop clears the session even when it ended"
    );
    let b = lock(&v.board);
    assert_eq!(b.ended.as_deref(), Some("connection lost (5 dead ticks)"));
    assert_eq!(b.events.len(), 1);
    assert_eq!(b.events[0].1, "lost");
    let evs = drain(&feed);
    assert!(matches!(&evs[0], FeedEvent::Frame { .. }));
    match &evs[1] {
        FeedEvent::Error { message, busy, .. } => {
            assert!(message.starts_with("link lost"), "{message}");
            assert!(!busy);
        }
        e => panic!("a dead link is never shown as live: {e:?}"),
    }
    assert!(matches!(&evs[2], FeedEvent::Stop { .. }));
    let _ = std::fs::remove_file(&p);
}

#[test]
fn a_failed_start_says_why_and_never_ticks() {
    let fake = Fake::new(Err("no adapter connected: pick a cable".into()), Vec::new());
    let mut rig = Rig::fake(&fake);
    let (mut v, p) = loaded(&mut rig, "fail");
    rig.start(&mut v).unwrap();
    rig.until(&mut v, done);
    assert_eq!(fake.calls(), ["start"]);
    let e = lock(&v.board).error.clone().unwrap();
    assert!(
        e.contains("start failed") && e.contains("no adapter connected"),
        "{e}"
    );
    // No adapter / no definition: refused before any job.
    let mut none = Rig::new(None);
    let ctx = none.ctx.clone();
    assert!(none
        .with_host(|h| v.start(h, &ctx))
        .unwrap_err()
        .contains("no adapter"));
    let mut empty = LivelogView::default();
    let e = rig.with_host(|h| empty.start(h, &ctx)).unwrap_err();
    assert!(e.contains("logger definition"), "{e}");
    // A bad option is refused before the engine is asked.
    v.opts.fast = true;
    v.opts.max_record = "lots".into();
    let e = rig.start(&mut v).unwrap_err();
    assert!(e.contains("max record"), "{e}");
    assert_eq!(fake.count("start"), 1);
    let _ = std::fs::remove_file(&p);
}

#[test]
fn dropping_the_view_stops_its_session() {
    let fake = Fake::new(Ok(start_report()), Vec::new());
    let mut rig = Rig::fake(&fake);
    let (mut v, p) = loaded(&mut rig, "drop");
    v.keep = true;
    rig.start(&mut v).unwrap();
    rig.until(&mut v, |v| lock(&v.board).ticks >= 1);
    drop(v);
    assert_eq!(fake.count("stop"), 1, "{:?}", fake.calls());
    let _ = std::fs::remove_file(&p);
}

#[test]
fn a_bad_definition_is_reported_not_loaded() {
    let mut rig = Rig::new(None);
    let mut v = LivelogView::default();
    rig.with_host(|h| v.open(h, PathBuf::from("/nonexistent/logger.xml")));
    rig.until(&mut v, |v| v.note.is_some());
    let (tone, n) = v.note.clone().unwrap();
    assert_eq!(tone, Tone::Crit);
    assert!(n.contains("/nonexistent/logger.xml"), "{n}");
    assert!(v.def.is_none());
}

// ---------------------------------------------------------------------------
// CSV rows (and GPS channels) from the session job
// ---------------------------------------------------------------------------

/// A recorder that keeps rows in memory.
struct Rows {
    on: bool,
    rows: Vec<[String; 8]>,
}

impl RowSink for Rows {
    fn recording(&self) -> bool {
        self.on
    }
    fn push_row(&mut self, row: &[String; 8]) {
        self.rows.push(row.clone());
    }
}

#[test]
fn the_job_records_every_sample_marker_and_gps_channel() {
    const NMEA: &str = "$GPRMC,123519,A,4807.038,N,01131.000,E,022.4,084.4,230394,003.1,W*6A\r\n\
$GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*47\r\n";
    let marker_tick = {
        let mut t = tick(1_700_000_000_050, 900.0, 101.0);
        t["samples"].as_array_mut().unwrap().push(
            json!({"paramId": "link", "name": "Link status", "text": "recovered", "note": "back"}),
        );
        t
    };
    let fake = Fake::new(
        Ok(start_report()),
        vec![Ok(tick(1_700_000_000_000, 800.0, 100.0)), Ok(marker_tick)],
    );
    let gps = crate::widgets::gps::GpsSource::default();
    let feed = opentune_gps::ReplayFeed::from_bytes("test", NMEA.as_bytes()).looping(true);
    gps.open_feed(Box::new(feed), "mem", "replay");
    let rows = Arc::new(Mutex::new(Rows {
        on: true,
        rows: Vec::new(),
    }));
    let ctl = Arc::new(Ctl::new(1, false, 0));
    let board = Arc::new(Mutex::new(Board::default()));
    let live = crate::host::LiveFeed::default();
    let sub = live.subscribe();
    let rec: Arc<Mutex<dyn RowSink>> = rows.clone();
    let run = Run {
        adapter: fake.clone(),
        spec: LiveSpec::default(),
        target: DiagTarget::ENGINE,
        ctl: ctl.clone(),
        board: board.clone(),
        feed: live,
        rec,
        gps: Some(gps),
        frames: None,
    };
    let jobs = Jobs::new(egui::Context::default());
    let mut h = jobs.spawn("live", move |p| session::run(p, run));
    wait("two ticks", || lock(&board).ticks >= 2);
    ctl.stop("test");
    wait("job", || h.take().is_some());
    let r = lock(&rows);
    let first: Vec<&str> = r
        .rows
        .iter()
        .filter(|c| c[0] == "1700000000000")
        .map(|c| c[1].as_str())
        .collect();
    assert_eq!(first[..2], ["rpm", "map"]);
    assert!(
        first.contains(&"gps_lat") && first.contains(&"gps_speed_kmh"),
        "{first:?}"
    );
    assert!(
        first.contains(&opentune_logdef::SPEED_ERROR_ID),
        "{first:?}"
    );
    let rpm = r.rows.iter().find(|c| c[1] == "rpm").unwrap();
    assert_eq!(
        rpm[2..7],
        ["Engine speed", "1", "800", "rpm", ""].map(String::from)
    );
    let link = r.rows.iter().find(|c| c[1] == "link").unwrap();
    assert_eq!((link[6].as_str(), link[7].as_str()), ("back", "recovered"));
    // The Drive view got the GPS channels with the ECU's.
    let frame = sub.try_iter().next().unwrap();
    match frame {
        FeedEvent::Frame { values, .. } => {
            assert!(values.iter().any(|v| v.key == "gps_lat"), "{values:?}");
        }
        e => panic!("{e:?}"),
    }
    assert_eq!(lock(&board).ended.as_deref(), Some("test"));
}

#[test]
fn rows_wait_for_a_busy_recorder() {
    let fake = Fake::new(Ok(start_report()), Vec::new());
    let rows = Arc::new(Mutex::new(Rows {
        on: true,
        rows: Vec::new(),
    }));
    let ctl = Arc::new(Ctl::new(1, false, 0));
    let board = Arc::new(Mutex::new(Board::default()));
    let rec: Arc<Mutex<dyn RowSink>> = rows.clone();
    let run = Run {
        adapter: fake.clone(),
        spec: LiveSpec::default(),
        target: DiagTarget::ENGINE,
        ctl: ctl.clone(),
        board: board.clone(),
        feed: Default::default(),
        rec,
        gps: None,
        frames: None,
    };
    let jobs = Jobs::new(egui::Context::default());
    let mut h = jobs.spawn("live", move |p| session::run(p, run));
    {
        // The recorder is busy (its save dialog is open) for a few ticks.
        let _busy = lock(&rows);
        wait("ticks while busy", || lock(&board).ticks >= 3);
    }
    ctl.stop("test");
    wait("job", || h.take().is_some());
    let ticks = lock(&board).ticks as usize;
    assert_eq!(lock(&rows).rows.len(), ticks * 2, "nothing lost while busy");
}

// ---------------------------------------------------------------------------
// The real path: the hub's live-log engine on a simulated bus
// ---------------------------------------------------------------------------

/// The hub adapter over a simulated engine ECU that implements `0x2C`, with
/// the hub test's DIDs (rpm 6904 x 0.25 = 1726, ect 90, map 101).
fn sim_adapter() -> Arc<crate::hublink::HubAdapter> {
    use opentune_hub::adapter::testkit::SimBus;
    use opentune_hub::adapter::AdapterHub;
    use opentune_udssim::server::{DddidConfig, ServerConfig, UdsServer};
    let hub = AdapterHub::spawn(|| {
        let bus = SimBus::default();
        let mut s = UdsServer::with_config(ServerConfig {
            dddid: DddidConfig {
                supported: true,
                ..DddidConfig::default()
            },
            ..ServerConfig::default()
        });
        s.set_did(0x1000, vec![0x1A, 0xF8]);
        s.set_did(0x1001, vec![0x5A]);
        s.set_did(0x1002, vec![0x00, 0x65]);
        bus.add(0x7E0, 0x7E8, s);
        bus.opener()
    });
    let a = Arc::new(crate::hublink::HubAdapter::new(hub));
    a.set_link(Some(crate::hublink::LinkCfg {
        driver: "sim".into(),
        port: String::new(),
        baud: 0,
        bus: "hs".into(),
    }));
    a
}

#[test]
fn a_fast_session_runs_through_the_hub_engine_and_clears_its_dynamic_did() {
    let a = sim_adapter();
    let dyn_a: Arc<dyn AdapterAccess> = a.clone();
    let mut rig = Rig::new(Some(dyn_a.clone()));
    let (mut v, p) = loaded(&mut rig, "hub");
    v.opts.fast = true;
    let feed = rig.shared.live.subscribe();
    rig.start(&mut v).unwrap();
    rig.until(&mut v, |v| {
        let b = lock(&v.board);
        b.ticks >= 3 || b.phase == Phase::Done
    });
    {
        let b = lock(&v.board);
        assert_eq!(b.error, None);
        assert_eq!(b.status["mode"], "fast-2c", "{:?}", b.status);
        let badge = model::mode_badge(&b.status).unwrap();
        assert!(badge.title.contains("0xF200"), "{}", badge.title);
        assert_eq!(b.chans.len(), 2);
        let rpm = b.chan("rpm").unwrap().last.clone().unwrap();
        assert_eq!(rpm.value, Some(1726.0));
        assert_eq!(
            b.chan("map").unwrap().last.as_ref().unwrap().value,
            Some(101.0)
        );
        assert_eq!(model::target_text(&b.target), "tx 7E0 / rx 7E8");
        assert_eq!(b.rate["requests_per_sample"], 1.0, "one 0x22 per sample");
    }
    // The session holds the adapter: a read from another panel is refused.
    let e = dyn_a
        .read("other panel", DiagTarget::ENGINE, |d| {
            d.request(&[0x3E, 0x00])
        })
        .unwrap_err();
    assert!(e.contains("busy"), "{e}");
    v.stop();
    rig.until(&mut v, done);
    assert_eq!(lock(&v.board).error, None);
    // Released, and the dynamic DID is gone on the ECU.
    let r = dyn_a
        .read("check", DiagTarget::ENGINE, |d| {
            d.request(&[0x22, 0xF2, 0x00])
        })
        .unwrap();
    assert_eq!(r, vec![0x7F, 0x22, 0x31], "DDDID cleared by stop");
    assert!(drain(&feed).iter().any(|e| matches!(
        e,
        FeedEvent::Frame { values, .. } if values.iter().any(|x| x.key == "rpm" && x.value == Some(1726.0))
    )));
    let _ = std::fs::remove_file(&p);
}

#[test]
fn the_view_draws_every_phase_without_panicking() {
    let fake = Fake::new(Ok(start_report()), Vec::new());
    let mut rig = Rig::fake(&fake);
    let mut idle = LivelogView::default();
    rig.frame(&mut idle);
    let (mut v, p) = loaded(&mut rig, "draw");
    v.opts.transport = "ssm".into();
    v.opts.fast = true;
    rig.frame(&mut v);
    assert!(!v.opts.fast, "SSM always batches: Fast is cleared");
    v.opts.transport = "uds".into();
    rig.start(&mut v).unwrap();
    rig.until(&mut v, |v| lock(&v.board).ticks >= 2);
    lock(&v.board).ssm = json!({"target": "ecu", "wire": "k-line", "ssm_id": "A1", "rom_id": "B2",
        "capability_bytes": 48, "logged": 2, "notes": ["x"], "not_logged": ["y"]});
    for _ in 0..3 {
        rig.frame(&mut v);
    }
    v.stop();
    rig.until(&mut v, done);
    rig.frame(&mut v);
    assert_eq!(utc_stamp(1_700_000_000_000), "20231114-221320");
    let _ = std::fs::remove_file(&p);
}
