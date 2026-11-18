//! VW TP2.0 under scripted faults: the hardened `VwTp20` client against the
//! fault-injecting `tp20sim` module. Each test names one recovery rule from
//! `docs/PROTOCOL-NOTES.md` ("TP2.0 hardening"). The module's clock is
//! virtual, so delays and timeouts here cost no wall-clock time.
use opentune_core::protocol::diag::DiagTransport;
use opentune_core::protocol::tp20channel::{CanDiag, Tp20Channel, Tp20Options};
use opentune_core::protocol::tp20sim::{
    Fault, FaultAction, FaultDir, FrameClass, Tp20Ecu, Tp20EcuConfig, Tp20SimLink,
};
use opentune_core::protocol::vwtp20::VwTp20;
use opentune_core::{ConnectConfig, Error};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const ADDR: u8 = 0x01;
const RX: u32 = 0x300;
const TESTER_TX: u32 = 0x740;
const T: u64 = 500;

/// `21 <tag hi> <tag lo> <len hi> <len lo> …` → `61 <tag hi> <tag lo>` and
/// `len` bytes derived from the tag.
fn app(req: &[u8]) -> Vec<u8> {
    match req {
        [0x21, th, tl, lh, ll, ..] => {
            let tag = u16::from_be_bytes([*th, *tl]);
            let n = u16::from_be_bytes([*lh, *ll]) as usize;
            let mut r = vec![0x61, *th, *tl];
            r.extend((0..n).map(|i| (i as u16).wrapping_mul(7).wrapping_add(tag) as u8));
            r
        }
        [0x22, ..] => vec![0x7F, 0x10, 0x78], // a responsePending for another service
        [sid, ..] => vec![0x7F, *sid, 0x11],
        [] => vec![0x7F, 0x00, 0x13],
    }
}

/// A request with a unique `tag`, `extra` filler bytes, asking for `reply`
/// data bytes.
fn req(tag: u16, extra: usize, reply: u16) -> Vec<u8> {
    let mut r = vec![0x21];
    r.extend(tag.to_be_bytes());
    r.extend(reply.to_be_bytes());
    r.extend((0..extra).map(|i| i as u8));
    r
}

fn ecu(cfg: Tp20EcuConfig) -> Arc<Mutex<Tp20Ecu>> {
    Tp20Ecu::new(
        Tp20EcuConfig {
            addr: ADDR,
            tester_tx_id: TESTER_TX,
            ..cfg
        },
        Box::new(app),
    )
    .shared()
}

fn faults(f: &[(FaultDir, FrameClass, usize, FaultAction)]) -> Vec<Fault> {
    f.iter()
        .map(|&(d, c, n, a)| Fault::new(d, c, n, a))
        .collect()
}

fn client(e: &Arc<Mutex<Tp20Ecu>>) -> VwTp20<Tp20SimLink> {
    VwTp20::new(Box::new(Tp20SimLink::new(e.clone())), ADDR, RX)
}

fn ok(tp: &mut VwTp20<Tp20SimLink>, r: &[u8]) {
    assert_eq!(tp.request(r, T).unwrap(), app(r), "request {r:02X?}");
}

/// Tester data opcodes (high nibble) sent on the channel, in order.
fn tester_ops(e: &Arc<Mutex<Tp20Ecu>>) -> Vec<u8> {
    e.lock()
        .unwrap()
        .log
        .iter()
        .filter(|(id, d)| *id == TESTER_TX && d.len() > 1 && d[0] < 0x40)
        .map(|(_, d)| d[0])
        .collect()
}

