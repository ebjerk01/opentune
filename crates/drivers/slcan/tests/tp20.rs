//! The slcan driver opening a VW TP2.0 channel through its real line protocol
//! (`SimSlcanPort` scripts the adapter, `tp20sim::Tp20Ecu` the module).
use opentune_core::protocol::tp20channel::CanTransportKind;
use opentune_core::protocol::tp20sim::{Tp20Ecu, Tp20EcuConfig};
use opentune_core::{ConnectConfig, Error, Interface, MemoryRegion};
use opentune_driver_slcan::sim::SimSlcanPort;
use opentune_driver_slcan::SlcanDriver;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn kwp(req: &[u8]) -> Vec<u8> {
    match req {
        [0x10, s] => vec![0x50, *s],
        [0x1A, 0x9B] => {
            let mut r = vec![0x5A, 0x9B];
            r.extend_from_slice(b"0AY907554C  9999");
            r.extend_from_slice(&[0; 10]);
            r.extend_from_slice(b"Haldex SIMULATED    ");
            r
        }
        [sid, ..] => vec![0x7F, *sid, 0x11],
        [] => vec![0x7F, 0, 0x13],
    }
}

fn module(cfg: Tp20EcuConfig) -> Arc<Mutex<Tp20Ecu>> {
    Tp20Ecu::new(
        Tp20EcuConfig {
            addr: 0x0A,
            tester_tx_id: 0x764,
            ..cfg
        },
        Box::new(kwp),
    )
    .shared()
}

fn cfg(opts: &[(&str, &str)]) -> ConnectConfig {
    let mut c = ConnectConfig::default();
    for (k, v) in opts {
        c.options.insert((*k).into(), (*v).into());
    }
    c
}

const TP20: &[(&str, &str)] = &[
    ("transport", "tp20"),
    ("tp20_addr", "0x0A"),
    ("tp20_keepalive_ms", "0"),
    ("tp20_timeout_ms", "200"),
    ("tp20_block_size", "3"),
];

#[test]
fn opens_a_tp20_channel_over_the_slcan_line_protocol() {
    let ecu = module(Tp20EcuConfig::default());
    let port = SimSlcanPort::tp20(ecu.clone());
    let mut d = SlcanDriver::new();
    assert!(d.capabilities().vwtp20);
    d.open_with_port(Box::new(port.clone()), &cfg(TP20))
        .unwrap();
    assert_eq!(d.transport_kind(), Some(CanTransportKind::Tp20));
    assert!(d.info().display_name.contains("TP2.0 to 0x0A"));

    let lines = port.lines();
    // Adapter setup, then the channel setup broadcast and the A0, on the wire.
    assert_eq!(&lines[..3], &["C", "S6", "O"]);
    assert_eq!(lines[3], "t20070AC00010000301");
    assert_eq!(lines[4], "t7646A0038AFF00FF");

    let t = d.diag().expect("diag() exposes the TP2.0 channel");
    assert_eq!(t.request(&[0x10, 0x89], 500).unwrap(), vec![0x50, 0x89]);
    let id = t.request(&[0x1A, 0x9B], 500).unwrap();
    assert_eq!(&id[2..12], b"0AY907554C");
    // Session reply (seq 0) ACKed with B1; the 48-byte identification reply
    // (8 frames, seq 1..8) after 3 and 6 frames and the last.
    let acks: Vec<String> = port
        .lines()
        .into_iter()
        .filter(|l| l.starts_with("t7641B"))
        .collect();
    assert_eq!(acks, vec!["t7641B1", "t7641B4", "t7641B7", "t7641B9"]);
    assert!(ecu.lock().unwrap().errors.is_empty());

    // UDS block flows are refused on a KWP2000 channel.
    assert!(matches!(d.read_ecu_id(), Err(Error::Unsupported(_))));
    assert!(matches!(
        d.read_memory(MemoryRegion::new(0, 16), &mut |_| {}),
        Err(Error::Unsupported(_))
    ));

    d.close().unwrap();
    assert_eq!(port.lines().last().unwrap(), "t7641A8");
    assert_eq!(ecu.lock().unwrap().disconnects, 1);
    assert!(d.diag().is_none());
}

