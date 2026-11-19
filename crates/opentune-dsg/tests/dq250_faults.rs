//! The DQ250 reader over the core TP2.0 stack with faults: lost ACKs, a
//! channel the module tears down mid-read, KWP2000 negative responses
//! (`0x78` responsePending handled and leaked, `0x31`, `0x22`), and seeded
//! fuzzing over random bus faults and over a transport answering garbage.
//!
//! The invariants: a read either returns exactly what the unit holds or a
//! recorded error, never another value; the reader never sends anything but
//! its read services; after the channel breaks the reader restarts the
//! session on the re-opened channel instead of reading on without it.
use opentune_core::protocol::diag::DiagTransport;
use opentune_core::protocol::tp20sim::{
    Fault, FaultAction, FaultDir, FrameClass, Tp20Ecu, Tp20EcuConfig,
};
use opentune_core::{Error, Result};
use opentune_dsg::sim::{open_sim_channel_resetting, SimDq250, SIM_TESTER_TX_ID};
use opentune_dsg::{
    decode_group, Dq250Reader, GroupLabels, DEFAULT_ID_OPTIONS, KWP_SESSION, READ_SERVICES,
    TP20_LOGICAL_ADDRESS,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

type Unit = Arc<Mutex<SimDq250>>;

fn lock(u: &Unit) -> std::sync::MutexGuard<'_, SimDq250> {
    u.lock().unwrap_or_else(|e| e.into_inner())
}

/// The unit behind a TP2.0 module; `inject` answers a request telegram with
/// the given reply instead of the unit (once per entry, in order).
fn bus(unit: &Unit, cfg: Tp20EcuConfig, inject: Vec<(Vec<u8>, Vec<u8>)>) -> Arc<Mutex<Tp20Ecu>> {
    let u = unit.clone();
    let mut inject = inject;
    Tp20Ecu::new(
        Tp20EcuConfig {
            addr: TP20_LOGICAL_ADDRESS,
            tester_tx_id: SIM_TESTER_TX_ID,
            ..cfg
        },
        Box::new(move |req| {
            if let Some(i) = inject.iter().position(|(q, _)| q == req) {
                lock(&u).seen.push(req.to_vec());
                return inject.remove(i).1;
            }
            lock(&u).answer(req)
        }),
    )
    .shared()
}

fn unit() -> Unit {
    Arc::new(Mutex::new(SimDq250 {
        dtcs: vec![(12345, 0xE3), (100, 0x23)],
        ..SimDq250::default()
    }))
}

fn expected_group(u: &Unit, g: u8) -> Vec<u8> {
    lock(u).groups[&g].clone()
}

fn read_only(u: &Unit) {
    for s in lock(u).services() {
        assert!(READ_SERVICES.contains(&s), "non-read service 0x{s:02X}");
    }
}

#[test]
fn lost_acks_are_recovered_by_the_transport() {
    for (dir, nth) in [
        (FaultDir::ToEcu, 0),
        (FaultDir::ToEcu, 2),
        (FaultDir::ToTester, 0),
        (FaultDir::ToTester, 3),
    ] {
        let u = unit();
        let ecu = bus(
            &u,
            Tp20EcuConfig {
                block_size: 2,
                faults: vec![Fault::new(dir, FrameClass::Ack, nth, FaultAction::Drop)],
                ..Tp20EcuConfig::default()
            },
            vec![],
        );
        let mut ch = open_sim_channel_resetting(ecu.clone(), u.clone(), 0).unwrap();
        {
            let mut r = Dq250Reader::start(&mut ch, Some(KWP_SESSION), 500);
            assert!(r.session_error.is_none());
            let id = r.identify(&DEFAULT_ID_OPTIONS);
            assert_eq!(id.identity.unwrap().part_number, "02E000000X");
            assert_eq!(r.dtcs().unwrap().len(), 2);
            for g in [1, 2] {
                let got = r.group(g, &GroupLabels::default());
                assert!(got.error.is_none(), "{dir:?} {nth}: {got:?}");
                let want = decode_group(g, &expected_group(&u, g), &GroupLabels::default());
                assert_eq!(got.cells, want.cells);
            }
        }
        ch.close();
        assert_eq!(
            ecu.lock().unwrap().faults_fired,
            1,
            "{dir:?} {nth}: the fault hit"
        );
        read_only(&u);
    }
}

