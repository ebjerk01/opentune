//! # Actuator tests (long-running adapter operation)
//!
//! Timed output tests over UDS InputOutputControlByIdentifier (0x2F), driven
//! by the actuator channels an adaptation definition **declares** — a thin
//! layer over `opentune_adaptation::actuator` (plan, run, report) on the
//! app-wide adapter hub.
//!
//! A test is one long-running hub operation ([`AdapterHub::start_op`]): it
//! holds the adapter from the session request to the return to the default
//! session, so no other operation talks to the bus meanwhile ("adapter
//! busy"). The engine enforces the duration itself, so the output is handed
//! back at the end even if the front end goes away; [`stop`] (the STOP
//! button, a panel switch, an adapter disconnect, the window closing) ends it
//! early through a stop flag. Every exchange lands in [`ActuatorState`] as it
//! happens, so a status read ([`status_of`]) never waits on the busy adapter.
//!
//! Safety gates (all checked by [`prepare`] before the adapter is touched):
//! * **Bench tier** — live tests run only on the alpha / bench channels;
//!   listing and planning ([`plan_ask`]) work everywhere (nothing is sent).
//! * **Per-run confirmation** — `ack` must be true for every start.
//! * **Declared channels only** — the channel, the option, the value range
//!   and the duration cap come from the definition. A **free-form** IO DID
//!   needs the Debug gate (alpha builds) *and* a second confirmation
//!   (`freeform_ack`), takes a raw state, is capped at
//!   [`FREEFORM_MAX_DURATION_MS`](opentune_adaptation::actuator::FREEFORM_MAX_DURATION_MS)
//!   and needs explicit Target ids.
//! * **Known target** — a test refuses an unknown module address.
//! * **SecurityAccess** — when the definition requires it, a seed/key id that
//!   resolves through [`SeedKeys`] (a plugin, or the user's armed SA2 script;
//!   OpenTune ships none).
//!
//! Moved verbatim from the desktop's `feat_actuator.rs` (which keeps the
//! `actuator_*` Tauri commands as glue), so another front end runs the
//! identical gates, run, stop and return-of-control path. The one seam is
//! [`ExchangeLog`]: where [`begin`] appends each exchange line once a test
//! has ended (the desktop's session log, `DebugLog`).

use crate::adapter::{self, AdapterHub, ConnKey, Session};
use crate::channel::BuildChannel;
use crate::seedkeys::SeedKeys;
use opentune_adaptation::actuator::{
    freeform_def, freeform_name, plan_test, run_test, Exchange, PlanOptions, RealClock, TestClock,
    TestLogin, TestOutcome, TestPlan, TestRequest, TestValue,
};
use opentune_adaptation::{AdaptationDef, IoControl};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Busy-slot name shown to other panels while a test runs.
pub const TEST_OP: &str = "actuator test";
/// How long `actuator_stop` waits for the test to hand control back.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Live {
    running: bool,
    started: Option<Instant>,
    exchanges: Vec<Exchange>,
    report: Option<Value>,
    error: Option<String>,
    stop_reason: Option<String>,
}

/// One test's shared state (the worker writes, commands read).
pub struct ActiveTest {
    plan: TestPlan,
    /// The module under test, as `Addr::json(false)`.
    pub target: Value,
    freeform: bool,
    stop: Arc<AtomicBool>,
    live: Mutex<Live>,
    done: (Mutex<bool>, Condvar),
}

impl ActiveTest {
    fn live(&self) -> std::sync::MutexGuard<'_, Live> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Ask the test to end now (it still hands control back).
    pub fn request_stop(&self, reason: &str) {
        {
            let mut l = self.live();
            if l.running && l.stop_reason.is_none() {
                l.stop_reason = Some(reason.to_string());
            }
        }
        self.stop.store(true, Ordering::SeqCst);
    }