#[test]
fn reply_acks_go_out_at_block_ends() {
    let ecu = module(Tp20EcuConfig::default());
    let port = SimSlcanPort::tp20(ecu.clone());
    let mut d = SlcanDriver::new();
    d.open_with_port(Box::new(port.clone()), &cfg(TP20))
        .unwrap();
    d.diag().unwrap().request(&[0x1A, 0x9B], 500).unwrap();
    let acks: Vec<String> = port
        .lines()
        .into_iter()
        .filter(|l| l.starts_with("t7641B"))
        .collect();
    // Reply frames carry seq 0..7: ACKs name the next seq after 3, 6, 8 frames.
    assert_eq!(acks, vec!["t7641B3", "t7641B6", "t7641B8"]);
}

#[test]
fn keep_alive_runs_over_slcan_during_a_pause() {
    let ecu = module(Tp20EcuConfig {
        idle_timeout: Some(Duration::from_millis(400)),
        ..Tp20EcuConfig::default()
    });
    let port = SimSlcanPort::tp20(ecu.clone());
    let mut d = SlcanDriver::new();
    let mut opts = TP20.to_vec();
    opts.retain(|(k, _)| *k != "tp20_keepalive_ms");
    opts.push(("tp20_keepalive_ms", "50"));
    d.open_with_port(Box::new(port.clone()), &cfg(&opts))
        .unwrap();
    std::thread::sleep(Duration::from_millis(1000));
    let r = d.diag().unwrap().request(&[0x10, 0x89], 500).unwrap();
    assert_eq!(r, vec![0x50, 0x89]);
    assert!(port.lines().iter().filter(|l| *l == "t7641A3").count() >= 3);
    assert_eq!(ecu.lock().unwrap().setups, 1);
    d.close().unwrap();
}

#[test]
fn silent_module_fails_open_naming_step_and_ids() {
    let ecu = module(Tp20EcuConfig {
        silent: true,
        ..Tp20EcuConfig::default()
    });
    let mut d = SlcanDriver::new();
    let err = d
        .open_with_port(Box::new(SimSlcanPort::tp20(ecu)), &cfg(TP20))
        .unwrap_err()
        .to_string();
    for want in ["slcan", "step 1", "0x0A", "0x20A", "0x200"] {
        assert!(err.contains(want), "missing {want:?} in: {err}");
    }
    assert!(d.diag().is_none());
}

#[test]
fn default_stays_isotp() {
    let ecu = module(Tp20EcuConfig::default());
    let port = SimSlcanPort::tp20(ecu.clone());
    let mut d = SlcanDriver::new();
    d.open_with_port(Box::new(port.clone()), &cfg(&[])).unwrap();
    assert_eq!(d.transport_kind(), Some(CanTransportKind::IsoTp));
    assert_eq!(port.lines(), vec!["C", "S6", "O"]);
    assert_eq!(d.info().display_name, "slcan USB-CAN (CANable/CANtact)");
    assert!(ecu.lock().unwrap().log.is_empty());
}

#[test]
fn bad_transport_options_are_refused_before_the_adapter_is_touched() {
    let port = SimSlcanPort::new(Box::new(|_, _| Vec::new()));
    let mut d = SlcanDriver::new();
    for opts in [
        &[("transport", "canfd")][..],
        &[("transport", "tp20")][..],
        &[("tp20_addr", "0x0A")][..],
    ] {
        let r = d.open_with_port(Box::new(port.clone()), &cfg(opts));
        assert!(matches!(r, Err(Error::Config(_))), "{opts:?}");
    }
    assert!(port.lines().is_empty());
}
