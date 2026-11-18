//! VW TP2.0 channels as the raw-CAN drivers open them ([`CanDiag::open`] with
//! `transport=tp20`), against the simulated module in `protocol::tp20sim`:
//! channel setup, parameter negotiation, multi-frame KWP2000 replies with ACK
//! windows, keep-alive during a pause, disconnect, and a silent module.
use opentune_core::protocol::diag::DiagTransport;
use opentune_core::protocol::isotp::CanLink;
use opentune_core::protocol::tp20channel::{CanDiag, CanTransportKind, Tp20Channel, Tp20Options};
use opentune_core::protocol::tp20sim::{Tp20Ecu, Tp20EcuConfig, Tp20SimLink};
use opentune_core::{ConnectConfig, Error, Result};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const ADDR: u8 = 0x0A;
const TESTER_TX: u32 = 0x764;

/// A small synthetic KWP2000 application.
fn kwp(req: &[u8]) -> Vec<u8> {
    match req {
        [0x10, s] => vec![0x50, *s],
        [0x1A, 0x9B] => {
            let mut r = vec![0x5A, 0x9B];
            r.extend((0u8..40).map(|i| b'A' + i % 26));
            r
        }
        [0x21, g] => {
            let mut r = vec![0x61, *g];
            r.extend_from_slice(&[0x1A, 0, 45, 0x1A, 0, 52, 0x06, 100, 138, 0x25, 0, 1]);
            r
        }
        [0x3B, rest @ ..] => vec![0x7B, rest.first().copied().unwrap_or(0)],
        [sid, ..] => vec![0x7F, *sid, 0x11],
        [] => vec![0x7F, 0x00, 0x13],
    }
}

fn ecu(cfg: Tp20EcuConfig) -> Arc<Mutex<Tp20Ecu>> {
    Tp20Ecu::new(cfg, Box::new(kwp)).shared()
}

fn ecu_cfg() -> Tp20EcuConfig {
    Tp20EcuConfig {
        addr: ADDR,
        tester_tx_id: TESTER_TX,
        ..Tp20EcuConfig::default()
    }
}

fn tp20_cfg(extra: &[(&str, &str)]) -> ConnectConfig {
    let mut c = ConnectConfig::default();
    c.options.insert("transport".into(), "tp20".into());
    c.options.insert("tp20_addr".into(), "0x0A".into());
    c.options.insert("tp20_keepalive_ms".into(), "0".into());
    c.options.insert("tp20_timeout_ms".into(), "300".into());
    for (k, v) in extra {
        c.options.insert((*k).into(), (*v).into());
    }
    c
}

fn open(e: &Arc<Mutex<Tp20Ecu>>, extra: &[(&str, &str)]) -> Result<CanDiag> {
    CanDiag::open(Box::new(Tp20SimLink::new(e.clone())), &tp20_cfg(extra))
}

/// The single-byte ACKs (`0xB_`) the tester sent on its channel.
fn tester_acks(e: &Arc<Mutex<Tp20Ecu>>) -> Vec<u8> {
    e.lock()
        .unwrap()
        .log
        .iter()
        .filter(|(id, d)| *id == TESTER_TX && d.len() == 1 && d[0] & 0xF0 == 0xB0)
        .map(|(_, d)| d[0])
        .collect()
}

#[test]
fn channel_setup_and_parameter_negotiation() {
    let e = ecu(Tp20EcuConfig {
        block_size: 2,
        ..ecu_cfg()
    });
    let d = open(&e, &[]).unwrap();
    assert_eq!(d.kind(), CanTransportKind::Tp20);
    let desc = d.describe();
    assert!(desc.contains("0x0A") && desc.contains("0x764") && desc.contains("0x300"));
    let g = e.lock().unwrap();
    // Setup on 0x200: [addr, C0, 00, 10, rx 0x300 lo/hi, KWP application].
    assert_eq!(
        g.log[0],
        (0x200, vec![0x0A, 0xC0, 0x00, 0x10, 0x00, 0x03, 0x01])
    );
    // A0 on the assigned id: block size 15, T1 = 100 ms (0x8A).
    assert_eq!(
        g.log[1],
        (TESTER_TX, vec![0xA0, 0x0F, 0x8A, 0xFF, 0x00, 0xFF])
    );
    assert!(g.is_open());
    assert_eq!(g.ecu_tx_id(), 0x300);
}

#[test]
fn default_config_builds_isotp_and_sends_nothing() {
    let e = ecu(ecu_cfg());
    let d = CanDiag::open(
        Box::new(Tp20SimLink::new(e.clone())),
        &ConnectConfig::default(),
    )
    .unwrap();
    assert_eq!(d.kind(), CanTransportKind::IsoTp);
    assert!(e.lock().unwrap().log.is_empty());
}