    fn finish(&self, r: Result<Value, String>) {
        {
            let mut l = self.live();
            l.running = false;
            match r {
                Ok(v) => l.report = Some(v),
                Err(e) => l.error = Some(e),
            }
        }
        let (m, cv) = &self.done;
        *m.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cv.notify_all();
    }

    /// Wait until the test has finished, up to `timeout`.
    pub fn wait_done(&self, timeout: Duration) -> bool {
        let (m, cv) = &self.done;
        let g = m.lock().unwrap_or_else(|e| e.into_inner());
        let (g, _) = cv
            .wait_timeout_while(g, timeout, |done| !*done)
            .unwrap_or_else(|e| e.into_inner());
        *g
    }

    /// The `actuator_status` JSON.
    pub fn status(&self) -> Value {
        let l = self.live();
        let elapsed = l
            .started
            .map(|t| t.elapsed().as_millis() as u64)
            .unwrap_or(0);
        json!({
            "running": l.running,
            "channel": self.plan.channel,
            "did": self.plan.did,
            "option": self.plan.option,
            "duration_ms": self.plan.duration_ms,
            "elapsed_ms": elapsed,
            "remaining_ms": if l.running { self.plan.duration_ms.saturating_sub(elapsed) } else { 0 },
            "freeform": self.freeform,
            "target": self.target,
            "exchanges": l.exchanges,
            "report": l.report,
            "error": l.error,
            "stop_reason": l.stop_reason,
        })
    }
}

/// Actuator state (Tauri-managed in the desktop): the current (or last) test.
#[derive(Default)]
pub struct ActuatorState {
    cur: Mutex<Option<Arc<ActiveTest>>>,
}

