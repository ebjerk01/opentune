//! The KWP2000 adaptation path against the synthetic module: over the core
//! TP2.0 stack (`Tp20Channel` on the core `Tp20Ecu` simulator, whose module
//! session is reset on every channel setup) and directly, as on a K-line.
//! All values are synthetic.
use opentune_adaptation::kwp::{
    self, AdaptMode, BasicSettingsOutcome, KwpLogin, KwpWriteOptions, KwpWriteOutcome,
};
use opentune_adaptation::sim::{lock, open_sim_channel, sim_bus, SimKwpModule, SimSeedKey};
use opentune_adaptation::{AdaptationDef, AdaptationError, ChannelKind, RawDiag};
use opentune_core::protocol::seedkey::SeedKey;
use opentune_core::protocol::tp20channel::Tp20Channel;
use opentune_core::protocol::tp20sim::{Tp20Ecu, Tp20EcuConfig};
use opentune_core::CoreDiag;
use std::sync::{Arc, Mutex};

const WSC: [u8; 6] = [0x00, 0x00, 0x01, 0x23, 0x45, 0x67];

fn def() -> AdaptationDef {
    let text = include_str!("fixtures/kwp-sim.json");
    let d = AdaptationDef::from_json(text).unwrap();
    d.validate().unwrap();
    d
}

struct Bus {
    unit: Arc<Mutex<SimKwpModule>>,
    ecu: Arc<Mutex<Tp20Ecu>>,
    ch: Tp20Channel,
}

fn bus_with(m: SimKwpModule, cfg: Tp20EcuConfig) -> Bus {
    let unit = Arc::new(Mutex::new(m));
    let ecu = sim_bus(unit.clone(), cfg);
    let ch = open_sim_channel(ecu.clone(), unit.clone(), 0).unwrap();
    Bus { unit, ecu, ch }
}

fn bus(cfg: Tp20EcuConfig) -> Bus {
    bus_with(SimKwpModule::default(), cfg)
}

fn breaking(n: usize) -> Tp20EcuConfig {
    Tp20EcuConfig {
        break_on_request: Some(n),
        ..Tp20EcuConfig::default()
    }
}

fn secured(sk: &dyn SeedKey) -> KwpLogin<'_> {
    KwpLogin::vag().with_security(kwp::VAG_SECURITY_LEVEL, sk)
}

fn confirmed() -> KwpWriteOptions {
    KwpWriteOptions {
        confirmed: true,
        workshop_code: Some(WSC),
        ..KwpWriteOptions::default()
    }
}

/// A K-line-style transport: KWP telegrams straight to the module.
fn kline(unit: &Arc<Mutex<SimKwpModule>>) -> impl RawDiag + '_ {
    move |req: &[u8]| -> Result<Vec<u8>, String> { Ok(lock(unit).answer(req)) }
}

// --- data format -------------------------------------------------------------

#[test]
fn kwp_kinds_parse_with_defaults_and_old_defs_still_parse() {
    let d = def();
    match &d.channel("Adaptation 5").unwrap().kind {
        ChannelKind::KwpAdaptation { channel, kwp, .. } => {
            assert_eq!(*channel, 5);
            assert!(!kwp.enumerate);
            assert_eq!(kwp.enter, 0xB8, "other fields keep their defaults");
        }
        k => panic!("{k:?}"),
    }
    assert_eq!(d.channel("Group 1").unwrap().kind.tag(), "kwp_group");
    assert!(d.channel("Basic settings 60").unwrap().kind.is_kwp());
    // A UDS-only definition from before the KWP kinds.
    let old = AdaptationDef::from_json(
        r#"{"id":"u","name":"U","channels":[{"name":"Fan","kind":"actuator","did":12304}]}"#,
    )
    .unwrap();
    assert!(old.validate().is_ok());
    assert!(!old.channels[0].kind.is_kwp());
    // Bad kwp addressing and inverted ranges are caught by validate().
    let bad = AdaptationDef::from_json(
        r#"{"id":"b","name":"B","channels":[{"name":"A","kind":"kwp_adaptation",
            "channel":1,"kwp":{"read":184}}]}"#,
    )
    .unwrap();
    assert!(matches!(
        bad.validate(),
        Err(AdaptationError::InvalidDef(_))
    ));
    let inv = AdaptationDef::from_json(
        r#"{"id":"b","name":"B","channels":[{"name":"A","kind":"kwp_adaptation",
            "channel":1,"min":9,"max":3}]}"#,
    )
    .unwrap();
    assert!(matches!(
        inv.validate(),
        Err(AdaptationError::InvalidDef(_))
    ));
}