#[test]
fn multi_frame_kwp_replies_with_ack_windows() {
    // We ask the module for blocks of 3; it asks us for blocks of 2.
    let e = ecu(Tp20EcuConfig {
        block_size: 2,
        ..ecu_cfg()
    });
    let mut d = open(&e, &[("tp20_block_size", "3")]).unwrap();

    // 0x1A 0x9B: 42-byte reply = 44-byte telegram = 7 frames, ACK-gated
    // after frames 3 and 6 and on the last one.
    let id = d.request(&[0x1A, 0x9B], 500).unwrap();
    assert_eq!(id.len(), 42);
    assert_eq!(&id[..2], &[0x5A, 0x9B]);
    assert_eq!(tester_acks(&e), vec![0xB3, 0xB6, 0xB7]);

    // 0x21: 14-byte reply = 3 frames (seq 7, 8, 9), one ACK at the end.
    let g1 = d.request(&[0x21, 0x01], 500).unwrap();
    assert_eq!(&g1[..3], &[0x61, 0x01, 0x1A]);
    assert_eq!(*tester_acks(&e).last().unwrap(), 0xBA);

    // A 20-byte request: 4 frames, we wait for the module's ACK after 2.
    let long: Vec<u8> = std::iter::once(0x3B).chain(1..20).collect();
    assert_eq!(d.request(&long, 500).unwrap(), vec![0x7B, 1]);
    let g = e.lock().unwrap();
    assert!(g.errors.is_empty(), "{:?}", g.errors);
    assert_eq!(g.requests.len(), 3);
    assert_eq!(g.requests[2], long);
    let ops: Vec<u8> = g
        .log
        .iter()
        .filter(|(id, d)| *id == TESTER_TX && d.len() > 1 && d[0] < 0x40)
        .map(|(_, d)| d[0])
        .collect();
    // request 1 (seq 0), request 2 (seq 1), then MORE|2, ACK_NOW|3, MORE|4, LAST|5.
    assert_eq!(ops, vec![0x10, 0x11, 0x22, 0x03, 0x24, 0x15]);
}

#[test]
fn not_ready_ack_and_unacknowledged_last_frame() {
    let e = ecu(Tp20EcuConfig {
        ack_not_ready_first: true,
        last_without_ack: true,
        ..ecu_cfg()
    });
    let mut d = open(&e, &[]).unwrap();
    assert_eq!(d.request(&[0x10, 0x89], 500).unwrap(), vec![0x50, 0x89]);
    assert_eq!(d.request(&[0x1A, 0x9B], 500).unwrap().len(), 42);
    // A 0x3 last frame is not acknowledged.
    assert!(tester_acks(&e).is_empty());
    assert!(e.lock().unwrap().errors.is_empty());
}

#[test]
fn keep_alive_holds_the_channel_through_a_pause() {
    let e = ecu(Tp20EcuConfig {
        idle_timeout: Some(Duration::from_millis(400)),
        ..ecu_cfg()
    });
    let mut d = open(&e, &[("tp20_keepalive_ms", "50")]).unwrap();
    d.request(&[0x10, 0x89], 500).unwrap();
    std::thread::sleep(Duration::from_millis(1200));
    // Three idle-timeouts later the channel still answers.
    assert_eq!(d.request(&[0x21, 0x03], 500).unwrap()[0], 0x61);
    let CanDiag::Tp20(ch) = &d else {
        panic!("not a TP2.0 channel")
    };
    assert!(ch.keep_alives() >= 3, "{}", ch.keep_alives());
    let g = e.lock().unwrap();
    assert!(g.keep_alives >= 3);
    assert_eq!(g.setups, 1, "the channel was never re-opened");
}

#[test]
fn without_keep_alive_an_idle_channel_times_out() {
    let e = ecu(Tp20EcuConfig {
        idle_timeout: Some(Duration::from_millis(150)),
        ..ecu_cfg()
    });
    let mut d = open(&e, &[]).unwrap();
    d.request(&[0x10, 0x89], 500).unwrap();
    std::thread::sleep(Duration::from_millis(400));
    assert!(d.request(&[0x21, 0x01], 200).is_err());
    assert_eq!(e.lock().unwrap().keep_alives, 0);
}

#[test]
fn close_stops_the_keep_alive_and_disconnects() {
    let e = ecu(ecu_cfg());
    let mut d = open(&e, &[("tp20_keepalive_ms", "20")]).unwrap();
    d.request(&[0x10, 0x89], 500).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    d.close();
    let after_close = {
        let g = e.lock().unwrap();
        assert_eq!(g.disconnects, 1);
        assert_eq!(g.log.last().unwrap(), &(TESTER_TX, vec![0xA8]));
        assert!(!g.is_open());
        g.keep_alives
    };
    assert!(after_close >= 1);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(e.lock().unwrap().keep_alives, after_close);
    // Closing twice (and dropping) sends nothing more.
    d.close();
    drop(d);
    assert_eq!(e.lock().unwrap().disconnects, 1);
}