#[test]
fn lost_ack_with_a_late_reply_is_retransmitted_once_and_executed_once() {
    // The module's ACK of our one-frame request is lost and its reply comes
    // 300 ms later: we break (A4) and resend; it re-ACKs the duplicate.
    let e = ecu(Tp20EcuConfig {
        reply_delay_ms: 300,
        faults: faults(&[(FaultDir::ToTester, FrameClass::Ack, 0, FaultAction::Drop)]),
        ..Tp20EcuConfig::default()
    });
    let mut tp = client(&e);
    ok(&mut tp, &req(1, 0, 4));
    assert_eq!(tp.stats.retransmissions, 1);
    let g = e.lock().unwrap();
    assert_eq!(g.requests.len(), 1, "executed exactly once");
    assert_eq!(g.breaks_received, 1);
    assert!(g.duplicates >= 1);
    let a4 = g.log.iter().filter(|(_, d)| d == &vec![0xA4]).count();
    assert_eq!(a4, 1);
}

#[test]
fn reply_after_a_lost_ack_is_the_implicit_ack() {
    let e = ecu(Tp20EcuConfig {
        faults: faults(&[(FaultDir::ToTester, FrameClass::Ack, 0, FaultAction::Drop)]),
        ..Tp20EcuConfig::default()
    });
    let mut tp = client(&e);
    ok(&mut tp, &req(2, 0, 30));
    assert_eq!(tp.stats.implicit_acks, 1);
    assert_eq!(tp.stats.retransmissions, 0);
}

#[test]
fn module_resends_a_block_whose_ack_we_lost() {
    // Our ACK of the module's first reply block is lost: after T1 it breaks
    // and resends; we discard the replay and ACK it again.
    let e = ecu(Tp20EcuConfig {
        faults: faults(&[(FaultDir::ToEcu, FrameClass::Ack, 0, FaultAction::Drop)]),
        ..Tp20EcuConfig::default()
    });
    let mut tp = client(&e);
    tp.block_size = 3; // the module ACK-gates its reply every 3 frames
    ok(&mut tp, &req(3, 0, 60));
    assert_eq!(tp.stats.breaks_received, 1);
    assert!(tp.stats.duplicates >= 3, "{:?}", tp.stats);
    let g = e.lock().unwrap();
    assert_eq!(g.retransmissions, 1);
    assert_eq!(g.gave_up, 0);
}

#[test]
fn module_resends_its_last_block_after_the_reply_was_delivered() {
    // Our ACK of the module's *last* frame is lost; we have already returned
    // the reply. Its A4 + replay arrives during the next exchange and is
    // re-ACKed without disturbing that exchange.
    let e = ecu(Tp20EcuConfig {
        reply_delay_ms: 50,
        faults: faults(&[(FaultDir::ToEcu, FrameClass::Ack, 0, FaultAction::Drop)]),
        ..Tp20EcuConfig::default()
    });
    let mut tp = client(&e);
    ok(&mut tp, &req(4, 0, 2));
    ok(&mut tp, &req(5, 0, 2));
    ok(&mut tp, &req(6, 0, 2));
    let g = e.lock().unwrap();
    assert_eq!(g.requests.len(), 3);
    assert_eq!(g.gave_up, 0);
}

#[test]
fn lost_reply_frame_is_recovered_by_the_module_break() {
    for nth in [1, 2, 5] {
        let e = ecu(Tp20EcuConfig {
            faults: faults(&[(FaultDir::ToTester, FrameClass::Data, nth, FaultAction::Drop)]),
            ..Tp20EcuConfig::default()
        });
        let mut tp = client(&e);
        tp.block_size = 4;
        ok(&mut tp, &req(10 + nth as u16, 0, 70));
        assert_eq!(tp.stats.gaps, 1, "nth {nth}: {:?}", tp.stats);
        assert_eq!(e.lock().unwrap().retransmissions, 1);
    }
}

#[test]
fn lost_request_frame_is_recovered_by_our_break() {
    // Our second request frame vanishes: the module never ACKs the block, we
    // break and resend it, and the module executes the request once.
    let e = ecu(Tp20EcuConfig {
        block_size: 3,
        faults: faults(&[(FaultDir::ToEcu, FrameClass::Data, 1, FaultAction::Drop)]),
        ..Tp20EcuConfig::default()
    });
    let mut tp = client(&e);
    let r = req(20, 40, 5);
    ok(&mut tp, &r);
    assert_eq!(tp.stats.retransmissions, 1);
    let g = e.lock().unwrap();
    assert_eq!(g.requests, vec![r]);
    assert_eq!(g.gaps, 1);
}

