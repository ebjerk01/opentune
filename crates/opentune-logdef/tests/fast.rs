//! Fast logging (UDS 0x2C dynamically defined DIDs) driven end to end against
//! the `opentune-udssim` conformance server.

use opentune_core::ecudef::{DataType, Endian};
use opentune_core::{CoreDiag, RawDiag};
use opentune_logdef::live::poll_once;
use opentune_logdef::{
    Addressing, Conversion, ConversionFit, EcuGroup, FastLogConfig, LiveSession, LogDef, LogMode,
    LogParam, ProtocolGroup, ProtocolKind, Sample, SourceFormat,
};
use opentune_udssim::server::{DddidConfig, ServerConfig, UdsServer};
use opentune_udssim::transport::SimTransport;

const MEM: u32 = 0x0010_0000;

fn param(id: &str, addressing: Addressing, factor: f64, offset: f64) -> LogParam {
    let protocol = match addressing {
        Addressing::Memory { .. } => ProtocolKind::EcuMode23,
        Addressing::ObdPid { .. } => ProtocolKind::Obd2Mode01,
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
        conversion: Some(Conversion {
            units: "u".into(),
            expr: "X".into(),
            format: None,
            fit: ConversionFit::Linear { factor, offset },
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

fn did(id: &str, did: u16) -> LogParam {
    param(id, Addressing::UdsDid { did }, 1.0, 0.0)
}

fn mem(id: &str, address: u32, length: u8, bit: Option<u8>) -> LogParam {
    param(
        id,
        Addressing::Memory {
            address,
            length,
            bit,
        },
        1.0,
        0.0,
    )
}

fn def(params: Vec<LogParam>) -> LogDef {
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

/// A mixed channel set: unsigned, signed and little-endian DIDs, a 4-byte DID
/// record, a 2-byte memory word and a single memory bit.
fn mixed_def() -> LogDef {
    let mut rpm = did("rpm", 0x1000);
    rpm.conversion.as_mut().unwrap().fit = ConversionFit::Linear {
        factor: 0.25,
        offset: 0.0,
    };
    let mut timing = did("timing", 0x1001);
    timing.storagetype = Some(DataType::S16);
    timing.conversion.as_mut().unwrap().fit = ConversionFit::Linear {
        factor: 0.1,
        offset: 0.0,
    };
    let mut le = did("le", 0x1002);
    le.endian = Endian::Little;
    let wide = did("wide", 0x1003);
    let mut load = mem("load", MEM, 2, None);
    load.storagetype = Some(DataType::S16);
    let flag = mem("flag", MEM + 2, 1, Some(3));
    def(vec![rpm, timing, load, le, flag, wide])
}

fn mixed_server(config: ServerConfig) -> UdsServer {
    let mut s = UdsServer::with_config(config);
    s.set_did(0x1000, vec![0x1A, 0xF8]); // 6904 → ×0.25 = 1726
    s.set_did(0x1001, vec![0xFF, 0x38]); // int16 -200 → ×0.1 = -20.0
    s.set_did(0x1002, vec![0x34, 0x12]); // little-endian 0x1234
    s.set_did(0x1003, vec![0xDE, 0xAD, 0xBE, 0xEF]);
    // load (int16 -2 = 0xFFFE), flag byte 0x08 (bit 3 set), padding.
    s.set_memory(u64::from(MEM), vec![0xFF, 0xFE, 0x08, 0x00]);
    s
}

fn assert_same(fast: &[Sample], polled: &[Sample]) {
    assert_eq!(fast.len(), polled.len());
    for (f, p) in fast.iter().zip(polled) {
        assert_eq!(f, p, "fast and polled samples differ for {}", f.param_id);
    }
}

#[test]
fn did_and_memory_channels_share_one_dynamic_did() {
    let d = mixed_def();
    let mut sim = SimTransport::new(mixed_server(ServerConfig::default()));
    let mut diag = CoreDiag::new(&mut sim, 1000);

    let mut session = LiveSession::new(&d, &[], FastLogConfig::fast()).unwrap();
    let st = session.start(&mut diag).clone();
    assert_eq!(st.mode, LogMode::Fast2c, "reason: {:?}", st.reason);
    assert!(st.reason.is_none());
    assert_eq!(st.dynamic_dids.len(), 1);
    assert_eq!(st.dynamic_dids[0].did, 0xF200);
    // 2 + 2 + 2 + 4 (DIDs) + 2 + 1 (memory) bytes.
    assert_eq!(st.dynamic_dids[0].record_len, 13);
    // Identifier sources first, then memory sources.
    assert_eq!(
        st.dynamic_dids[0].channels,
        ["rpm", "timing", "le", "wide", "load", "flag"]
    );
    assert!(st.polled.is_empty());

    let fast = session.tick(&mut diag);
    assert_eq!(session.status().tick_requests, 1, "one 0x22 per sample");
    let polled = poll_once(&mut diag, &d, &[]);
    assert_same(&fast, &polled);

    // Output order is definition order, and the slices decode correctly.
    let by_id = |id: &str| fast.iter().find(|s| s.param_id == id).unwrap().clone();
    assert_eq!(
        fast.iter().map(|s| s.param_id.as_str()).collect::<Vec<_>>(),
        ["rpm", "timing", "load", "le", "flag", "wide"]
    );
    assert!((by_id("rpm").value.unwrap() - 1726.0).abs() < 1e-9);
    let timing = by_id("timing");
    assert_eq!(timing.raw, Some(0xFF38), "raw keeps the bit pattern");
    assert!((timing.value.unwrap() + 20.0).abs() < 1e-9, "signed int16");
    assert_eq!(by_id("le").raw, Some(0x1234), "little-endian slice");
    assert_eq!(by_id("wide").raw, Some(0xDEAD_BEEF), "4-byte slice");
    assert!((by_id("load").value.unwrap() + 2.0).abs() < 1e-9);
    let flag = by_id("flag");
    assert_eq!(flag.raw, Some(1));
    assert_eq!(flag.note, "bit 3");

    // Values change between samples: the record is read live.
    sim.server_mut().set_did(0x1000, vec![0x00, 0x04]);
    sim.server_mut().write_memory(u64::from(MEM + 2), &[0x00]);
    let mut diag = CoreDiag::new(&mut sim, 1000);
    let next = session.tick(&mut diag);
    assert!((next[0].value.unwrap() - 1.0).abs() < 1e-9);
    assert_eq!(next[4].raw, Some(0));

    session.stop(&mut diag);
    assert!(
        sim.server().dynamic_dids().is_empty(),
        "stop clears the dynamic DIDs"
    );
}

#[test]
fn channels_split_across_two_dynamic_dids() {
    let d = mixed_def();
    let mut sim = SimTransport::new(mixed_server(ServerConfig::default()));
    let mut diag = CoreDiag::new(&mut sim, 1000);
    let cfg = FastLogConfig {
        max_record_len: 8,
        ..FastLogConfig::fast()
    };
    let mut session = LiveSession::new(&d, &[], cfg).unwrap();
    let st = session.start(&mut diag).clone();
    assert_eq!(st.mode, LogMode::Fast2c, "reason: {:?}", st.reason);
    let dids: Vec<u16> = st.dynamic_dids.iter().map(|d| d.did).collect();
    assert_eq!(dids, [0xF200, 0xF201]);
    // rpm(2) timing(2) load(2) le(2) = 8 | flag(1) wide(4) = 5.
    assert_eq!(st.dynamic_dids[0].record_len, 8);
    assert_eq!(st.dynamic_dids[1].record_len, 5);
    assert!(st.dynamic_dids.iter().all(|d| d.record_len <= 8));

    let fast = session.tick(&mut diag);
    assert_eq!(
        session.status().tick_requests,
        2,
        "one read per dynamic DID"
    );
    let polled = poll_once(&mut diag, &d, &[]);
    assert_same(&fast, &polled);
    assert_eq!(sim.server().dynamic_dids(), [0xF200, 0xF201]);
}

#[test]
fn record_limit_and_did_budget_leave_the_rest_polled() {
    let d = mixed_def();
    let mut sim = SimTransport::new(mixed_server(ServerConfig::default()));
    let mut diag = CoreDiag::new(&mut sim, 1000);
    let cfg = FastLogConfig {
        first_did: 0xF3FF,
        max_dids: 1,
        max_record_len: 3,
        ..FastLogConfig::fast()
    };
    let mut session = LiveSession::new(&d, &[], cfg).unwrap();
    let st = session.start(&mut diag).clone();
    assert_eq!(st.mode, LogMode::Fast2c);
    assert_eq!(st.dynamic_dids.len(), 1);
    assert_eq!(st.dynamic_dids[0].did, 0xF3FF);
    // rpm (2 bytes) + flag (1 byte) fill the single 3-byte DID; "wide" (4
    // bytes) exceeds the record limit; the rest find no DID left.
    assert_eq!(st.dynamic_dids[0].channels, ["rpm", "flag"]);
    assert_eq!(st.polled, ["timing", "load", "le", "wide"]);
    assert!(st.notes.iter().any(|n| n.contains("record limit")));
    assert!(st.notes.iter().any(|n| n.contains("dynamic DIDs are full")));
    let fast = session.tick(&mut diag);
    assert_eq!(session.status().tick_requests, 5);
    assert_same(&fast, &poll_once(&mut diag, &d, &[]));
}

#[test]
fn falls_back_to_polling_when_the_ecu_refuses_2c() {
    let d = mixed_def();
    let cases: Vec<(&str, ServerConfig, Option<u8>)> = vec![
        (
            "NRC 0x11",
            ServerConfig {
                dddid: DddidConfig {
                    supported: false,
                    ..DddidConfig::default()
                },
                ..ServerConfig::default()
            },
            None,
        ),
        (
            "NRC 0x22",
            ServerConfig {
                dddid: DddidConfig {
                    allowed_sessions: vec![0x03],
                    ..DddidConfig::default()
                },
                ..ServerConfig::default()
            },
            None,
        ),
        (
            "NRC 0x33",
            ServerConfig {
                dddid: DddidConfig {
                    require_security: true,
                    ..DddidConfig::default()
                },
                ..ServerConfig::default()
            },
            None,
        ),
        (
            // The ECU's own record limit is smaller than the planned record.
            "NRC 0x31",
            ServerConfig {
                dddid: DddidConfig {
                    max_record_len: 4,
                    ..DddidConfig::default()
                },
                ..ServerConfig::default()
            },
            None,
        ),
        // An injected subFunctionNotSupported on the first 0x2C request.
        ("NRC 0x12", ServerConfig::default(), Some(0x12)),
    ];
    for (expect, config, inject) in cases {
        let mut sim = SimTransport::new(mixed_server(config));
        if let Some(code) = inject {
            sim.faults().negative_response(0x2C, code);
        }
        let mut diag = CoreDiag::new(&mut sim, 1000);
        let mut session = LiveSession::new(&d, &[], FastLogConfig::fast()).unwrap();
        let st = session.start(&mut diag).clone();
        assert_eq!(st.mode, LogMode::Polling, "{expect}");
        let reason = st.reason.unwrap_or_default();
        assert!(reason.contains(expect), "{expect}: reason was {reason}");
        assert!(st.dynamic_dids.is_empty());
        assert_eq!(st.polled.len(), 6);
        // Logging carries on, per channel.
        let samples = session.tick(&mut diag);
        assert_eq!(session.status().tick_requests, 6);
        assert_same(&samples, &poll_once(&mut diag, &d, &[]));
        assert!(
            sim.server().dynamic_dids().is_empty(),
            "{expect}: a refused setup leaves nothing defined"
        );
    }
}

#[test]
fn polling_mode_when_fast_is_not_requested() {
    let d = mixed_def();
    let mut sim = SimTransport::new(mixed_server(ServerConfig::default()));
    let mut diag = CoreDiag::new(&mut sim, 1000);
    let mut session = LiveSession::new(&d, &[], FastLogConfig::default()).unwrap();
    let st = session.start(&mut diag).clone();
    assert_eq!(st.mode, LogMode::Polling);
    assert_eq!(st.setup_requests, 0, "no probing without --fast");
    assert_same(&session.tick(&mut diag), &poll_once(&mut diag, &d, &[]));
}

#[test]
fn redefines_after_a_session_drop() {
    let d = mixed_def();
    // 0x2C only in the extended session, as on many real ECUs.
    let config = ServerConfig {
        dddid: DddidConfig {
            allowed_sessions: vec![0x03],
            ..DddidConfig::default()
        },
        ..ServerConfig::default()
    };
    let mut sim = SimTransport::new(mixed_server(config));
    let cfg = FastLogConfig {
        session: Some(0x03),
        ..FastLogConfig::fast()
    };
    let mut session = LiveSession::new(&d, &[], cfg).unwrap();
    {
        let mut diag = CoreDiag::new(&mut sim, 1000);
        assert_eq!(session.start(&mut diag).mode, LogMode::Fast2c);
        session.tick(&mut diag);
    }
    // The ECU falls back to the default session (S3 timeout / another tester):
    // ISO 14229-1 drops the dynamic DIDs with it.
    sim.server_mut().handle(&[0x10, 0x01]);
    assert!(sim.server().dynamic_dids().is_empty());

    let mut diag = CoreDiag::new(&mut sim, 1000);
    let samples = session.tick(&mut diag);
    let st = session.status().clone();
    assert_eq!(st.mode, LogMode::Fast2c);
    assert_eq!(st.redefines, 1);
    assert!(samples.iter().all(|s| s.raw.is_some()), "{samples:?}");
    assert_same(&samples, &poll_once(&mut diag, &d, &[]));
    assert_eq!(sim.server().session(), 0x03, "session re-entered");
    assert_eq!(sim.server().dynamic_dids(), [0xF200]);
}

#[test]
fn falls_back_when_the_redefine_fails() {
    let d = mixed_def();
    let mut sim = SimTransport::new(mixed_server(ServerConfig::default()));
    let mut session = LiveSession::new(&d, &[], FastLogConfig::fast()).unwrap();
    {
        let mut diag = CoreDiag::new(&mut sim, 1000);
        assert_eq!(session.start(&mut diag).mode, LogMode::Fast2c);
    }
    // Definition lost, and the ECU now refuses 0x2C entirely.
    sim.server_mut().handle(&[0x11, 0x01]);
    sim.server_mut().config.dddid.supported = false;

    let mut diag = CoreDiag::new(&mut sim, 1000);
    let samples = session.tick(&mut diag);
    let st = session.status().clone();
    assert_eq!(st.mode, LogMode::Polling);
    let reason = st.reason.unwrap_or_default();
    assert!(
        reason.contains("re-define") && reason.contains("NRC 0x11"),
        "{reason}"
    );
    // The tick still produced every channel, by polling.
    assert_same(&samples, &poll_once(&mut diag, &d, &[]));
    assert!(samples.iter().all(|s| s.raw.is_some()));
}

#[test]
fn obd_and_unknown_channels_stay_polled_next_to_the_dynamic_did() {
    let mut channels = vec![did("a", 0x1000), did("missing", 0x7777)];
    channels.push(param("pid", Addressing::ObdPid { pid: 0x0C }, 1.0, 0.0));
    channels.push(param("unknown", Addressing::Unknown, 1.0, 0.0));
    channels.push(did("b", 0x1001));
    let d = def(channels);
    let mut sim = SimTransport::new(mixed_server(ServerConfig::default()));
    let mut diag = CoreDiag::new(&mut sim, 1000);
    let mut session = LiveSession::new(&d, &[], FastLogConfig::fast()).unwrap();
    let st = session.start(&mut diag).clone();
    assert_eq!(st.mode, LogMode::Fast2c);
    assert_eq!(st.dynamic_dids[0].channels, ["a", "b"]);
    assert_eq!(st.polled, ["missing", "pid", "unknown"]);
    assert!(st.notes.iter().any(|n| n.starts_with("missing: probe")));
    let fast = session.tick(&mut diag);
    assert_same(&fast, &poll_once(&mut diag, &d, &[]));
    // The parameter selection is honoured too.
    let only = LiveSession::new(&d, &["b"], FastLogConfig::fast()).unwrap();
    assert_eq!(only.params().len(), 1);
}

/// Every `0x2C` request the engine sends names a DDDID in 0xF200..=0xF3FF,
/// and `stop` clears each one it defined.
#[test]
fn only_dynamic_range_dids_are_defined_and_all_are_cleared() {
    let d = mixed_def();
    let mut sim = SimTransport::new(mixed_server(ServerConfig::default()));
    let mut sent: Vec<Vec<u8>> = Vec::new();
    {
        let mut core = CoreDiag::new(&mut sim, 1000);
        let mut rec = |req: &[u8]| -> Result<Vec<u8>, String> {
            sent.push(req.to_vec());
            core.request(req)
        };
        let cfg = FastLogConfig {
            max_record_len: 4,
            ..FastLogConfig::fast()
        };
        let mut session = LiveSession::new(&d, &[], cfg).unwrap();
        session.start(&mut rec);
        session.tick(&mut rec);
        session.stop(&mut rec);
    }
    let dddid: Vec<&Vec<u8>> = sent.iter().filter(|r| r[0] == 0x2C).collect();
    assert!(!dddid.is_empty());
    for r in &dddid {
        let id = u16::from_be_bytes([r[2], r[3]]);
        assert!((0xF200..=0xF3FF).contains(&id), "{r:02X?}");
    }
    // Clear-before-define for each DDDID, then clear on stop.
    let clears = dddid.iter().filter(|r| r[1] == 0x03).count();
    let defined: std::collections::BTreeSet<u16> = dddid
        .iter()
        .filter(|r| r[1] != 0x03)
        .map(|r| u16::from_be_bytes([r[2], r[3]]))
        .collect();
    assert_eq!(clears, 2 * defined.len());
    assert!(sim.server().dynamic_dids().is_empty());
}

/// The point of the mode: requests per sample. Eight channels (six DIDs, two
/// memory words) polled cost eight requests per sample; packed, one.
#[test]
fn fast_mode_cuts_requests_per_sample() {
    let mut channels: Vec<LogParam> = (0..6).map(|i| did(&format!("d{i}"), 0x2000 + i)).collect();
    channels.push(mem("m0", MEM, 2, None));
    channels.push(mem("m1", MEM + 2, 2, None));
    let d = def(channels);
    let server = || {
        let mut s = UdsServer::new();
        for i in 0..6u16 {
            s.set_did(0x2000 + i, vec![0x00, i as u8]);
        }
        s.set_memory(u64::from(MEM), vec![0x01, 0x02, 0x03, 0x04]);
        s
    };
    const ROUNDS: usize = 50;

    let mut sim = SimTransport::new(server());
    {
        let mut diag = CoreDiag::new(&mut sim, 1000);
        for _ in 0..ROUNDS {
            poll_once(&mut diag, &d, &[]);
        }
    }
    let polling = sim.request_count();

    let mut sim = SimTransport::new(server());
    let st = {
        let mut diag = CoreDiag::new(&mut sim, 1000);
        let mut session = LiveSession::new(&d, &[], FastLogConfig::fast()).unwrap();
        session.start(&mut diag);
        for _ in 0..ROUNDS {
            session.tick(&mut diag);
        }
        session.stop(&mut diag);
        session.status().clone()
    };
    let fast = sim.request_count();

    let per_sample_polling = polling as f64 / ROUNDS as f64;
    let per_sample_fast = st.requests_per_tick();
    println!(
        "polling: {per_sample_polling} req/sample; fast-2c: {per_sample_fast} req/sample \
         (+{} setup/teardown); total {polling} vs {fast} over {ROUNDS} samples",
        st.setup_requests
    );
    assert_eq!(per_sample_polling, 8.0);
    assert_eq!(per_sample_fast, 1.0);
    // Setup: 6 probes + clear + 1 defineByIdentifier + 1 defineByMemoryAddress;
    // teardown: 1 clear.
    assert_eq!(st.setup_requests, 10);
    assert_eq!(fast as u64, st.setup_requests + st.tick_requests);
    assert!(polling >= 6 * fast, "{polling} vs {fast}");
}