// --- reads over TP2.0 --------------------------------------------------------

#[test]
fn group_read_over_tp20_decodes_cells() {
    let mut b = bus(Tp20EcuConfig::default());
    let r = {
        let mut d = CoreDiag::new(&mut b.ch, 500);
        kwp::read_group(&mut d, &def(), "Group 1", &KwpLogin::vag()).unwrap()
    };
    assert_eq!(r.group, 1);
    assert_eq!(r.cells.len(), 3);
    assert_eq!(r.cells[0].value, Some(90.0)); // 0x1A: b - a
    assert_eq!(r.cells[0].unit, "°C");
    assert_eq!(r.cells[1].value, Some(50.0)); // 0x21: 0.01 * a * b
    assert_eq!(r.cells[2].value, None, "unknown formula stays raw");
    assert!(!r.reestablished);
    assert!(b.ecu.lock().unwrap().errors.is_empty());
    b.ch.close();
}

#[test]
fn session_lost_with_the_channel_is_reestablished_for_a_read() {
    // The module closes the channel (A8) instead of answering `21 01`.
    let mut b = bus(breaking(1));
    let r = {
        let mut d = CoreDiag::new(&mut b.ch, 500);
        kwp::read_group(&mut d, &def(), "Group 1", &KwpLogin::vag()).unwrap()
    };
    assert!(r.reestablished);
    assert_eq!(r.cells[0].value, Some(90.0));
    assert_eq!(b.ch.reopens(), 1);
    let u = lock(&b.unit);
    // setup + re-open each reset the module's session; `10 89` came again
    // before the read on the new channel.
    assert_eq!(u.resets, 2);
    let reqs: Vec<_> = u.seen.iter().map(|r| r[0]).collect();
    assert_eq!(reqs, vec![0x10, 0x10, 0x21]);
}

#[test]
fn reopened_channel_really_has_no_session() {
    // Without the engine: after a break, the same request on the re-opened
    // channel is refused (0x80) until a session is started again.
    let mut b = bus(breaking(1));
    let mut d = CoreDiag::new(&mut b.ch, 500);
    assert_eq!(d.request(&[0x10, 0x89]).unwrap(), vec![0x50, 0x89]);
    assert!(
        d.request(&[0x21, 0x01]).is_err(),
        "the module broke the channel"
    );
    assert_eq!(d.request(&[0x21, 0x01]).unwrap(), vec![0x7F, 0x21, 0x80]);
}

#[test]
fn adaptation_read_over_tp20() {
    let mut b = bus(Tp20EcuConfig::default());
    let r = {
        let mut d = CoreDiag::new(&mut b.ch, 500);
        kwp::read_adaptation(&mut d, &def(), "Adaptation 1", &KwpLogin::vag()).unwrap()
    };
    assert_eq!(r.raw, 0x0200);
    assert_eq!(r.engineering, 256.0);
    assert_eq!(r.unit, "rpm");
    let m = r.measured.unwrap();
    assert_eq!((m.formula, m.value), (0x1A, Some(30.0)));
    let u = lock(&b.unit);
    assert!(!u.adaptation_mode, "exited after the read");
    assert_eq!(
        u.seen.iter().skip(1).cloned().collect::<Vec<_>>(),
        vec![
            vec![0x31, 0xB8, 0x00, 0x00],
            vec![0x31, 0xB8, 0x01, 0x03],
            vec![0x31, 0xB9, 0x01, 0x03, 0x01],
            vec![0x31, 0xBA, 0x01, 0x03],
            vec![0x32, 0xB8, 0x01, 0x03],
        ]
    );
    // Read-only: no test value, no save, no SecurityAccess.
    assert!(u
        .seen
        .iter()
        .all(|r| r[0] != 0x27 && r.get(1) != Some(&0xBB)));
}