#[test]
fn channel_torn_down_mid_read_reopens_and_restarts_the_session() {
    // The module closes the channel (A8) instead of answering its 3rd request
    // (0: session, 1: group 1, 2: group 2 ...).
    let u = unit();
    let ecu = bus(
        &u,
        Tp20EcuConfig {
            break_on_request: Some(2),
            ..Tp20EcuConfig::default()
        },
        vec![],
    );
    let mut ch = open_sim_channel_resetting(ecu.clone(), u.clone(), 0).unwrap();
    {
        let mut r = Dq250Reader::start(&mut ch, Some(KWP_SESSION), 500);
        assert!(r.group(1, &GroupLabels::default()).error.is_none());
        let g = r.group(2, &GroupLabels::default());
        assert!(g.error.is_none(), "{g:?}");
        assert_eq!(
            g.cells,
            decode_group(2, &expected_group(&u, 2), &GroupLabels::default()).cells
        );
        assert!(g.notes.iter().any(|n| n.contains("session")), "{g:?}");
        assert_eq!(r.reestablished, 1);
        // The next reads run in the new session without further restarts.
        assert!(r.group(1, &GroupLabels::default()).error.is_none());
        assert_eq!(r.dtcs().unwrap().len(), 2);
        assert_eq!(r.reestablished, 1);
    }
    assert!(ch.reopens() >= 1);
    ch.close();
    let e = ecu.lock().unwrap();
    assert_eq!(e.setups, 2, "one re-open");
    read_only(&u);
    // Session, group 1, (group 2 broken), session again, group 2, ...
    let seen = lock(&u).seen.clone();
    assert_eq!(seen[0], vec![0x10, KWP_SESSION]);
    assert_eq!(seen[2], vec![0x10, KWP_SESSION], "{seen:02X?}");
}

#[test]
fn identify_and_dtcs_also_survive_a_teardown() {
    for at in 1..=4 {
        let u = unit();
        let ecu = bus(
            &u,
            Tp20EcuConfig {
                break_on_request: Some(at),
                ..Tp20EcuConfig::default()
            },
            vec![],
        );
        let mut ch = open_sim_channel_resetting(ecu.clone(), u.clone(), 0).unwrap();
        {
            let mut r = Dq250Reader::start(&mut ch, Some(KWP_SESSION), 500);
            let id = r.identify(&DEFAULT_ID_OPTIONS);
            assert_eq!(id.records.len(), 2, "break at {at}: {id:?}");
            assert_eq!(id.errors.len(), 1, "only the absent 0x97");
            assert_eq!(r.dtcs().unwrap().len(), 2, "break at {at}");
            assert_eq!(r.reestablished, 1, "break at {at}");
        }
        ch.close();
        read_only(&u);
    }
}

#[test]
fn response_pending_is_waited_out_and_a_leaked_one_is_an_error() {
    // 0x78 handled by the transport: two pending replies before each answer.
    let u = unit();
    let ecu = bus(
        &u,
        Tp20EcuConfig {
            response_pending: 2,
            pending_interval_ms: 50,
            ..Tp20EcuConfig::default()
        },
        vec![],
    );
    let mut ch = open_sim_channel_resetting(ecu.clone(), u.clone(), 0).unwrap();
    {
        let mut r = Dq250Reader::start(&mut ch, Some(KWP_SESSION), 500);
        assert!(r.session_error.is_none());
        let g = r.group(1, &GroupLabels::default());
        assert!(g.error.is_none(), "{g:?}");
        assert_eq!(g.cells.len(), 4);
    }
    ch.close();
    assert!(ecu.lock().unwrap().pending_sent >= 4);

    // Over TP2.0 the transport owns 0x78: a pending reply never followed by
    // the answer ends in a timeout, which (like a broken channel) restarts
    // the session and reads again. The value read is the unit's.
    let u = unit();
    let ecu = bus(
        &u,
        Tp20EcuConfig::default(),
        vec![(vec![0x21, 0x01], vec![0x7F, 0x21, 0x78])],
    );
    let mut ch = open_sim_channel_resetting(ecu, u.clone(), 0).unwrap();
    {
        let mut r = Dq250Reader::start(&mut ch, Some(KWP_SESSION), 300);
        let g = r.group(1, &GroupLabels::default());
        assert!(g.error.is_none(), "{g:?}");
        assert_eq!(
            g.cells,
            decode_group(1, &expected_group(&u, 1), &GroupLabels::default()).cells
        );
        assert_eq!(r.reestablished, 1);
    }
    ch.close();

    // A 0x78 that reaches the reader (a transport that did not wait) is a
    // recorded error, never cells, and not retried as a lost session.
    let mut t = Script(vec![
        (vec![0x10, KWP_SESSION], vec![0x50, KWP_SESSION]),
        (vec![0x21, 0x01], vec![0x7F, 0x21, 0x78]),
        (vec![0x21, 0x01], vec![0x61, 0x01, 0x1A, 0, 60]),
    ]);
    let mut r = Dq250Reader::start(&mut t, Some(KWP_SESSION), 300);
    let g = r.group(1, &GroupLabels::default());
    assert!(g.cells.is_empty() && g.raw_hex.is_empty());
    assert!(g.error.as_deref().unwrap().contains("0x78"), "{g:?}");
    assert_eq!(r.reestablished, 0);
    // The next read is unaffected.
    assert_eq!(
        r.group(1, &GroupLabels::default()).cells[0].value,
        Some(60.0)
    );
}

