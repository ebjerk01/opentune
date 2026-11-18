//! The DQ250 read over the core TP2.0 stack: `Tp20Channel` (setup, keep-alive,
//! teardown) on a `Tp20SimLink` to the core `Tp20Ecu` simulator, with a
//! scripted DQ250 responder behind it. Everything between `Dq250Reader` and
//! the simulated CAN frames is production code.
use opentune_core::protocol::tp20sim::Tp20EcuConfig;
use opentune_ctrldef::ControllerDef;
use opentune_dsg::sim::{open_sim_channel, sim_bus, SimDq250, SIM_TESTER_TX_ID};
use opentune_dsg::{
    Dq250Reader, FamilyHint, GroupLabels, MeaningState, StorageState, DEFAULT_ID_OPTIONS,
    KWP_SESSION, READ_SERVICES, TP20_LOGICAL_ADDRESS, TP20_SETUP_ID, TP20_TESTER_RX_ID,
};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn ctrldef() -> ControllerDef {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ctrldefs/vag-dq250-02e.json");
    ControllerDef::from_json(&std::fs::read_to_string(p).unwrap()).unwrap()
}

#[test]
fn full_read_is_read_only_and_tears_down() {
    let unit = SimDq250 {
        dtcs: vec![(12345, 0xE3), (100, 0x23)],
        ..SimDq250::default()
    };
    let unit = Arc::new(Mutex::new(unit));
    let ecu = sim_bus(
        unit.clone(),
        Tp20EcuConfig {
            block_size: 2,
            ..Tp20EcuConfig::default()
        },
    );
    let mut ch = open_sim_channel(ecu.clone(), 0).unwrap();
    assert!(ch.describe().contains("TP2.0 to 0x02"));
    let labels = GroupLabels::from_ctrldef(&ctrldef());
    {
        let mut r = Dq250Reader::start(&mut ch, Some(KWP_SESSION), 500);
        assert!(r.session_error.is_none());

        let id = r.identify(&DEFAULT_ID_OPTIONS);
        assert!(id.responded());
        let i = id.identity.as_ref().unwrap();
        assert_eq!(i.part_number, "02E000000X");
        assert_eq!(i.sw_version, "0000");
        assert_eq!(i.component, "SIMULATED DSG");
        assert_eq!(i.family, FamilyHint::Dq250_02e);
        assert_eq!(i.hardware_number.as_deref(), Some("02E000000Y"));
        // 0x97 is refused by the responder: recorded, not fatal.
        assert_eq!(i.system_name, None);
        assert_eq!(id.records.len(), 2);
        assert_eq!(id.errors.len(), 1);
        assert_eq!(id.errors[0].option, 0x97);

        let d = r.dtcs().unwrap();
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].vag_number, 12345);
        assert_eq!(d[0].status.storage, StorageState::Present);
        assert!(d[0].status.warning_lamp);
        assert_eq!(d[1].status.storage, StorageState::StoredNotPresent);

        let g1 = r.group(1, &labels);
        assert!(g1.error.is_none());
        assert_eq!(g1.cells.len(), 4);
        assert_eq!(g1.cells[0].value, Some(60.0));
        assert_eq!(g1.cells[3].value, None, "unknown formula stays raw");
        // The definition has no meanings yet: every cell is unconfirmed.
        assert!(g1
            .cells
            .iter()
            .all(|c| c.state == MeaningState::Unconfirmed && c.meaning.is_none()));
        let g9 = r.group(9, &labels);
        assert!(g9.error.as_deref().unwrap().contains("31"), "{g9:?}");
    }
    ch.close();

    // Read-only on the wire: only session / identification / DTC / group reads.
    for s in unit.lock().unwrap().services() {
        assert!(READ_SERVICES.contains(&s), "non-read service 0x{s:02X}");
    }
    let e = ecu.lock().unwrap();
    assert!(e.errors.is_empty(), "{:?}", e.errors);
    assert_eq!((e.setups, e.disconnects), (1, 1));
    assert!(e
        .log
        .iter()
        .all(|(id, _)| *id == TP20_SETUP_ID || *id == SIM_TESTER_TX_ID));
    // Setup to 0x02 asking for replies on 0x300.
    assert_eq!(
        e.log[0],
        (
            TP20_SETUP_ID,
            vec![TP20_LOGICAL_ADDRESS, 0xC0, 0x00, 0x10, 0x00, 0x03, 0x01]
        )
    );
    assert_eq!(e.ecu_tx_id(), TP20_TESTER_RX_ID);
}

