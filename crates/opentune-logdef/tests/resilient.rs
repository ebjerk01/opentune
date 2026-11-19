//! Live logging over an arbitrary transport: `LiveSession` (polling and
//! `0x2C` fast mode) over core `IsoTp` on the fault-injecting
//! `protocol::faultlink` bus, with the UDS simulator as the ECU. Covers the
//! quiet bus (nothing until queried), foreign traffic, slow `0x78` replies,
//! and `run_csv_resilient` through a dropped link: drop → reconnect →
//! re-define → resume, recovery without reopen, and giving up. Virtual time:
//! no real sleeps.

use opentune_core::ecudef::{DataType, Endian};
use opentune_core::protocol::faultlink::{FailKind, Fault, FaultBus, FaultLink, SimNode};
use opentune_core::protocol::IsoTp;
use opentune_core::resilient::{Clock, VirtualClock};
use opentune_core::{OwnedDiag, RawDiag};
use opentune_logdef::live::poll_once;
use opentune_logdef::{
    Addressing, Conversion, ConversionFit, EcuGroup, EndReason, FastLogConfig, LiveSession,
    LogDef, LogMode, LogParam, ProtocolGroup, ProtocolKind, ReconnectPolicy, RunLimits,
    RunOptions, SourceFormat, LINK_PARAM_ID,
};
use opentune_udssim::server::UdsServer;
use std::sync::{Arc, Mutex};

const MEM: u32 = 0x0010_0000;
const BASE_MS: u64 = 1_700_000_000_000;

fn param(id: &str, addressing: Addressing, factor: f64) -> LogParam {
    let protocol = match addressing {
        Addressing::Memory { .. } => ProtocolKind::EcuMode23,
        _ => ProtocolKind::UdsMode22,
    };
    LogParam {
        id: id.into(),
        name: id.into(),
        description: String::new(),
        units: "u".into(),
        protocol,
        addressing,
        storagetype: None,
        endian: Endian::Big,
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
    }
}

/// rpm (DID, ×0.25), timing (signed DID, ×0.1), load (2-byte memory word):
/// a fast-mode define of these is a multi-frame ISO-TP request.
fn def() -> LogDef {
    let mut timing = param("timing", Addressing::UdsDid { did: 0x1001 }, 0.1);
    timing.storagetype = Some(DataType::S16);
    let params = vec![
        param("rpm", Addressing::UdsDid { did: 0x1000 }, 0.25),
        timing,
        param(
            "load",
            Addressing::Memory {
                address: MEM,
                length: 2,
                bit: None,
            },
            1.0,
        ),
    ];
    LogDef {
        format: SourceFormat::RomRaiderLogger,
        version: None,
        convert_factors: Vec::new(),
        protocols: vec![ProtocolGroup {
            id: "uds".into(),
            kind: ProtocolKind::UdsMode22,
            baud: None,
            ecus: vec![EcuGroup {
                id: None,
                name: None,
                ecu_type: None,
                params,
            }],
        }],
        warnings: Vec::new(),
    }
}

fn server() -> Arc<Mutex<UdsServer>> {
    let mut s = UdsServer::new();
    s.set_did(0x1000, vec![0x1A, 0xF8]); // 6904 × 0.25 = 1726
    s.set_did(0x1001, vec![0xFF, 0x38]); // int16 -200 × 0.1 = -20
    s.set_memory(u64::from(MEM), vec![0x01, 0x2C, 0, 0]); // 300
    Arc::new(Mutex::new(s))
}

fn node(srv: &Arc<Mutex<UdsServer>>) -> SimNode {
    let srv = Arc::clone(srv);
    SimNode::new(
        0x7E0,
        0x7E8,
        Box::new(move |req| {
            let r = srv.lock().unwrap().handle(req);
            (!r.is_empty()).then_some(r)
        }),
    )
}

/// The embedder's wiring: its `CanLink` → core `IsoTp` → an owning `RawDiag`.
fn diag(link: FaultLink) -> OwnedDiag {
    OwnedDiag::new(Box::new(IsoTp::new(Box::new(link), 0x7E0, 0x7E8)), 50)
}

