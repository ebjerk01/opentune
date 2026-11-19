//! Diagnostics upload tests. Every endpoint here is fake: the reserved
//! `.invalid` domain, or a mock HTTPS server on the loopback interface with a
//! self-signed test certificate (`testdata/`, protects nothing).

use super::*;
use crate::diaglog::{session_header, DiagLog, SessionInfo, Src};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc;

const TOKEN: &str = "tok_synthetic_0123456789";
const FAKE: &str = "https://diag.example.invalid/v1/upload";
const T0: u64 = 1_800_000_000;

fn tmp(tag: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("ot-diagup-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn block<F: std::future::Future>(f: F) -> F::Output {
    tauri::async_runtime::block_on(f)
}

// ---- a test log --------------------------------------------------------------

struct Log {
    dir: PathBuf,
    log: DiagLog,
}

impl Log {
    fn new(tag: &str) -> Log {
        let dir = tmp(tag).join("logs");
        let log = DiagLog::new();
        // The previous session: started cleanly (first frame painted).
        log.open(&dir, header(T0 - 7200)).unwrap();
        let l = Log { dir, log };
        l.put(
            T0 - 7200,
            Level::Info,
            Src::Startup,
            "ui: first frame painted (page +900 ms)",
        );
        l
    }
    fn put(&self, secs: u64, level: Level, src: Src, msg: &str) {
        let mut r = Record::new(level, src, msg, None);
        r.ts = ts_at(secs);
        self.log.write(r).unwrap();
    }
    /// A clean session start: header + first frame.
    fn clean_session(&self, secs: u64) {
        self.log.write(header(secs)).unwrap();
        self.put(
            secs,
            Level::Info,
            Src::Startup,
            "ui: first frame painted (page +900 ms)",
        );
    }
}

fn header(secs: u64) -> Record {
    let mut h = session_header(&SessionInfo {
        app: "OpenTune".into(),
        version: "0.0.0-test".into(),
        channel: "bench".into(),
        webkit: Some("2.46.0".into()),
        render: json!({ "dmabuf": { "enabled": false } }),
    });
    h.ts = ts_at(secs);
    h
}

/// A log whose previous session panicked; the current one starts at T0.
fn crashed_log(tag: &str) -> Log {
    let l = Log::new(tag);
    l.put(
        T0 - 7100,
        Level::Error,
        Src::Rust,
        "panic: index out of bounds",
    );
    l.clean_session(T0);
    l
}

// ---- a test transport --------------------------------------------------------

#[derive(Default)]
struct Mock {
    calls: Mutex<Vec<Vec<u8>>>,
    next: Mutex<VecDeque<Outcome>>,
}

impl Mock {
    fn n(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
    fn last(&self) -> String {
        String::from_utf8(self.calls.lock().unwrap().last().unwrap().clone()).unwrap()
    }
    fn then(&self, o: Outcome) {
        self.next.lock().unwrap().push_back(o);
    }
}

impl Sender for Mock {
    fn post<'a>(
        &'a self,
        body: &'a [u8],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Outcome> + Send + 'a>> {
        self.calls.lock().unwrap().push(body.to_vec());
        let o = self
            .next
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Outcome::Sent(200));
        Box::pin(async move { o })
    }
}

fn upload(mode: DiagMode) -> DiagUpload {
    DiagUpload {
        endpoint: Url::parse(FAKE).unwrap(),
        token: Some(TOKEN.into()),
        label: Some("test build".into()),
        mode,
    }
}

fn auto(crash: bool, on_error: bool, periodic_min: Option<u32>) -> DiagMode {
    DiagMode::Auto(AutoPolicy {
        crash_on_next_start: crash,
        on_error,
        periodic_min,
    })
}

fn bench() -> BuildChannel {
    BuildChannel::new(BuildChannel::BENCH)
}

fn engine(resolved: Resolved, log: Option<&Log>, mock: &Arc<Mock>) -> Engine {
    let store_dir = log
        .map(|l| l.dir.parent().unwrap().join("diagnostics"))
        .unwrap_or_else(|| tmp("store").join("diagnostics"));
    Engine::new(
        resolved,
        EngineParts {
            store_dir: Some(store_dir),
            log_dir: log.map(|l| l.dir.clone()),
            ctx: RedactContext {
                home: Some("/home/tuner".into()),
                user: Some("tuner".into()),
                host: Some("garagebox".into()),
                secrets: Vec::new(),
            },
            version: "0.0.0-test".into(),
            channel: "bench".into(),
            sender: Some(mock.clone() as Arc<dyn Sender>),
            now: T0,
        },
    )
}

fn embedded(mode: DiagMode) -> Resolved {
    resolve(Some(upload(mode)), &EnvVars::default(), &bench())
}

fn env_manual() -> EnvVars {
    EnvVars {
        endpoint: Some(FAKE.into()),
        token: Some(TOKEN.into()),
        ..EnvVars::default()
    }
}

// ---- configuration -------------------------------------------------------------

#[test]
fn nothing_configured_means_nothing_sent() {
    let r = resolve(None, &EnvVars::default(), &bench());
    assert!(r.upload.is_none() && !r.env_off);
    let mock = Arc::new(Mock::default());
    let l = crashed_log("none");
    let e = engine(r, Some(&l), &mock);
    e.set(Some(true), None, Some(true), None).unwrap();
    let s = e.status(T0);
    assert_eq!(s["configured"], false);
    assert_eq!(s["mode"], "none");
    assert!(s["crash_offer"].is_null());
    let err = e.preview(None, false, T0).unwrap_err();
    assert!(err.contains("no diagnostics upload is configured"), "{err}");
    assert!(block(e.tick(T0 + 3600)).is_none());
    assert_eq!(mock.n(), 0);
}

#[test]
fn the_environment_gives_manual_mode_only() {
    for switch in [None, Some("auto"), Some("on"), Some("1")] {
        let env = EnvVars {
            switch: switch.map(Into::into),
            ..env_manual()
        };
        let r = resolve(None, &env, &bench());
        let u = r.upload.as_ref().expect("configured from the environment");
        assert_eq!(u.mode, DiagMode::Manual, "{switch:?}");
        assert_eq!(r.source, Some(Source::Env));
        let mock = Arc::new(Mock::default());
        let l = crashed_log("envmanual");
        let e = engine(r, Some(&l), &mock);
        assert!(e.auto_policy().is_none());
        e.set(Some(true), Some(false), Some(true), None).unwrap();
        assert!(block(e.tick(T0 + 3600)).is_none(), "no automatic send");
        assert_eq!(mock.n(), 0);
        // The crash is only offered.
        assert_eq!(e.status(T0)["crash_offer"]["reason"], "panic");
    }
    // An embedder's target wins over the environment's.
    let r = resolve(Some(upload(DiagMode::Manual)), &env_manual(), &bench());
    assert_eq!(r.source, Some(Source::Embedder));
    assert!(r.notes.iter().any(|n| n.contains("ignored")));
}

#[test]
fn opentune_diag_off_beats_an_embedders_auto_mode() {
    let env = EnvVars {
        switch: Some("OFF".into()),
        ..env_manual()
    };
    let r = resolve(Some(upload(auto(true, true, Some(10)))), &env, &bench());
    assert!(r.env_off && r.upload.is_none());
    let mock = Arc::new(Mock::default());
    let l = crashed_log("envoff");
    l.put(T0 + 5, Level::Error, Src::Js, "boom");
    let e = engine(r, Some(&l), &mock);
    e.set(Some(true), Some(false), Some(true), None).unwrap();
    assert!(e.auto_policy().is_none());
    for t in [T0, T0 + 700, T0 + 7200] {
        assert!(block(e.tick(t)).is_none());
    }
    assert!(e.preview(None, false, T0).is_err());
    assert_eq!(mock.n(), 0);
    let s = e.status(T0);
    assert_eq!(s["env_off"], true);
    assert_eq!(s["auto_active"], false);
    assert_eq!(s["notice_needed"], false);
}

#[test]
fn https_only_unless_the_developer_flag() {
    let mut u = upload(DiagMode::Manual);
    u.endpoint = Url::parse("http://diag.example.invalid/upload").unwrap();
    let r = resolve(Some(u.clone()), &EnvVars::default(), &bench());
    assert!(r.upload.is_none());
    assert!(r.notes.iter().any(|n| n.contains("HTTPS")), "{:?}", r.notes);
    let dev = EnvVars {
        insecure: Some("1".into()),
        ..EnvVars::default()
    };
    // The flag is ignored on the stable channel ...
    let stable = BuildChannel::new(BuildChannel::STABLE);
    let r = resolve(Some(u.clone()), &dev, &stable);
    assert!(r.upload.is_none() && !r.insecure);
    // ... and honoured on alpha / bench.
    let r = resolve(Some(u), &dev, &bench());
    assert!(r.upload.is_some() && r.insecure);
    // Credentials in the URL and other schemes are refused.
    for bad in [
        "https://user:pw@diag.example.invalid/",
        "ftp://diag.example.invalid/",
    ] {
        let mut u = upload(DiagMode::Manual);
        u.endpoint = Url::parse(bad).unwrap();
        assert!(resolve(Some(u), &dev, &bench()).upload.is_none(), "{bad}");
    }
    let r = resolve(
        None,
        &EnvVars {
            endpoint: Some("not a url".into()),
            ..EnvVars::default()
        },
        &bench(),
    );
    assert!(r.upload.is_none() && !r.notes.is_empty());
}

// ---- manual mode -------------------------------------------------------------

#[test]
fn manual_needs_endpoint_consent_and_a_click() {
    let l = Log::new("manual");
    l.clean_session(T0);
    l.put(
        T0 + 1,
        Level::Error,
        Src::Cmd,
        "image_info failed: /home/tuner/cars/stock.bin",
    );
    let mock = Arc::new(Mock::default());
    let e = engine(resolve(None, &env_manual(), &bench()), Some(&l), &mock);
    // No consent: no preview, no send.
    let err = e.preview(Some("note"), false, T0).unwrap_err();
    assert!(err.contains("Allow sending diagnostics"), "{err}");
    e.set(Some(true), None, None, None).unwrap();
    let p = e
        .preview(Some("it froze on the Maps panel"), false, T0 + 10)
        .unwrap();
    let text = p["text"].as_str().unwrap().to_string();
    assert_eq!(p["bytes"].as_u64().unwrap() as usize, text.len());
    assert!(text.contains("it froze on the Maps panel"));
    assert!(
        text.contains("<PATH>/stock.bin") && !text.contains("tuner"),
        "{text}"
    );
    // A preview alone sends nothing.
    assert_eq!(mock.n(), 0);
    assert!(block(e.send_preview("nope", T0 + 11)).is_err());
    assert_eq!(mock.n(), 0);
    // The click sends exactly the previewed bytes, once.
    let id = p["id"].as_str().unwrap();
    let h = block(e.send_preview(id, T0 + 12)).unwrap();
    assert_eq!(h.status, "sent (200)");
    assert_eq!(h.trigger, "manual");
    assert_eq!(mock.n(), 1);
    assert_eq!(mock.last(), text);
    assert!(
        block(e.send_preview(id, T0 + 13)).is_err(),
        "a preview is sent once"
    );
    assert_eq!(e.history_payload(&h.id).unwrap(), text);
    // Consent withdrawn: refused again.
    e.set(Some(false), None, None, None).unwrap();
    assert!(e.preview(None, false, T0 + 20).is_err());
    // The setting is persisted.
    let again = engine(resolve(None, &env_manual(), &bench()), Some(&l), &mock);
    assert_eq!(again.status(T0)["allow_send"], false);
    again.set(Some(true), None, None, None).unwrap();
    let again = engine(resolve(None, &env_manual(), &bench()), Some(&l), &mock);
    assert_eq!(again.status(T0)["allow_send"], true);
    assert_eq!(again.status(T0)["history"].as_array().unwrap().len(), 1);
}

#[test]
fn after_a_crash_manual_mode_only_offers() {
    let l = crashed_log("offer");
    let mock = Arc::new(Mock::default());
    let e = engine(resolve(None, &env_manual(), &bench()), Some(&l), &mock);
    let s = e.status(T0);
    assert_eq!(s["crash_offer"]["reason"], "panic");
    assert_eq!(mock.n(), 0, "nothing is sent on start");
    e.set(Some(true), None, None, None).unwrap();
    let p = e.preview(None, true, T0).unwrap();
    let text = p["text"].as_str().unwrap();
    assert_eq!(p["trigger"], "crash");
    assert!(text.contains("panic: index out of bounds"));
    assert!(text.contains("\"reason\": \"panic\""));
    block(e.send_preview(p["id"].as_str().unwrap(), T0 + 1)).unwrap();
    assert!(e.status(T0)["crash_offer"].is_null(), "offered once");
    // Dismissing works too (a fresh store).
    let l2 = crashed_log("dismiss");
    let e2 = engine(resolve(None, &env_manual(), &bench()), Some(&l2), &mock);
    assert!(!e2.status(T0)["crash_offer"].is_null());
    e2.set(None, None, None, Some(true)).unwrap();
    assert!(e2.status(T0)["crash_offer"].is_null());
}

#[test]
fn crash_detection() {
    let r = |msg: &str, level: Level, src: Src| Record::new(level, src, msg, None);
    let h = header(T0);
    let clean = vec![
        h.clone(),
        r(
            "ui: first frame painted (page +1 ms)",
            Level::Info,
            Src::Startup,
        ),
    ];
    assert_eq!(payload::crash_reason(&clean), None);
    let mut panicked = clean.clone();
    panicked.push(r("panic: boom", Level::Error, Src::Rust));
    assert_eq!(payload::crash_reason(&panicked), Some("panic"));
    let no_frame = vec![
        h.clone(),
        r("setup plugin: done", Level::Info, Src::Startup),
    ];
    assert_eq!(payload::crash_reason(&no_frame), Some("no first frame"));
    // The previous session, not the current one; a rotation fragment does not count.
    let mut all = no_frame.clone();
    all.extend(clean.clone());
    assert_eq!(payload::previous_crash(&all).unwrap().1, "no first frame");
    let mut frag = vec![r("half a session", Level::Info, Src::Rust)];
    frag.extend(clean.clone());
    assert!(payload::previous_crash(&frag).is_none());
    assert!(payload::previous_crash(&clean).is_none());
}

// ---- auto mode -----------------------------------------------------------------

#[test]
fn auto_waits_for_the_notice_and_the_off_switch_stops_everything() {
    let l = crashed_log("auto-off");
    l.put(T0 + 5, Level::Error, Src::Js, "TypeError: x is undefined");
    let mock = Arc::new(Mock::default());
    let e = engine(embedded(auto(true, true, Some(10))), Some(&l), &mock);
    let s = e.status(T0);
    assert_eq!(s["mode"], "auto");
    assert_eq!(s["notice_needed"], true);
    assert_eq!(s["host"], "diag.example.invalid");
    assert!(
        block(e.tick(T0 + 30)).is_none(),
        "not before the notice was shown"
    );
    // Turned off (from the notice): nothing at all, crash report included.
    e.set(None, Some(true), Some(true), None).unwrap();
    assert!(e.auto_policy().is_none());
    for t in [T0 + 60, T0 + 700, T0 + 4000, T0 + 90_000] {
        assert!(block(e.tick(t)).is_none());
    }
    assert_eq!(mock.n(), 0);
    let s = e.status(T0);
    assert_eq!(s["queue"]["items"], 0);
    assert_eq!(s["auto_active"], false);
    // The off switch is persisted.
    let e2 = engine(embedded(auto(true, true, Some(10))), Some(&l), &mock);
    assert!(e2.auto_policy().is_none());
    // Back on: the crash report goes first, without a click.
    e2.set(None, Some(false), None, None).unwrap();
    let h = block(e2.tick(T0 + 100)).unwrap();
    assert_eq!(h.trigger, "crash");
    assert_eq!(mock.n(), 1);
    assert!(mock.last().contains("panic: index out of bounds"));
    assert!(mock.last().contains("\"label\": \"test build\""));
    // Reported once, also across restarts.
    let e3 = engine(embedded(auto(true, false, None)), Some(&l), &mock);
    assert!(block(e3.tick(T0 + 5000)).is_none());
    assert_eq!(mock.n(), 1);
}

#[test]
fn errors_are_coalesced_and_rate_limited() {
    let l = Log::new("coalesce");
    l.clean_session(T0);
    for i in 0..30 {
        l.put(
            T0 + 1 + i,
            Level::Error,
            Src::Js,
            &format!("error number {i}"),
        );
    }
    let mock = Arc::new(Mock::default());
    let e = engine(embedded(auto(true, true, None)), Some(&l), &mock);
    e.set(None, None, Some(true), None).unwrap();
    let h = block(e.tick(T0 + 60)).unwrap();
    assert_eq!(h.trigger, "error");
    assert_eq!(mock.n(), 1, "30 errors, one report");
    let body = mock.last();
    assert!(body.contains("error number 0") && body.contains("error number 29"));
    // A new burst within 10 minutes waits ...
    for i in 0..10 {
        l.put(T0 + 100 + i, Level::Error, Src::Cmd, &format!("later {i}"));
    }
    for t in [T0 + 120, T0 + 300, T0 + 600] {
        assert!(block(e.tick(t)).is_none(), "no send before the slot");
    }
    assert_eq!(mock.n(), 1);
    assert!(e.status(T0 + 300)["next_slot"].is_string());
    // ... and goes out as one report with only the new records.
    block(e.tick(T0 + 661)).unwrap();
    assert_eq!(mock.n(), 2);
    let body = mock.last();
    assert!(body.contains("later 0") && body.contains("later 9"));
    assert!(!body.contains("error number 5"));
    // Nothing new: nothing sent.
    assert!(block(e.tick(T0 + 2000)).is_none());
    assert_eq!(mock.n(), 2);
}

#[test]
fn rate_limit_slots() {
    use store::{next_slot, MIN_GAP_S, PER_DAY};
    assert_eq!(next_slot(&[], T0, MIN_GAP_S), None);
    assert_eq!(next_slot(&[T0], T0 + 1, MIN_GAP_S), Some(T0 + MIN_GAP_S));
    assert_eq!(next_slot(&[T0], T0 + MIN_GAP_S, MIN_GAP_S), None);
    assert_eq!(next_slot(&[T0], T0 + 1, 0), None, "manual: no gap");
    // 20 a day, manual sends included.
    let day: Vec<u64> = (0..PER_DAY as u64).map(|i| T0 + i * 700).collect();
    let last = *day.last().unwrap();
    assert_eq!(next_slot(&day, last + 700, MIN_GAP_S), Some(T0 + 86_400));
    assert_eq!(next_slot(&day, last + 1, 0), Some(T0 + 86_400));
    assert_eq!(next_slot(&day, T0 + 86_400, MIN_GAP_S), None);
}

#[test]
fn the_daily_cap_applies_to_manual_sends() {
    let l = Log::new("daycap");
    l.clean_session(T0);
    let mock = Arc::new(Mock::default());
    let e = engine(resolve(None, &env_manual(), &bench()), Some(&l), &mock);
    e.set(Some(true), None, None, None).unwrap();
    for i in 0..store::PER_DAY as u64 {
        let p = e.preview(None, false, T0 + i).unwrap();
        block(e.send_preview(p["id"].as_str().unwrap(), T0 + i)).unwrap();
    }
    let p = e.preview(None, false, T0 + 100).unwrap();
    let err = block(e.send_preview(p["id"].as_str().unwrap(), T0 + 100)).unwrap_err();
    assert!(err.contains("daily limit"), "{err}");
    assert_eq!(mock.n(), store::PER_DAY);
}

#[test]
fn offline_queue_retries_at_the_next_slot_without_storms() {
    let l = Log::new("queue");
    l.clean_session(T0);
    l.put(T0 + 1, Level::Error, Src::Js, "first error");
    let mock = Arc::new(Mock::default());
    let e = engine(embedded(auto(true, true, None)), Some(&l), &mock);
    e.set(None, None, Some(true), None).unwrap();
    mock.then(Outcome::Failed(
        "cannot connect: network unreachable".into(),
    ));
    let h = block(e.tick(T0 + 60)).unwrap();
    assert!(
        h.status.starts_with("queued: cannot connect"),
        "{}",
        h.status
    );
    assert_eq!(e.status(T0)["queue"]["items"], 1);
    // No retry storm: the failed attempt used the slot.
    for t in (T0 + 61..T0 + 600).step_by(60) {
        assert!(block(e.tick(t)).is_none());
    }
    assert_eq!(mock.n(), 1);
    // Still offline at the next slot: kept, tried again later.
    mock.then(Outcome::Failed("timed out".into()));
    let h = block(e.tick(T0 + 661)).unwrap();
    assert!(h.status.contains("retry failed"), "{}", h.status);
    assert_eq!(e.status(T0)["queue"]["items"], 1);
    // Online again: delivered and removed.
    let h = block(e.tick(T0 + 1300)).unwrap();
    assert_eq!(h.status, "sent (200)");
    assert_eq!(e.status(T0)["queue"]["items"], 0);
    assert_eq!(mock.n(), 3);
    assert!(mock.last().contains("first error"));
    // A 4xx is not retried.
    l.put(T0 + 1400, Level::Error, Src::Js, "second error");
    mock.then(Outcome::Rejected(401));
    let h = block(e.tick(T0 + 2000)).unwrap();
    assert_eq!(h.status, "rejected (401)");
    assert_eq!(e.status(T0)["queue"]["items"], 0);
}

#[test]
fn the_offline_queue_is_capped_dropping_the_oldest() {
    let dir = tmp("qcap");
    let mut st = Store::open(Some(dir.clone()));
    let mib = vec![b'x'; 900 * 1024];
    for i in 0..4 {
        let dropped = st.enqueue(
            QueueEntry {
                id: format!("q-{i}"),
                trigger: "error".into(),
                created: ts_at(T0 + i),
                bytes: mib.len(),
                tries: 0,
            },
            &mib,
        );
        assert!(!dropped.contains(&format!("q-{i}")));
    }
    assert!(st.queue_bytes() <= store::QUEUE_CAP);
    let ids: Vec<&str> = st.queue().iter().map(|q| q.id.as_str()).collect();
    assert_eq!(ids, ["q-2", "q-3"], "the oldest were dropped");
    assert!(!dir.join("queue").join("q-0.json").exists());
    // Too big for the queue at all.
    let huge = vec![b'y'; store::QUEUE_CAP + 1];
    let d = st.enqueue(
        QueueEntry {
            id: "q-big".into(),
            trigger: "error".into(),
            created: ts_at(T0),
            bytes: huge.len(),
            tries: 0,
        },
        &huge,
    );
    assert_eq!(d, ["q-big"]);
    // History keeps the last 20 with their payloads.
    for i in 0..25u64 {
        st.add_history(
            HistoryEntry {
                id: format!("h-{i}"),
                ts: ts_at(T0 + i),
                trigger: "manual".into(),
                bytes: 2,
                status: "sent (200)".into(),
            },
            b"{}",
        );
    }
    assert_eq!(st.history().len(), store::HISTORY_LEN);
    assert_eq!(st.history()[0].id, "h-5");
    assert!(st.history_payload("h-4").is_none());
    assert_eq!(st.history_payload("h-24").unwrap(), b"{}");
    assert!(st.history_payload("../state").is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn periodic_summary_only_when_something_happened() {
    let l = Log::new("periodic");
    l.clean_session(T0);
    let mock = Arc::new(Mock::default());
    let e = engine(embedded(auto(false, false, Some(15))), Some(&l), &mock);
    e.set(None, None, Some(true), None).unwrap();
    assert!(block(e.tick(T0 + 10 * 60)).is_none(), "not due yet");
    assert!(
        block(e.tick(T0 + 15 * 60)).is_none(),
        "due, but nothing new"
    );
    e.panel_opened("maps");
    e.panel_opened("maps");
    e.panel_opened("../weird panel<script>");
    l.put(
        T0 + 20 * 60,
        Level::Warn,
        Src::Cmd,
        "obd_live failed: timeout",
    );
    assert!(
        block(e.tick(T0 + 20 * 60)).is_none(),
        "next one 15 min after the last check"
    );
    let h = block(e.tick(T0 + 31 * 60)).unwrap();
    assert_eq!(h.trigger, "periodic");
    let v: Value = serde_json::from_str(&mock.last()).unwrap();
    assert_eq!(v["summary"]["panels_opened"]["maps"], 2);
    assert_eq!(v["summary"]["session"]["warnings"], 1);
    assert_eq!(
        v["records"].as_array().unwrap().len(),
        0,
        "a summary carries no records"
    );
    assert_eq!(v["app"]["version"], "0.0.0-test");
    assert_eq!(v["app"]["webkit"], "2.46.0");
    let names: Vec<&String> = v["summary"]["panels_opened"]
        .as_object()
        .unwrap()
        .keys()
        .collect();
    assert!(
        names.iter().all(|n| !n.contains('<') && !n.contains(' ')),
        "{names:?}"
    );
}

// ---- payload ---------------------------------------------------------------------

#[test]
fn the_token_never_appears_anywhere() {
    let s = SecretString::new(TOKEN);
    assert_eq!(format!("{s}"), "<redacted>");
    assert!(!format!("{s:?}").contains(TOKEN));
    let u = upload(auto(true, true, None));
    assert!(!format!("{u:?}").contains(TOKEN));
    assert!(!format!("{:?}", env_manual()).contains(TOKEN));
    assert!(!format!("{:?}", resolve(Some(u), &EnvVars::default(), &bench())).contains(TOKEN));
    // A log record that repeats the token or the endpoint.
    let l = Log::new("token");
    l.clean_session(T0);
    l.put(
        T0 + 1,
        Level::Error,
        Src::Rust,
        &format!("upload to {FAKE} with {TOKEN} failed"),
    );
    let mock = Arc::new(Mock::default());
    let e = engine(embedded(auto(true, true, None)), Some(&l), &mock);
    e.set(Some(true), None, Some(true), None).unwrap();
    let p = e.preview(None, false, T0 + 2).unwrap();
    let text = p["text"].as_str().unwrap();
    assert!(!text.contains(TOKEN) && !text.contains(FAKE), "{text}");
    assert!(text.contains("<SECRET>"));
    // A transport error that repeats them.
    mock.then(Outcome::Failed(format!(
        "error sending request for url ({FAKE}): bad token {TOKEN}"
    )));
    let h = block(e.tick(T0 + 60)).unwrap();
    assert!(
        !h.status.contains(TOKEN) && !h.status.contains(FAKE),
        "{}",
        h.status
    );
    let status = e.status(T0 + 60).to_string();
    assert!(
        !status.contains(TOKEN) && !status.contains("/v1/upload"),
        "{status}"
    );
    let sent = mock.last();
    assert!(!sent.contains(TOKEN));
    // Nor on disk.
    let store_dir = l.dir.parent().unwrap().join("diagnostics");
    for f in walk(&store_dir) {
        let b = std::fs::read_to_string(&f).unwrap_or_default();
        assert!(!b.contains(TOKEN), "{} holds the token", f.display());
    }
}

fn walk(d: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

#[test]
fn payloads_hold_only_redacted_log_records() {
    let l = Log::new("only");
    l.clean_session(T0);
    let vin = ["ZZZ", "7AB", "2CD", "3E", "5", "91", "746"].concat();
    let hex: Vec<String> = vin.bytes().map(|b| format!("{b:02X}")).collect();
    l.put(
        T0 + 1,
        Level::Info,
        Src::Rust,
        &format!("[7E0] -> 22 F1 90  <- 62 F1 90 {}", hex.join(" ")),
    );
    l.put(
        T0 + 2,
        Level::Warn,
        Src::Cmd,
        "identify failed for 03L906018AJ at 192.168.4.20 on garagebox",
    );
    l.put(T0 + 3, Level::Info, Src::Rust, "-> 27 02 11 22 33 44");
    // Files next to the log (a dump, a datalog) are never read.
    std::fs::write(l.dir.join("stock.bin"), format!("IMAGE-CONTENT {vin}")).unwrap();
    std::fs::write(l.dir.join("drive.csv"), "DATALOG-CONTENT,1,2").unwrap();
    let mock = Arc::new(Mock::default());
    let e = engine(resolve(None, &env_manual(), &bench()), Some(&l), &mock);
    e.set(Some(true), None, None, None).unwrap();
    let text = e.preview(Some("my car"), false, T0 + 5).unwrap()["text"]
        .as_str()
        .unwrap()
        .to_string();
    for leak in [
        vin.as_str(),
        &hex.join(" "),
        "03L906018",
        "192.168.4.20",
        "garagebox",
        "11 22 33 44",
        "IMAGE-CONTENT",
        "DATALOG-CONTENT",
    ] {
        assert!(!text.contains(leak), "{leak} in {text}");
    }
    for marker in ["<VIN>", "<PART>", "<IP>", "<HOST>", "<masked: 4 bytes>"] {
        assert!(text.contains(marker), "{marker} missing");
    }
    let v: Value = serde_json::from_str(&text).unwrap();
    let mut keys: Vec<&String> = v.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "app",
            "created",
            "note",
            "omitted_records",
            "records",
            "schema",
            "summary",
            "trigger"
        ]
    );
    assert_eq!(v["schema"], payload::SCHEMA);
}

#[test]
fn payloads_are_capped() {
    let l = Log::new("cap");
    l.clean_session(T0);
    let big = "q".repeat(1900);
    for i in 0..700u64 {
        let mut r = Record::new(
            Level::Warn,
            Src::Js,
            format!("{i} {big}"),
            Some(json!({ "stack": "s".repeat(3900) })),
        );
        r.ts = ts_at(T0 + 1 + i);
        l.log.write(r).unwrap();
    }
    let mock = Arc::new(Mock::default());
    let e = engine(resolve(None, &env_manual(), &bench()), Some(&l), &mock);
    e.set(Some(true), None, None, None).unwrap();
    let p = e.preview(None, false, T0 + 2000).unwrap();
    let bytes = p["bytes"].as_u64().unwrap() as usize;
    assert!(bytes <= payload::MAX_PAYLOAD, "{bytes}");
    let v: Value = serde_json::from_str(p["text"].as_str().unwrap()).unwrap();
    assert!(v["omitted_records"].as_u64().unwrap() > 0);
    let recs = v["records"].as_array().unwrap();
    assert!(
        recs.last().unwrap()["msg"]
            .as_str()
            .unwrap()
            .starts_with("699 "),
        "the newest are kept"
    );
}

// ---- the real HTTPS client against a mock endpoint --------------------------------

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

/// A one-thread HTTPS server on 127.0.0.1 with the self-signed test
/// certificate. Each request it reads is passed on; it answers 200.
fn mock_endpoint(conns: usize) -> (u16, mpsc::Receiver<String>) {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    let cert = include_bytes!("testdata/mock-endpoint-cert.der");
    let key = include_bytes!("testdata/mock-endpoint-key.pk8.der");
    let cfg = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![CertificateDer::from(cert.to_vec())],
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.to_vec())),
    )
    .unwrap();
    let cfg = Arc::new(cfg);
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for s in l.incoming().take(conns) {
            let Ok(tcp) = s else { continue };
            let _ = tcp.set_read_timeout(Some(Duration::from_secs(10)));
            let conn = rustls::ServerConnection::new(cfg.clone()).unwrap();
            let mut tls = rustls::StreamOwned::new(conn, tcp);
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                match tls.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
                if let Some(h) = find(&buf, b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..h]).to_ascii_lowercase();
                    let len = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if buf.len() >= h + 4 + len {
                        break;
                    }
                }
            }
            if buf.is_empty() {
                continue; // the client refused the certificate
            }
            let _ =
                tls.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
            let _ = tls.flush();
            let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
        }
    });
    (port, rx)
}