#[test]
fn keep_alive_holds_the_channel_between_reads() {
    let unit = Arc::new(Mutex::new(SimDq250::default()));
    let ecu = sim_bus(
        unit,
        Tp20EcuConfig {
            idle_timeout: Some(Duration::from_millis(150)),
            ..Tp20EcuConfig::default()
        },
    );
    let mut ch = open_sim_channel(ecu.clone(), 30).unwrap();
    {
        let mut r = Dq250Reader::start(&mut ch, Some(KWP_SESSION), 500);
        assert!(r.group(2, &GroupLabels::default()).error.is_none());
        // Idle for longer than the unit's channel timeout: only the keep-alive
        // keeps the channel (and the session) alive.
        std::thread::sleep(Duration::from_millis(400));
        let g = r.group(2, &GroupLabels::default());
        assert!(g.error.is_none(), "{g:?}");
        assert_eq!(g.cells.len(), 3);
    }
    assert!(ch.keep_alives() > 0);
    ch.close();
    let e = ecu.lock().unwrap();
    assert!(e.keep_alives > 0);
    assert_eq!(e.disconnects, 1);
    assert!(e.errors.is_empty(), "{:?}", e.errors);
}

#[test]
fn refused_session_and_missing_options_are_tolerated() {
    let mut unit = SimDq250 {
        session: None,
        require_session: false,
        ..SimDq250::default()
    };
    unit.ids.remove(&0x9B);
    let unit = Arc::new(Mutex::new(unit));
    let mut ch = open_sim_channel(sim_bus(unit.clone(), Tp20EcuConfig::default()), 0).unwrap();
    let mut r = Dq250Reader::start(&mut ch, Some(KWP_SESSION), 500);
    assert!(r.session_error.is_some());
    // 0x9B and 0x97 refused, 0x91 answers; an out-of-range option is not sent.
    let id = r.identify(&[0x9B, 0x91, 0x97, 0x31]);
    let i = id.identity.unwrap();
    assert!(i.part_number.is_empty());
    assert_eq!(i.hardware_number.as_deref(), Some("02E000000Y"));
    assert_eq!(id.errors.len(), 3);
    assert!(id.errors[2].error.contains("not sent"));
    assert_eq!(r.dtcs().unwrap(), vec![]);
    drop(r);
    ch.close();
    assert!(!unit.lock().unwrap().seen.iter().any(|q| q == &[0x1A, 0x31]));
}

#[test]
fn absent_gearbox_fails_at_channel_setup() {
    let unit = Arc::new(Mutex::new(SimDq250::default()));
    let ecu = sim_bus(
        unit,
        Tp20EcuConfig {
            silent: true,
            ..Tp20EcuConfig::default()
        },
    );
    let err = open_sim_channel(ecu, 0).err().unwrap().to_string();
    assert!(err.contains("0x02"), "{err}");
}

#[test]
fn ctrldef_channel_matches_the_engine() {
    let def = ctrldef();
    let tp = def
        .diagnostics
        .iter()
        .find(|c| c.name == "tp20")
        .expect("tp20 channel");
    assert_eq!(tp.tp20_address, Some(TP20_LOGICAL_ADDRESS));
    assert_eq!(tp.rx_id, Some(TP20_TESTER_RX_ID));
    assert_eq!(tp.session, Some(KWP_SESSION));
    assert_eq!(tp.identification.as_deref(), Some("1A9B"));
    // Measuring-group meanings are parked: none may be in the definition
    // until a real capture backs them (then update this test with them).
    assert!(GroupLabels::from_ctrldef(&def).is_empty());

    // The hook: a definition's kwp_measuring_block entry labels its cell.
    let mut v: serde_json::Value =
        serde_json::from_str(&serde_json::to_string(&def).unwrap()).unwrap();
    v["live"] = serde_json::json!([{
        "name": "example", "unit": "degC",
        "source": {"kind": "kwp_measuring_block", "group": 1, "cell": 0},
        "notes": "test only"
    }]);
    let def2 = ControllerDef::from_json(&v.to_string()).unwrap();
    let labels = GroupLabels::from_ctrldef(&def2);
    let g = opentune_dsg::decode_group(1, &[0x1A, 0, 60, 0x1A, 0, 61], &labels);
    assert_eq!(g.cells[0].state, MeaningState::FromDefinition);
    assert_eq!(g.cells[0].meaning.as_ref().unwrap().source, "vag-dq250-02e");
    assert_eq!(g.cells[1].state, MeaningState::Unconfirmed);
}