#[test]
fn adaptation_read_honours_kwp_overrides() {
    let unit = Arc::new(Mutex::new(SimKwpModule::default()));
    let mut d = kline(&unit);
    let r = kwp::read_adaptation(&mut d, &def(), "Adaptation 5", &KwpLogin::vag()).unwrap();
    assert_eq!(r.raw, 1000);
    // `"enumerate": false`: no `31 B8 00 00`.
    assert!(lock(&unit)
        .seen
        .iter()
        .all(|r| r != &[0x31, 0xB8, 0x00, 0x00]));
}

// --- writes ------------------------------------------------------------------

#[test]
fn save_over_tp20_unlocks_saves_and_verifies() {
    let mut b = bus(Tp20EcuConfig::default());
    let sk = SimSeedKey;
    let rep = {
        let mut d = CoreDiag::new(&mut b.ch, 500);
        kwp::write_adaptation(
            &mut d,
            &def(),
            "Adaptation 1",
            0x0300,
            AdaptMode::Save,
            &secured(&sk),
            &confirmed(),
        )
        .unwrap()
    };
    assert_eq!(rep.outcome, KwpWriteOutcome::Verified, "{rep:?}");
    assert!(rep.is_ok() && rep.exited);
    assert_eq!(rep.before.as_ref().unwrap().raw, 0x0200);
    assert_eq!(rep.after.as_ref().unwrap().raw, 0x0300);
    let login = rep.login.as_ref().unwrap();
    assert_eq!(login.security_level, Some(0x03));
    assert_eq!(login.seed_key.as_deref(), Some("kwpsim-test"));
    let u = lock(&b.unit);
    assert_eq!(u.channels[&1], 0x0300);
    assert!(!u.adaptation_mode);
    // The wire follows the plan, line for line.
    assert_eq!(u.seen.len(), rep.plan.len());
    let save = u.seen.iter().find(|r| r[..2] == [0x31, 0xBB]).unwrap();
    assert_eq!(
        save,
        &[&[0x31, 0xBB, 0x01, 0x03, 0x03, 0x00][..], &WSC].concat()
    );
    assert_eq!(&u.seen[2][..2], &[0x27, 0x04]);
}

#[test]
fn test_mode_applies_temporarily_and_stores_nothing() {
    let unit = Arc::new(Mutex::new(SimKwpModule::default()));
    let mut d = kline(&unit);
    let sk = SimSeedKey;
    let opts = KwpWriteOptions {
        workshop_code: None, // a test needs no workshop code
        ..confirmed()
    };
    let rep = kwp::write_adaptation(
        &mut d,
        &def(),
        "Adaptation 1",
        0x0250,
        AdaptMode::Test,
        &secured(&sk),
        &opts,
    )
    .unwrap();
    assert_eq!(rep.outcome, KwpWriteOutcome::Tested);
    assert_eq!(rep.after.unwrap().raw, 0x0250);
    let u = lock(&unit);
    assert_eq!(u.channels[&1], 0x0200, "a test is not saved");
    assert_eq!(u.test_value, None, "exit discards it");
    assert!(u.seen.iter().all(|r| r.get(1) != Some(&0xBB)));
}

