//! Timed actuator tests (`opentune_adaptation::actuator`) against the
//! `opentune-udssim` UDS server: session and SecurityAccess, the hold with
//! tester present, and returnControlToECU at the end, on a stop, after a
//! refused control request, after a link failure during the hold, and the loud
//! report when control cannot be returned. Synthetic DIDs only.

use opentune_adaptation::actuator::{
    plan_test, run_test, Phase, PlanOptions, TestClock, TestLogin, TestOutcome, TestPlan,
    TestRequest, TestValue,
};
use opentune_adaptation::{AdaptationDef, AdaptationError, IoControl, RawDiag};
use opentune_udssim::driver::TestSeedKey;
use opentune_udssim::server::UdsServer;

const FAN: u16 = 0x3010;

fn def(secured: bool) -> AdaptationDef {
    AdaptationDef::from_json(&format!(
        r#"{{"id":"t","name":"T","requires_security":{secured},"channels":[
          {{"name":"Fan","kind":"actuator","did":{FAN},"state":[0],
           "options":["short_term_adjustment","reset_to_default","freeze_current_state"],
           "value":{{"offset":0,"width":1,"scaling":{{"factor":1,"unit":"%"}}}},
           "min":0,"max":100,"max_duration_ms":10000}},
          {{"name":"Ghost","kind":"actuator","did":2457,"state":[1]}}
        ]}}"#
    ))
    .unwrap()
}

fn plan(d: &AdaptationDef, ch: &str, option: IoControl, value: Option<u64>, ms: u64) -> TestPlan {
    plan_test(
        d,
        &TestRequest {
            channel: ch.into(),
            option,
            value: value.map(TestValue::Raw),
            state: None,
            duration_ms: ms,
        },
        PlanOptions::default(),
    )
    .unwrap()
}

/// A module: the udssim server (0x2F allowed in the extended session only),
/// an optional fault hook, and the IO state seen at each tester present.
struct Ecu {
    server: UdsServer,
    fault: Box<dyn FnMut(&[u8]) -> Option<Result<Vec<u8>, String>>>,
    held: Vec<bool>,
    sent: Vec<Vec<u8>>,
}

impl Ecu {
    fn new() -> Self {
        let mut server = UdsServer::new();
        server.set_io(FAN, vec![0x00]);
        server.restrict_service_to_sessions(0x2F, &[0x03]);
        Ecu {
            server,
            fault: Box::new(|_| None),
            held: Vec::new(),
            sent: Vec::new(),
        }
    }
    fn with_fault(mut self, f: impl FnMut(&[u8]) -> Option<Result<Vec<u8>, String>> + 'static) -> Self {
        self.fault = Box::new(f);
        self
    }
    fn count(&self, prefix: &[u8]) -> usize {
        self.sent.iter().filter(|r| r.starts_with(prefix)).count()
    }
}

impl RawDiag for Ecu {
    fn request(&mut self, req: &[u8]) -> Result<Vec<u8>, String> {
        self.sent.push(req.to_vec());
        if req[0] == 0x3E {
            self.held.push(self.server.io_overridden(FAN));
        }
        if let Some(r) = (self.fault)(req) {
            return r;
        }
        Ok(self.server.handle(req))
    }
}

/// A simulated clock: `wait` advances time; a stop can be scheduled.
#[derive(Default)]
struct Clock {
    now: u64,
    stop_at: Option<u64>,
    live: usize,
}

impl TestClock for Clock {
    fn now_ms(&mut self) -> u64 {
        self.now
    }
    fn wait(&mut self, ms: u64) {
        // Like the real clock: a scheduled stop cuts the wait short.
        self.now = match self.stop_at {
            Some(s) if self.now < s => (self.now + ms).min(s),
            _ => self.now + ms,
        };
    }
    fn stop_requested(&mut self) -> bool {
        self.stop_at.is_some_and(|s| self.now >= s)
    }
    fn on_exchange(&mut self, _ex: &opentune_adaptation::actuator::Exchange) {
        self.live += 1;
    }
}

fn go() -> TestLogin<'static> {
    TestLogin {
        confirmed: true,
        seed_key: None,
    }
}