/// Passes requests through and injects `fault` on the bus after `after`
/// requests (so a test can kill the link between rounds).
struct Trigger {
    inner: Box<dyn RawDiag>,
    bus: FaultBus,
    after: u64,
    fault: Fault,
    n: u64,
}

impl RawDiag for Trigger {
    fn request(&mut self, req: &[u8]) -> Result<Vec<u8>, String> {
        if self.n == self.after {
            self.bus.fail(self.fault);
        }
        self.n += 1;
        self.inner.request(req)
    }
}

/// `(round, param_id, value, note, text)` per CSV row.
fn rows(csv: &str) -> Vec<(u64, String, String, String, String)> {
    let mut lines = csv.lines();
    assert_eq!(
        lines.next(),
        Some("round,param_id,name,raw,value,units,note,text,ts")
    );
    lines
        .map(|l| {
            let f: Vec<&str> = l.split(',').collect();
            assert_eq!(f.len(), 9, "row is not 9 fields: {l}");
            assert!(f[8].parse::<u64>().unwrap() >= BASE_MS, "ts: {l}");
            (
                f[0].parse().unwrap(),
                f[1].to_string(),
                f[4].to_string(),
                f[6].to_string(),
                f[7].to_string(),
            )
        })
        .collect()
}

fn markers(r: &[(u64, String, String, String, String)]) -> Vec<String> {
    r.iter()
        .filter(|x| x.1 == LINK_PARAM_ID)
        .map(|x| {
            assert!(x.2.is_empty(), "a marker row carries no value");
            x.4.clone()
        })
        .collect()
}

#[test]
fn polling_and_fast_over_isotp_on_a_quiet_noisy_slow_bus() {
    let srv = server();
    let bus = FaultBus::new()
        .with_node(node(&srv).with_pending(2))
        .with_noise(0x7E9, &[0x03, 0x7F, 0x22, 0x31])
        .with_noise(0x280, &[0x11; 8]);
    let mut d = diag(bus.link());
    let d = &mut d as &mut dyn RawDiag;

    let polled = poll_once(d, &def(), &[]);
    let values: Vec<Option<f64>> = polled.iter().map(|s| s.value).collect();
    assert_eq!(values, [Some(1726.0), Some(-20.0), Some(300.0)], "{polled:?}");

    let mut s = LiveSession::new(&def(), &[], FastLogConfig::fast()).unwrap();
    let st = s.start(d).clone();
    assert_eq!(st.mode, LogMode::Fast2c, "{:?}", st.reason);
    assert_eq!(st.dynamic_dids[0].channels.len(), 3);
    for _ in 0..3 {
        assert_eq!(s.tick(d), polled);
    }
    assert_eq!(s.status().tick_requests, 3, "one request per tick");
    assert_eq!(srv.lock().unwrap().dynamic_dids(), [0xF200]);
    s.stop(d);
    assert!(srv.lock().unwrap().dynamic_dids().is_empty());
}