#[test]
fn a_cut_off_group_answer_is_an_error_not_an_empty_group() {
    // Regression: `61 07` (the echo, nothing after it) read as a group with
    // no cells and no error.
    for answer in [vec![0x61, 0x07], vec![0x61, 0x07, 0x1A, 0x00]] {
        let mut t = Script(vec![
            (vec![0x10, KWP_SESSION], vec![0x50, KWP_SESSION]),
            (vec![0x21, 0x07], answer.clone()),
        ]);
        let mut r = Dq250Reader::start(&mut t, Some(KWP_SESSION), 300);
        let g = r.group(7, &GroupLabels::default());
        assert!(g.cells.is_empty(), "{answer:02X?}");
        assert!(
            g.error
                .as_deref()
                .unwrap()
                .contains("without a single cell"),
            "{g:?}"
        );
    }
}

/// Answers from a script, in order; anything else is a protocol error.
struct Script(Vec<(Vec<u8>, Vec<u8>)>);

impl DiagTransport for Script {
    fn request(&mut self, req: &[u8], _timeout_ms: u64) -> Result<Vec<u8>> {
        match self.0.iter().position(|(q, _)| q == req) {
            Some(i) => Ok(self.0.remove(i).1),
            None => Err(Error::Protocol(format!("unscripted {req:02X?}"))),
        }
    }
}

#[test]
fn refusals_0x31_and_0x22_are_recorded_not_guessed() {
    let u = unit();
    let ecu = bus(
        &u,
        Tp20EcuConfig::default(),
        vec![
            (vec![0x18, 0x02, 0xFF, 0x00], vec![0x7F, 0x18, 0x22]),
            (vec![0x1A, 0x9B], vec![0x7F, 0x1A, 0x22]),
        ],
    );
    let mut ch = open_sim_channel_resetting(ecu, u.clone(), 0).unwrap();
    {
        let mut r = Dq250Reader::start(&mut ch, Some(KWP_SESSION), 500);
        // 0x31: group not implemented.
        let g = r.group(9, &GroupLabels::default());
        assert!(g.cells.is_empty());
        assert!(g.error.as_deref().unwrap().contains("0x31"), "{g:?}");
        // 0x22 on the DTC read: an error, not "no codes".
        let e = r.dtcs().unwrap_err();
        assert!(e.contains("0x22"), "{e}");
        // 0x22 on the VAG record: recorded; the other options still read.
        let id = r.identify(&DEFAULT_ID_OPTIONS);
        assert!(id
            .errors
            .iter()
            .any(|e| e.option == 0x9B && e.error.contains("0x22")));
        let i = id.identity.unwrap();
        assert!(i.part_number.is_empty());
        assert_eq!(i.hardware_number.as_deref(), Some("02E000000Y"));
        // A plain refusal is not a lost session: no restart.
        assert_eq!(r.reestablished, 0);
    }
    ch.close();

    // Without a session the unit refuses every read with 0x22: each is an
    // error and nothing is decoded.
    let u = Arc::new(Mutex::new(SimDq250 {
        session: None,
        ..SimDq250::default()
    }));
    let ecu = bus(&u, Tp20EcuConfig::default(), vec![]);
    let mut ch = open_sim_channel_resetting(ecu, u.clone(), 0).unwrap();
    {
        let mut r = Dq250Reader::start(&mut ch, Some(KWP_SESSION), 500);
        assert!(r.session_error.is_some());
        assert!(r.group(1, &GroupLabels::default()).error.is_some());
        assert!(r.dtcs().is_err());
        assert!(!r.identify(&DEFAULT_ID_OPTIONS).responded());
    }
    ch.close();
    read_only(&u);
}

// ---------------------------------------------------------------------------
// Seeded fuzzing
// ---------------------------------------------------------------------------

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

