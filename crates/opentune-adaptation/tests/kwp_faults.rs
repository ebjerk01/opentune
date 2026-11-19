//! The KWP2000 adaptation path under injected faults, against the synthetic
//! module (`opentune_adaptation::sim`). Seeded and deterministic (house style
//! of the core `tp20_fuzz.rs`), fast in the normal `cargo test`.
//!
//! One fault per flow, at a random request: the request is lost before the
//! module sees it (a negative response, or a transport failure), or the
//! module executes it and the answer is lost, cut short or mis-echoed. Plus
//! random module behaviour (refused or corrupted saves, channel select behind
//! SecurityAccess) and, end to end, random TP2.0 bus faults.
//!
//! What must hold, whatever happens:
//! * a write is refused before any I/O unless confirmed, with a session and
//!   (adaptation test / save) a seed/key handler; basic settings without a
//!   handler start nothing on a module that wants a login;
//! * `Err` from a write means the module holds nothing new: no test value
//!   and no save reached it;
//! * success (`Verified` / `Tested`) only when the module really holds the
//!   saved value (save) or kept its stored value (test), and the read-back
//!   equals the request — a failed save is never reported as success;
//! * after a session-loss symptom nothing more is sent;
//! * a read that succeeds returns exactly the module's data;
//! * basic settings: `Err` means the module is not in basic settings,
//!   `Completed` means it was stopped again.
use opentune_adaptation::kwp::{
    self, AdaptMode, BasicSettingsOutcome, KwpLogin, KwpWriteOptions, KwpWriteOutcome,
};
use opentune_adaptation::sim::{lock, open_sim_channel, sim_bus, SimKwpModule, SimSeedKey};
use opentune_adaptation::{AdaptationDef, AdaptationError};
use opentune_core::protocol::tp20sim::{Fault, FaultAction, FaultDir, FrameClass, Tp20EcuConfig};
use opentune_core::CoreDiag;
use std::sync::{Arc, Mutex};

const WSC: [u8; 6] = [0x00, 0x00, 0x01, 0x23, 0x45, 0x67];
const CASES: u64 = 1500;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[self.below(xs.len() as u64) as usize]
    }
}

fn def() -> AdaptationDef {
    let d = AdaptationDef::from_json(include_str!("fixtures/kwp-sim.json")).unwrap();
    d.validate().unwrap();
    d
}

fn confirmed() -> KwpWriteOptions {
    KwpWriteOptions {
        confirmed: true,
        workshop_code: Some(WSC),
        ..KwpWriteOptions::default()
    }
}

#[derive(Debug, Clone, Copy)]
enum Fault1 {
    /// The module never sees the request; it answers with this NRC.
    LostNrc(u8),
    /// The module never sees the request; the transport fails.
    LostTransport,
    /// The module executes it; the answer is lost.
    AnswerLost,
    /// The module executes it; the answer is cut to this many bytes.
    AnswerCut(usize),
    /// The module executes it; the answer echoes the wrong routine.
    AnswerMisEchoed,
}

/// A K-line-style transport to `unit` that applies `fault` to request
/// `at` (0-based). Records which requests the module saw (`delivered`).
struct Faulty {
    unit: Arc<Mutex<SimKwpModule>>,
    at: usize,
    fault: Fault1,
    n: usize,
    /// Request index the fault hit, once it did.
    hit: Option<usize>,
}

impl Faulty {
    fn call(&mut self, req: &[u8]) -> Result<Vec<u8>, String> {
        let i = self.n;
        self.n += 1;
        if i != self.at {
            return Ok(lock(&self.unit).answer(req));
        }
        self.hit = Some(i);
        match self.fault {
            Fault1::LostNrc(c) => Ok(vec![0x7F, req[0], c]),
            Fault1::LostTransport => Err("no answer (timeout)".into()),
            Fault1::AnswerLost => {
                lock(&self.unit).answer(req);
                Err("answer lost (timeout)".into())
            }
            Fault1::AnswerCut(n) => {
                let mut r = lock(&self.unit).answer(req);
                r.truncate(n.min(r.len().saturating_sub(1)));
                Ok(r)
            }
            Fault1::AnswerMisEchoed => {
                let mut r = lock(&self.unit).answer(req);
                if r.len() >= 2 && r[0] != 0x7F {
                    r[1] ^= 0x01;
                }
                Ok(r)
            }
        }
    }
}