#[test]
fn drop_reconnect_redefine_resume() {
    let clock = VirtualClock::new(BASE_MS);
    let srv = server();
    let bus = FaultBus::new()
        .with_node(node(&srv))
        .with_clock(clock.clone());
    let mut s = LiveSession::new(&def(), &[], FastLogConfig::fast()).unwrap();
    let mut first = diag(bus.link());
    assert_eq!(s.start(&mut first).mode, LogMode::Fast2c);
    // Three good fast rounds (one request each), then the adapter is gone.
    let link = Trigger {
        inner: Box::new(first),
        bus: bus.clone(),
        after: 3,
        fault: Fault::UntilReopen(FailKind::Io),
        n: 0,
    };
    bus.refuse_opens(2); // still unplugged for the first two attempts
    let (b2, s2) = (bus.clone(), Arc::clone(&srv));
    let mut open = move || -> Result<Box<dyn RawDiag>, String> {
        // While it was gone the ECU restarted and forgot its dynamic DIDs.
        s2.lock().unwrap().power_cycle();
        let l = b2.reopen().map_err(|e| e.to_string())?;
        Ok(Box::new(diag(l)))
    };
    let opts = RunOptions {
        limits: RunLimits {
            rounds: Some(8),
            duration_ms: None,
            interval_ms: 100,
        },
        policy: ReconnectPolicy {
            dead_after: 2,
            initial_backoff_ms: 100,
            max_backoff_ms: 1000,
            ..ReconnectPolicy::reconnecting()
        },
        stop: None,
        clock: &clock,
    };
    let mut csv = Vec::new();
    let rep = s
        .run_csv_resilient(Box::new(link), &mut open, &opts, None, &mut csv)
        .unwrap();
    let csv = String::from_utf8(csv).unwrap();
    assert_eq!(
        (rep.rounds, rep.dead_rounds, rep.drops, rep.reconnects),
        (8, 2, 1, 1),
        "{rep:?}"
    );
    assert_eq!((rep.reopen_attempts, rep.ended), (3, EndReason::Rounds));
    assert!(rep.stopped_cleanly);

    let r = rows(&csv);
    assert_eq!(
        markers(&r),
        ["lost", "reopen-failed", "reopen-failed", "reconnected"]
    );
    for (round, id, value, note, _) in r.iter().filter(|x| x.1 != LINK_PARAM_ID) {
        if (3..5).contains(round) {
            // Dead rounds: no value, the transport error instead.
            assert!(value.is_empty(), "round {round} {id}: {value}");
            assert!(note.contains("simulated"), "round {round} {id}: {note}");
        } else {
            assert!(!value.is_empty() && note.is_empty(), "round {round} {id}");
        }
    }
    let rpm: Vec<&str> = r
        .iter()
        .filter(|x| x.1 == "rpm" && !x.2.is_empty())
        .map(|x| x.2.as_str())
        .collect();
    assert_eq!(rpm, ["1726"; 6]);
    // start() re-defined the dynamic DIDs on the new link (the tick never had
    // to), and the clean stop cleared them.
    assert_eq!(s.status().mode, LogMode::Fast2c);
    assert_eq!(s.status().redefines, 0);
    assert!(srv.lock().unwrap().dynamic_dids().is_empty());
    // Every pause is a real (virtual) sleep: 3 live intervals, one dead-round
    // backoff, attempts after 100/200/400 ms, 2 more live intervals.
    assert_eq!(clock.slept_ms(), 300 + 100 + 700 + 200);
    assert_eq!(clock.now_ms(), 1300);

    // The Log viewer opens it: three plotted channels, the link status
    // listed as a text channel, the outage a gap.
    let log = opentune_logview::parse(&csv, &opentune_logview::ParseOptions::default()).unwrap();
    assert_eq!(log.format, opentune_logview::LogFormat::OpentuneLong);
    let names: Vec<&str> = log.channels.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["rpm", "timing", "load"]);
    let rpm = &log.channels[0].values;
    assert_eq!(rpm.iter().filter(|v| **v == 1726.0).count(), 6);
    assert!(rpm.iter().any(|v| v.is_nan()));
    assert!(
        log.notes.iter().any(|n| n.contains("Link status = reconnected")),
        "{:?}",
        log.notes
    );
}

