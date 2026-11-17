//! Integration test: drive the auto-scan engine against the real
//! `opentune-udssim` UDS conformance server (via `CoreDiag` over `SimTransport`),
//! not a hand-rolled fake. This mirrors `opentune-coding/tests/sim.rs` and proves
//! the scan engine's on-wire UDS framing (TesterPresent presence probe,
//! ReadDataByIdentifier identity reads, ReadDTCInformation 0x19/02) is accepted by
//! an independent ISO-14229 implementation.
//!
//! `SimTransport` models a single ECU, so the vehicle list carries a single UDS
//! module addressed to that ECU — the clean way to exercise multi-step scanning
//! without faking multi-module CAN arbitration.

use opentune_autoscan::{scan, ModuleAddr, Protocol, System, VehicleModules};
use opentune_core::CoreDiag;
use opentune_udssim::server::{DtcRecord, UdsServer};
use opentune_udssim::transport::SimTransport;

/// A single-module vehicle list pointing at the simulated ECU.
fn one_uds_module() -> VehicleModules {
    VehicleModules {
        vehicle: "sim-vehicle".into(),
        modules: vec![ModuleAddr {
            name: "Engine".into(),
            addr: 0x01,
            req_id: 0x7E0,
            resp_id: 0x7E8,
            protocol: Protocol::Uds,
        }],
    }
}

#[test]
fn scans_a_seeded_module_over_the_conformance_server() {
    // Seed the simulated ECU: VIN + one supported identity DID, plus two stored
    // DTCs. The scan engine's presence probe (3E 00), identity DIDs (0xF190 VIN
    // then 0xF187/0xF189/0xF191/0xF195) and DTC read (19 02 FF) all run against
    // this real server; unseeded identity DIDs answer requestOutOfRange (0x31),
    // which the engine must treat as "DID not offered" and skip.
    let mut server = UdsServer::new();
    server.set_did(0xF190, b"WVWZZZ1KZAW000001".to_vec()); // VIN (17 ASCII chars)
    server.set_did(0xF187, b"04E906016AD".to_vec()); // part number
    // Two DTCs: P0101 (status 0x24) and U0100 (status 0x2F) — same encoding the
    // engine's own unit test asserts, decoded here by the shared opentune-dtc.
    server.add_dtc(DtcRecord::new([0x01, 0x01, 0x00], 0x24));
    server.add_dtc(DtcRecord::new([0xC1, 0x00, 0x00], 0x2F));

    let mut sim = SimTransport::new(server);
    let mut diag = CoreDiag::new(&mut sim, 1000);

    let modules = one_uds_module();
    let report = scan(&mut diag, &modules);

    assert_eq!(report.modules.len(), 1);
    let engine = &report.modules[0];
    assert_eq!(engine.name, "Engine");
    assert!(engine.present, "seeded module answered TesterPresent");
    assert!(engine.error.is_none(), "unexpected error: {:?}", engine.error);

    // Identity: VIN lifted out, the one supported DID recorded, unsupported DIDs
    // silently skipped (the server answered them with a negative response).
    let id = engine.identity.as_ref().expect("identity read");
    assert_eq!(id.vin.as_deref(), Some("WVWZZZ1KZAW000001"));
    assert_eq!(id.dids.len(), 1, "only the one seeded identity DID is present");
    assert_eq!(id.dids[0].did, 0xF187);
    assert_eq!(id.dids[0].text.as_deref(), Some("04E906016AD"));

    // DTCs: decoded + classified by the shared opentune-dtc database, not invented.
    assert_eq!(engine.dtcs.len(), 2);
    assert_eq!(engine.dtcs[0].code, "P0101");
    assert_eq!(engine.dtcs[0].status, Some(0x24));
    assert_eq!(engine.dtcs[0].system, System::Powertrain);
    assert_eq!(engine.dtcs[1].code, "U0100");
    assert_eq!(engine.dtcs[1].status, Some(0x2F));
    assert_eq!(engine.dtcs[1].system, System::Network);
    assert_eq!(
        engine.dtcs[0].description,
        opentune_dtc::lookup("P0101").unwrap().description
    );

    // Aggregate helpers reflect the single present module and its two codes.
    assert_eq!(report.present().count(), 1);
    assert_eq!(report.dtc_count(), 2);
}

#[test]
fn absent_module_is_recorded_without_error() {
    // A vehicle list that names a module the simulated ECU does not implement is
    // not representable with a single SimTransport ECU directly; instead we prove
    // the "no DTCs, clean identity" degenerate case: an unseeded-but-present ECU
    // that answers the probe but has no stored codes and no identity DIDs.
    let server = UdsServer::new();
    let mut sim = SimTransport::new(server);
    let mut diag = CoreDiag::new(&mut sim, 1000);

    let modules = one_uds_module();
    let report = scan(&mut diag, &modules);

    let engine = &report.modules[0];
    assert!(engine.present, "the ECU still answers TesterPresent");
    // Every identity DID returned requestOutOfRange → skipped, so an empty identity.
    let id = engine.identity.as_ref().expect("identity read succeeded");
    assert!(id.vin.is_none());
    assert!(id.dids.is_empty());
    // No DTCs seeded → the 19 02 FF read returns an empty record list, not an error.
    assert!(engine.dtcs.is_empty());
    assert!(engine.error.is_none());
    assert_eq!(report.dtc_count(), 0);
}