impl ActuatorState {
    /// The current (or last) test.
    pub fn current(&self) -> Option<Arc<ActiveTest>> {
        self.cur.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// Feeds the engine's exchanges into the shared state as they happen.
struct LiveClock {
    inner: RealClock,
    test: Arc<ActiveTest>,
}

impl TestClock for LiveClock {
    fn now_ms(&mut self) -> u64 {
        self.inner.now_ms()
    }
    fn wait(&mut self, ms: u64) {
        self.inner.wait(ms)
    }
    fn stop_requested(&mut self) -> bool {
        self.inner.stop_requested()
    }
    fn on_exchange(&mut self, ex: &Exchange) {
        self.test.live().exchanges.push(ex.clone());
    }
}

// ---------------------------------------------------------------------------
// Request parsing (shared by plan and start)
// ---------------------------------------------------------------------------

/// What the UI asks for.
#[derive(Debug, Clone, Default)]
pub struct Ask {
    /// Definition file (ignored for a free-form test).
    pub def_path: Option<String>,
    /// Channel name (ignored for a free-form test).
    pub channel: Option<String>,
    /// Option token (`return`, `reset`, `freeze`, `adjust`, …).
    pub option: String,
    /// Engineering value.
    pub value: Option<f64>,
    /// Raw value (alternative to `value`).
    pub raw: Option<u64>,
    /// Hold time.
    pub duration_ms: u64,
    /// Free-form IO DID (hex), alpha Debug only.
    pub freeform_did: Option<String>,
    /// Free-form raw state (hex).
    pub state: Option<String>,
    /// Free-form: does the module need SecurityAccess first.
    pub freeform_security: bool,
}

/// A planned test plus what the start needs to know about its definition.
pub struct Planned {
    pub plan: TestPlan,
    pub def_text: Option<String>,
    pub freeform: bool,
}

fn hexs(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_did(s: &str) -> Result<u16, String> {
    let t = s.trim();
    let h = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    u16::from_str_radix(h, 16).map_err(|_| format!("free-form DID '{s}': expected 4 hex digits"))
}

// Moved verbatim from the desktop, which declares no MSRV; `is_multiple_of`
// (Rust 1.87) is newer than this crate's declared 1.75.
#[allow(clippy::incompatible_msrv)]
fn parse_hex(s: &str) -> Result<Vec<u8>, String> {
    let clean: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if clean.is_empty() || !clean.len().is_multiple_of(2) {
        return Err(format!("state '{s}': expected whole hex bytes"));
    }
    (0..clean.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&clean[i..i + 2], 16).map_err(|_| format!("invalid hex in '{s}'"))
        })
        .collect()
}

/// Read, parse and validate an adaptation definition; also returns its text.
pub fn load_def(path: &str) -> Result<(AdaptationDef, String), String> {
    let s = std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
    let def = AdaptationDef::from_json(&s).map_err(|e| format!("parse adaptation def: {e}"))?;
    def.validate().map_err(|e| e.to_string())?;
    Ok((def, s))
}

/// The Debug gate: a verbatim copy of the desktop's `debug::alpha_only` (which
/// stays in the desktop for its Debug commands), so the gate is the same
/// `debug_enabled()` check. [`plan_ask`] replaces the error text anyway.
fn alpha_only(channel: &BuildChannel) -> Result<(), String> {
    if channel.debug_enabled() {
        Ok(())
    } else {
        Err("debug commands are available only in alpha builds".into())
    }
}

/// Validate and plan. A free-form DID needs the Debug gate (alpha builds).
pub fn plan_ask(ask: &Ask, build: &BuildChannel) -> Result<Planned, String> {
    let option = IoControl::parse(&ask.option).map_err(|e| e.to_string())?;
    let value = match (ask.value, ask.raw) {
        (Some(_), Some(_)) => return Err("give a value or a raw value, not both".into()),
        (Some(v), None) => Some(TestValue::Engineering(v)),
        (None, Some(r)) => Some(TestValue::Raw(r)),
        (None, None) => None,
    };
    let freeform = ask
        .freeform_did
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let (def, def_text, name, state, opts) = match freeform {
        Some(did) => {
            alpha_only(build).map_err(|_| {
                "a free-form IO DID is available only through the Debug gate of alpha builds; \
                 use a definition that declares the channel"
                    .to_string()
            })?;
            let did = parse_did(did)?;
            let state = match ask
                .state
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                Some(s) => Some(parse_hex(s)?),
                None => None,
            };
            (
                freeform_def(did, ask.freeform_security, None),
                None,
                freeform_name(did),
                state,
                PlanOptions {
                    allow_raw_state: true,
                },
            )
        }
        None => {
            let path = ask
                .def_path
                .as_deref()
                .filter(|p| !p.trim().is_empty())
                .ok_or("choose an adaptation definition first")?;
            let (def, text) = load_def(path)?;
            let name = ask
                .channel
                .clone()
                .filter(|c| !c.is_empty())
                .ok_or("choose an actuator channel")?;
            if ask.state.as_deref().is_some_and(|s| !s.trim().is_empty()) {
                return Err(
                    "a raw control state is accepted only for a free-form test (alpha Debug); \
                     a declared channel uses its declared state and value range"
                        .into(),
                );
            }
            (def, Some(text), name, None, PlanOptions::default())
        }
    };
    let plan = plan_test(
        &def,
        &TestRequest {
            channel: name,
            option,
            value,
            state,
            duration_ms: ask.duration_ms,
        },
        opts,
    )
    .map_err(|e| e.to_string())?;
    Ok(Planned {
        plan,
        def_text,
        freeform: freeform.is_some(),
    })
}

// ---------------------------------------------------------------------------
// Start
// ---------------------------------------------------------------------------

/// Everything a start needs once the UI's arguments are parsed.
pub struct Start {
    pub planned: Planned,
    pub key: ConnKey,
    pub timeout_ms: u64,
    pub seedkey: Option<String>,
}