#[test]
fn random_bus_faults_never_yield_a_wrong_value() {
    let cases = 300;
    let (mut opened, mut ok_reads, mut failed_reads, mut restarts) = (0, 0, 0, 0);
    for seed in 0..cases {
        let mut r = Rng(0xD5_6000 + seed);
        let actions = [
            FaultAction::Drop,
            FaultAction::Duplicate,
            FaultAction::Reorder,
            FaultAction::WrongSeq(1 + r.below(15) as u8),
            FaultAction::Delay(r.below(300)),
            FaultAction::Disconnect,
        ];
        let classes = [FrameClass::Data, FrameClass::Ack, FrameClass::Any];
        let faults = (0..r.below(3))
            .map(|_| {
                Fault::new(
                    r.pick(&[FaultDir::ToEcu, FaultDir::ToTester]),
                    r.pick(&classes),
                    r.below(12) as usize,
                    r.pick(&actions),
                )
            })
            .collect();
        let u = unit();
        let ecu = bus(
            &u,
            Tp20EcuConfig {
                block_size: 1 + r.below(15) as u8,
                response_pending: r.below(2) as u32,
                break_on_request: (r.below(3) == 0).then(|| r.below(6) as usize),
                faults,
                ..Tp20EcuConfig::default()
            },
            vec![],
        );
        let Ok(mut ch) = open_sim_channel_resetting(ecu.clone(), u.clone(), 0) else {
            continue;
        };
        opened += 1;
        {
            let mut r2 = Dq250Reader::start(&mut ch, Some(KWP_SESSION), 300);
            for g in [1u8, 2, 1, 9] {
                let got = r2.group(g, &GroupLabels::default());
                match (&got.error, lock(&u).groups.get(&g).cloned()) {
                    (None, Some(rec)) => {
                        ok_reads += 1;
                        let want = decode_group(g, &rec, &GroupLabels::default());
                        assert_eq!(got.cells, want.cells, "seed {seed} group {g}");
                        assert_eq!(got.raw_hex, want.raw_hex, "seed {seed}");
                    }
                    (None, None) => panic!("seed {seed}: group {g} read without existing"),
                    (Some(_), _) => {
                        failed_reads += 1;
                        assert!(got.cells.is_empty() && got.raw_hex.is_empty())
                    }
                }
            }
            restarts += r2.reestablished;
            if let Ok(d) = r2.dtcs() {
                let want: Vec<(u16, u8)> = lock(&u).dtcs.clone();
                let got: Vec<(u16, u8)> = d.iter().map(|x| (x.code, x.status_byte)).collect();
                assert_eq!(got, want, "seed {seed}");
            }
        }
        ch.close();
        read_only(&u);
    }
    // The faults really bit: most channels opened, reads both succeeded and
    // failed, and some succeeded only after a session restart.
    assert!(opened > cases * 3 / 4, "{opened}");
    assert!(
        ok_reads > 300 && failed_reads > 300,
        "{ok_reads} / {failed_reads}"
    );
    assert!(restarts > 10, "{restarts}");
}

/// Answers each request with a random reply: garbage, a truncated or
/// mis-echoed positive response, an NRC, or a transport error.
struct Garbage(Rng, Vec<Vec<u8>>);

impl DiagTransport for Garbage {
    fn request(&mut self, req: &[u8], _timeout_ms: u64) -> Result<Vec<u8>> {
        self.1.push(req.to_vec());
        let r = &mut self.0;
        let sid = req[0];
        Ok(match r.below(8) {
            0 => return Err(Error::Timeout(1)),
            1 => return Err(Error::Protocol("channel broke".into())),
            2 => vec![0x7F, sid, r.next() as u8],
            3 => vec![0x7F],
            4 => vec![],
            5 => (0..r.below(40)).map(|_| r.next() as u8).collect(),
            _ => {
                // A positive reply with a random (maybe wrong) echo.
                let mut v = vec![sid.wrapping_add(0x40)];
                if r.below(4) != 0 {
                    v.push(req.get(1).copied().unwrap_or(0));
                } else {
                    v.push(r.next() as u8);
                }
                v.extend((0..r.below(50)).map(|_| r.next() as u8));
                v
            }
        })
    }
}

#[test]
fn a_garbage_answering_unit_never_panics_or_decodes_mis_echoed_data() {
    for seed in 0..2000u64 {
        let mut t = Garbage(Rng(seed), Vec::new());
        {
            let mut r = Dq250Reader::start(&mut t, Some(KWP_SESSION), 10);
            let id = r.identify(&[0x9B, 0x91, 0x97, 0x86, 0x20]);
            assert_eq!(id.records.len() + id.errors.len(), 5, "seed {seed}");
            for rec in &id.records {
                assert!(rec.ascii.is_ascii());
            }
            if let Ok(d) = r.dtcs() {
                assert!(d.iter().all(|x| x.vag_number == u32::from(x.code)));
            }
            let g = r.group(7, &GroupLabels::default());
            if g.error.is_none() {
                assert!(!g.cells.is_empty() && g.cells.len() <= 256);
                assert!(g.raw_hex.len() % 2 == 0);
            } else {
                assert!(g.cells.is_empty());
            }
        }
        let sent: BTreeMap<u8, usize> = t.1.iter().fold(BTreeMap::new(), |mut m, q| {
            *m.entry(q[0]).or_default() += 1;
            m
        });
        for s in sent.keys() {
            assert!(READ_SERVICES.contains(s), "seed {seed}: sent 0x{s:02X}");
        }
        assert!(
            !t.1.iter().any(|q| q == &[0x1A, 0x20]),
            "out-of-range option sent"
        );
    }
}
