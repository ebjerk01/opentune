//! Deterministic seeded fuzzing of the VW TP2.0 client against the
//! fault-injecting simulated module (`tp20sim`). Each case draws a module
//! configuration (block size, T1/T3, responsePending chains, reply delays,
//! channel tests, disconnects), a client configuration and a script of bus
//! faults, then runs a few uniquely tagged requests and checks:
//!
//! * nothing panics, and every request returns within a wall-clock bound;
//! * a request either returns exactly its own reply or a typed error
//!   (`Protocol` / `Timeout`) — never another request's reply, a stale or
//!   duplicated payload, or a corrupted one;
//! * the module executes each request at most once, in the order sent, and
//!   only requests that were sent;
//! * without faults or module-side disruptions every request succeeds and
//!   the module saw no protocol violation.
//!
//! `cargo test` runs [`DEFAULT_CASES`]. For a longer run set
//! `OPENTUNE_TP20_FUZZ_CASES=200000`; to replay one failing case set
//! `OPENTUNE_TP20_FUZZ_SEED=<seed>` (printed on failure).
use opentune_core::protocol::diag::DiagTransport;
use opentune_core::protocol::tp20sim::{
    Fault, FaultAction, FaultDir, FrameClass, Tp20Ecu, Tp20EcuConfig, Tp20SimLink,
};
use opentune_core::protocol::vwtp20::VwTp20;
use opentune_core::Error;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::{Duration, Instant};

const DEFAULT_CASES: u64 = 3000;
const TIMEOUT_MS: u64 = 500;
/// Wall-clock bound per request. The module's clock is virtual, so a
/// request only costs CPU time (plus at most a few 100 µs separations).
const REQUEST_BOUND: Duration = Duration::from_secs(5);

/// splitmix64: small, fast, good enough for test-case generation.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Uniform in `0..n`.
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    /// True with probability 1/n.
    fn one_in(&mut self, n: u64) -> bool {
        self.below(n) == 0
    }
    fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[self.below(xs.len() as u64) as usize]
    }
}

/// `21 <tag> <len> …` → `61 <tag>` + `len` bytes derived from the tag.
fn app(req: &[u8]) -> Vec<u8> {
    match req {
        [0x21, th, tl, lh, ll, ..] => {
            let tag = u16::from_be_bytes([*th, *tl]);
            let n = u16::from_be_bytes([*lh, *ll]) as usize;
            let mut r = vec![0x61, *th, *tl];
            r.extend((0..n).map(|i| (i as u16).wrapping_mul(31).wrapping_add(tag) as u8));
            r
        }
        _ => vec![0x7F, req.first().copied().unwrap_or(0), 0x11],
    }
}

struct Case {
    ecu: Tp20EcuConfig,
    client_bs: u8,
    retries: u8,
    max_pending: u32,
    requests: Vec<Vec<u8>>,
}