fn random_fault(r: &mut Rng) -> Fault1 {
    match r.below(5) {
        0 => Fault1::LostNrc(r.pick(&[0x10, 0x11, 0x12, 0x22, 0x31, 0x33, 0x35, 0x7F, 0x80])),
        1 => Fault1::LostTransport,
        2 => Fault1::AnswerLost,
        3 => Fault1::AnswerCut(r.below(6) as usize),
        _ => Fault1::AnswerMisEchoed,
    }
}

fn random_module(r: &mut Rng) -> SimKwpModule {
    SimKwpModule {
        select_needs_security: r.below(3) == 0,
        refuse_save: (r.below(6) == 0).then(|| r.pick(&[0x22, 0x31, 0x33])),
        corrupt_save: (r.below(6) == 0).then(|| r.below(4096) as u16),
        ..SimKwpModule::default()
    }
}

fn is_test_value(q: &[u8]) -> bool {
    q.len() == 6 && q[..4] == [0x31, 0xB9, 0x01, 0x03]
}

fn is_save(q: &[u8]) -> bool {
    q.len() >= 2 && q[..2] == [0x31, 0xBB]
}

#[test]
fn writes_never_report_a_value_the_module_does_not_hold() {
    let d = def();
    let sk = SimSeedKey;
    let login = KwpLogin::vag().with_security(kwp::VAG_SECURITY_LEVEL, &sk);
    let mut outcomes = [0usize; 5];
    for seed in 0..CASES {
        let mut r = Rng(0xAD_A000 + seed);
        let module = random_module(&mut r);
        let (channel, number) = r.pick(&[("Adaptation 1", 1u8), ("Adaptation 5", 5)]);
        let stored_before = module.channels[&number];
        let unit = Arc::new(Mutex::new(module));
        let mode = r.pick(&[AdaptMode::Save, AdaptMode::Test]);
        let new_raw = r.below(4097) as u16;
        let mut t = Faulty {
            unit: unit.clone(),
            at: r.below(14) as usize,
            fault: random_fault(&mut r),
            n: 0,
            hit: None,
        };
        let res = {
            let mut diag = |q: &[u8]| t.call(q);
            kwp::write_adaptation(&mut diag, &d, channel, new_raw, mode, &login, &confirmed())
        };
        let u = lock(&unit);
        let stored = u.channels[&number];
        let ctx = format!(
            "seed {seed}: {mode:?} {channel} {new_raw:#x} fault {:?} at {}",
            t.fault, t.at
        );
        match &res {
            Err(e) => {
                outcomes[0] += 1;
                assert!(
                    !matches!(e, AdaptationError::NotConfirmed(_)),
                    "{ctx}: gate refused a confirmed write"
                );
                assert_eq!(stored, stored_before, "{ctx}: Err but the module changed");
                assert!(
                    !u.seen.iter().any(|q| is_test_value(q) || is_save(q)),
                    "{ctx}: Err after a write reached the module: {e}"
                );
            }
            Ok(rep) => {
                assert_eq!(rep.mode, mode);
                if rep.is_ok() {
                    outcomes[1] += 1;
                    assert_eq!(rep.after.as_ref().unwrap().raw, new_raw, "{ctx}");
                }
                match &rep.outcome {
                    KwpWriteOutcome::Verified => {
                        assert_eq!(mode, AdaptMode::Save, "{ctx}");
                        assert_eq!(stored, new_raw, "{ctx}: verified but not stored");
                    }
                    KwpWriteOutcome::Tested => {
                        assert_eq!(mode, AdaptMode::Test, "{ctx}");
                        assert_eq!(stored, stored_before, "{ctx}: a test stored the value");
                    }
                    KwpWriteOutcome::VerifyFailed { read_back } => {
                        outcomes[2] += 1;
                        assert_ne!(*read_back, new_raw, "{ctx}");
                    }
                    KwpWriteOutcome::Aborted { session_lost, .. } => {
                        outcomes[3] += 1;
                        if *session_lost {
                            // Nothing after the failing request.
                            assert_eq!(t.n, t.hit.unwrap() + 1, "{ctx}: sent after a lost session");
                        }
                    }
                    KwpWriteOutcome::Planned => panic!("{ctx}: planned without dry_run"),
                }
                if mode == AdaptMode::Save && stored != new_raw {
                    outcomes[4] += 1;
                    assert!(!rep.is_ok(), "{ctx}: a failed save reported as success");
                }
            }
        }
    }
    // Every class of result happened.
    assert!(outcomes.iter().all(|&n| n > 0), "{outcomes:?}");
}

