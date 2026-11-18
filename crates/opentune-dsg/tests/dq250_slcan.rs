//! End to end through the slcan driver: it opens the VW TP2.0 channel
//! (`transport=tp20`, address 0x02) over its ASCII line protocol to a
//! simulated adapter on a bus with the scripted DQ250 responder.
use opentune_core::protocol::tp20sim::Tp20EcuConfig;
use opentune_core::{ConnectConfig, Interface};
use opentune_driver_slcan::sim::SimSlcanPort;
use opentune_driver_slcan::SlcanDriver;
use opentune_dsg::sim::{sim_bus, SimDq250};
use opentune_dsg::{
    dsg_connect_config, Dq250Reader, GroupLabels, DEFAULT_ID_OPTIONS, KWP_SESSION, READ_SERVICES,
};
use std::sync::{Arc, Mutex};

#[test]
fn dq250_read_over_simulated_slcan() {
    let unit = SimDq250 {
        dtcs: vec![(4321, 0x60)],
        ..SimDq250::default()
    };
    let unit = Arc::new(Mutex::new(unit));
    let ecu = sim_bus(unit.clone(), Tp20EcuConfig::default());
    let port = SimSlcanPort::tp20(ecu.clone());

    let mut drv = SlcanDriver::new();
    let cfg = dsg_connect_config(ConnectConfig::default()).unwrap();
    drv.open_with_port(Box::new(port.clone()), &cfg).unwrap();
    assert!(drv.info().display_name.contains("TP2.0 to 0x02"));
    {
        let mut r = Dq250Reader::start(drv.diag().unwrap(), Some(KWP_SESSION), 1000);
        assert!(r.session_error.is_none());
        let id = r.identify(&DEFAULT_ID_OPTIONS);
        assert_eq!(id.identity.unwrap().part_number, "02E000000X");
        assert_eq!(r.dtcs().unwrap()[0].vag_number, 4321);
        let g = r.group(1, &GroupLabels::default());
        assert_eq!(g.cells.len(), 4);
    }
    drv.close().unwrap();

    assert!(unit
        .lock()
        .unwrap()
        .services()
        .iter()
        .all(|s| READ_SERVICES.contains(s)));
    let e = ecu.lock().unwrap();
    assert!(e.errors.is_empty(), "{:?}", e.errors);
    assert_eq!((e.setups, e.disconnects), (1, 1));
    // Setup to 0x02 asking for replies on 0x300, A8 at the end.
    let lines = port.lines();
    assert!(
        lines.iter().any(|l| l == "t200702C00010000301"),
        "{lines:?}"
    );
    assert_eq!(lines.last().unwrap(), "t7401A8");
}
