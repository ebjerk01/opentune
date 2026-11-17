//! Integration test: drive the coding engine against the real `opentune-udssim`
//! UDS conformance server (via `SimTransport` + `CoreDiag`), not a hand-rolled
//! fake. This proves the engine's on-wire UDS framing is accepted by an
//! independent ISO-14229 implementation.

use opentune_coding::{
    decode, read_coding, Addressing, CodingDef, CodingOption, CodingParam, ParamKind,
};
use opentune_core::CoreDiag;
use opentune_udssim::server::UdsServer;
use opentune_udssim::transport::SimTransport;

fn sample_def() -> CodingDef {
    CodingDef {
        id: "sim-mod".into(),
        name: "Sim module".into(),
        module: "synthetic".into(),
        requires_security: false,
        addressing: Addressing::Did {
            dids: vec![0x0401, 0x0402],
        },
        params: vec![
            CodingParam {
                name: "Start-Stop".into(),
                block: 0,
                byte: 0,
                bit: 0,
                len: 1,
                kind: ParamKind::Bool,
                options: vec![],
                note: String::new(),
            },
            CodingParam {
                name: "DRL mode".into(),
                block: 0,
                byte: 0,
                bit: 1,
                len: 2,
                kind: ParamKind::Enum,
                options: vec![
                    CodingOption {
                        value: 0,
                        label: "off".into(),
                    },
                    CodingOption {
                        value: 1,
                        label: "on".into(),
                    },
                    CodingOption {
                        value: 2,
                        label: "auto".into(),
                    },
                ],
                note: String::new(),
            },
            CodingParam {
                name: "Country".into(),
                block: 1,
                byte: 2,
                bit: 0,
                len: 8,
                kind: ParamKind::Number,
                options: vec![],
                note: String::new(),
            },
        ],
    }
}

#[test]
fn reads_coding_from_the_conformance_server() {
    // Seed the simulated ECU's data identifiers with the raw coding bytes.
    let mut server = UdsServer::new();
    server.set_did(0x0401, vec![0b0000_0101u8, 0x00]); // SS=1, DRL=2 (auto)
    server.set_did(0x0402, vec![0x00u8, 0x00, 0x2A]); // Country = 42

    let mut sim = SimTransport::new(server);
    let mut diag = CoreDiag::new(&mut sim, 1000);

    let def = sample_def();
    let coding = read_coding(&mut diag, &def).expect("read coding over the UDS sim");

    // The engine issued one RDBI (0x22) exchange per addressed DID.
    assert_eq!(coding.blocks.len(), 2);
    assert_eq!(coding.blocks[0].did, 0x0401);
    assert_eq!(coding.blocks[0].bytes, vec![0b0000_0101u8, 0x00]);
    assert_eq!(coding.blocks[1].did, 0x0402);
    assert_eq!(coding.blocks[1].bytes, vec![0x00u8, 0x00, 0x2A]);

    // Decoding the bytes the sim returned yields the expected parameters.
    let params = decode(&def, &coding).expect("decode");
    let ss = params.iter().find(|p| p.name == "Start-Stop").unwrap();
    assert_eq!(ss.value, 1);
    let drl = params.iter().find(|p| p.name == "DRL mode").unwrap();
    assert_eq!(drl.value, 2);
    assert_eq!(drl.label, "auto");
    let country = params.iter().find(|p| p.name == "Country").unwrap();
    assert_eq!(country.value, 42);
}

#[test]
fn unknown_did_surfaces_as_an_error_not_a_guess() {
    // The server has no 0x0401 → it answers requestOutOfRange (0x31); the engine
    // must surface that as an error rather than inventing coding bytes.
    let server = UdsServer::new();
    let mut sim = SimTransport::new(server);
    let mut diag = CoreDiag::new(&mut sim, 1000);

    let def = sample_def();
    let result = read_coding(&mut diag, &def);
    assert!(
        result.is_err(),
        "unseeded DID must not yield fabricated bytes"
    );
}
