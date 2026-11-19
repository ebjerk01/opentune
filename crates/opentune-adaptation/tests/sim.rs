//! Integration test: drive the adaptation engine against the real
//! `opentune-udssim` UDS conformance server, not a hand-rolled fake.

use opentune_adaptation::{
    read_value, AdaptationChannel, AdaptationDef, ChannelKind, NumericField, Scaling,
};
use opentune_core::CoreDiag;
use opentune_udssim::server::UdsServer;
use opentune_udssim::transport::SimTransport;

fn def() -> AdaptationDef {
    AdaptationDef {
        id: "sim".into(),
        name: "Sim".into(),
        module: "synthetic".into(),
        requires_security: false,
        security_level: None,
        channels: vec![AdaptationChannel {
            name: "Rail pressure".into(),
            kind: ChannelKind::Value {
                did: 0x1234,
                field: NumericField {
                    offset: 0,
                    width: 2,
                    scaling: Scaling {
                        factor: 0.1,
                        offset: 0.0,
                        unit: "bar".into(),
                    },
                },
                min: None,
                max: None,
            },
            note: String::new(),
        }],
    }
}

#[test]
fn reads_value_channel_from_the_conformance_server() {
    let mut server = UdsServer::new();
    server.set_did(0x1234, vec![0x01u8, 0x90]); // 0x0190 = 400 → ×0.1 = 40.0
    let mut sim = SimTransport::new(server);
    let mut diag = CoreDiag::new(&mut sim, 1000);

    let r = read_value(&mut diag, &def(), "Rail pressure").expect("read value over the UDS sim");
    assert_eq!(r.raw, 0x0190);
    assert!((r.engineering - 40.0).abs() < 1e-9);
    assert_eq!(r.unit, "bar");
}

#[test]
fn unseeded_channel_errors_rather_than_guessing() {
    let server = UdsServer::new(); // no 0x1234 seeded
    let mut sim = SimTransport::new(server);
    let mut diag = CoreDiag::new(&mut sim, 1000);
    assert!(read_value(&mut diag, &def(), "Rail pressure").is_err());
}