/// Check the gates, resolve the target and the seed/key handler, and build
/// the start. Nothing touches the adapter.
#[allow(clippy::too_many_arguments)]
pub fn prepare(
    ask: &Ask,
    build: &BuildChannel,
    keys: &SeedKeys,
    conn: (&str, Option<&str>, Option<&str>, Option<&str>),
    tx_id: Option<&str>,
    rx_id: Option<&str>,
    timeout_ms: u64,
    ack: bool,
    freeform_ack: bool,
    seedkey: Option<&str>,
) -> Result<Start, String> {
    if !build.debug_enabled() {
        return Err(
            "live actuator tests are a Bench-tier feature (alpha / bench builds) until they are \
             vehicle-tested; Plan works in every build and sends nothing"
                .into(),
        );
    }
    if !ack {
        return Err(
            "actuator test refused: confirm this run — it drives a real output on the module (ack required)"
                .into(),
        );
    }
    let planned = plan_ask(ask, build)?;
    if planned.freeform && !freeform_ack {
        return Err(
            "free-form IO control refused: no definition declares this DID, so nothing limits \
             the state you send — tick the second (free-form) confirmation"
                .into(),
        );
    }
    let addr = adapter::write_addr(
        adapter::resolve(tx_id, rx_id, planned.def_text.as_deref())?,
        "actuator test",
    )?;
    let seedkey = seedkey
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if planned.plan.requires_security {
        let id = seedkey.as_deref().ok_or_else(|| {
            format!(
                "this module needs SecurityAccess (level 0x{:02X}) before IO control: enter a \
                 seed/key id (from a plugin, or 'sa2' with your armed SA2 script). OpenTune ships \
                 no seed/key algorithm.",
                planned.plan.security_level
            )
        })?;
        keys.require(id)?;
    }
    let (driver, port, baud, bus) = conn;
    Ok(Start {
        planned,
        key: ConnKey::new(driver, port, baud, bus, addr),
        timeout_ms,
        seedkey,
    })
}

/// Where [`begin`] appends one line per exchange once a test has ended, on
/// the adapter worker, before the test is marked finished (the desktop's
/// session log `DebugLog` implements it).
pub trait ExchangeLog: Clone + Send + 'static {
    /// Append one line.
    fn push(&self, line: String);
}

/// Queue a prepared test on the hub. Returns as soon as it is queued.
pub fn begin<L: ExchangeLog>(
    hub: &AdapterHub,
    state: &ActuatorState,
    keys: &SeedKeys,
    log: &L,
    start: Start,
) -> Result<Arc<ActiveTest>, String> {
    let mut slot = state.cur.lock().unwrap_or_else(|e| e.into_inner());
    if slot.as_ref().is_some_and(|t| t.live().running) {
        return Err("an actuator test is already running — stop it first".into());
    }
    let Start {
        planned,
        key,
        timeout_ms,
        seedkey,
    } = start;
    let addr = key.addr;
    let test = Arc::new(ActiveTest {
        plan: planned.plan,
        target: addr.json(false),
        freeform: planned.freeform,
        stop: Arc::new(AtomicBool::new(false)),
        live: Mutex::new(Live {
            running: true,
            started: Some(Instant::now()),
            ..Live::default()
        }),
        done: (Mutex::new(false), Condvar::new()),
    });
    let job = test.clone();
    let keys = keys.clone();
    let log = log.clone();
    hub.start_op(TEST_OP, move |s: &mut Session| {
        let r = run_on(s, &key, timeout_ms, &job, &keys, seedkey.as_deref());
        for ex in job.live().exchanges.iter() {
            log.push(format!(
                "[actuator {:03X}] -> {}  <- {}",
                addr.tx_id,
                hexs(&ex.request),
                ex.response
                    .as_deref()
                    .map(hexs)
                    .or_else(|| ex.error.clone())
                    .unwrap_or_default()
            ));
        }
        job.finish(r);
    })?;
    *slot = Some(test.clone());
    Ok(test)
}

