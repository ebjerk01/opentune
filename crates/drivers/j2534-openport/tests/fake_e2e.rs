//! The real `openport2` driver code end to end against the offline fake
//! OpenPort 2.0 (`opentune_driver_openport2::fake`), with the independent
//! `opentune-udssim` UDS server as the ECU behind the simulated CAN bus:
//! identify, block read (requestUpload), block write (requestDownload), the
//! filter / flow-control handling and the error paths. Every read path runs
//! with merged reads and with the device output split into small chunks.
//!
//! Offline only: none of this has touched a real cable (hardware validation
//! pending, docs/DRIVERS.md §3l).
use opentune_core::{ConnectConfig, Error, Interface, MemoryRegion, Protocol};
use opentune_driver_openport2::fake::{
    Chunking, EcuHandler, FakeConfig, FakeFaults, FakeHandle, FakeOp2, FakePort,
};
use opentune_driver_openport2::OpenPort2Driver;
use opentune_udssim::{ServerConfig, UdsServer};
use std::sync::{Arc, Mutex};

const VIN: &[u8] = b"OPENPORTFAKE00001";
const BASE: u32 = 0x0001_0000;

fn image(len: usize) -> Vec<u8> {
    (0..len as u32).map(|i| (i ^ (i >> 5)) as u8).collect()
}

fn server() -> Arc<Mutex<UdsServer>> {
    let mut s = UdsServer::with_config(ServerConfig {
        max_block_len: 0x0102,
        ..ServerConfig::default()
    });
    s.set_did(0xF190, VIN.to_vec());
    s.set_did(0xF187, b"OT-OP2-FAKE".to_vec());
    s.set_did(0xF18A, b"OPENTUNE".to_vec());
    s.set_memory(BASE as u64, image(0x1000));
    Arc::new(Mutex::new(s))
}

/// The UDS server as the ECU: the first request for `pending_sid` is answered
/// with one responsePending (0x78) before the real reply.
fn ecu(server: Arc<Mutex<UdsServer>>, pending_sid: Option<u8>) -> EcuHandler {
    let mut pending = pending_sid;
    Box::new(move |req: &[u8]| {
        let mut out = Vec::new();
        if pending.is_some() && pending == req.first().copied() {
            out.push(vec![0x7F, req[0], 0x78]);
            pending = None;
        }
        let mut s = server.lock().unwrap();
        if let Some(r) = s.respond(req) {
            out.push(r);
        }
        out
    })
}

fn open(
    cfg: &ConnectConfig,
    fake: FakeConfig,
    chunking: Chunking,
    server: Arc<Mutex<UdsServer>>,
) -> (Result<OpenPort2Driver, Error>, FakeHandle) {
    let (dev, handle) = FakeOp2::new(fake, ecu(server, Some(0x35)));
    let mut d = OpenPort2Driver::new();
    let r = d
        .open_with_port(Box::new(FakePort::new(dev, chunking)), cfg)
        .map(|_| d);
    (r, handle)
}

fn chunkings() -> Vec<Chunking> {
    vec![
        Chunking::Whole,
        Chunking::Bytes(1),
        Chunking::Bytes(7),
        Chunking::Pattern(vec![3, 64, 1, 17, 2]),
    ]
}

