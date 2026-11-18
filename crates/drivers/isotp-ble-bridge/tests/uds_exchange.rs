//! UDS over the bridge protocol, end to end against the simulated firmware:
//! `UdsClient` / the `Interface` pipeline → driver → codec (split frames over
//! BLE, a chopped byte stream over serial) → mock bridge → an independent UDS
//! server (`opentune-udssim`) as the ECU.
use opentune_core::protocol::uds::UdsClient;
use opentune_core::{ConnectConfig, Error, Interface, MemoryRegion};
use opentune_driver_isotp_ble::codec::{self, Setting, FLAG_SPLIT};
use opentune_driver_isotp_ble::mock::{MockBridge, MockHandle};
use opentune_driver_isotp_ble::{IsoTpBleDriver, Link};
use opentune_udssim::server::{ServerConfig, UdsServer};
use std::time::Duration;

const VIN: &[u8] = b"WVWZZZ3CZ9E000000";

fn server(max_block_len: u32) -> UdsServer {
    let mut s = UdsServer::with_config(ServerConfig {
        max_block_len,
        ..ServerConfig::default()
    });
    s.set_did(0xF190, VIN.to_vec());
    s.set_did(
        0xF1A0,
        (0..300u32).map(|i| (i * 13) as u8).collect::<Vec<_>>(),
    );
    s.set_memory(
        0x8000_0000,
        (0..9000u32)
            .map(|i| (i ^ (i >> 7)) as u8)
            .collect::<Vec<_>>(),
    );
    s
}

/// The UDS server as a mock ECU: interposes its `responsePending` frames.
fn ecu(mut s: UdsServer) -> impl FnMut(u16, &[u8]) -> Vec<Vec<u8>> + Send {
    move |_tx, req| {
        let Some(resp) = s.respond(req) else {
            return Vec::new();
        };
        let mut out = vec![vec![0x7F, req[0], 0x78]; s.take_pending()];
        out.push(resp);
        out
    }
}

fn open(link: MockBridge, cfg: &ConnectConfig) -> IsoTpBleDriver {
    let mut d = IsoTpBleDriver::new();
    d.open_with_link(Box::new(link), cfg).expect("open");
    d
}

fn transports(max_block_len: u32) -> Vec<(&'static str, MockBridge, MockHandle)> {
    let (b20, h20) = MockBridge::ble(20, ecu(server(max_block_len)));
    let (b244, h244) = MockBridge::ble(244, ecu(server(max_block_len)));
    let (ser, hser) = MockBridge::serial(ecu(server(max_block_len)));
    vec![
        ("ble mtu 23", b20, h20),
        ("ble mtu 247", b244, h244),
        ("serial", ser, hser),
    ]
}

#[test]
fn read_ecu_id_and_a_multi_frame_did() {
    for (what, link, handle) in transports(0x0402) {
        let mut d = open(link, &ConnectConfig::default());
        let id = d.read_ecu_id().unwrap();
        assert_eq!(id.vin.as_deref(), Some("WVWZZZ3CZ9E000000"), "{what}");
        let t = d.diag().expect("diag transport");
        let mut uds = UdsClient::new(t);
        let big = uds.read_data_by_id(0xF1A0).unwrap();
        assert_eq!(big.len(), 300, "{what}");
        assert_eq!(big[299], (299u32 * 13) as u8, "{what}");
        // Unknown DID -> negative response surfaces as an error, not a hang.
        assert!(matches!(
            uds.read_data_by_id(0x1234),
            Err(Error::NegativeResponse { .. })
        ));

        let st = handle.state();
        assert_eq!(st.codec_errors, 0, "{what}");
        assert!(st.requests.iter().all(|(tx, _)| *tx == 0x7E0));
        if what == "ble mtu 23" {
            assert!(st.writes.iter().all(|w| w.len() <= 20));
            // The 300-byte DID came back split: first chunk carries the flag.
            assert!(st
                .notifications
                .iter()
                .any(|n| n[0] == codec::HEADER_ID && n[1] & FLAG_SPLIT != 0));
            assert!(st.notifications.iter().any(|n| n[0] == codec::PARTIAL_ID));
        }
    }
}