#[test]
fn session_lost_mid_save_aborts_and_sends_nothing_more() {
    // Requests: 10, 27 03, 27 04, B8 00 00, B8 01 03, B9 ch, BA, B9 value,
    // then the save (index 8): the module closes the channel instead.
    let mut b = bus(breaking(8));
    let sk = SimSeedKey;
    let rep = {
        let mut d = CoreDiag::new(&mut b.ch, 500);
        kwp::write_adaptation(
            &mut d,
            &def(),
            "Adaptation 1",
            0x0300,
            AdaptMode::Save,
            &secured(&sk),
            &confirmed(),
        )
        .unwrap()
    };
    match &rep.outcome {
        KwpWriteOutcome::Aborted {
            step, session_lost, ..
        } => {
            assert_eq!(step, "save");
            assert!(session_lost);
        }
        o => panic!("{o:?}"),
    }
    assert!(!rep.exited, "no exit into a re-opened channel");
    assert_eq!(
        b.ecu.lock().unwrap().requests.len(),
        9,
        "nothing after the break"
    );
    assert_eq!(b.ch.reopens(), 0, "the engine did not re-open the channel");
    assert_eq!(lock(&b.unit).channels[&1], 0x0200, "not saved");

    // The next flow starts its own session + SecurityAccess on the re-opened
    // channel and succeeds.
    let rep = {
        let mut d = CoreDiag::new(&mut b.ch, 500);
        kwp::write_adaptation(
            &mut d,
            &def(),
            "Adaptation 1",
            0x0300,
            AdaptMode::Save,
            &secured(&sk),
            &confirmed(),
        )
        .unwrap()
    };
    assert_eq!(rep.outcome, KwpWriteOutcome::Verified);
    assert_eq!(b.ch.reopens(), 1);
    assert_eq!(lock(&b.unit).channels[&1], 0x0300);
}

#[test]
fn negative_responses_surface_with_kwp_names() {
    let unit = Arc::new(Mutex::new(SimKwpModule::default()));
    let mut d = kline(&unit);
    let mut g = def();
    g.channels.push(opentune_adaptation::AdaptationChannel {
        name: "Group 9".into(),
        kind: ChannelKind::KwpGroup { group: 9 },
        note: String::new(),
    });
    match kwp::read_group(&mut d, &g, "Group 9", &KwpLogin::vag()) {
        Err(AdaptationError::NegativeResponse {
            service,
            code,
            name,
        }) => {
            assert_eq!((service, code, name), (0x21, 0x31, "requestOutOfRange"));
        }
        r => panic!("{r:?}"),
    }
    // A channel the module does not have: refused at the select, before any
    // value is sent -> Err (nothing written), and the mode is exited.
    let sk = SimSeedKey;
    let e = kwp::write_adaptation(
        &mut d,
        &def(),
        "Adaptation 9",
        1,
        AdaptMode::Save,
        &secured(&sk),
        &confirmed(),
    )
    .unwrap_err();
    assert!(
        matches!(e, AdaptationError::NegativeResponse { code: 0x31, .. }),
        "{e}"
    );
    assert!(!lock(&unit).adaptation_mode);

    // A refused save after the test value: Aborted (no session loss), exited.
    lock(&unit).refuse_save = Some(0x22);
    let rep = kwp::write_adaptation(
        &mut d,
        &def(),
        "Adaptation 1",
        0x0300,
        AdaptMode::Save,
        &secured(&sk),
        &confirmed(),
    )
    .unwrap();
    match &rep.outcome {
        KwpWriteOutcome::Aborted {
            step,
            session_lost,
            message,
        } => {
            assert_eq!(step, "save");
            assert!(!session_lost);
            assert!(message.contains("conditionsNotCorrect"), "{message}");
        }
        o => panic!("{o:?}"),
    }
    assert!(rep.exited);
    assert_eq!(lock(&unit).channels[&1], 0x0200);
}

#[test]
fn verify_failure_is_reported() {
    let unit = Arc::new(Mutex::new(SimKwpModule {
        corrupt_save: Some(0x0123),
        ..SimKwpModule::default()
    }));
    let mut d = kline(&unit);
    let sk = SimSeedKey;
    let rep = kwp::write_adaptation(
        &mut d,
        &def(),
        "Adaptation 1",
        0x0300,
        AdaptMode::Save,
        &secured(&sk),
        &confirmed(),
    )
    .unwrap();
    assert_eq!(
        rep.outcome,
        KwpWriteOutcome::VerifyFailed { read_back: 0x0123 }
    );
}

struct WrongKey;
impl SeedKey for WrongKey {
    fn id(&self) -> &str {
        "wrong"
    }
    fn compute(&self, seed: &[u8], _level: u8) -> opentune_core::Result<Vec<u8>> {
        Ok(seed.to_vec())
    }
}