#[test]
fn duplicated_and_reordered_frames_never_duplicate_a_payload() {
    let script = [
        (
            FaultDir::ToTester,
            FrameClass::Data,
            1,
            FaultAction::Duplicate,
        ),
        (
            FaultDir::ToTester,
            FrameClass::Data,
            4,
            FaultAction::Reorder,
        ),
        (FaultDir::ToEcu, FrameClass::Data, 2, FaultAction::Duplicate),
        (FaultDir::ToEcu, FrameClass::Data, 5, FaultAction::Reorder),
        (
            FaultDir::ToTester,
            FrameClass::Ack,
            1,
            FaultAction::Duplicate,
        ),
        (FaultDir::ToEcu, FrameClass::Ack, 1, FaultAction::Duplicate),
    ];
    let e = ecu(Tp20EcuConfig {
        block_size: 4,
        faults: faults(&script),
        ..Tp20EcuConfig::default()
    });
    let mut tp = client(&e);
    tp.block_size = 5;
    let reqs: Vec<_> = (0..4).map(|i| req(30 + i, 25, 40)).collect();
    for r in &reqs {
        ok(&mut tp, r);
    }
    let g = e.lock().unwrap();
    assert_eq!(g.requests, reqs, "each request executed once, in order");
    assert_eq!(g.faults_fired, script.len());
}

#[test]
fn wrong_sequence_in_an_unacknowledged_reply_is_an_error_then_reopens() {
    let e = ecu(Tp20EcuConfig {
        last_without_ack: true,
        faults: faults(&[(
            FaultDir::ToTester,
            FrameClass::Data,
            2,
            FaultAction::WrongSeq(3),
        )]),
        ..Tp20EcuConfig::default()
    });
    let mut tp = client(&e);
    let err = tp.request(&req(40, 0, 30), T).unwrap_err();
    assert!(
        matches!(&err, Error::Protocol(m) if m.contains("sequence error")),
        "{err}"
    );
    assert!(!tp.is_open());
    assert!(tp.last_break().unwrap().contains("sequence error"));
    // The next request sets up a fresh channel and gets its own answer.
    ok(&mut tp, &req(41, 0, 30));
    assert_eq!(tp.stats.reopens, 1);
    assert_eq!(e.lock().unwrap().setups, 2);
}

#[test]
fn sequence_numbers_wrap_in_long_telegrams_both_ways() {
    for (ecu_bs, our_bs) in [(15, 15), (4, 7), (0, 1), (1, 0x0F)] {
        let e = ecu(Tp20EcuConfig {
            block_size: ecu_bs,
            ..Tp20EcuConfig::default()
        });
        let mut tp = client(&e);
        tp.block_size = our_bs;
        // ~45 request frames and ~90 reply frames: several wraps each way.
        for i in 0..3 {
            ok(&mut tp, &req(50 + i, 300, 600));
        }
        let g = e.lock().unwrap();
        assert!(g.errors.is_empty(), "bs {ecu_bs}/{our_bs}: {:?}", g.errors);
        assert_eq!(g.requests.len(), 3);
    }
}

#[test]
fn module_block_size_is_honoured_and_clamped() {
    // ACK_NOW (0x0_) every `bs` frames. A longer block could not be
    // retransmitted unambiguously with a 4-bit sequence, so 0 ("ACK only the
    // last frame") and 0x20 are both treated as 15.
    for (bs, gate) in [
        (0u8, Some(15)),
        (1, Some(1)),
        (3, Some(3)),
        (15, Some(15)),
        (0x20, Some(15)),
    ] {
        let e = ecu(Tp20EcuConfig {
            block_size: bs,
            ..Tp20EcuConfig::default()
        });
        let mut tp = client(&e);
        ok(&mut tp, &req(60, 200, 1)); // 207-byte payload = 30 frames
        let ops = tester_ops(&e);
        assert_eq!(ops.len(), 30);
        for (i, op) in ops.iter().enumerate() {
            let want = if i == 29 {
                0x10
            } else if gate.is_some_and(|g: usize| (i + 1) % g == 0) {
                0x00
            } else {
                0x20
            };
            assert_eq!(op & 0xF0, want, "bs {bs:#x} frame {i}");
        }
        assert!(e.lock().unwrap().errors.is_empty());
    }
}