#[test]
fn reads_return_the_module_data_or_an_error() {
    let d = def();
    let login = KwpLogin::vag();
    let mut reestablished = 0;
    for seed in 0..CASES {
        let mut r = Rng(0x2EAD_0000 + seed);
        let unit = Arc::new(Mutex::new(SimKwpModule::default()));
        let mut t = Faulty {
            unit: unit.clone(),
            at: r.below(8) as usize,
            fault: random_fault(&mut r),
            n: 0,
            hit: None,
        };
        let mut diag = |q: &[u8]| t.call(q);
        if r.below(2) == 0 {
            if let Ok(g) = kwp::read_group(&mut diag, &d, "Group 1", &login) {
                // A module that answers short cannot be told from one with a
                // shorter group, but what is read is its data, at least one
                // whole cell, and a ragged tail is noted.
                let full = lock(&unit).groups[&1].clone();
                assert!(full.starts_with(&g.raw) && g.raw.len() >= 3, "seed {seed}");
                assert_eq!(g.cells.len(), g.raw.len() / 3);
                assert_eq!(g.notes.is_empty(), g.raw.len() % 3 == 0, "seed {seed}");
                reestablished += usize::from(g.reestablished);
            }
        } else if let Ok(a) = kwp::read_adaptation(&mut diag, &d, "Adaptation 1", &login) {
            assert_eq!(a.raw, lock(&unit).channels[&1], "seed {seed}");
            assert!((a.engineering - f64::from(a.raw) * 0.5).abs() < 1e-9);
            reestablished += usize::from(a.reestablished);
        }
        // A read never writes.
        let u = lock(&unit);
        assert!(
            !u.seen.iter().any(|q| is_test_value(q) || is_save(q)),
            "seed {seed}"
        );
        assert!(u.test_value.is_none() && u.basic_active.is_none());
    }
    assert!(reestablished > 0, "some reads recovered a lost session");
}

#[test]
fn basic_settings_end_stopped_or_report_why_not() {
    let d = def();
    let sk = SimSeedKey;
    let login = KwpLogin::vag().with_security(kwp::VAG_SECURITY_LEVEL, &sk);
    let mut kinds = [0usize; 4];
    for seed in 0..CASES {
        let mut r = Rng(0xB5_0000 + seed);
        // Half the modules run basic settings without a login, half refuse
        // with 0x33 until unlocked (the flow then logs in and starts again).
        let unit = Arc::new(Mutex::new(SimKwpModule {
            basic_settings_needs_security: r.below(2) == 0,
            ..SimKwpModule::default()
        }));
        let polls = 1 + r.below(4) as u32;
        let mut t = Faulty {
            unit: unit.clone(),
            at: r.below(8 + polls as u64 + 1) as usize,
            fault: random_fault(&mut r),
            n: 0,
            hit: None,
        };
        let res = {
            let mut diag = |q: &[u8]| t.call(q);
            kwp::run_basic_settings(
                &mut diag,
                &d,
                "Basic settings 60",
                polls,
                0,
                &login,
                &confirmed(),
            )
        };
        let active = lock(&unit).basic_active;
        let ctx = format!("seed {seed}: fault {:?} at {}", t.fault, t.at);
        match res {
            Err(_) => {
                kinds[0] += 1;
                assert_eq!(active, None, "{ctx}: Err but basic settings run");
            }
            Ok(rep) => match rep.outcome {
                BasicSettingsOutcome::Completed => {
                    kinds[1] += 1;
                    assert_eq!(rep.samples.len(), polls as usize, "{ctx}");
                    assert!(rep.started && rep.stopped);
                    assert_eq!(active, None, "{ctx}: completed but still running");
                    for s in &rep.samples {
                        let full = lock(&unit).basic_settings[&60].clone();
                        assert!(full.starts_with(&s.raw[1..]) && s.raw.len() > 3, "{ctx}");
                    }
                }
                BasicSettingsOutcome::StopFailed { .. } => {
                    kinds[2] += 1;
                    assert!(rep.started && !rep.stopped);
                }
                BasicSettingsOutcome::Aborted { session_lost, .. } => {
                    kinds[3] += 1;
                    if session_lost {
                        assert_eq!(t.n, t.hit.unwrap() + 1, "{ctx}: sent after a lost session");
                    } else if rep.stopped {
                        assert_eq!(active, None, "{ctx}");
                    }
                }
                BasicSettingsOutcome::Planned => panic!("{ctx}"),
            },
        }
    }
    assert!(kinds.iter().all(|&n| n > 0), "{kinds:?}");
}