#[test]
fn adjustment_is_held_with_tester_present_then_returned() {
    let d = def(false);
    let p = plan(&d, "Fan", IoControl::ShortTermAdjustment, Some(60), 5000);
    let mut ecu = Ecu::new();
    let mut clock = Clock::default();
    let rep = run_test(&mut ecu, &p, &go(), &mut clock).unwrap();
    assert_eq!(rep.outcome, TestOutcome::Completed, "{rep:?}");
    assert!(rep.applied && rep.control_returned && rep.safe && rep.default_session);
    assert_eq!(rep.held_ms, 5000);
    assert_eq!(rep.status, None);
    assert_eq!(rep.response, vec![60], "the module echoes the driven state");
    // Tester present at 2 s and 4 s, the output overridden each time.
    assert_eq!(ecu.held, vec![true, true]);
    assert_eq!(ecu.sent[0], vec![0x10, 0x03]);
    assert_eq!(ecu.sent[1], vec![0x2F, 0x30, 0x10, 0x03, 60]);
    assert_eq!(ecu.sent[ecu.sent.len() - 2], vec![0x2F, 0x30, 0x10, 0x00]);
    assert_eq!(ecu.sent.last().unwrap(), &vec![0x10, 0x01]);
    assert!(!ecu.server.io_overridden(FAN));
    assert_eq!(ecu.server.io_state(FAN), Some(&[0x00][..]));
    assert_eq!(clock.live, rep.exchanges.len(), "every exchange is fed live");
    assert!(rep.exchanges.iter().all(|e| e.error.is_none()));
}

#[test]
fn reset_and_freeze_hold_then_return() {
    let d = def(false);
    for opt in [IoControl::ResetToDefault, IoControl::FreezeCurrentState] {
        let p = plan(&d, "Fan", opt, None, 1000);
        let mut ecu = Ecu::new();
        let rep = run_test(&mut ecu, &p, &go(), &mut Clock::default()).unwrap();
        assert_eq!(rep.outcome, TestOutcome::Completed);
        assert_eq!(ecu.sent[1], vec![0x2F, 0x30, 0x10, opt.byte()]);
        assert!(!ecu.server.io_overridden(FAN));
    }
}

#[test]
fn stop_ends_the_hold_early_and_returns_control() {
    let d = def(false);
    let p = plan(&d, "Fan", IoControl::ShortTermAdjustment, Some(10), 10_000);
    let mut ecu = Ecu::new();
    let mut clock = Clock {
        stop_at: Some(3000),
        ..Clock::default()
    };
    let rep = run_test(&mut ecu, &p, &go(), &mut clock).unwrap();
    assert_eq!(rep.outcome, TestOutcome::Stopped);
    assert!(rep.stopped && rep.control_returned && rep.safe);
    assert_eq!(rep.held_ms, 3000);
    assert!(!ecu.server.io_overridden(FAN));
}

#[test]
fn refused_control_request_still_returns_control() {
    // "Ghost" names a DID the module does not have: requestOutOfRange.
    let d = def(false);
    let p = plan(&d, "Ghost", IoControl::ShortTermAdjustment, None, 2000);
    let mut ecu = Ecu::new();
    let rep = run_test(&mut ecu, &p, &go(), &mut Clock::default()).unwrap();
    let TestOutcome::ApplyFailed { message } = &rep.outcome else {
        panic!("{rep:?}")
    };
    assert!(message.contains("requestOutOfRange"), "{message}");
    assert!(!rep.applied && rep.safe, "a clean refusal applied nothing");
    assert_eq!(rep.held_ms, 0);
    assert_eq!(ecu.count(&[0x3E]), 0, "no hold after a refusal");
    assert_eq!(ecu.count(&[0x2F, 0x09, 0x99, 0x00]), 2, "return still sent (and retried)");
}

#[test]
fn a_refused_session_drives_nothing() {
    // The session is refused: nothing is driven, nothing needs returning.
    let d = def(false);
    let p = plan(&d, "Fan", IoControl::ShortTermAdjustment, Some(1), 1000);
    let mut ecu = Ecu::new().with_fault(|r| (r == [0x10, 0x03]).then(|| Ok(vec![0x7F, 0x10, 0x22])));
    let rep = run_test(&mut ecu, &p, &go(), &mut Clock::default()).unwrap();
    assert!(matches!(rep.outcome, TestOutcome::Aborted { phase: Phase::Session, .. }), "{rep:?}");
    assert!(rep.safe && !rep.applied);
    assert_eq!(ecu.count(&[0x2F]), 0);
}

#[test]
fn link_failure_during_the_hold_returns_control() {
    // Tester present times out once; the module still takes the return.
    let d = def(false);
    let p = plan(&d, "Fan", IoControl::ShortTermAdjustment, Some(50), 6000);
    let mut ecu =
        Ecu::new().with_fault(|r| (r[0] == 0x3E).then(|| Err("timeout after 1000 ms".to_string())));
    let rep = run_test(&mut ecu, &p, &go(), &mut Clock::default()).unwrap();
    let TestOutcome::HoldFailed { message } = &rep.outcome else {
        panic!("{rep:?}")
    };
    assert!(message.contains("timeout"), "{message}");
    assert!(rep.control_returned && rep.safe);
    assert_eq!(rep.held_ms, 2000);
    assert!(!ecu.server.io_overridden(FAN));
}

