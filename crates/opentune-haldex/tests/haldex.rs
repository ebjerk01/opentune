//! End-to-end tests of the Haldex engines against simulated units: the Gen4
//! read over a real `VwTp20` transport, and the OpenHaldex UDS client against
//! the independent `opentune-udssim` server and the OpenHaldex simulator.
use opentune_core::protocol::vwtp20::VwTp20;
use opentune_core::CoreDiag;
use opentune_haldex::gen4::{
    read_gen4, Gen4ReadOptions, GenerationHint, TP20_LOGICAL_ADDRESS, TP20_SETUP_ID,
    TP20_TESTER_RX_ID,
};
use opentune_haldex::openhaldex::{Mode, OhError, OpenHaldexUds, DID_LOCK, DID_SLIP};
use opentune_haldex::sim::{SimGen4, SimGen4Link, SimOpenHaldex, SIM_TESTER_TX_ID};
use opentune_udssim::server::UdsServer;
use opentune_udssim::transport::SimTransport;

#[test]
fn gen4_read_is_read_only_and_complete() {
    let mut unit = SimGen4::default();
    unit.dtcs = vec![(1234, 0x23)];
    let r = read_gen4(&mut unit, &Gen4ReadOptions::default());
    assert!(r.session_error.is_none());
    let id = r.identity.as_ref().unwrap();
    assert_eq!(id.part_number, "0AY907554C");
    assert_eq!(id.generation, GenerationHint::Gen4);
    let dtcs = r.dtcs.as_ref().unwrap();
    assert_eq!(dtcs.len(), 1);
    assert_eq!((dtcs[0].vag_number, dtcs[0].status), (1234, 0x23));
    // Groups 1, 3 decoded; 125 raw; 126 not implemented by the unit.
    assert_eq!(r.groups.len(), 4);
    let g1 = &r.groups[0];
    assert_eq!(g1.values[0].label, Some("oil temperature"));
    assert_eq!(g1.values[0].value, Some(45.0));
    assert!((g1.values[2].value.unwrap() - 13.8).abs() < 1e-9);
    let g3 = &r.groups[1];
    assert_eq!(g3.values.len(), 4);
    assert!((g3.values[3].value.unwrap() - 0.86).abs() < 1e-9);
    assert_eq!(r.groups[2].values[0].value, None);
    assert!(r.groups[3].error.is_some());
    // Only session, identification, read-DTC and measuring-block services.
    for sid in unit.services() {
        assert!(
            matches!(sid, 0x10 | 0x1A | 0x18 | 0x21),
            "non-read service 0x{sid:02X} sent"
        );
    }
}

#[test]
fn gen4_read_runs_over_vw_tp20_at_address_0a() {
    let link = SimGen4Link::new(SimGen4::default());
    let ids = link.tester_ids.clone();
    let unit = link.unit.clone();
    let mut tp = VwTp20::new(Box::new(link), TP20_LOGICAL_ADDRESS, TP20_TESTER_RX_ID);
    let r = read_gen4(&mut tp, &Gen4ReadOptions::default());
    assert!(r.responded(), "{r:?}");
    assert_eq!(r.identity.unwrap().component, "Haldex SIMULATED");
    assert_eq!(r.dtcs, Some(vec![]));
    assert_eq!(r.groups[1].values.len(), 4);
    tp.disconnect();
    // The tester only ever transmitted on the setup broadcast and its channel.
    assert!(ids
        .borrow()
        .iter()
        .all(|&id| id == TP20_SETUP_ID || id == SIM_TESTER_TX_ID));
    assert!(unit
        .borrow()
        .services()
        .iter()
        .all(|s| [0x10, 0x1A, 0x18, 0x21].contains(s)));
}

#[test]
fn gen4_wrong_address_gets_no_channel() {
    let link = SimGen4Link::new(SimGen4::default());
    let mut tp = VwTp20::new(Box::new(link), 0x01, TP20_TESTER_RX_ID);
    let r = read_gen4(&mut tp, &Gen4ReadOptions::default());
    assert!(!r.responded());
    assert!(r.identity_error.is_some());
}

#[test]
fn openhaldex_reads_against_udssim() {
    let mut server = UdsServer::new();
    server.set_did(DID_LOCK, vec![40, 0xB2, 3]);
    server.set_did(DID_SLIP, vec![0x02, 0xFE, 0x80, 0x80]);
    let mut sim = SimTransport::new(server);
    let mut diag = CoreDiag::new(&mut sim, 500);
    let mut oh = OpenHaldexUds::new(&mut diag);
    let lock = oh.read_lock().unwrap();
    assert_eq!(lock.commanded_pct, 40);
    assert_eq!(lock.engagement_raw, 0xB2);
    assert_eq!(lock.mode, Some(Mode::Split6040));
    let slip = oh.read_slip().unwrap();
    assert_eq!(
        (slip.fl, slip.fr, slip.rl, slip.rr),
        (Some(2), Some(-2), None, None)
    );
    // An unknown DID is a negative response, surfaced as such.
    let mut server = UdsServer::new();
    server.set_did(0xF190, b"X".to_vec());
    let mut sim = SimTransport::new(server);
    let mut diag = CoreDiag::new(&mut sim, 500);
    let mut oh = OpenHaldexUds::new(&mut diag);
    assert!(matches!(
        oh.read_lock(),
        Err(OhError::Negative { service: 0x22, .. })
    ));
}

#[test]
fn openhaldex_set_mode_confirms_by_read_back() {
    let mut unit = SimOpenHaldex::default();
    {
        let mut oh = OpenHaldexUds::new(&mut unit);
        let st = oh.set_mode(Mode::Split5050).unwrap();
        assert_eq!(st.mode, Some(Mode::Split5050));
        assert_eq!(st.commanded_pct, 100);
    }
    assert_eq!(unit.mode, Mode::Split5050);
    // A disabled controller ignores the request: the read-back catches it.
    let mut unit = SimOpenHaldex {
        controller_enabled: false,
        ..SimOpenHaldex::default()
    };
    let mut oh = OpenHaldexUds::new(&mut unit);
    assert!(matches!(oh.set_mode(Mode::Fwd), Err(OhError::Protocol(_))));
    // Status reads never write.
    let mut unit = SimOpenHaldex::default();
    let mut oh = OpenHaldexUds::new(&mut unit);
    oh.read_lock().unwrap();
    oh.read_slip().unwrap();
    assert!(unit.seen.iter().all(|r| r[0] == 0x22));
}