#[test]
fn block_read_and_write_with_4k_iso_tp_messages() {
    // maxNumberOfBlockLength 4093 -> 4095-byte uploads and downloads, the
    // largest message the firmware's ISO-TP buffer takes.
    for (what, link, _handle) in transports(0x0FFD) {
        let mut d = open(link, &ConnectConfig::default());
        let region = MemoryRegion::new(0x8000_0000, 9000);
        let mut calls = 0;
        let data = d.read_memory(region, &mut |_| calls += 1).unwrap();
        assert_eq!(data.len(), 9000, "{what}");
        assert!(data
            .iter()
            .enumerate()
            .all(|(i, b)| *b == (i ^ (i >> 7)) as u8));
        assert!(calls >= 3, "{what}: progress reported");

        let image: Vec<u8> = (0..6000u32).map(|i| (i * 7 + 3) as u8).collect();
        let target = MemoryRegion::new(0x9000_0000, image.len() as u32);
        d.write_memory(target, &image, &mut |_| {}).unwrap();
        let back = d.read_memory(target, &mut |_| {}).unwrap();
        assert_eq!(back, image, "{what}");
    }
}

#[test]
fn response_pending_is_drained_and_stale_answers_are_skipped() {
    let script = |_tx: u16, req: &[u8]| -> Vec<Vec<u8>> {
        match req {
            // A routine that keeps the tester waiting.
            [0x31, 0x01, 0x02, 0x03] => vec![
                vec![0x7F, 0x31, 0x78],
                vec![0x7F, 0x31, 0x78],
                vec![0x71, 0x01, 0x02, 0x03, 0x00],
            ],
            // A late answer to some earlier request arrives first.
            [0x22, 0xF1, 0x90] => vec![vec![0x50, 0x03, 0x00, 0x32, 0x01, 0xF4], {
                let mut v = vec![0x62, 0xF1, 0x90];
                v.extend_from_slice(VIN);
                v
            }],
            _ => vec![vec![0x7F, req[0], 0x11]],
        }
    };
    let (link, handle) = MockBridge::ble(20, script);
    let mut d = open(link, &ConnectConfig::default());
    let mut uds = UdsClient::new(d.diag().unwrap());
    let r = uds.routine_control(0x01, 0x0203, &[]).unwrap();
    // The client strips the echoed control type + routine id.
    assert_eq!(r, vec![0x00]);
    assert_eq!(uds.read_data_by_id(0xF190).unwrap(), VIN);
    assert_eq!(handle.state().requests.len(), 2, "request sent once each");
    assert!(d.bridge().unwrap().unmatched() >= 1);
}

#[test]
fn other_links_and_unsupported_ids() {
    // The Haldex link (7E5/7ED) is in the firmware's table.
    let (link, handle) = MockBridge::ble(20, |tx: u16, req: &[u8]| {
        assert_eq!(tx, 0x7E5);
        vec![[&[req[0] + 0x40][..], &req[1..]].concat()]
    });
    let cfg = ConnectConfig {
        tx_id: 0x7E5,
        rx_id: 0x7ED,
        ..ConnectConfig::default()
    };
    let mut d = open(link, &cfg);
    let mut uds = UdsClient::new(d.diag().unwrap());
    uds.tester_present().unwrap();
    assert_eq!(handle.state().requests[0], (0x7E5, vec![0x3E, 0x00]));

    // A pair outside the table is refused at open …
    let (link, _) = MockBridge::ble(20, |_: u16, _: &[u8]| Vec::new());
    let mut bad = ConnectConfig {
        tx_id: 0x710,
        rx_id: 0x77A,
        ..ConnectConfig::default()
    };
    let e = IsoTpBleDriver::new()
        .open_with_link(Box::new(link), &bad)
        .unwrap_err();
    assert!(matches!(e, Error::Config(_)));
    // … and with any_ids=1 the stock firmware drops it, so it times out.
    bad.options.insert("any_ids".into(), "1".into());
    let (link, handle) = MockBridge::ble(20, |_: u16, _: &[u8]| vec![vec![0x7E, 0x00]]);
    let mut d = open(link, &bad);
    let mut uds = UdsClient::new(d.diag().unwrap());
    uds.timeout_ms = 20;
    assert!(matches!(uds.tester_present(), Err(Error::Timeout(_))));
    assert_eq!(handle.state().dropped, vec![(0x710, 0x77A, 2)]);
}