/// The test itself, on the worker's session.
fn run_on(
    s: &mut Session,
    key: &ConnKey,
    timeout_ms: u64,
    test: &Arc<ActiveTest>,
    keys: &SeedKeys,
    seedkey: Option<&str>,
) -> Result<Value, String> {
    let r = s.with_diag(key, timeout_ms, |d| {
        let handler = match seedkey {
            Some(id) if test.plan.requires_security => Some(keys.require(id)?),
            _ => None,
        };
        let login = TestLogin {
            confirmed: true,
            seed_key: handler.as_deref(),
        };
        let mut clock = LiveClock {
            inner: RealClock::new(test.stop.clone()),
            test: test.clone(),
        };
        run_test(d, &test.plan, &login, &mut clock).map_err(|e| e.to_string())
    });
    match r {
        Ok(rep) => {
            // A failed exchange may leave the adapter in a bad state: re-open
            // it for the next operation.
            if !matches!(rep.outcome, TestOutcome::Completed | TestOutcome::Stopped) {
                s.invalidate();
            }
            serde_json::to_value(&rep).map_err(|e| e.to_string())
        }
        Err(e) => Err(e),
    }
}

/// The current (or last) test's status; an idle shape before the first.
pub fn status_of(tests: &ActuatorState) -> Value {
    match tests.current() {
        Some(t) => t.status(),
        None => json!({ "running": false, "exchanges": [], "report": null }),
    }
}