#[test]
fn identify_and_block_read_through_the_driver() {
    for chunking in chunkings() {
        let what = format!("{chunking:?}");
        let srv = server();
        let (d, h) = open(
            &ConnectConfig::default(),
            FakeConfig::default(),
            chunking,
            srv,
        );
        let mut d = d.unwrap_or_else(|e| panic!("{what}: open: {e}"));
        assert_eq!(d.info().firmware.as_deref(), Some("1.17.4877"), "{what}");
        assert_eq!(d.battery_millivolts().unwrap(), Some(12_480), "{what}");

        let id = d.read_ecu_id().unwrap();
        assert_eq!(id.vin.as_deref(), Some("OPENPORTFAKE00001"), "{what}");
        assert_eq!(id.supplier.as_deref(), Some("OPENTUNE"), "{what}");

        let mut calls = 0;
        let data = d
            .read_memory(MemoryRegion::new(BASE, 0x600), &mut |_| calls += 1)
            .unwrap();
        assert_eq!(data, image(0x1000)[..0x600], "{what}");
        assert!(calls >= 2, "{what}: progress reported");
        d.close().unwrap();

        let log = h.log();
        // The documented session: identify, reset, close-all, open raw CAN
        // (flags before baud, ignored 4th field), PASS filter on 0x7E8.
        assert_eq!(log.lines[0], "ati", "{what}");
        assert_eq!(log.lines[1], "atz 1", "{what}");
        assert_eq!(log.lines[2], "ata 2", "{what}");
        assert_eq!(log.lines[3], "ato5 0 500000 0 3", "{what}");
        assert_eq!(log.lines[4], "atf5 1 0 4 4", "{what}");
        assert_eq!(log.opened, vec![(5, 0, 500_000)], "{what}");
        let n = log.lines.len();
        assert!(log.lines[n - 2].starts_with("atc5 "), "{what}");
        assert!(log.lines[n - 1].starts_with("ata "), "{what}");
        assert!(!log.saw_bootloader);
        // Every host frame is a padded classic frame with a non-zero budget.
        let host: Vec<_> = log.bus.iter().filter(|f| f.from_host).collect();
        assert!(host.len() > 15, "{what}: {} frames", host.len());
        assert!(host.iter().all(|f| f.id == 0x7E0 && f.data.len() == 8));
        assert!(log
            .transmits
            .iter()
            .all(|&(p, fl, b)| p == 5 && fl == 0 && b == 100_000));
    }
}

#[test]
fn the_filter_is_a_pass_filter_on_the_response_id() {
    let (d, h) = open(
        &ConnectConfig::default(),
        FakeConfig::default(),
        Chunking::Whole,
        server(),
    );
    let mut d = d.unwrap();
    let f = &h.log().filters;
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].filter_type, 1);
    assert_eq!(f[0].mask, 0x7FFu32.to_be_bytes());
    assert_eq!(f[0].pattern, 0x7E8u32.to_be_bytes());
    d.close().unwrap();
    assert!(h.log().filters.is_empty(), "atc/ata drop the filter");
}

#[test]
fn background_traffic_is_filtered_by_the_cable() {
    // Another node broadcasts on 0x123 and a second ECU answers on 0x7E9:
    // the PASS filter keeps both off the USB link.
    let fake = FakeConfig {
        faults: FakeFaults {
            noise: vec![
                (0x123, vec![1, 2, 3, 4]),
                (0x7E9, vec![0x03, 0x7F, 0x22, 0x31]),
            ],
            ..FakeFaults::default()
        },
        ..FakeConfig::default()
    };
    let (d, _h) = open(
        &ConnectConfig::default(),
        fake,
        Chunking::Bytes(5),
        server(),
    );
    let mut d = d.unwrap();
    let id = d.read_ecu_id().unwrap();
    assert_eq!(id.vin.as_deref(), Some("OPENPORTFAKE00001"));
    let st = d.stats().unwrap();
    assert_eq!(st.foreign_frames, 0);
    // One transmit indication per frame sent, all dropped.
    assert!(st.indications >= 6, "{st:?}");
}

#[test]
fn frames_that_arrive_before_the_transmit_ack_are_kept() {
    // The ECU's answer (and its flow control) can overtake the `aro` of the
    // frame that triggered it; the old driver threw such frames away.
    for chunking in chunkings() {
        let fake = FakeConfig {
            faults: FakeFaults {
                frames_before_ack: true,
                ..FakeFaults::default()
            },
            ..FakeConfig::default()
        };
        let (d, _h) = open(&ConnectConfig::default(), fake, chunking, server());
        let mut d = d.unwrap();
        assert!(d.read_ecu_id().unwrap().vin.is_some());
        let data = d
            .read_memory(MemoryRegion::new(BASE + 0x100, 0x300), &mut |_| {})
            .unwrap();
        assert_eq!(data, image(0x1000)[0x100..0x400]);
    }
}

