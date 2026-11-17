//! The live polling loop, driven against the real `opentune-udssim` UDS
//! conformance server.

use opentune_core::ecudef::Endian;
use opentune_core::CoreDiag;
use opentune_logdef::live::{poll_once, poll_to_csv};
use opentune_logdef::{
    Addressing, Conversion, ConversionFit, EcuGroup, LogDef, LogParam, ProtocolGroup, ProtocolKind,
    SourceFormat,
};
use opentune_udssim::server::UdsServer;
use opentune_udssim::transport::SimTransport;

fn param(id: &str, did: u16, factor: f64, offset: f64, units: &str) -> LogParam {
    LogParam {
        id: id.into(),
        name: id.into(),
        description: String::new(),
        units: units.into(),
        protocol: ProtocolKind::UdsMode22,
        addressing: Addressing::UdsDid { did },
        storagetype: None,
        endian: Endian::Big,
        conversion: Some(Conversion {
            units: units.into(),
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

#[test]
fn polls_and_scales_a_did_from_the_sim() {
    let mut server = UdsServer::new();
    server.set_did(0x1000, vec![0x01u8, 0x90]); // 0x0190 = 400 → ×0.1 = 40.0
    let mut sim = SimTransport::new(server);
    let mut diag = CoreDiag::new(&mut sim, 1000);

    let d = def(vec![param("rpm", 0x1000, 0.1, 0.0, "x1000rpm")]);
    let samples = poll_once(&mut diag, &d, &[]);

    assert_eq!(samples.len(), 1);
    let s = &samples[0];
    assert_eq!(s.raw, Some(0x0190));
    assert!((s.value.unwrap() - 40.0).abs() < 1e-9);
    assert!(s.note.is_empty(), "note should be empty on success: {}", s.note);
}

#[test]
fn unseeded_did_is_noted_not_dropped() {
    let server = UdsServer::new(); // nothing seeded
    let mut sim = SimTransport::new(server);
    let mut diag = CoreDiag::new(&mut sim, 1000);

    let d = def(vec![param("boost", 0x2000, 1.0, 0.0, "kPa")]);
    let samples = poll_once(&mut diag, &d, &[]);

    assert_eq!(samples.len(), 1, "a failed read must still produce a sample");
    assert!(samples[0].raw.is_none());
    assert!(!samples[0].note.is_empty(), "the failure must be noted");
}

#[test]
fn csv_has_a_header_and_one_row_per_sample_per_round() {
    let mut server = UdsServer::new();
    server.set_did(0x1000, vec![0x00u8, 0x64]); // 100
    let mut sim = SimTransport::new(server);
    let mut diag = CoreDiag::new(&mut sim, 1000);

    let d = def(vec![param("temp", 0x1000, 1.0, -40.0, "C")]);
    let mut buf: Vec<u8> = Vec::new();
    poll_to_csv(&mut diag, &d, &[], 3, 0, &mut buf).unwrap();
    let text = String::from_utf8(buf).unwrap();

    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "round,param_id,name,raw,value,units,note");
    assert_eq!(lines.len(), 1 + 3, "header + 3 rounds × 1 param");
    assert!(lines[1].contains("temp"));
    assert!(lines[1].contains("60"), "100 - 40 = 60: {}", lines[1]); // scaled value
}