#[test]
fn security_refusal_stops_before_anything_is_written() {
    let unit = Arc::new(Mutex::new(SimKwpModule::default()));
    let mut d = kline(&unit);
    let sk = WrongKey;
    for _ in 0..4 {
        let e = kwp::write_adaptation(
            &mut d,
            &def(),
            "Adaptation 1",
            0x0300,
            AdaptMode::Save,
            &secured(&sk),
            &confirmed(),
        )
        .unwrap_err();
        assert!(matches!(e, AdaptationError::SecurityDenied(_)), "{e}");
    }
    let u = lock(&unit);
    assert!(
        u.seen.iter().all(|r| r[0] != 0x31),
        "no routine was started"
    );
    assert_eq!(u.channels[&1], 0x0200);
    // After three wrong keys the module answers exceededNumberOfAttempts.
    assert!(u.seen.len() >= 12);
    // Basic settings with a wrong key: refused the same way.
    drop(u);
    let e = kwp::run_basic_settings(
        &mut d,
        &def(),
        "Basic settings 60",
        1,
        0,
        &secured(&sk),
        &confirmed(),
    )
    .unwrap_err();
    assert!(matches!(e, AdaptationError::SecurityDenied(_)));
    assert_eq!(lock(&unit).basic_active, None);
}

#[test]
fn the_confirmation_gate_refuses_before_any_io() {
    let unit = Arc::new(Mutex::new(SimKwpModule::default()));
    let mut d = kline(&unit);
    let sk = SimSeedKey;
    let d1 = def();
    let save = |d: &mut dyn RawDiag, login: &KwpLogin<'_>, opts: &KwpWriteOptions| {
        kwp::write_adaptation(d, &d1, "Adaptation 1", 0x0300, AdaptMode::Save, login, opts)
    };
    fn not<T>(r: Result<T, AdaptationError>) -> bool {
        matches!(r, Err(AdaptationError::NotConfirmed(_)))
    }
    // Not confirmed.
    let unconfirmed = KwpWriteOptions {
        confirmed: false,
        ..confirmed()
    };
    assert!(not(save(&mut d, &secured(&sk), &unconfirmed)));
    // No seed/key handler (OpenTune ships none).
    assert!(not(save(&mut d, &KwpLogin::vag(), &confirmed())));
    // No session.
    let no_session = KwpLogin {
        session: None,
        ..secured(&sk)
    };
    assert!(not(save(&mut d, &no_session, &confirmed())));
    // A save without the workshop code.
    let no_wsc = KwpWriteOptions {
        workshop_code: None,
        ..confirmed()
    };
    assert!(not(save(&mut d, &secured(&sk), &no_wsc)));
    // Basic settings, unconfirmed.
    assert!(not(kwp::run_basic_settings(
        &mut d,
        &def(),
        "Basic settings 60",
        1,
        0,
        &secured(&sk),
        &unconfirmed,
    )));
    // Out of range (max 4096).
    assert!(matches!(
        kwp::write_adaptation(
            &mut d,
            &def(),
            "Adaptation 1",
            5000,
            AdaptMode::Save,
            &secured(&sk),
            &confirmed()
        ),
        Err(AdaptationError::InvalidValue { .. })
    ));
    assert!(
        lock(&unit).seen.is_empty(),
        "nothing was sent for the refusals"
    );
}

