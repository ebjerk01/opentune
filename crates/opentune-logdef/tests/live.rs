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

/// A param with an explicit addressing/protocol and a linear conversion.
fn param_with(
    id: &str,
    protocol: ProtocolKind,
    addressing: Addressing,
    factor: f64,
    offset: f64,
    units: &str,
) -> LogParam {
    LogParam {
        id: id.into(),
        name: id.into(),
        description: String::new(),
        units: units.into(),
        protocol,
        addressing,
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

fn param(id: &str, did: u16, factor: f64, offset: f64, units: &str) -> LogParam {
    param_with(
        id,
        ProtocolKind::UdsMode22,
        Addressing::UdsDid { did },
        factor,
        offset,
        units,
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

// ---------------------------------------------------------------------------
// OBD-II PID and raw-memory reads. The `opentune-udssim` server models UDS
// services (RDBI etc.) but not OBD-II mode 01 or ReadMemoryByAddress, so these
// drive a scripted `RawDiag` closure that both asserts the exact request bytes
// and returns a canned response. The closure form uses the blanket `RawDiag`
// impl in `opentune-core`.

#[test]
fn polls_and_scales_an_obd_pid() {
    // PID 0x0C (engine RPM): response 41 0C 1A F8 → 0x1AF8 = 6904, ×0.25 = 1726.
    let mut diag = |req: &[u8]| -> Result<Vec<u8>, String> {
        assert_eq!(req, [0x01, 0x0C], "mode-01 request for PID 0x0C");
        Ok(vec![0x41, 0x0C, 0x1A, 0xF8])
    };
    let d = def(vec![param_with(
        "rpm",
        ProtocolKind::Obd2Mode01,
        Addressing::ObdPid { pid: 0x0C },
        0.25,
        0.0,
        "rpm",
    )]);
    let samples = poll_once(&mut diag, &d, &[]);
    assert_eq!(samples.len(), 1);
    let s = &samples[0];
    assert_eq!(s.raw, Some(0x1AF8));
    assert!((s.value.unwrap() - 1726.0).abs() < 1e-9);
    assert!(s.note.is_empty(), "note should be empty on success: {}", s.note);
}

#[test]
fn obd_pid_negative_response_is_noted() {
    let mut diag = |_req: &[u8]| -> Result<Vec<u8>, String> {
        Ok(vec![0x7F, 0x01, 0x12]) // subFunctionNotSupported
    };
    let d = def(vec![param_with(
        "rpm",
        ProtocolKind::Obd2Mode01,
        Addressing::ObdPid { pid: 0x0C },
        1.0,
        0.0,
        "rpm",
    )]);
    let samples = poll_once(&mut diag, &d, &[]);
    assert_eq!(samples.len(), 1);
    assert!(samples[0].raw.is_none());
    assert!(
        samples[0].note.contains("negative response"),
        "note: {}",
        samples[0].note
    );
}

#[test]
fn obd_pid_short_response_is_noted_not_guessed() {
    let mut diag = |_req: &[u8]| -> Result<Vec<u8>, String> {
        Ok(vec![0x41, 0x0C]) // echo but no data bytes
    };
    let d = def(vec![param_with(
        "rpm",
        ProtocolKind::Obd2Mode01,
        Addressing::ObdPid { pid: 0x0C },
        1.0,
        0.0,
        "rpm",
    )]);
    let samples = poll_once(&mut diag, &d, &[]);
    assert!(samples[0].raw.is_none());
    assert!(samples[0].note.contains("short response"), "note: {}", samples[0].note);
}

#[test]
fn reads_memory_by_address() {
    // 4-byte address 0x00100000 + 1-byte length 2, ALFID 0x14.
    let mut diag = |req: &[u8]| -> Result<Vec<u8>, String> {
        assert_eq!(
            req,
            [0x23, 0x14, 0x00, 0x10, 0x00, 0x00, 0x02],
            "ReadMemoryByAddress with 4-byte addr + 1-byte len"
        );
        Ok(vec![0x63, 0x12, 0x34]) // 0x1234 = 4660
    };
    let d = def(vec![param_with(
        "map",
        ProtocolKind::EcuMode23,
        Addressing::Memory {
            address: 0x0010_0000,
            length: 2,
            bit: None,
        },
        1.0,
        0.0,
        "raw",
    )]);
    let samples = poll_once(&mut diag, &d, &[]);
    assert_eq!(samples[0].raw, Some(0x1234));
    assert!((samples[0].value.unwrap() - 4660.0).abs() < 1e-9);
    assert!(samples[0].note.is_empty(), "note: {}", samples[0].note);
}

#[test]
fn reads_a_single_bit_from_memory() {
    // One byte 0x08; bit 3 is set → surfaced as 1.
    let mut diag = |req: &[u8]| -> Result<Vec<u8>, String> {
        assert_eq!(req, [0x23, 0x14, 0x00, 0x20, 0x00, 0x00, 0x01]);
        Ok(vec![0x63, 0x08])
    };
    let d = def(vec![param_with(
        "clutch",
        ProtocolKind::EcuMode23,
        Addressing::Memory {
            address: 0x0020_0000,
            length: 1,
            bit: Some(3),
        },
        1.0,
        0.0,
        "flag",
    )]);
    let samples = poll_once(&mut diag, &d, &[]);
    assert_eq!(samples[0].raw, Some(1));
    assert!((samples[0].value.unwrap() - 1.0).abs() < 1e-9);
    assert!(samples[0].note.contains("bit 3"), "note: {}", samples[0].note);
}

#[test]
fn memory_negative_response_is_noted() {
    let mut diag = |_req: &[u8]| -> Result<Vec<u8>, String> {
        Ok(vec![0x7F, 0x23, 0x31]) // requestOutOfRange
    };
    let d = def(vec![param_with(
        "map",
        ProtocolKind::EcuMode23,
        Addressing::Memory {
            address: 0x0010_0000,
            length: 2,
            bit: None,
        },
        1.0,
        0.0,
        "raw",
    )]);
    let samples = poll_once(&mut diag, &d, &[]);
    assert!(samples[0].raw.is_none());
    assert!(
        samples[0].note.contains("negative response"),
        "note: {}",
        samples[0].note
    );
}

#[test]
fn memory_short_response_is_noted_not_guessed() {
    // Asked for 2 bytes, only 1 came back → must be noted, not padded.
    let mut diag = |_req: &[u8]| -> Result<Vec<u8>, String> { Ok(vec![0x63, 0x12]) };
    let d = def(vec![param_with(
        "map",
        ProtocolKind::EcuMode23,
        Addressing::Memory {
            address: 0x0010_0000,
            length: 2,
            bit: None,
        },
        1.0,
        0.0,
        "raw",
    )]);
    let samples = poll_once(&mut diag, &d, &[]);
    assert!(samples[0].raw.is_none());
    assert!(samples[0].note.contains("short response"), "note: {}", samples[0].note);
}

#[test]
fn unknown_addressing_is_noted() {
    let mut diag = |_req: &[u8]| -> Result<Vec<u8>, String> {
        panic!("Unknown addressing must not issue any request");
    };
    let d = def(vec![param_with(
        "mystery",
        ProtocolKind::UdsMode22,
        Addressing::Unknown,
        1.0,
        0.0,
        "",
    )]);
    let samples = poll_once(&mut diag, &d, &[]);
    assert!(samples[0].raw.is_none());
    assert!(samples[0].note.contains("addressing"), "note: {}", samples[0].note);
}