#[test]
fn settings_over_ble_and_serial() {
    for (what, link, handle) in transports(0x0402) {
        let mut cfg = ConnectConfig::default();
        cfg.options.insert("stmin_us".into(), "500".into());
        let mut d = open(link, &cfg);
        assert_eq!(
            handle.state().stmin_us.get(&(0x7E0, 0x7E8)),
            Some(&500),
            "{what}"
        );
        let b = d.bridge().unwrap();
        let t = Duration::from_millis(50);
        // Over BLE the firmware zeroes reply flags; matching falls back to ids.
        assert_eq!(
            b.get_setting(Setting::IsoTpStminUs, 0x7E0, 0x7E8, t)
                .unwrap(),
            500u16.to_le_bytes(),
            "{what}"
        );
        assert_eq!(
            b.get_setting(Setting::GapName, 0, 0, t).unwrap(),
            codec::DEFAULT_DEVICE_NAME.as_bytes(),
            "{what}"
        );
        b.send_packet(&codec::Packet::set_gap_name("OT_BRIDGE").unwrap())
            .unwrap();
        b.set_setting(Setting::BleSendDelay, 0, 0, &3u16.to_le_bytes())
            .unwrap();
        assert_eq!(
            b.get_setting(Setting::GapName, 0, 0, t).unwrap(),
            b"OT_BRIDGE",
            "{what}"
        );
        assert_eq!(
            b.get_setting(Setting::BleSendDelay, 0, 0, t).unwrap(),
            [3, 0],
            "{what}"
        );
        assert!(matches!(
            b.get_setting(Setting::Password, 0, 0, t),
            Err(Error::Unsupported(_))
        ));
        // UDS still works after the settings traffic.
        let mut uds = UdsClient::new(d.diag().unwrap());
        assert_eq!(uds.read_data_by_id(0xF190).unwrap(), VIN, "{what}");
    }
}

#[test]
fn long_settings_frames_at_the_default_mtu() {
    let (link, handle) = MockBridge::ble(20, |_: u16, _: &[u8]| Vec::new());
    let mut d = open(link, &ConnectConfig::default());
    let b = d.bridge().unwrap();
    // 8 + 14 bytes needs a split; the GAP name's number survives the split
    // flag (8 | 0x08 == 8), so the firmware still files it correctly.
    b.send_packet(&codec::Packet::set_gap_name("FOURTEEN_CHARS").unwrap())
        .unwrap();
    assert_eq!(
        handle.state().settings.get(&(Setting::GapName as u8)),
        Some(&b"FOURTEEN_CHARS".to_vec())
    );
    // A long password would be filed under setting 15: refused up front.
    let pw = codec::Packet::set_setting(Setting::Password, 0, 0, &[b'x'; 30]).unwrap();
    assert!(matches!(b.send_packet(&pw), Err(Error::Unsupported(_))));
    assert_eq!(
        handle.state().writes.len(),
        2,
        "only the two GAP chunks went out"
    );
}

#[test]
fn persist_mode_frames_reach_the_device() {
    let (link, handle) = MockBridge::ble(20, |_: u16, _: &[u8]| Vec::new());
    let mut d = open(link, &ConnectConfig::default());
    let b = d.bridge().unwrap();
    b.send_packet(&codec::Packet::persist_add(0x7E0, 0x7E8, &[0x22, 0xF4, 0x0C]).unwrap())
        .unwrap();
    b.send_packet(&codec::Packet::persist_start()).unwrap();
    let st = handle.state();
    assert!(st.persist_enabled);
    assert_eq!(st.persist, vec![(0x7E0, 0x7E8, vec![0x22, 0xF4, 0x0C])]);
    b.send_packet(&codec::Packet::persist_stop(true)).unwrap();
    let st = handle.state();
    assert!(!st.persist_enabled);
    assert!(st.persist.is_empty());
}

#[test]
fn the_mock_rejects_writes_longer_than_the_att_payload() {
    let (mut link, _) = MockBridge::ble(20, |_: u16, _: &[u8]| Vec::new());
    assert!(link.write(&[0xF1; 21]).is_err());
}