#[test]
fn dry_run_plans_without_a_handler_and_sends_nothing() {
    let unit = Arc::new(Mutex::new(SimKwpModule::default()));
    let mut d = kline(&unit);
    let opts = KwpWriteOptions {
        dry_run: true,
        workshop_code: Some(WSC),
        ..KwpWriteOptions::default()
    };
    let rep = kwp::write_adaptation(
        &mut d,
        &def(),
        "Adaptation 1",
        0x0300,
        AdaptMode::Save,
        &KwpLogin::vag(),
        &opts,
    )
    .unwrap();
    assert_eq!(rep.outcome, KwpWriteOutcome::Planned);
    assert!(rep
        .plan
        .iter()
        .any(|l| l.starts_with("31 BB 01 03 03 00 00 00 01 23 45 67")));
    assert!(rep.plan[0].starts_with("10 89"));
    let bs = kwp::run_basic_settings(
        &mut d,
        &def(),
        "Basic settings 60",
        3,
        0,
        &KwpLogin::vag(),
        &opts,
    )
    .unwrap();
    assert_eq!(bs.outcome, BasicSettingsOutcome::Planned);
    assert!(bs.plan.iter().any(|l| l.starts_with("31 B8 00 3C")));
    assert!(bs.plan.iter().any(|l| l.starts_with("32 B8 00 3C")));
    assert!(lock(&unit).seen.is_empty());
}

// --- basic settings ----------------------------------------------------------

#[test]
fn basic_settings_over_kline_start_poll_stop() {
    let unit = Arc::new(Mutex::new(SimKwpModule::default()));
    let mut d = kline(&unit);
    let sk = SimSeedKey;
    let rep = kwp::run_basic_settings(
        &mut d,
        &def(),
        "Basic settings 60",
        3,
        0,
        &secured(&sk),
        &confirmed(),
    )
    .unwrap();
    assert_eq!(rep.outcome, BasicSettingsOutcome::Completed);
    assert!(rep.started && rep.stopped);
    assert_eq!(rep.samples.len(), 3);
    let s = &rep.samples[0];
    assert_eq!(s.status, 0x00);
    assert_eq!(s.cells.len(), 2);
    assert_eq!(s.cells[0].value, Some(10.0)); // 0x21: 0.01 * 100 * 10
    assert_eq!(s.cells[1].value, Some(80.0)); // 0x1A
    let u = lock(&unit);
    assert_eq!(u.basic_active, None, "stopped");
    assert_eq!(u.seen.last().unwrap(), &vec![0x32, 0xB8, 0x00, 60]);
}

#[test]
fn basic_settings_session_lost_mid_poll_sends_no_stop() {
    // 10, 27 03, 27 04, B8 00 00, B8 00 3C, BA (poll 0), BA (poll 1: break).
    let mut b = bus(breaking(6));
    let sk = SimSeedKey;
    let rep = {
        let mut d = CoreDiag::new(&mut b.ch, 500);
        kwp::run_basic_settings(
            &mut d,
            &def(),
            "Basic settings 60",
            5,
            0,
            &secured(&sk),
            &confirmed(),
        )
        .unwrap()
    };
    assert_eq!(rep.samples.len(), 1);
    assert!(rep.started && !rep.stopped);
    assert!(matches!(
        rep.outcome,
        BasicSettingsOutcome::Aborted {
            session_lost: true,
            ..
        }
    ));
    assert_eq!(b.ecu.lock().unwrap().requests.len(), 7);
    assert_eq!(b.ch.reopens(), 0);
}

#[test]
fn basic_settings_refused_start_is_an_error() {
    let unit = Arc::new(Mutex::new(SimKwpModule {
        basic_settings: Default::default(),
        ..SimKwpModule::default()
    }));
    let mut d = kline(&unit);
    let sk = SimSeedKey;
    let e = kwp::run_basic_settings(
        &mut d,
        &def(),
        "Basic settings 60",
        1,
        0,
        &secured(&sk),
        &confirmed(),
    )
    .unwrap_err();
    assert!(matches!(
        e,
        AdaptationError::NegativeResponse { code: 0x31, .. }
    ));
}

#[test]
fn wrong_kind_is_rejected() {
    let unit = Arc::new(Mutex::new(SimKwpModule::default()));
    let mut d = kline(&unit);
    assert!(matches!(
        kwp::read_group(&mut d, &def(), "Adaptation 1", &KwpLogin::vag()),
        Err(AdaptationError::ChannelKindMismatch { .. })
    ));
    assert!(matches!(
        opentune_adaptation::read_value(&mut d, &def(), "Group 1"),
        Err(AdaptationError::ChannelKindMismatch { .. })
    ));
    assert!(lock(&unit).seen.is_empty());
}