#[test]
fn a_lost_link_reports_control_not_returned_loudly() {
    // Everything after the control request times out: the output may still be
    // forced, the return is retried once, and the report says so.
    let d = def(false);
    let p = plan(&d, "Fan", IoControl::ShortTermAdjustment, Some(50), 4000);
    let mut seen_control = false;
    let mut ecu = Ecu::new().with_fault(move |r| {
        if seen_control {
            return Some(Err("timeout after 1000 ms".into()));
        }
        seen_control = r[0] == 0x2F;
        None
    });
    let rep = run_test(&mut ecu, &p, &go(), &mut Clock::default()).unwrap();
    assert!(matches!(rep.outcome, TestOutcome::ControlReturnFailed { .. }), "{rep:?}");
    assert!(rep.applied && !rep.control_returned && !rep.safe);
    assert_eq!(rep.return_attempts, 2);
    assert!(!rep.default_session);
    assert!(ecu.server.io_overridden(FAN), "the module really is still forced");
}

#[test]
fn security_runs_first_when_the_definition_requires_it() {
    let d = def(true);
    let p = plan(&d, "Fan", IoControl::ShortTermAdjustment, Some(20), 1000);
    // No handler: refused before any I/O.
    let mut ecu = Ecu::new();
    let e = run_test(&mut ecu, &p, &go(), &mut Clock::default()).unwrap_err();
    assert!(matches!(e, AdaptationError::NotConfirmed(_)), "{e}");
    assert!(ecu.sent.is_empty());
    // Not confirmed: refused before any I/O.
    let e = run_test(&mut ecu, &p, &TestLogin::default(), &mut Clock::default()).unwrap_err();
    assert!(e.to_string().contains("confirm"), "{e}");
    assert!(ecu.sent.is_empty());
    // With the simulator's test-only handler: unlocked, then driven.
    let sk = TestSeedKey;
    let login = TestLogin {
        confirmed: true,
        seed_key: Some(&sk),
    };
    let rep = run_test(&mut ecu, &p, &login, &mut Clock::default()).unwrap();
    assert_eq!(rep.outcome, TestOutcome::Completed, "{rep:?}");
    assert!(rep.security_unlocked);
    assert_eq!(&ecu.sent[1], &[0x27, 0x01]);
    assert_eq!(ecu.sent[2][..2], [0x27, 0x02]);
    let key_ex = &rep.exchanges[2];
    assert_eq!(key_ex.phase, Phase::Security);
    assert_eq!(key_ex.response.as_deref(), Some(&[0x67, 0x02][..]));
    // Back in the default session the module locks again.
    assert!(!ecu.server.is_unlocked());
}

#[test]
fn a_wrong_key_aborts_before_driving_anything() {
    struct Wrong;
    impl opentune_core::protocol::seedkey::SeedKey for Wrong {
        fn id(&self) -> &str {
            "wrong"
        }
        fn compute(&self, seed: &[u8], _l: u8) -> opentune_core::Result<Vec<u8>> {
            Ok(seed.to_vec())
        }
    }
    let d = def(true);
    let p = plan(&d, "Fan", IoControl::ShortTermAdjustment, Some(20), 1000);
    let mut ecu = Ecu::new();
    let rep = run_test(
        &mut ecu,
        &p,
        &TestLogin {
            confirmed: true,
            seed_key: Some(&Wrong),
        },
        &mut Clock::default(),
    )
    .unwrap();
    assert!(matches!(rep.outcome, TestOutcome::Aborted { phase: Phase::Security, .. }), "{rep:?}");
    assert_eq!(ecu.count(&[0x2F]), 0);
}

#[test]
fn return_control_alone_is_a_one_shot() {
    let d = def(false);
    let p = plan(&d, "Fan", IoControl::ReturnControlToEcu, None, 0);
    let mut ecu = Ecu::new();
    ecu.server.handle(&[0x10, 0x03]);
    ecu.server.handle(&[0x2F, 0x30, 0x10, 0x03, 0x42]);
    assert!(ecu.server.io_overridden(FAN));
    let rep = run_test(&mut ecu, &p, &go(), &mut Clock::default()).unwrap();
    assert_eq!(rep.outcome, TestOutcome::Completed);
    assert!(rep.control_returned);
    assert_eq!(ecu.count(&[0x2F]), 1);
    assert!(!ecu.server.io_overridden(FAN));
}