#[test]
fn unconfirmed_or_unsecured_writes_send_nothing() {
    let d = def();
    let sk = SimSeedKey;
    let mut r = Rng(0x6A7E);
    for _ in 0..200 {
        let unit = Arc::new(Mutex::new(SimKwpModule::default()));
        let sent = std::cell::Cell::new(0);
        let mut diag = |q: &[u8]| -> Result<Vec<u8>, String> {
            sent.set(sent.get() + 1);
            Ok(lock(&unit).answer(q))
        };
        let (login, opts) = match r.below(4) {
            0 => (
                KwpLogin::vag().with_security(3, &sk),
                KwpWriteOptions {
                    confirmed: false,
                    ..confirmed()
                },
            ),
            1 => (KwpLogin::vag(), confirmed()),
            2 => (KwpLogin::none().with_security(3, &sk), confirmed()),
            _ => (
                KwpLogin::vag().with_security(3, &sk),
                KwpWriteOptions {
                    workshop_code: None,
                    ..confirmed()
                },
            ),
        };
        let mode = AdaptMode::Save;
        let e = kwp::write_adaptation(&mut diag, &d, "Adaptation 1", 1, mode, &login, &opts);
        assert!(matches!(e, Err(AdaptationError::NotConfirmed(_))), "{e:?}");
        let e = kwp::run_basic_settings(&mut diag, &d, "Basic settings 60", 1, 0, &login, &opts);
        // The basic-settings gate needs no workshop code, and no seed/key
        // handler up front: without one, a module that wants a login (the
        // default synthetic module) refuses and nothing is started.
        if opts.workshop_code.is_some() && login.seed_key.is_none() {
            assert!(matches!(e, Err(AdaptationError::LoginRequired(_))), "{e:?}");
            let u = lock(&unit);
            assert!(u.basic_active.is_none() && !u.services().contains(&0x27));
        } else if opts.workshop_code.is_some() {
            assert!(matches!(e, Err(AdaptationError::NotConfirmed(_))), "{e:?}");
            assert_eq!(sent.get(), 0);
        }
    }
}

#[test]
fn saves_over_a_faulty_tp20_bus_are_verified_only_when_stored() {
    let d = def();
    let sk = SimSeedKey;
    let login = KwpLogin::vag().with_security(kwp::VAG_SECURITY_LEVEL, &sk);
    let (mut verified, mut not_ok) = (0, 0);
    for seed in 0..150u64 {
        let mut r = Rng(0x7F20_0000 + seed);
        let faults = (0..1 + r.below(2))
            .map(|_| {
                let seq = 1 + r.below(15) as u8;
                let actions = [
                    FaultAction::Drop,
                    FaultAction::Duplicate,
                    FaultAction::Reorder,
                    FaultAction::WrongSeq(seq),
                    FaultAction::Disconnect,
                ];
                Fault::new(
                    r.pick(&[FaultDir::ToEcu, FaultDir::ToTester]),
                    r.pick(&[FrameClass::Data, FrameClass::Ack, FrameClass::Any]),
                    r.below(40) as usize,
                    r.pick(&actions),
                )
            })
            .collect();
        let unit = Arc::new(Mutex::new(random_module(&mut r)));
        let ecu = sim_bus(
            unit.clone(),
            Tp20EcuConfig {
                block_size: 1 + r.below(15) as u8,
                break_on_request: (r.below(3) == 0).then(|| r.below(11) as usize),
                faults,
                ..Tp20EcuConfig::default()
            },
        );
        let Ok(mut ch) = open_sim_channel(ecu, unit.clone(), 0) else {
            continue;
        };
        let new_raw = r.below(4097) as u16;
        let res = {
            let mut diag = CoreDiag::new(&mut ch, 300);
            kwp::write_adaptation(
                &mut diag,
                &d,
                "Adaptation 1",
                new_raw,
                AdaptMode::Save,
                &login,
                &confirmed(),
            )
        };
        ch.close();
        let stored = lock(&unit).channels[&1];
        match res {
            Ok(rep) if rep.is_ok() => {
                verified += 1;
                assert_eq!(stored, new_raw, "seed {seed}: verified but not stored");
            }
            Ok(_) => not_ok += 1,
            Err(_) => {
                not_ok += 1;
                assert_eq!(stored, 0x0200, "seed {seed}: Err but the module changed");
            }
        }
    }
    assert!(verified > 0 && not_ok > 0, "{verified} / {not_ok}");
}