#[test]
fn timing_parameter_variants_stay_bounded() {
    // T1 = 0 (use the floor), T1 = 6.3 s (bounded by the request timeout),
    // T3 = 6.3 s (separation clamped to 100 ms).
    for (t1, t3) in [(0x00, 0x00), (0xFF, 0x00), (0x0A, 0x00), (0x8A, 0xFF)] {
        let e = ecu(Tp20EcuConfig {
            t1,
            t3,
            block_size: 0,
            faults: faults(&[(FaultDir::ToTester, FrameClass::Ack, 0, FaultAction::Drop)]),
            reply_delay_ms: 2000,
            ..Tp20EcuConfig::default()
        });
        let mut tp = client(&e);
        let t0 = Instant::now();
        // 3 frames: two separations, then the ACK of the last one is lost.
        let r = tp.request(&req(70, 12, 1), T);
        assert!(r.is_err(), "the reply comes after the timeout");
        assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
        assert_eq!(tp.t1, t1);
        assert_eq!(tp.st_min, t3);
    }
}

#[test]
fn response_pending_chain_is_waited_through_with_the_extended_timeout() {
    let e = ecu(Tp20EcuConfig {
        response_pending: 5,
        pending_interval_ms: 2000, // longer than the request timeout
        ..Tp20EcuConfig::default()
    });
    let mut tp = client(&e);
    ok(&mut tp, &req(80, 0, 8));
    assert_eq!(tp.stats.response_pending, 5);
    assert_eq!(e.lock().unwrap().pending_sent, 5);
}

#[test]
fn response_pending_chain_is_bounded() {
    let e = ecu(Tp20EcuConfig {
        response_pending: 10,
        ..Tp20EcuConfig::default()
    });
    let mut tp = client(&e);
    tp.max_response_pending = 3;
    let err = tp.request(&req(81, 0, 1), T).unwrap_err().to_string();
    assert!(
        err.contains("responsePending") && err.contains("limit 3"),
        "{err}"
    );
}

#[test]
fn response_pending_for_another_service_is_not_waited_on() {
    let e = ecu(Tp20EcuConfig::default());
    let mut tp = client(&e);
    // 0x22 is answered with 7F 10 78: not ours, handed back as-is.
    assert_eq!(
        tp.request(&[0x22, 0x01], T).unwrap(),
        vec![0x7F, 0x10, 0x78]
    );
}

#[test]
fn module_disconnect_mid_session_reopens_on_the_next_request() {
    let e = ecu(Tp20EcuConfig {
        break_on_request: Some(1),
        ..Tp20EcuConfig::default()
    });
    let mut tp = client(&e);
    ok(&mut tp, &req(90, 0, 3));
    let err = tp.request(&req(91, 0, 3), T).unwrap_err().to_string();
    assert!(err.contains("closed the TP2.0 channel (A8)"), "{err}");
    assert!(!tp.is_open());
    ok(&mut tp, &req(92, 0, 3));
    assert_eq!(tp.stats.reopens, 1);
    assert_eq!(e.lock().unwrap().setups, 2);
}

#[test]
fn module_disconnect_in_the_middle_of_a_reply() {
    let e = ecu(Tp20EcuConfig {
        faults: faults(&[(
            FaultDir::ToTester,
            FrameClass::Data,
            3,
            FaultAction::Disconnect,
        )]),
        ..Tp20EcuConfig::default()
    });
    let mut tp = client(&e);
    assert!(tp.request(&req(93, 0, 80), T).is_err());
    ok(&mut tp, &req(94, 0, 80));
}