#[test]
fn link_that_comes_back_on_its_own_without_reconnect() {
    let clock = VirtualClock::new(BASE_MS);
    let srv = server();
    let bus = FaultBus::new()
        .with_node(node(&srv))
        .with_clock(clock.clone());
    // Plain polling: 3 requests per round. After 2 rounds the bus sleeps for
    // 3 rounds' worth of calls (send + receive per request), then wakes.
    let mut s = LiveSession::new(&def(), &[], FastLogConfig::default()).unwrap();
    let mut first = diag(bus.link());
    assert_eq!(s.start(&mut first).mode, LogMode::Polling);
    let link = Trigger {
        inner: Box::new(first),
        bus: bus.clone(),
        after: 6,
        fault: Fault::Calls(18, FailKind::Silent),
        n: 0,
    };
    let mut open = || -> Result<Box<dyn RawDiag>, String> { panic!("no reopen without --reconnect") };
    let opts = RunOptions {
        limits: RunLimits {
            rounds: Some(7),
            duration_ms: None,
            interval_ms: 0,
        },
        policy: ReconnectPolicy {
            dead_after: 2,
            initial_backoff_ms: 250,
            max_backoff_ms: 1000,
            ..ReconnectPolicy::default()
        },
        stop: None,
        clock: &clock,
    };
    let mut csv = Vec::new();
    let rep = s
        .run_csv_resilient(Box::new(link), &mut open, &opts, None, &mut csv)
        .unwrap();
    assert_eq!(
        (rep.dead_rounds, rep.drops, rep.recoveries, rep.reconnects),
        (3, 1, 1, 0)
    );
    let r = rows(&String::from_utf8(csv).unwrap());
    assert_eq!(markers(&r), ["lost", "recovered"]);
    let dead_notes: Vec<&str> = r
        .iter()
        .filter(|x| (2..5).contains(&x.0) && x.1 != LINK_PARAM_ID)
        .map(|x| x.3.as_str())
        .collect();
    assert_eq!(dead_notes.len(), 9);
    assert!(dead_notes.iter().all(|n| n.contains("timed out")), "{dead_notes:?}");
    // Backoff after each dead round (250, 500, 1000) plus the virtual time
    // the 9 silent requests waited (50 ms each).
    assert_eq!(clock.slept_ms(), 250 + 500 + 1000);
    assert_eq!(clock.now_ms(), 1750 + 9 * 50);
}

#[test]
fn gives_up_and_leaves_the_dead_link_alone() {
    let clock = VirtualClock::new(BASE_MS);
    let srv = server();
    let bus = FaultBus::new()
        .with_node(node(&srv))
        .with_clock(clock.clone());
    let mut s = LiveSession::new(&def(), &[], FastLogConfig::fast()).unwrap();
    let mut first = diag(bus.link());
    s.start(&mut first);
    let link = Trigger {
        inner: Box::new(first),
        bus: bus.clone(),
        after: 1,
        fault: Fault::UntilReopen(FailKind::Timeout),
        n: 0,
    };
    bus.refuse_opens(u32::MAX);
    let b2 = bus.clone();
    let mut open = move || -> Result<Box<dyn RawDiag>, String> {
        b2.reopen().map(|l| Box::new(diag(l)) as Box<dyn RawDiag>).map_err(|e| e.to_string())
    };
    let opts = RunOptions {
        limits: RunLimits::default(), // no round or time limit
        policy: ReconnectPolicy {
            dead_after: 1,
            initial_backoff_ms: 100,
            max_backoff_ms: 300,
            max_attempts: Some(4),
            ..ReconnectPolicy::reconnecting()
        },
        stop: None,
        clock: &clock,
    };
    let mut csv = Vec::new();
    let rep = s
        .run_csv_resilient(Box::new(link), &mut open, &opts, None, &mut csv)
        .unwrap();
    assert_eq!((rep.ended, rep.reopen_attempts), (EndReason::GaveUp, 4));
    assert!(!rep.stopped_cleanly);
    let csv = String::from_utf8(csv).unwrap();
    let r = rows(&csv);
    assert_eq!(
        markers(&r),
        ["lost", "reopen-failed", "reopen-failed", "reopen-failed", "gave-up"]
    );
    assert!(csv.lines().last().unwrap().contains("gave up reconnecting after 4 attempt(s)"));
    // Never a hot spin: one sleep before each attempt (100, 200, 300, 300).
    assert_eq!(clock.slept_ms(), 900);
    // Nothing could clear the dynamic DID on a dead link; it is not pretended.
    assert_eq!(srv.lock().unwrap().dynamic_dids(), [0xF200]);
    assert_eq!(bus.stats().refused_opens, 4);
}