fn gen_case(seed: u64) -> Case {
    let mut r = Rng(seed);
    let mut faults = Vec::new();
    if !r.one_in(4) {
        for _ in 0..1 + r.below(5) {
            let dir = if r.one_in(2) {
                FaultDir::ToEcu
            } else {
                FaultDir::ToTester
            };
            let class = r.pick(&[
                FrameClass::Any,
                FrameClass::Data,
                FrameClass::Data,
                FrameClass::Data,
                FrameClass::Ack,
                FrameClass::Ack,
                FrameClass::Params,
                FrameClass::ChannelTest,
                FrameClass::Break,
                FrameClass::Disconnect,
                FrameClass::Setup,
            ]);
            let action = match r.below(12) {
                0..=3 => FaultAction::Drop,
                4..=5 => FaultAction::Duplicate,
                6..=7 => FaultAction::Reorder,
                8..=9 => FaultAction::WrongSeq(1 + r.below(15) as u8),
                10 if r.one_in(2) => FaultAction::Disconnect,
                // Delays move frames in virtual time only; restrict them to
                // management frames (see FaultAction::Delay).
                _ => FaultAction::Delay(1 + r.below(300)),
            };
            let action = match (action, class) {
                (FaultAction::Delay(_), FrameClass::Any | FrameClass::Data) => FaultAction::Drop,
                (a, _) => a,
            };
            faults.push(Fault::new(dir, class, r.below(24) as usize, action));
        }
        // A rewritten data sequence next to a lost/duplicated/reordered
        // frame can forge the missing frame (frame k+1's data labelled k):
        // a corruption CAN's CRC rules out on a real bus and no TP2.0
        // receiver can detect. Keep such a rewrite only when it is the sole
        // data-affecting fault in its direction.
        let data_hit = |f: &Fault| matches!(f.class, FrameClass::Any | FrameClass::Data);
        let snapshot = faults.clone();
        for f in faults.iter_mut() {
            if data_hit(f) && matches!(f.action, FaultAction::WrongSeq(_)) {
                let others = snapshot
                    .iter()
                    .filter(|g| g.dir == f.dir && data_hit(g) && **g != *f)
                    .count();
                if others > 0 {
                    f.action = FaultAction::Drop;
                }
            }
        }
    }
    let ecu = Tp20EcuConfig {
        block_size: r.pick(&[0, 1, 2, 3, 4, 5, 8, 15, 15, 15, 0x20]),
        last_without_ack: r.one_in(6),
        ack_not_ready_first: r.one_in(6),
        t1: r.pick(&[0x8A, 0x8A, 0x00, 0x0A, 0x4A, 0x81, 0xC1, 0xFF]),
        t3: if r.one_in(25) { 0x01 } else { 0x00 },
        max_retransmits: r.below(4) as u8,
        response_pending: if r.one_in(4) { r.below(4) as u32 } else { 0 },
        pending_interval_ms: r.pick(&[0, 0, 50, 400, 3000]),
        reply_delay_ms: if r.one_in(2) { 0 } else { r.below(700) },
        channel_test_before_reply: r.one_in(8),
        break_on_request: if r.one_in(10) {
            Some(r.below(4) as usize)
        } else {
            None
        },
        faults,
        ..Tp20EcuConfig::default()
    };
    let n = 1 + r.below(4);
    let requests = (0..n)
        .map(|i| {
            let extra = if r.one_in(10) {
                r.below(400)
            } else {
                r.below(60)
            } as usize;
            let reply = if r.one_in(10) {
                r.below(700)
            } else {
                r.below(120)
            } as u16;
            let tag = ((seed as u16) << 3) ^ i as u16;
            let mut q = vec![0x21];
            q.extend(tag.to_be_bytes());
            q.extend(reply.to_be_bytes());
            q.extend((0..extra).map(|k| (k as u8) ^ (tag as u8)));
            q
        })
        .collect();
    Case {
        ecu,
        client_bs: 1 + r.below(15) as u8,
        retries: r.below(4) as u8,
        max_pending: r.pick(&[32, 32, 2]),
        requests,
    }
}

/// Whether the case is free of anything that may legitimately fail a request.
fn benign(c: &Case) -> bool {
    let e = &c.ecu;
    e.faults.is_empty()
        && e.break_on_request.is_none()
        && e.reply_delay_ms < TIMEOUT_MS
        && (e.response_pending == 0 || e.pending_interval_ms < 5000)
        && e.response_pending <= c.max_pending
}

/// What a batch of cases exercised.
#[derive(Debug, Default)]
struct Coverage {
    requests: u64,
    ok: u64,
    errors: u64,
    faults_fired: u64,
    retransmissions: u64,
    module_retransmissions: u64,
    gaps: u64,
    duplicates: u64,
    implicit_acks: u64,
    reopens: u64,
    pending: u64,
    breaks_received: u64,
}

fn run_case(seed: u64) -> Coverage {
    let c = gen_case(seed);
    let ecu = Tp20Ecu::new(c.ecu.clone(), Box::new(app)).shared();
    let replay = env_u64("OPENTUNE_TP20_FUZZ_SEED").is_some();
    ecu.lock().unwrap().record_wire = replay;
    let mut tp = VwTp20::new(Box::new(Tp20SimLink::new(ecu.clone())), 0x01, 0x300);
    tp.block_size = c.client_bs;
    tp.max_block_retries = c.retries;
    tp.max_response_pending = c.max_pending;
    let mut sent = Vec::new();
    let mut ok = 0usize;
    for q in &c.requests {
        let t0 = Instant::now();
        let r = tp.request(q, TIMEOUT_MS);
        let took = t0.elapsed();
        if replay {
            let mut g = ecu.lock().unwrap();
            for (t, dir, id, d) in g.wire.drain(..) {
                let arrow = if dir == FaultDir::ToEcu { "->" } else { "<-" };
                eprintln!("{t:>6} {arrow} {id:03X} {d:02X?}");
            }
            eprintln!(
                "request {:02X?}: {:?}",
                &q[..5],
                r.as_ref().map(|v| v.len())
            );
        }
        assert!(took < REQUEST_BOUND, "request took {took:?}");
        sent.push(q.clone());
        match r {
            Ok(resp) => {
                assert_eq!(resp, app(q), "wrong payload for request {:02X?}", &q[..5]);
                ok += 1;
            }
            Err(Error::Protocol(m)) => assert!(!m.is_empty()),
            Err(Error::Timeout(_)) => {}
            Err(e) => panic!("untyped/unexpected error class: {e:?}"),
        }
    }
    let g = ecu.lock().unwrap();
    // Executed at most once each, in order, and only what was sent.
    let mut it = sent.iter();
    for (i, x) in g.requests.iter().enumerate() {
        assert!(
            it.any(|s| s == x),
            "module request #{i} {:02X?} was not sent, is duplicated or out of order",
            &x[..x.len().min(5)]
        );
    }
    if benign(&c) {
        assert_eq!(ok, c.requests.len(), "a benign case failed: {:?}", tp.stats);
        assert!(g.errors.is_empty(), "{:?}", g.errors);
    }
    let s = &tp.stats;
    Coverage {
        requests: c.requests.len() as u64,
        ok: ok as u64,
        errors: (c.requests.len() - ok) as u64,
        faults_fired: g.faults_fired as u64,
        retransmissions: s.retransmissions as u64,
        module_retransmissions: g.retransmissions as u64,
        gaps: s.gaps as u64,
        duplicates: s.duplicates as u64,
        implicit_acks: s.implicit_acks as u64,
        reopens: s.reopens as u64,
        pending: s.response_pending as u64,
        breaks_received: s.breaks_received as u64,
    }
}

fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok())
}

#[test]
fn tp20_fault_fuzz() {
    if let Some(seed) = env_u64("OPENTUNE_TP20_FUZZ_SEED") {
        eprintln!("{:#?}", gen_case(seed).ecu);
        eprintln!("{:?}", run_case(seed));
        return;
    }
    let cases = env_u64("OPENTUNE_TP20_FUZZ_CASES").unwrap_or(DEFAULT_CASES);
    let t0 = Instant::now();
    let mut failed = Vec::new();
    let mut cov = Coverage::default();
    for seed in 0..cases {
        let s = seed.wrapping_mul(0x2545_F491_4F6C_DD1D) ^ 0x0D15_EA5E;
        match catch_unwind(AssertUnwindSafe(|| run_case(s))) {
            Ok(c) => {
                cov.requests += c.requests;
                cov.ok += c.ok;
                cov.errors += c.errors;
                cov.faults_fired += c.faults_fired;
                cov.retransmissions += c.retransmissions;
                cov.module_retransmissions += c.module_retransmissions;
                cov.gaps += c.gaps;
                cov.duplicates += c.duplicates;
                cov.implicit_acks += c.implicit_acks;
                cov.reopens += c.reopens;
                cov.pending += c.pending;
                cov.breaks_received += c.breaks_received;
            }
            Err(_) => {
                failed.push(s);
                if failed.len() >= 5 {
                    break;
                }
            }
        }
    }
    assert!(
        failed.is_empty(),
        "failing seeds (replay with OPENTUNE_TP20_FUZZ_SEED=<seed>): {failed:?}"
    );
    eprintln!("tp20 fuzz: {cases} cases in {:?}: {cov:?}", t0.elapsed());
    // Every recovery path is actually reached (not just the happy path).
    if cases >= DEFAULT_CASES {
        for (name, n) in [
            ("failed requests", cov.errors),
            ("faults fired", cov.faults_fired),
            ("our retransmissions", cov.retransmissions),
            ("module retransmissions", cov.module_retransmissions),
            ("receive gaps", cov.gaps),
            ("duplicates", cov.duplicates),
            ("implicit ACKs", cov.implicit_acks),
            ("re-opens", cov.reopens),
            ("responsePending", cov.pending),
            ("module breaks", cov.breaks_received),
        ] {
            assert!(n >= 20, "fuzz reached {name} only {n} times: {cov:?}");
        }
        assert!(cov.ok * 2 > cov.requests, "most requests succeed: {cov:?}");
    }
}

#[test]
fn generator_covers_the_fault_space() {
    // Guard against a generator change silently dropping coverage.
    let mut seen = std::collections::HashSet::new();
    let mut benign_cases = 0;
    for seed in 0..DEFAULT_CASES {
        let c = gen_case(seed.wrapping_mul(0x2545_F491_4F6C_DD1D) ^ 0x0D15_EA5E);
        benign_cases += benign(&c) as u32;
        for f in &c.ecu.faults {
            seen.insert(format!("{:?}", std::mem::discriminant(&f.action)));
        }
    }
    assert_eq!(seen.len(), 6, "every fault action is drawn");
    assert!(benign_cases > 100, "{benign_cases}");
}