#[test]
fn module_channel_tests_are_answered() {
    let e = ecu(Tp20EcuConfig {
        channel_test_before_reply: true,
        ..Tp20EcuConfig::default()
    });
    let mut tp = client(&e);
    for i in 0..3 {
        ok(&mut tp, &req(100 + i, 0, 20));
    }
    let g = e.lock().unwrap();
    assert_eq!(g.channel_tests_sent, 3);
    assert_eq!(g.channel_test_replies, 3);
}

#[test]
fn late_reply_after_a_timeout_is_never_taken_for_the_next_one() {
    // The first reply comes after our timeout. The channel is torn down, so
    // the second request (on a new channel) gets its own reply.
    let e = ecu(Tp20EcuConfig {
        reply_delay_ms: 2 * T,
        ..Tp20EcuConfig::default()
    });
    let mut tp = client(&e);
    assert!(matches!(
        tp.request(&req(110, 0, 4), T),
        Err(Error::Timeout(_))
    ));
    e.lock().unwrap().cfg.reply_delay_ms = 0;
    ok(&mut tp, &req(111, 0, 4));
}

#[test]
fn unanswered_block_gives_up_after_the_retries() {
    for retries in [0u8, 1, 3] {
        let e = ecu(Tp20EcuConfig::default());
        let mut tp = client(&e);
        tp.max_block_retries = retries;
        tp.open(T).unwrap();
        e.lock().unwrap().cfg.silent = true;
        let t0 = Instant::now();
        let err = tp.request(&req(120, 0, 1), T).unwrap_err().to_string();
        assert!(err.contains("no ACK"), "{err}");
        assert!(t0.elapsed() < Duration::from_secs(1));
        let g = e.lock().unwrap();
        let a4 = g.log.iter().filter(|(_, d)| d == &vec![0xA4]).count();
        assert_eq!(a4, retries as usize);
        // Torn down with an A8 after giving up.
        assert_eq!(g.log.last().unwrap().1, vec![0xA8]);
    }
}

#[test]
fn module_gives_up_when_we_never_ack() {
    // Every one of our ACKs is lost: the module retransmits twice, then
    // closes the channel; we report the break and recover on the next request.
    let mut script = Vec::new();
    for n in 0..3 {
        script.push((FaultDir::ToEcu, FrameClass::Ack, n, FaultAction::Drop));
    }
    let e = ecu(Tp20EcuConfig {
        faults: faults(&script),
        ..Tp20EcuConfig::default()
    });
    let mut tp = client(&e);
    tp.block_size = 2;
    let err = tp.request(&req(130, 0, 40), T).unwrap_err().to_string();
    assert!(err.contains("A8"), "{err}");
    assert_eq!(e.lock().unwrap().gave_up, 1);
    ok(&mut tp, &req(131, 0, 40));
}

#[test]
fn lost_setup_or_parameter_response_is_a_named_error() {
    for (class, step) in [
        (FrameClass::Setup, "step 1"),
        (FrameClass::Params, "step 2"),
    ] {
        let e = ecu(Tp20EcuConfig {
            faults: faults(&[(FaultDir::ToTester, class, 0, FaultAction::Drop)]),
            ..Tp20EcuConfig::default()
        });
        let mut tp = client(&e);
        let err = tp.request(&req(140, 0, 1), T).unwrap_err().to_string();
        assert!(err.contains(step), "{err}");
        // Retrying opens the channel.
        ok(&mut tp, &req(141, 0, 1));
    }
}

#[test]
fn oversized_and_empty_requests_are_refused_before_the_bus() {
    let e = ecu(Tp20EcuConfig::default());
    let mut tp = client(&e);
    assert!(matches!(tp.request(&[], T), Err(Error::Protocol(_))));
    assert!(matches!(
        tp.request(&vec![0x21; 0x1000], T),
        Err(Error::Protocol(_))
    ));
    assert!(e.lock().unwrap().log.is_empty());
    // The 12-bit maximum itself goes through.
    let mut big = req(150, 0x0FFF - 5, 1);
    big.truncate(0x0FFF);
    ok(&mut tp, &big);
}