/// End the running test now: control is handed back to the ECU, then the
/// final status is returned. Safe to call when nothing runs.
pub fn stop(tests: &ActuatorState, reason: &str) -> Result<Value, String> {
    let Some(t) = tests.current() else {
        return Ok(status_of(tests));
    };
    t.request_stop(reason);
    if !t.wait_done(STOP_TIMEOUT) {
        return Err(format!(
            "the actuator test did not finish within {} s of the stop request — the adapter may \
             be hung; check the module and cycle the ignition before driving",
            STOP_TIMEOUT.as_secs()
        ));
    }
    Ok(t.status())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{testkit::SimBus, Addr};
    use opentune_adaptation::actuator::FREEFORM_MAX_DURATION_MS;
    use opentune_udssim::server::UdsServer;

    /// Stands in for the desktop's `DebugLog` (the same shape: a shared list
    /// of lines).
    #[derive(Clone, Default)]
    struct DebugLog(Arc<Mutex<Vec<String>>>);

    impl ExchangeLog for DebugLog {
        fn push(&self, line: String) {
            self.0.lock().unwrap().push(line);
        }
    }

    fn pack(rel: &str) -> String {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packs/vcar-demo")
            .join(rel)
            .to_string_lossy()
            .into_owned()
    }

    fn bench() -> BuildChannel {
        BuildChannel::new(BuildChannel::BENCH)
    }

    fn ask(def: &str, ch: &str, opt: &str, value: Option<f64>, ms: u64) -> Ask {
        Ask {
            def_path: Some(pack(def)),
            channel: Some(ch.into()),
            option: opt.into(),
            value,
            duration_ms: ms,
            ..Ask::default()
        }
    }

    fn prep(
        a: &Ask,
        build: &BuildChannel,
        ack: bool,
        ff_ack: bool,
        sk: Option<&str>,
    ) -> Result<Start, String> {
        prepare(
            a,
            build,
            &SeedKeys::new(vec![crate::seedkeys::testkeys::vcar]),
            ("vcar", Some("demo@actuator-unit"), None, Some("hs")),
            None,
            None,
            200,
            ack,
            ff_ack,
            sk,
        )
    }

    #[test]
    fn gates_stable_ack_range_target_security_and_freeform() {
        let fan = ask(
            "adaptation/engine.json",
            "Radiator fan",
            "adjust",
            Some(40.0),
            3000,
        );
        // Stable channel: refused, but planning still works.
        let stable = BuildChannel::new(BuildChannel::STABLE);
        let e = prep(&fan, &stable, true, false, None).err().unwrap();
        assert!(e.contains("Bench-tier"), "{e}");
        assert!(plan_ask(&fan, &stable).is_ok());
        // No per-run confirmation.
        assert!(prep(&fan, &bench(), false, false, None)
            .err()
            .unwrap()
            .contains("ack required"));
        // Out of the declared range / above the duration cap / undeclared option.
        let mut a = fan.clone();
        a.value = Some(150.0);
        assert!(prep(&a, &bench(), true, false, None)
            .err()
            .unwrap()
            .contains("declared range"));
        a = fan.clone();
        a.duration_ms = 10_001;
        assert!(prep(&a, &bench(), true, false, None)
            .err()
            .unwrap()
            .contains("limit"));
        let lamp = ask(
            "adaptation/airbag.json",
            "Airbag warning lamp",
            "freeze",
            None,
            1000,
        );
        assert!(prep(&lamp, &bench(), true, false, Some("vcar-test"))
            .err()
            .unwrap()
            .contains("not declared"));
        // A raw state on a declared channel is refused.
        a = fan.clone();
        a.state = Some("FF".into());
        assert!(prep(&a, &bench(), true, false, None)
            .err()
            .unwrap()
            .contains("free-form"));
        // The airbag module needs SecurityAccess: no id / unknown id refused.
        let lamp = ask(
            "adaptation/airbag.json",
            "Airbag warning lamp",
            "adjust",
            None,
            1000,
        );
        assert!(prep(&lamp, &bench(), true, false, None)
            .err()
            .unwrap()
            .contains("SecurityAccess"));
        assert!(prep(&lamp, &bench(), true, false, Some("oem-x")).is_err());
        assert!(prep(&lamp, &bench(), true, false, Some("vcar-test")).is_ok());
        // Free-form: alpha Debug gate, second confirmation, explicit target.
        let ff = Ask {
            option: "adjust".into(),
            duration_ms: 1000,
            freeform_did: Some("0500".into()),
            state: Some("01".into()),
            ..Ask::default()
        };
        assert!(prep(&ff, &stable, true, true, None)
            .err()
            .unwrap()
            .contains("Bench-tier"));
        assert!(plan_ask(&ff, &stable).err().unwrap().contains("Debug gate"));
        assert!(prep(&ff, &bench(), true, false, None)
            .err()
            .unwrap()
            .contains("second"));
        assert!(prep(&ff, &bench(), true, true, None)
            .err()
            .unwrap()
            .contains("CAN address is unknown"));
        let mut long = ff.clone();
        long.duration_ms = FREEFORM_MAX_DURATION_MS + 1;
        assert!(plan_ask(&long, &bench()).is_err());
        // Fine: a declared value in range.
        let s = prep(&fan, &bench(), true, false, None).unwrap();
        assert_eq!(s.planned.plan.record, vec![40]);
        assert_eq!(s.key.addr, Addr::ENGINE);
    }

    #[test]
    fn a_test_on_the_target_returns_control_and_leaves_the_engine_alone() {
        let bus = SimBus::default();
        let mut eng = UdsServer::new();
        eng.set_io(0x0500, vec![0x00]);
        bus.add(0x7E0, 0x7E8, eng);
        let mut s = bus.session();
        let a = ask(
            "adaptation/engine.json",
            "Radiator fan",
            "adjust",
            Some(55.0),
            300,
        );
        let st = prep(&a, &bench(), true, false, None).unwrap();
        let test = Arc::new(ActiveTest {
            plan: st.planned.plan,
            target: json!(null),
            freeform: false,
            stop: Arc::new(AtomicBool::new(false)),
            live: Mutex::new(Live {
                running: true,
                ..Live::default()
            }),
            done: (Mutex::new(false), Condvar::new()),
        });
        let key = ConnKey::new("sim", None, None, None, Addr::ENGINE);
        let v = run_on(&mut s, &key, 100, &test, &SeedKeys::default(), None).unwrap();
        assert_eq!(v["outcome"], "completed", "{v}");
        assert_eq!(v["control_returned"], true);
        let sent = bus.sent_to(0x7E0);
        assert_eq!(sent[1], vec![0x2F, 0x05, 0x00, 0x03, 55]);
        assert!(sent.contains(&vec![0x2F, 0x05, 0x00, 0x00]));
        assert!(!bus.ecus.borrow()[&(0x7E0, 0x7E8)].io_overridden(0x0500));
        assert_eq!(
            test.live().exchanges.len(),
            sent.len(),
            "live log saw every exchange"
        );
    }

    /// The real hub with the `vcar` driver from the registry: a timed test to
    /// the end, a STOP mid-test, and a secured module unlocked with the
    /// virtual car's test-only key.
    #[test]
    fn hub_runs_timed_tests_on_the_virtual_car() {
        let hub = AdapterHub::with_registry();
        let state = ActuatorState::default();
        let log = DebugLog::default();
        let keys = SeedKeys::new(vec![crate::seedkeys::testkeys::vcar]);
        let reqs = |v: &Value| -> Vec<String> {
            v["exchanges"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["request"].as_str().unwrap().to_string())
                .collect()
        };

        // 1. Runs to the end; meanwhile the adapter is busy for everyone else.
        // The hold is long enough that the busy checks below run while the
        // test is still going, even on a heavily loaded build machine (400 ms
        // flaked under a parallel gate run).
        let a = ask(
            "adaptation/engine.json",
            "Radiator fan",
            "adjust",
            Some(30.0),
            3000,
        );
        let t = begin(
            &hub,
            &state,
            &keys,
            &log,
            prep(&a, &bench(), true, false, None).unwrap(),
        )
        .unwrap();
        let again = prep(&a, &bench(), true, false, None).unwrap();
        assert!(begin(&hub, &state, &keys, &log, again)
            .err()
            .unwrap()
            .contains("already running"));
        let busy = hub.run("obd live", |_s| Ok(())).unwrap_err();
        assert!(
            busy.contains("adapter busy") && busy.contains(TEST_OP),
            "{busy}"
        );
        assert!(t.wait_done(Duration::from_secs(20)));
        let v = t.status();
        assert_eq!(v["report"]["outcome"], "completed", "{v}");
        assert_eq!(v["report"]["control_returned"], true);
        let r = reqs(&v);
        assert_eq!(r[0], "10 03");
        assert!(r.contains(&"2F 05 00 03 1E".to_string()), "{r:?}");
        assert_eq!(r[r.len() - 2], "2F 05 00 00");
        assert_eq!(r[r.len() - 1], "10 01");
        assert!(hub.run("after", |_s| Ok(1)).is_ok(), "slot freed");

        // 2. STOP ends a long test early and still returns control.
        let a = ask(
            "adaptation/engine.json",
            "Radiator fan",
            "adjust",
            Some(80.0),
            10_000,
        );
        let t0 = Instant::now();
        begin(
            &hub,
            &state,
            &keys,
            &log,
            prep(&a, &bench(), true, false, None).unwrap(),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let v = stop(&state, "STOP pressed").unwrap();
        assert!(t0.elapsed() < Duration::from_secs(5));
        assert_eq!(v["report"]["outcome"], "stopped", "{v}");
        assert_eq!(v["report"]["control_returned"], true);
        assert_eq!(v["stop_reason"], "STOP pressed");
        assert_eq!(v["running"], false);

        // 3. A secured module: SecurityAccess first, with the test-only key.
        let a = ask(
            "adaptation/airbag.json",
            "Airbag warning lamp",
            "adjust",
            None,
            200,
        );
        let t = begin(
            &hub,
            &state,
            &keys,
            &log,
            prep(&a, &bench(), true, false, Some("vcar-test")).unwrap(),
        )
        .unwrap();
        assert!(t.wait_done(Duration::from_secs(20)));
        let v = t.status();
        assert_eq!(v["report"]["outcome"], "completed", "{v}");
        assert_eq!(v["report"]["security_unlocked"], true);
        assert_eq!(reqs(&v)[1], "27 01");
        assert!(log
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.contains("[actuator 715]")));
    }

    #[test]
    fn stop_without_a_test_is_harmless() {
        let st = ActuatorState::default();
        assert_eq!(stop(&st, "panel closed").unwrap()["running"], false);
    }
}