#[test]
fn ecu_block_size_flow_control_on_download_and_upload() {
    // The ECU asks for flow control after every 2 consecutive frames: the
    // host ISO-TP must wait for each FC on the long transferData requests.
    let srv = server();
    let fake = FakeConfig {
        ecu_block_size: 2,
        ..FakeConfig::default()
    };
    let (d, h) = open(
        &ConnectConfig::default(),
        fake,
        Chunking::Bytes(9),
        srv.clone(),
    );
    let mut d = d.unwrap();
    let payload: Vec<u8> = (0..0x200u32).map(|i| (i * 7) as u8).collect();
    d.write_memory(MemoryRegion::new(0x0002_0000, 0x200), &payload, &mut |_| {})
        .unwrap();
    assert_eq!(srv.lock().unwrap().downloaded(), Some(&payload[..]));
    let fcs = h
        .log()
        .bus
        .iter()
        .filter(|f| !f.from_host && f.data[0] == 0x30)
        .count();
    assert!(fcs > 10, "ECU flow controls seen: {fcs}");
    // And the reverse direction: the host's own FC (BS 0) lets the ECU send
    // whole 0x102-byte blocks.
    let back = d
        .read_memory(MemoryRegion::new(BASE, 0x300), &mut |_| {})
        .unwrap();
    assert_eq!(back, image(0x1000)[..0x300]);
    let host_fcs: Vec<_> = h
        .log()
        .bus
        .iter()
        .filter(|f| f.from_host && f.data[0] == 0x30)
        .map(|f| f.data.clone())
        .collect();
    assert!(!host_fcs.is_empty());
    assert!(host_fcs
        .iter()
        .all(|f| f[..3] == [0x30, 0, 0] && f.len() == 8));
}

#[test]
fn extended_29bit_ids_set_the_flags_everywhere() {
    let cfg = ConnectConfig {
        tx_id: 0x18DA_10F1,
        rx_id: 0x18DA_F110,
        ..ConnectConfig::default()
    };
    let fake = FakeConfig {
        ecu_request_id: 0x18DA_10F1,
        ecu_response_id: 0x18DA_F110,
        ..FakeConfig::default()
    };
    let (d, h) = open(&cfg, fake, Chunking::Pattern(vec![4, 13]), server());
    let mut d = d.unwrap();
    assert!(d.read_ecu_id().unwrap().vin.is_some());
    let log = h.log();
    assert_eq!(log.lines[3], "ato5 256 500000 0 3");
    assert_eq!(log.lines[4], "atf5 1 256 4 4");
    assert_eq!(log.filters[0].mask, 0x1FFF_FFFFu32.to_be_bytes());
    assert!(log.transmits.iter().all(|&(_, fl, _)| fl == 0x100));
    assert!(log
        .bus
        .iter()
        .filter(|f| f.from_host)
        .all(|f| f.id == 0x18DA_10F1));
}

#[test]
fn stale_replies_garbage_and_loopback_echoes_are_skipped() {
    let fake = FakeConfig {
        faults: FakeFaults {
            stale_replies: 6,
            garbage_on_open: b"\x00\x13arQ junk ar\xffxx\r\n".to_vec(),
            loopback_echo: true,
            ..FakeFaults::default()
        },
        ..FakeConfig::default()
    };
    for chunking in chunkings() {
        let (d, _h) = open(&ConnectConfig::default(), fake.clone(), chunking, server());
        let mut d = d.unwrap();
        assert!(d.read_ecu_id().unwrap().vin.is_some());
        let st = d.stats().unwrap();
        assert!(st.stale_replies >= 6, "{st:?}");
        assert!(st.garbage_bytes > 0, "{st:?}");
        assert!(st.indications > 0, "{st:?}");
    }
}