// ---- Tp20Channel (the driver-facing channel with the keep-alive thread) ----

fn channel(e: &Arc<Mutex<Tp20Ecu>>, keepalive_ms: u64) -> Tp20Channel {
    Tp20Channel::open(
        Box::new(Tp20SimLink::new(e.clone())),
        &Tp20Options {
            keepalive_ms,
            timeout_ms: 200,
            ..Tp20Options::new(ADDR)
        },
    )
    .unwrap()
}

#[test]
fn keep_alive_survives_one_lost_answer() {
    // nth 0 is the A1 answering our A0; nth 1 answers the first keep-alive.
    let e = ecu(Tp20EcuConfig {
        faults: faults(&[(FaultDir::ToTester, FrameClass::Params, 1, FaultAction::Drop)]),
        ..Tp20EcuConfig::default()
    });
    let mut ch = channel(&e, 20);
    std::thread::sleep(Duration::from_millis(200));
    assert!(ch.is_open());
    assert!(ch.keep_alives() >= 2, "{}", ch.keep_alives());
    assert_eq!(
        ch.request(&req(160, 0, 2), T).unwrap(),
        app(&req(160, 0, 2))
    );
    // Keep-alives are spaced by the idle interval, never back to back.
    let g = e.lock().unwrap();
    let gaps: Vec<_> = g.keep_alive_times.windows(2).map(|w| w[1] - w[0]).collect();
    // The retry after the lost answer is immediate; the others wait 20 ms.
    assert!(
        gaps.iter()
            .filter(|d| **d >= Duration::from_millis(15))
            .count()
            >= gaps.len() - 1
    );
}

#[test]
fn channel_reopens_after_a_break_and_after_reopen() {
    let e = ecu(Tp20EcuConfig {
        break_on_request: Some(0),
        ..Tp20EcuConfig::default()
    });
    let mut ch = channel(&e, 0);
    assert!(ch.request(&req(170, 0, 2), T).is_err());
    assert_eq!(
        ch.request(&req(171, 0, 2), T).unwrap(),
        app(&req(171, 0, 2))
    );
    assert_eq!(ch.reopens(), 1);
    // A lost channel (keep-alive failed) needs an explicit reopen.
    let e2 = ecu(Tp20EcuConfig::default());
    let mut ch2 = channel(&e2, 20);
    e2.lock().unwrap().cfg.silent = true;
    std::thread::sleep(Duration::from_millis(600));
    let err = ch2.request(&req(172, 0, 2), T).unwrap_err().to_string();
    assert!(
        err.contains("keep-alive failed") && err.contains("reopen"),
        "{err}"
    );
    e2.lock().unwrap().cfg.silent = false;
    ch2.reopen().unwrap();
    assert!(ch2.is_open());
    assert_eq!(
        ch2.request(&req(173, 0, 2), T).unwrap(),
        app(&req(173, 0, 2))
    );
    assert!(ch2.stats().reopens >= 1);
}

#[test]
fn ack_retries_option_is_parsed_and_bounded() {
    let mut c = ConnectConfig::default();
    c.options.insert("transport".into(), "tp20".into());
    c.options.insert("tp20_addr".into(), "1".into());
    c.options.insert("tp20_ack_retries".into(), "0".into());
    let o = Tp20Options::from_config(&c).unwrap().unwrap();
    assert_eq!(o.ack_retries, 0);
    c.options.insert("tp20_ack_retries".into(), "6".into());
    assert!(matches!(
        Tp20Options::from_config(&c),
        Err(Error::Config(_))
    ));
    c.options.insert("tp20_ack_retries".into(), "1".into());
    c.options.insert("tp20_keepalive_ms".into(), "0".into());
    let e = ecu(Tp20EcuConfig::default());
    let d = CanDiag::open(Box::new(Tp20SimLink::new(e.clone())), &c).unwrap();
    assert!(d.describe().contains("TP2.0"));
}