#[test]
fn https_client_against_a_mock_endpoint() {
    let (port, rx) = mock_endpoint(2);
    let url = Url::parse(&format!("https://127.0.0.1:{port}/upload")).unwrap();
    let body = br#"{"schema":"opentune-diagnostics/1"}"#;
    // The self-signed certificate is accepted only with the developer flag.
    let dev =
        transport::HttpsSender::new(url.clone(), Some(TOKEN.into()), true, "0.0.0-test").unwrap();
    assert_eq!(block(dev.post(body)), Outcome::Sent(200));
    let req = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let lower = req.to_ascii_lowercase();
    assert!(lower.starts_with("post /upload http/1.1"), "{req}");
    assert!(lower.contains(&format!(
        "authorization: bearer {}",
        TOKEN.to_ascii_lowercase()
    )));
    assert!(lower.contains("content-type: application/json"));
    assert!(req.ends_with(std::str::from_utf8(body).unwrap()));
    let strict =
        transport::HttpsSender::new(url.clone(), Some(TOKEN.into()), false, "0.0.0-test").unwrap();
    match block(strict.post(body)) {
        Outcome::Failed(e) => assert!(!e.contains(TOKEN), "{e}"),
        o => panic!("an unverified certificate must be refused: {o:?}"),
    }
    assert!(
        rx.recv_timeout(Duration::from_millis(500)).is_err(),
        "nothing was delivered"
    );
    // Plain HTTP is refused before any connection.
    let http = Url::parse(&format!("http://127.0.0.1:{port}/upload")).unwrap();
    assert!(transport::HttpsSender::new(http, None, false, "0.0.0-test").is_err());
    // Over the cap: not sent.
    let huge = vec![b' '; payload::MAX_PAYLOAD + 1];
    assert_eq!(block(dev.post(&huge)), Outcome::Rejected(413));
}