#[test]
fn device_errors_surface_with_their_j2534_names() {
    // A second program holds the channel: ato5 -> are 20.
    let fake = FakeConfig {
        faults: FakeFaults {
            error_reply: vec![("ato5".into(), 20)],
            ..FakeFaults::default()
        },
        ..FakeConfig::default()
    };
    let (d, h) = open(&ConnectConfig::default(), fake, Chunking::Whole, server());
    let e = d.err().expect("open must fail").to_string();
    assert!(
        e.contains("ERR_CHANNEL_IN_USE") && e.contains("ato5"),
        "{e}"
    );
    // The failed open still releases the cable.
    assert!(h.log().lines.last().unwrap().starts_with("ata "));

    // Nothing else on the bus acknowledges: every transmit answers are 9.
    let fake = FakeConfig {
        faults: FakeFaults {
            bus_no_ack: true,
            ..FakeFaults::default()
        },
        ..FakeConfig::default()
    };
    let (d, _h) = open(&ConnectConfig::default(), fake, Chunking::Whole, server());
    let e = d.unwrap().read_ecu_id().unwrap_err().to_string();
    assert!(
        e.contains("ERR_TIMEOUT") && e.contains("not acknowledged"),
        "{e}"
    );

    // A rejected filter.
    let fake = FakeConfig {
        faults: FakeFaults {
            error_reply: vec![("atf5".into(), 12)],
            ..FakeFaults::default()
        },
        ..FakeConfig::default()
    };
    let (d, _h) = open(&ConnectConfig::default(), fake, Chunking::Whole, server());
    let e = d.err().unwrap().to_string();
    assert!(e.contains("ERR_EXCEEDED_LIMIT"), "{e}");
}

#[test]
fn a_silent_device_times_out_instead_of_hanging() {
    // Not an OpenPort (or the SD card makes it a mass-storage device).
    let fake = FakeConfig {
        faults: FakeFaults {
            silent: vec!["ati".into()],
            ..FakeFaults::default()
        },
        ..FakeConfig::default()
    };
    let (d, h) = open(&ConnectConfig::default(), fake, Chunking::Whole, server());
    let e = d.err().unwrap().to_string();
    assert!(e.contains("no answer to `ati`"), "{e}");
    assert_eq!(h.log().lines, vec!["ati"], "nothing else is sent");

    // The filter command is never answered.
    let fake = FakeConfig {
        faults: FakeFaults {
            silent: vec!["atf5".into()],
            ..FakeFaults::default()
        },
        ..FakeConfig::default()
    };
    let (d, _h) = open(&ConnectConfig::default(), fake, Chunking::Whole, server());
    let e = d.err().unwrap().to_string();
    assert!(e.contains("did not answer `atf5`"), "{e}");

    // The ECU never answers: a CAN-level timeout, not a protocol error.
    let (dev, _h) = FakeOp2::new(FakeConfig::default(), Box::new(|_req: &[u8]| Vec::new()));
    let mut d = OpenPort2Driver::new();
    d.open_with_port(
        Box::new(FakePort::new(dev, Chunking::Whole)),
        &ConnectConfig::default(),
    )
    .unwrap();
    let e = d.read_ecu_id().unwrap_err();
    assert!(matches!(e, Error::Timeout(_)), "{e}");
}

#[test]
fn bad_settings_are_refused_before_anything_is_sent() {
    let bad: Vec<ConnectConfig> = vec![
        ConnectConfig {
            bitrate: 0,
            ..ConnectConfig::default()
        },
        ConnectConfig {
            bitrate: 123_456,
            ..ConnectConfig::default()
        },
        ConnectConfig {
            options: [("tx_timeout_ms".to_string(), "0".to_string())].into(),
            ..ConnectConfig::default()
        },
        ConnectConfig {
            options: [("bitrate".to_string(), "fast".to_string())].into(),
            ..ConnectConfig::default()
        },
        ConnectConfig {
            protocol: Protocol::Iso14230Kline,
            ..ConnectConfig::default()
        },
    ];
    for cfg in bad {
        let (d, h) = open(&cfg, FakeConfig::default(), Chunking::Whole, server());
        assert!(d.is_err(), "{cfg:?}");
        assert!(
            h.log().lines.is_empty(),
            "{cfg:?}: sent {:?}",
            h.log().lines
        );
    }
    // An option bit rate overrides the config field.
    let cfg = ConnectConfig {
        options: [("bitrate".to_string(), "250000".to_string())].into(),
        ..ConnectConfig::default()
    };
    let (d, h) = open(&cfg, FakeConfig::default(), Chunking::Whole, server());
    d.unwrap();
    assert_eq!(h.log().opened, vec![(5, 0, 250_000)]);
}

#[test]
fn not_connected_before_open() {
    let mut d = OpenPort2Driver::new();
    assert!(matches!(d.read_ecu_id(), Err(Error::NotConnected)));
    assert!(matches!(d.battery_millivolts(), Err(Error::NotConnected)));
    assert!(!d.capabilities().programming_voltage);
}