#[test]
fn dropping_the_channel_disconnects() {
    let e = ecu(ecu_cfg());
    drop(open(&e, &[("tp20_keepalive_ms", "20")]).unwrap());
    assert_eq!(e.lock().unwrap().disconnects, 1);
}

#[test]
fn silent_module_is_a_clear_error() {
    let e = ecu(Tp20EcuConfig {
        silent: true,
        ..ecu_cfg()
    });
    let msg = match open(&e, &[]) {
        Err(err) => err.to_string(),
        Ok(_) => panic!("a silent module opened a channel"),
    };
    for want in ["step 1", "0x0A", "0x20A", "0x200", "0x300", "300 ms"] {
        assert!(msg.contains(want), "missing {want:?} in: {msg}");
    }
}

#[test]
fn silent_module_on_a_busy_bus_still_times_out() {
    let e = ecu(Tp20EcuConfig {
        silent: true,
        ..ecu_cfg()
    });
    let link = Tp20SimLink::new(e.clone()).with_noise(0x2C0, &[0, 0, 0, 0, 0x50, 0, 0, 0]);
    let t0 = Instant::now();
    let r = CanDiag::open(Box::new(link), &tp20_cfg(&[]));
    assert!(matches!(r, Err(Error::Protocol(_))));
    assert!(t0.elapsed() < Duration::from_secs(3), "{:?}", t0.elapsed());
}

#[test]
fn unrelated_bus_traffic_is_ignored() {
    let e = ecu(Tp20EcuConfig {
        block_size: 2,
        ..ecu_cfg()
    });
    let link = Tp20SimLink::new(e.clone()).with_noise(0x2C0, &[0x10, 0x80, 0, 0, 0x50, 0, 0, 0]);
    let mut d = CanDiag::open(Box::new(link), &tp20_cfg(&[("tp20_block_size", "3")])).unwrap();
    assert_eq!(d.request(&[0x1A, 0x9B], 500).unwrap().len(), 42);
    assert!(e.lock().unwrap().errors.is_empty());
}

/// Passes frames through except the module's `A1` parameter responses.
struct DropA1(Tp20SimLink);

impl CanLink for DropA1 {
    fn send_frame(&mut self, id: u32, data: &[u8]) -> Result<()> {
        self.0.send_frame(id, data)
    }
    fn recv_frame(&mut self, timeout_ms: u64) -> Result<(u32, Vec<u8>)> {
        loop {
            let (id, d) = self.0.recv_frame(timeout_ms)?;
            if d.first() != Some(&0xA1) {
                return Ok((id, d));
            }
        }
    }
}

#[test]
fn missing_parameter_response_names_step_two() {
    let e = ecu(ecu_cfg());
    let r = Tp20Channel::open(
        Box::new(DropA1(Tp20SimLink::new(e.clone()))),
        &Tp20Options {
            keepalive_ms: 0,
            timeout_ms: 200,
            ..Tp20Options::new(ADDR)
        },
    );
    let msg = match r {
        Err(err) => err.to_string(),
        Ok(_) => panic!("opened without an A1"),
    };
    for want in ["step 2", "0x0A", "0x764", "0x300"] {
        assert!(msg.contains(want), "missing {want:?} in: {msg}");
    }
}

#[test]
fn failed_keep_alive_marks_the_channel_lost() {
    let e = ecu(ecu_cfg());
    let mut d = open(
        &e,
        &[("tp20_keepalive_ms", "30"), ("tp20_timeout_ms", "100")],
    )
    .unwrap();
    d.request(&[0x10, 0x89], 500).unwrap();
    // The module goes away (ignition off, unplugged).
    e.lock().unwrap().cfg.silent = true;
    std::thread::sleep(Duration::from_millis(300));
    let err = d.request(&[0x21, 0x01], 200).unwrap_err().to_string();
    assert!(err.contains("keep-alive failed"), "{err}");
    let CanDiag::Tp20(ch) = &d else {
        panic!("not a TP2.0 channel")
    };
    assert!(!ch.is_open());
}

#[test]
fn isotp_accessor_refuses_on_a_tp20_channel() {
    let e = ecu(ecu_cfg());
    let mut d = open(&e, &[]).unwrap();
    let Err(err) = d.isotp("slcan") else {
        panic!("a TP2.0 channel handed out an ISO-TP transport")
    };
    assert!(matches!(err, Error::Unsupported(_)));
    assert!(err.to_string().contains("KWP2000"));
}