#[test]
fn end_to_end_manual_send_over_https() {
    let (port, rx) = mock_endpoint(1);
    let l = Log::new("e2e");
    l.clean_session(T0);
    l.put(
        T0 + 1,
        Level::Error,
        Src::Js,
        "Uncaught TypeError at /home/tuner/x.js",
    );
    let env = EnvVars {
        endpoint: Some(format!("https://127.0.0.1:{port}/diag")),
        token: Some(TOKEN.into()),
        insecure: Some("1".into()),
        ..EnvVars::default()
    };
    let r = resolve(None, &env, &bench());
    assert!(r.insecure);
    let e = Engine::new(
        r,
        EngineParts {
            store_dir: Some(l.dir.parent().unwrap().join("diagnostics")),
            log_dir: Some(l.dir.clone()),
            ctx: RedactContext::default(),
            version: "0.0.0-test".into(),
            channel: "bench".into(),
            sender: None, // the real HTTPS client
            now: T0,
        },
    );
    e.set(Some(true), None, None, None).unwrap();
    let p = e.preview(Some("e2e"), false, T0 + 2).unwrap();
    let h = block(e.send_preview(p["id"].as_str().unwrap(), T0 + 3)).unwrap();
    assert_eq!(h.status, "sent (200)");
    let req = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(
        req.ends_with(p["text"].as_str().unwrap()),
        "the previewed bytes went out"
    );
    assert!(req.contains("<PATH>/x.js"));
}
