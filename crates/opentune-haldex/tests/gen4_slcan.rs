//! End to end: the Haldex Gen4 read through the slcan driver, which opens a
//! VW TP2.0 channel (`transport=tp20`) over its ASCII line protocol to a
//! simulated adapter on a bus with a simulated Gen4 controller. Everything
//! between `read_gen4` and the simulated CAN frames is production code.
use opentune_core::protocol::diag::DiagTransport;
use opentune_core::protocol::tp20sim::{Tp20Ecu, Tp20EcuConfig};
use opentune_core::{ConnectConfig, Interface};
use opentune_driver_slcan::sim::SimSlcanPort;
use opentune_driver_slcan::SlcanDriver;
use opentune_haldex::gen4::{
    gen4_connect_config, read_gen4, Gen4ReadOptions, GenerationHint, TP20_LOGICAL_ADDRESS,
    TP20_SETUP_ID,
};
use opentune_haldex::sim::{SimGen4, SIM_TESTER_TX_ID};
use std::sync::{Arc, Mutex};

fn bus(unit: Arc<Mutex<SimGen4>>, cfg: Tp20EcuConfig) -> Arc<Mutex<Tp20Ecu>> {
    Tp20Ecu::new(
        Tp20EcuConfig {
            addr: TP20_LOGICAL_ADDRESS,
            tester_tx_id: SIM_TESTER_TX_ID,
            ..cfg
        },
        Box::new(move |req| {
            unit.lock()
                .unwrap()
                .request(req, 0)
                .unwrap_or_else(|_| vec![0x7F, req.first().copied().unwrap_or(0), 0x10])
        }),
    )
    .shared()
}

#[test]
fn gen4_read_over_simulated_slcan() {
    let mut unit = SimGen4::default();
    unit.dtcs = vec![(1234, 0x23)];
    let unit = Arc::new(Mutex::new(unit));
    // The unit asks for blocks of 2, so the tester's requests are ACK-gated too.
    let ecu = bus(
        unit.clone(),
        Tp20EcuConfig {
            block_size: 2,
            ..Tp20EcuConfig::default()
        },
    );
    let port = SimSlcanPort::tp20(ecu.clone());

    let mut drv = SlcanDriver::new();
    let cfg = gen4_connect_config(ConnectConfig::default()).unwrap();
    drv.open_with_port(Box::new(port.clone()), &cfg).unwrap();
    assert!(drv.info().display_name.contains("TP2.0 to 0x0A"));
    let r = read_gen4(drv.diag().unwrap(), &Gen4ReadOptions::default());
    drv.close().unwrap();

    assert!(r.responded(), "{r:?}");
    assert!(r.session_error.is_none());
    let id = r.identity.as_ref().unwrap();
    assert_eq!(id.part_number, "0AY907554C");
    assert_eq!(id.generation, GenerationHint::Gen4);
    assert_eq!(id.component, "Haldex SIMULATED");
    let dtcs = r.dtcs.as_ref().unwrap();
    assert_eq!((dtcs[0].vag_number, dtcs[0].status), (1234, 0x23));
    assert_eq!(r.groups[0].values[0].value, Some(45.0));
    assert_eq!(r.groups[1].values.len(), 4);
    assert!(r.groups[3].error.is_some(), "group 126 is not implemented");

    // Read-only: only session, identification, read-codes and measuring blocks.
    assert!(unit
        .lock()
        .unwrap()
        .services()
        .iter()
        .all(|s| [0x10, 0x1A, 0x18, 0x21].contains(s)));
    let e = ecu.lock().unwrap();
    assert!(e.errors.is_empty(), "{:?}", e.errors);
    assert_eq!(e.setups, 1);
    assert_eq!(e.disconnects, 1);
    // The tester only transmitted on the setup broadcast and its channel id.
    assert!(e
        .log
        .iter()
        .all(|(id, _)| *id == TP20_SETUP_ID || *id == SIM_TESTER_TX_ID));
    // On the wire: setup to 0x0A asking for replies on 0x300, then A0.
    let lines = port.lines();
    assert_eq!(lines[3], "t20070AC00010000301");
    assert!(lines[4].starts_with("t7646A00F"));
    assert_eq!(lines.last().unwrap(), "t7641A8");
}

#[test]
fn absent_gen4_fails_at_open_with_a_clear_reason() {
    let unit = Arc::new(Mutex::new(SimGen4::default()));
    let ecu = bus(
        unit,
        Tp20EcuConfig {
            silent: true,
            ..Tp20EcuConfig::default()
        },
    );
    let mut drv = SlcanDriver::new();
    let mut base = ConnectConfig::default();
    base.options.insert("tp20_timeout_ms".into(), "200".into());
    let cfg = gen4_connect_config(base).unwrap();
    let err = drv
        .open_with_port(Box::new(SimSlcanPort::tp20(ecu)), &cfg)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("0x0A") && err.contains("0x20A") && err.contains("step 1"),
        "{err}"
    );
}
