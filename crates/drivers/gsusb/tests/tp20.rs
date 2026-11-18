//! The gsusb driver opening a VW TP2.0 channel through its frame layer
//! (`open_with_link` with the core `Tp20SimLink` onto a simulated module) —
//! the same transport selection `open` runs over a real CAN interface.
use opentune_core::protocol::tp20channel::CanTransportKind;
use opentune_core::protocol::tp20sim::{Tp20Ecu, Tp20EcuConfig, Tp20SimLink};
use opentune_core::{ConnectConfig, Error, Interface, MemoryRegion};
use opentune_driver_gsusb::GsUsbDriver;
use std::sync::{Arc, Mutex};

fn kwp(req: &[u8]) -> Vec<u8> {
    match req {
        [0x10, s] => vec![0x50, *s],
        [0x21, g] => {
            let mut r = vec![0x61, *g];
            r.extend((0u8..30).map(|i| i.wrapping_mul(7)));
            r
        }
        [sid, ..] => vec![0x7F, *sid, 0x11],
        [] => vec![0x7F, 0, 0x13],
    }
}

fn module(cfg: Tp20EcuConfig) -> Arc<Mutex<Tp20Ecu>> {
    Tp20Ecu::new(
        Tp20EcuConfig {
            addr: 0x02,
            tester_tx_id: 0x740,
            ..cfg
        },
        Box::new(kwp),
    )
    .shared()
}

fn tp20_cfg() -> ConnectConfig {
    let mut c = ConnectConfig::default();
    for (k, v) in [
        ("transport", "tp20"),
        ("tp20_addr", "0x02"),
        ("tp20_keepalive_ms", "0"),
        ("tp20_timeout_ms", "200"),
        ("tp20_block_size", "2"),
    ] {
        c.options.insert(k.into(), v.into());
    }
    c
}

#[test]
fn tp20_channel_through_the_frame_layer() {
    let ecu = module(Tp20EcuConfig::default());
    let mut d = GsUsbDriver::new();
    assert!(d.capabilities().vwtp20);
    // A busy bus: unrelated traffic precedes every module frame.
    let link = Tp20SimLink::new(ecu.clone()).with_noise(0x2C0, &[0, 0x80, 0, 0, 0x50, 0, 0, 0]);
    d.open_with_link(Box::new(link), &tp20_cfg()).unwrap();
    assert_eq!(d.transport_kind(), Some(CanTransportKind::Tp20));
    assert!(d.info().display_name.contains("TP2.0 to 0x02"));

    let t = d.diag().unwrap();
    assert_eq!(t.request(&[0x10, 0x89], 500).unwrap(), vec![0x50, 0x89]);
    // 32-byte measuring-block reply: 5 frames in blocks of 2.
    let g = t.request(&[0x21, 0x07], 500).unwrap();
    assert_eq!(g.len(), 32);
    assert_eq!(&g[..2], &[0x61, 0x07]);
    {
        let e = ecu.lock().unwrap();
        assert!(e.errors.is_empty(), "{:?}", e.errors);
        assert_eq!(e.log[0].0, 0x200);
        assert_eq!(e.log[0].1[0], 0x02);
    }
    assert!(matches!(d.read_ecu_id(), Err(Error::Unsupported(_))));
    assert!(matches!(
        d.write_memory(MemoryRegion::new(0, 1), &[0], &mut |_| {}),
        Err(Error::Unsupported(_))
    ));

    d.close().unwrap();
    let e = ecu.lock().unwrap();
    assert_eq!(e.disconnects, 1);
    assert_eq!(e.log.last().unwrap(), &(0x740, vec![0xA8]));
}

#[test]
fn silent_module_is_a_clear_open_error() {
    let ecu = module(Tp20EcuConfig {
        silent: true,
        ..Tp20EcuConfig::default()
    });
    let mut d = GsUsbDriver::new();
    let err = d
        .open_with_link(Box::new(Tp20SimLink::new(ecu)), &tp20_cfg())
        .unwrap_err()
        .to_string();
    for want in ["gsusb", "step 1", "0x02", "0x202", "0x200"] {
        assert!(err.contains(want), "missing {want:?} in: {err}");
    }
    assert!(d.diag().is_none());
}

#[test]
fn default_config_stays_isotp() {
    let ecu = module(Tp20EcuConfig::default());
    let mut d = GsUsbDriver::new();
    d.open_with_link(
        Box::new(Tp20SimLink::new(ecu.clone())),
        &ConnectConfig::default(),
    )
    .unwrap();
    assert_eq!(d.transport_kind(), Some(CanTransportKind::IsoTp));
    assert!(ecu.lock().unwrap().log.is_empty());
}
