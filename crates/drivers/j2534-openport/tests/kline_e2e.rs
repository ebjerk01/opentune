//! The `openport2` driver's K-line path (ISO 14230 channel 4, ISO 9141
//! channel 3) end to end against the offline fake OpenPort 2.0, with the
//! fake's **synthetic** KWP2000 ECU (made-up identity, no real ECU data):
//! fast and five-baud init, identify and reads through the core
//! `Kwp2000Client`, timing options, the read-only allow-list, timeouts and
//! the receive-record layouts. Plus the receive-only raw-CAN capture
//! (`can_monitor`). Offline only: the K-line replies are modelled from the
//! public notes (docs/DRIVERS.md §3l); only a cable and an ECU can confirm
//! them.
use opentune_core::protocol::kwp2000::Kwp2000Client;
use opentune_core::{
    CanMonitor, ConnectConfig, DiagLayer, Error, Interface, MemoryRegion, MonitorOpts, Protocol,
};
use opentune_driver_openport2::fake::{
    synthetic_kwp_ecu, Chunking, FakeConfig, FakeFaults, FakeHandle, FakeOp2, FakePort,
    KLineEcuHeader, KLineRxLayout,
};
use opentune_driver_openport2::OpenPort2Driver;

fn kcfg(opts: &[(&str, &str)]) -> ConnectConfig {
    let mut c = ConnectConfig {
        tx_id: 0x01,
        rx_id: 0xF1,
        ..ConnectConfig::default()
    };
    c.options.insert("bus".into(), "kline".into());
    for (k, v) in opts {
        c.options.insert((*k).into(), (*v).into());
    }
    c
}

fn no_can_ecu() -> opentune_driver_openport2::fake::EcuHandler {
    Box::new(|_req: &[u8]| Vec::new())
}

fn open_with(
    cfg: &ConnectConfig,
    fake: FakeConfig,
    chunking: Chunking,
    tweak: impl FnOnce(&mut FakeOp2),
) -> (Result<OpenPort2Driver, Error>, FakeHandle) {
    let (mut dev, handle) = FakeOp2::new(fake, no_can_ecu());
    tweak(&mut dev);
    let mut d = OpenPort2Driver::new();
    let r = d
        .open_with_port(Box::new(FakePort::new(dev, chunking)), cfg)
        .map(|_| d);
    (r, handle)
}

fn open(cfg: &ConnectConfig, fake: FakeConfig) -> (Result<OpenPort2Driver, Error>, FakeHandle) {
    open_with(cfg, fake, Chunking::Whole, |_| {})
}

fn att_count(h: &FakeHandle) -> usize {
    h.log()
        .lines
        .iter()
        .filter(|l| l.starts_with("att"))
        .count()
}

#[test]
fn fast_init_identify_and_local_id_in_every_layout() {
    let chunkings = [
        Chunking::Whole,
        Chunking::Bytes(1),
        Chunking::Pattern(vec![3, 64, 1, 17, 2]),
    ];
    for checksum in ["cable", "host"] {
        for rx_checksum in [true, false] {
            for layout in [
                KLineRxLayout::StartDataEnd,
                KLineRxLayout::DataOnly,
                KLineRxLayout::Fragmented,
            ] {
                for chunking in chunkings.clone() {
                    let what = format!("{checksum} rx_cs={rx_checksum} {layout:?} {chunking:?}");
                    let fake = FakeConfig {
                        kline_rx_checksum: rx_checksum,
                        kline_rx_layout: layout,
                        ..FakeConfig::default()
                    };
                    let cfg = kcfg(&[("checksum", checksum)]);
                    let (d, h) = open_with(&cfg, fake, chunking, |_| {});
                    let mut d = d.unwrap_or_else(|e| panic!("{what}: open: {e}"));
                    assert_eq!(d.kline_key_bytes(), Some((0xEF, 0x8F)), "{what}");
                    let fw = d.info().firmware.unwrap();
                    assert!(fw.contains("keybytes EF 8F"), "{what}: {fw}");

                    let id = d.read_ecu_id().unwrap_or_else(|e| panic!("{what}: {e}"));
                    assert_eq!(id.vin.as_deref(), Some("OTKLINEFAKE000001"), "{what}");
                    assert_eq!(id.hw_number.as_deref(), Some("OT-HW-KL01"), "{what}");
                    assert_eq!(id.sw_number.as_deref(), Some("OT-SW-KL01"), "{what}");
                    assert_eq!(id.ecu_name.as_deref(), Some("SYNTH KWP ECU"), "{what}");

                    // The existing core client, unchanged, over diag().
                    let t = d.diag().expect("K-line diag transport");
                    let mut kwp = Kwp2000Client::new(t);
                    assert_eq!(
                        kwp.read_local_id(0x01).unwrap(),
                        vec![0x12, 0x34, 0xFE],
                        "{what}"
                    );
                    kwp.tester_present().unwrap();
                    d.close().unwrap();

                    let log = h.log();
                    let flags = if checksum == "host" { 512 } else { 0 };
                    let init_len = if checksum == "host" { 5 } else { 4 };
                    assert_eq!(log.lines[0], "ati", "{what}");
                    assert_eq!(log.lines[1], "atz 1", "{what}");
                    assert_eq!(log.lines[2], "ata 2", "{what}");
                    assert_eq!(log.lines[3], format!("ato4 {flags} 10400 0 3"), "{what}");
                    assert_eq!(log.lines[4], "atf4 1 0 1 4", "{what}");
                    assert_eq!(log.lines[5], format!("aty4 {init_len} 0 5"), "{what}");
                    assert_eq!(log.opened, vec![(4, flags, 10_400)], "{what}");
                    assert_eq!(log.inits, vec!["fast 4 1"], "{what}");
                    // Every K-line transmit: channel 4, 1 s budget; no CAN.
                    assert!(!log.transmits.is_empty());
                    assert!(log.transmits.iter().all(|t| *t == (4, 0, 1_000_000)));
                    assert!(log.lines.iter().all(|l| !l.starts_with("ato5")
                        && !l.starts_with("att5")
                        && !l.starts_with("atx")));
                    let n = log.lines.len();
                    assert!(log.lines[n - 2].starts_with("atc4 "), "{what}");
                    assert!(log.lines[n - 1].starts_with("ata "), "{what}");
                    // The host's frames carry the checksum only in host mode.
                    // First request after the init: 1A 80.
                    let req = log.bus.iter().find(|f| f.from_host).unwrap();
                    assert_eq!(req.data[..5], [0x82, 0x01, 0xF1, 0x1A, 0x80], "{what}");
                    assert_eq!(req.data.len(), if checksum == "host" { 6 } else { 5 });
                }
            }
        }
    }
}

#[test]
fn five_baud_init_reports_the_key_bytes() {
    for unnumbered in [false, true] {
        let fake = FakeConfig {
            kline_key_bytes: (0x6B, 0x8F),
            faults: FakeFaults {
                kline_unnumbered_init: unnumbered,
                ..FakeFaults::default()
            },
            ..FakeConfig::default()
        };
        let (d, h) = open(&kcfg(&[("init", "5baud")]), fake);
        let mut d = d.unwrap();
        assert_eq!(d.kline_key_bytes(), Some((0x6B, 0x8F)));
        let id = d.read_ecu_id().unwrap();
        assert_eq!(id.vin.as_deref(), Some("OTKLINEFAKE000001"));
        let log = h.log();
        assert!(
            log.lines.iter().any(|l| l.starts_with("atw4 1 ")),
            "{:?}",
            log.lines
        );
        assert_eq!(log.inits, vec!["5baud 4 1"]);
        assert!(log.lines.iter().all(|l| !l.starts_with("aty")));
    }

    // A separate five-baud address (init_addr) and the ISO 9141 channel.
    let fake = FakeConfig {
        kline_ecu_addr: 0x33,
        ..FakeConfig::default()
    };
    let mut cfg = kcfg(&[("bus", "iso9141"), ("init_addr", "33")]);
    cfg.tx_id = 0x33;
    let (d, h) = open(&cfg, fake);
    let mut d = d.unwrap();
    assert_eq!(
        d.read_ecu_id().unwrap().vin.as_deref(),
        Some("OTKLINEFAKE000001")
    );
    let log = h.log();
    assert_eq!(
        log.opened,
        vec![(3, 512, 10_400)],
        "host checksum by default"
    );
    assert!(
        log.lines.iter().any(|l| l.starts_with("atw3 51 ")),
        "{:?}",
        log.lines
    );
}

#[test]
fn an_ecu_definition_selects_k_line_without_the_bus_option() {
    let mut cfg = ConnectConfig {
        protocol: Protocol::Iso14230Kline,
        diag_layer: DiagLayer::Kwp2000,
        bitrate: 10_400,
        tx_id: 1,
        rx_id: 0,
        ..ConnectConfig::default()
    };
    cfg.options.insert("init".into(), "fast".into());
    let (d, h) = open(&cfg, FakeConfig::default());
    let mut d = d.unwrap();
    assert!(d.read_ecu_id().unwrap().vin.is_some());
    assert_eq!(
        h.log().opened,
        vec![(4, 512, 10_400)],
        "host checksum by default"
    );
}

#[test]
fn timing_options_are_written_before_the_init() {
    let cfg = kcfg(&[
        ("p1_max_ms", "10"),
        ("p4_min_ms", "0"),
        ("w1_ms", "600"),
        ("w5_ms", "300"),
        ("tinil_ms", "25"),
        ("twup_ms", "50"),
        ("five_baud_mod", "0"),
        ("init", "5baud"),
    ]);
    let (d, h) = open(&cfg, FakeConfig::default());
    d.unwrap();
    let log = h.log();
    assert_eq!(
        log.config,
        vec![
            (4, 7, 20),
            (4, 12, 0),
            (4, 14, 600),
            (4, 18, 300),
            (4, 20, 25),
            (4, 21, 50),
            (4, 33, 0),
            // Five-baud init without w4_ms: W4 = 40 ms.
            (4, 17, 40),
        ]
    );
    let first_ats = log
        .lines
        .iter()
        .position(|l| l.starts_with("ats4"))
        .unwrap();
    let filter = log
        .lines
        .iter()
        .position(|l| l.starts_with("atf4"))
        .unwrap();
    let init = log
        .lines
        .iter()
        .position(|l| l.starts_with("atw4"))
        .unwrap();
    assert!(first_ats < filter && filter < init, "{:?}", log.lines);

    // A parameter the cable refuses fails the open, which releases the cable.
    let fake = FakeConfig {
        faults: FakeFaults {
            error_reply: vec![("ats4".into(), 1)],
            ..FakeFaults::default()
        },
        ..FakeConfig::default()
    };
    let (d, h) = open(&kcfg(&[("w1_ms", "300")]), fake);
    let e = d.err().unwrap().to_string();
    assert!(e.contains("ERR_NOT_SUPPORTED"), "{e}");
    assert!(h.log().lines.last().unwrap().starts_with("ata "));
}

#[test]
fn block_read_over_k_line_uses_read_memory_by_address() {
    let (d, h) = open(&kcfg(&[]), FakeConfig::default());
    let mut d = d.unwrap();
    let mut calls = 0;
    let data = d
        .read_memory(MemoryRegion::new(0x1000, 600), &mut |_| calls += 1)
        .unwrap();
    let want: Vec<u8> = (0x1000u32..0x1000 + 600)
        .map(|x| (x ^ (x >> 8)) as u8)
        .collect();
    assert_eq!(data, want);
    assert!(calls >= 3);
    // 250 + 250 + 100 bytes: three 0x23 requests after the init.
    let reads = h
        .log()
        .bus
        .iter()
        .filter(|f| f.from_host && f.data.get(3) == Some(&0x23))
        .count();
    assert_eq!(reads, 3);
}

#[test]
fn k_line_is_read_only_before_anything_reaches_the_cable() {
    let (d, h) = open(&kcfg(&[]), FakeConfig::default());
    let mut d = d.unwrap();
    assert!(!d.capabilities().write_memory, "no write path on K-line");
    assert!(d.capabilities().kline);
    let before = att_count(&h);
    let e = d
        .write_memory(MemoryRegion::new(0, 4), &[1, 2, 3, 4], &mut |_| {})
        .unwrap_err();
    assert!(matches!(e, Error::Unsupported(_)), "{e}");
    let t = d.diag().unwrap();
    for write_class in [
        &[0x3B, 0x01, 0xAA][..],
        &[0x34, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00],
        &[0x36, 0x01, 0x02],
        &[0x31, 0x01],
        &[0x2E, 0xF1, 0x90, 0x00],
        &[0x14, 0xFF, 0x00],
        &[0x11, 0x01],
        &[0x10, 0x85],
    ] {
        let e = t.request(write_class, 500).unwrap_err();
        assert!(
            matches!(e, Error::Unsupported(_)),
            "{write_class:02X?}: {e}"
        );
    }
    assert_eq!(att_count(&h), before, "a refused request sent nothing");
    // A read still goes out.
    t.request(&[0x21, 0x02], 500).unwrap();
    assert_eq!(att_count(&h), before + 1);
}

#[test]
fn response_pending_and_foreign_frames_are_waited_through() {
    let fake = FakeConfig {
        faults: FakeFaults {
            kline_foreign_frame: true,
            ..FakeFaults::default()
        },
        ..FakeConfig::default()
    };
    let (d, _h) = open_with(&kcfg(&[]), fake, Chunking::Bytes(5), |dev| {
        let mut inner = synthetic_kwp_ecu();
        let mut first = true;
        dev.set_kline_ecu(Box::new(move |req: &[u8]| {
            let mut out = Vec::new();
            if first && req.first() == Some(&0x21) {
                first = false;
                out.push(vec![0x7F, 0x21, 0x78]);
            }
            out.extend(inner(req));
            out
        }));
    });
    let mut d = d.unwrap();
    let t = d.diag().unwrap();
    let mut kwp = Kwp2000Client::new(t);
    assert_eq!(kwp.read_local_id(0x05).unwrap(), vec![0x12, 0x34, 0xFA]);
    assert!(
        d.stats().unwrap().foreign_frames >= 2,
        "other tester's frames skipped"
    );
}

#[test]
fn silent_ecus_and_cables_time_out_with_a_reason() {
    // No ECU answers the fast init: the cable says ERR_FAILED, the driver
    // names the likely causes and releases the cable.
    let fake = FakeConfig {
        faults: FakeFaults {
            kline_silent_ecu: true,
            ..FakeFaults::default()
        },
        ..FakeConfig::default()
    };
    for init in ["fast", "5baud"] {
        let (d, h) = open(&kcfg(&[("init", init)]), fake.clone());
        let e = d.err().unwrap().to_string();
        assert!(e.contains("no answer from the ECU"), "{init}: {e}");
        assert!(h.log().lines.last().unwrap().starts_with("ata "));
    }
    // A five-baud init to the wrong address gets no answer either.
    let (d, _) = open(
        &kcfg(&[("init", "5baud"), ("init_addr", "10")]),
        FakeConfig::default(),
    );
    assert!(d.is_err());

    // init=none: the ECU was never woken, so a request times out.
    let (d, _) = open(&kcfg(&[("init", "none")]), FakeConfig::default());
    let mut d = d.unwrap();
    assert_eq!(d.kline_key_bytes(), None);
    let t = d.diag().unwrap();
    let e = t.request(&[0x1A, 0x90], 300).unwrap_err();
    assert!(matches!(e, Error::Timeout(_)), "{e}");

    // A cable that never answers `aty`.
    let fake = FakeConfig {
        faults: FakeFaults {
            silent: vec!["aty4".into()],
            ..FakeFaults::default()
        },
        ..FakeConfig::default()
    };
    let (d, _) = open(&kcfg(&[]), fake);
    let e = d.err().unwrap().to_string();
    assert!(e.contains("did not answer `aty4`"), "{e}");
}

#[test]
fn the_k_line_filter_is_a_pass_all_pass_filter() {
    let (d, h) = open(&kcfg(&[]), FakeConfig::default());
    d.unwrap();
    let f = h.log().filters;
    assert_eq!(f.len(), 1);
    assert_eq!((f[0].proto, f[0].filter_type, f[0].txflags), (4, 1, 0));
    assert_eq!(
        (f[0].mask.clone(), f[0].pattern.clone()),
        (vec![0], vec![0])
    );
    // A filter the cable does not acknowledge fails the open.
    let fake = FakeConfig {
        faults: FakeFaults {
            silent: vec!["atf4".into()],
            ..FakeFaults::default()
        },
        ..FakeConfig::default()
    };
    let (d, _) = open(&kcfg(&[]), fake);
    assert!(d.is_err(), "an unanswered filter fails the open");
}

#[test]
fn bad_k_line_options_are_refused_before_anything_is_sent() {
    let mut far = kcfg(&[]);
    far.tx_id = 0x7E0;
    let cases = [
        kcfg(&[("baud", "9600")]),
        kcfg(&[("init", "warp")]),
        kcfg(&[("checksum", "maybe")]),
        kcfg(&[("p1_max_ms", "500")]),
        kcfg(&[("w4_ms", "-1")]),
        kcfg(&[("twup_ms", "20")]),
        kcfg(&[("five_baud_mod", "7")]),
        kcfg(&[("tester", "G1")]),
        kcfg(&[("target", "zz")]),
        kcfg(&[("addressing", "broadcast")]),
        kcfg(&[("addressing", "none"), ("target", "33")]),
        kcfg(&[("session", "1234")]),
        kcfg(&[("bus", "iso9141"), ("init", "fast")]),
        far,
    ];
    for cfg in cases {
        let (d, h) = open(&cfg, FakeConfig::default());
        let e = d.err().expect("refused");
        assert!(matches!(e, Error::Config(_)), "{:?}: {e}", cfg.options);
        assert!(h.log().lines.is_empty(), "{:?}: nothing sent", cfg.options);
    }
    let mut d = OpenPort2Driver::new();
    assert!(
        d.open(&kcfg(&[("baud", "9600")])).is_err(),
        "before the port"
    );
}

#[test]
fn the_can_path_is_unchanged_without_a_k_line_option() {
    for bus in [None, Some("hs"), Some("can")] {
        let mut cfg = ConnectConfig::default();
        if let Some(b) = bus {
            cfg.options.insert("bus".into(), b.into());
        }
        let (d, h) = open(&cfg, FakeConfig::default());
        let d = d.unwrap();
        assert!(d.capabilities().write_memory);
        assert_eq!(d.kline_key_bytes(), None);
        let log = h.log();
        assert_eq!(
            log.lines,
            vec!["ati", "atz 1", "ata 2", "ato5 0 500000 0 3", "atf5 1 0 4 4"],
            "{bus:?}"
        );
        assert!(log.inits.is_empty() && log.config.is_empty());
    }
}

// --- request header addressing (ISO 14230-2 format byte, key byte 1) -------

/// The host's K-line frames on the fake bus, in order.
fn host_frames(h: &FakeHandle) -> Vec<Vec<u8>> {
    h.log()
        .bus
        .iter()
        .filter(|f| f.from_host)
        .map(|f| f.data.clone())
        .collect()
}

/// A fake ECU like the bench one: woken by a five-baud init at the OBD
/// functional address 0x33 it answers functional requests to 0x33 only;
/// woken at its physical address 0x01, physical requests to 0x01 only.
fn obd_fake(kb1: u8) -> FakeConfig {
    FakeConfig {
        kline_ecu_header: KLineEcuHeader::Obd(0x33),
        kline_key_bytes: (kb1, 0x8F),
        ..FakeConfig::default()
    }
}

#[test]
fn after_a_five_baud_init_at_0x33_requests_go_functional_to_0x33() {
    // The bench line: --tx-id 01 --opt init=5baud --opt init_addr=33.
    let cfg = kcfg(&[("init", "5baud"), ("init_addr", "33")]);
    for kb1 in [0xEF, 0xE9, 0x6B, 0x6D] {
        let (d, h) = open(&cfg, obd_fake(kb1));
        let mut d = d.unwrap_or_else(|e| panic!("{kb1:02X}: {e}"));
        let id = d.read_ecu_id().unwrap();
        assert_eq!(id.vin.as_deref(), Some("OTKLINEFAKE000001"));
        let fw = d.info().firmware.unwrap();
        assert!(
            fw.contains(&format!(
                "keybytes {kb1:02X} 8F; header Cn 33 F1 (functional)"
            )),
            "{fw}"
        );
        assert!(fw.contains("normal timing"), "{fw}");
        let log = h.log();
        assert_eq!(log.inits, vec!["5baud 4 51"]);
        // identify's first request: 1A 80, functional, to 0x33, from F1.
        assert_eq!(
            host_frames(&h)[0][..5],
            [0xC2, 0x33, 0xF1, 0x1A, 0x80],
            "{kb1:02X}"
        );
        assert!(host_frames(&h).iter().all(|f| f[0] & 0xC0 == 0xC0));
    }

    // The same ECU woken at its physical address: physical to 0x01.
    let (d, h) = open(&kcfg(&[("init", "5baud")]), obd_fake(0xEF));
    let mut d = d.unwrap();
    assert!(d.read_ecu_id().unwrap().vin.is_some());
    assert_eq!(host_frames(&h)[0][..5], [0x82, 0x01, 0xF1, 0x1A, 0x80]);
    assert!(d
        .info()
        .firmware
        .unwrap()
        .contains("header 8n 01 F1 (physical)"));
    // And after a fast init (StartCommunication physical to tx_id, as before).
    let (d, h) = open(&kcfg(&[]), obd_fake(0xEF));
    let mut d = d.unwrap();
    assert!(d.read_ecu_id().unwrap().vin.is_some());
    assert_eq!(h.log().inits, vec!["fast 4 1"]);
    assert_eq!(host_frames(&h)[0][..4], [0x82, 0x01, 0xF1, 0x1A]);
}

#[test]
fn explicit_target_and_addressing_options_win() {
    // Physical requests after a functional wake-up: what the bench saw
    // before this change (no answer within P2).
    for opts in [
        &[("target", "01")][..],
        &[("addressing", "physical")],
        &[("target", "0x01"), ("addressing", "physical")],
    ] {
        let mut o = vec![("init", "5baud"), ("init_addr", "33")];
        o.extend_from_slice(opts);
        let (d, h) = open(&kcfg(&o), obd_fake(0xEF));
        let mut d = d.unwrap();
        let e = d.diag().unwrap().request(&[0x1A, 0x90], 200).unwrap_err();
        assert!(matches!(e, Error::Timeout(_)), "{opts:?}: {e}");
        let f = &host_frames(&h)[0];
        assert_eq!(f[0], 0x82, "{opts:?}: physical");
        assert_eq!(f[2], 0xF1);
    }
    // addressing=functional to the physical address, and a target of its
    // own; the default fake answers any addressed header to its address.
    let (d, h) = open(
        &kcfg(&[("addressing", "functional")]),
        FakeConfig::default(),
    );
    let mut d = d.unwrap();
    assert!(d.read_ecu_id().unwrap().vin.is_some());
    assert_eq!(host_frames(&h)[0][..3], [0xC2, 0x01, 0xF1]);
    let (d, h) = open(&kcfg(&[("target", "10")]), FakeConfig::default());
    let mut d = d.unwrap();
    let _ = d.diag().unwrap().request(&[0x1A, 0x90], 200);
    assert_eq!(host_frames(&h)[0][..3], [0x82, 0x10, 0xF1]);
    // A five-baud address other than 0x33 stays physical.
    let fake = FakeConfig {
        kline_ecu_addr: 0x10,
        ..FakeConfig::default()
    };
    let (d, h) = open(&kcfg(&[("init", "5baud"), ("init_addr", "10")]), fake);
    d.unwrap().read_ecu_id().unwrap();
    assert_eq!(host_frames(&h)[0][..3], [0x82, 0x10, 0xF1]);
}

#[test]
fn key_byte_1_header_variants_end_to_end() {
    // KB1 E5 (HB0 + AL0 only): the 1-byte header, both ways.
    let fake = FakeConfig {
        kline_ecu_header: KLineEcuHeader::NoAddress,
        kline_key_bytes: (0xE5, 0x8F),
        ..FakeConfig::default()
    };
    for checksum in ["cable", "host"] {
        let (d, h) = open(&kcfg(&[("checksum", checksum)]), fake.clone());
        let mut d = d.unwrap_or_else(|e| panic!("{checksum}: {e}"));
        assert_eq!(
            d.read_ecu_id().unwrap().sw_number.as_deref(),
            Some("OT-SW-KL01")
        );
        assert!(d
            .info()
            .firmware
            .unwrap()
            .contains("header 0n (no address)"));
        assert_eq!(host_frames(&h)[0][..3], [0x02, 0x1A, 0x80], "{checksum}");
    }
    // KB1 EA (HB1 + AL1 only): a separate length byte, both ways.
    let fake = FakeConfig {
        kline_len_byte_only: true,
        kline_key_bytes: (0xEA, 0x8F),
        ..FakeConfig::default()
    };
    let (d, h) = open(&kcfg(&[]), fake);
    let mut d = d.unwrap();
    assert!(d.read_ecu_id().unwrap().vin.is_some());
    let t = d.diag().unwrap();
    let mut kwp = Kwp2000Client::new(t);
    assert_eq!(kwp.read_local_id(0x01).unwrap(), vec![0x12, 0x34, 0xFE]);
    assert_eq!(
        host_frames(&h)[0][..6],
        [0x80, 0x01, 0xF1, 0x02, 0x1A, 0x80]
    );
    assert!(d
        .info()
        .firmware
        .unwrap()
        .contains("header 80 01 F1 (physical), length byte"));

    // Refused at the open, after the init and before any request; the
    // cable is released.
    for (kb, opts, why) in [
        ((0xE5, 0x8F), &[("target", "01")][..], "HB1 = 0"),
        ((0xE9, 0x8F), &[("addressing", "none")][..], "HB0 = 0"),
        ((0x61, 0x8F), &[][..], "HB1 = 0"),
        ((0x6C, 0x8F), &[][..], "AL0 = AL1 = 0"),
        ((0x01, 0x8A), &[][..], "neither ISO 14230"),
    ] {
        let fake = FakeConfig {
            kline_key_bytes: kb,
            ..FakeConfig::default()
        };
        let (d, h) = open(&kcfg(opts), fake);
        let e = d.err().expect("refused");
        assert!(matches!(e, Error::Unsupported(_)), "{kb:02X?}: {e}");
        assert!(e.to_string().contains(why), "{kb:02X?}: {e}");
        assert_eq!(att_count(&h), 0, "{kb:02X?}: no request sent");
        assert!(h.log().lines.last().unwrap().starts_with("ata "));
    }
}

/// A fake ECU that wakes as an ISO 9141-2 OBD participant at 0x33 (source
/// address 0x10) and ignores every KWP2000 header.
fn iso9141_fake(kb: (u8, u8)) -> FakeConfig {
    FakeConfig {
        kline_ecu_header: KLineEcuHeader::Iso9141Obd { source: 0x10 },
        kline_key_bytes: kb,
        ..FakeConfig::default()
    }
}

#[test]
fn an_ecu_that_wakes_as_iso_9141_2_gets_obd_requests_only() {
    for (kb, checksum) in [
        ((0x08, 0x08), "cable"),
        ((0x08, 0x08), "host"),
        ((0x94, 0x94), "cable"),
    ] {
        let what = format!("{kb:02X?} {checksum}");
        let cfg = kcfg(&[
            ("init", "5baud"),
            ("init_addr", "33"),
            ("checksum", checksum),
        ]);
        let (d, h) = open(&cfg, iso9141_fake(kb));
        let mut d = d.unwrap_or_else(|e| panic!("{what}: {e}"));
        assert_eq!(h.log().inits, vec!["5baud 4 51"]);
        let fw = d.info().firmware.unwrap();
        assert!(
            fw.contains(&format!(
                "keybytes {:02X} {:02X}; ISO 9141-2 OBD: header 68 6A F1, KWP2000 refused)",
                kb.0, kb.1
            )),
            "{what}: {fw}"
        );

        // identify (KWP2000 1A) is refused before anything is sent.
        let e = d.read_ecu_id().unwrap_err();
        assert!(matches!(e, Error::Unsupported(_)), "{what}: {e}");
        let want = format!(
            "ECU woke as ISO 9141-2 OBD (key bytes {:02X} {:02X}); KWP2000 needs its physical \
             init address (init_addr=<ecu>",
            kb.0, kb.1
        );
        assert!(e.to_string().contains(&want), "{what}: {e}");
        assert_eq!(att_count(&h), 0, "{what}: nothing sent");
        let e = d
            .read_memory(MemoryRegion::new(0, 4), &mut |_| {})
            .unwrap_err();
        assert!(matches!(e, Error::Unsupported(_)), "{what}: {e}");
        assert_eq!(att_count(&h), 0, "{what}: no 0x23 either");

        // OBD mode 01 PID 00 works, with the legislated header.
        let t = d.diag().unwrap();
        let r = t.request(&[0x01, 0x00], 500).unwrap();
        assert_eq!(r, vec![0x41, 0x00, 0xBE, 0x1F, 0xA8, 0x13], "{what}");
        let f = &host_frames(&h)[0];
        assert_eq!(f[..5], [0x68, 0x6A, 0xF1, 0x01, 0x00], "{what}");
        if checksum == "host" {
            assert_eq!(f.len(), 6);
            assert_eq!(
                f[5],
                0x68u8.wrapping_add(0x6A).wrapping_add(0xF1).wrapping_add(1)
            );
        } else {
            assert_eq!(f.len(), 5);
        }
        // OBD modes the shared guard refuses stay unsent (04 clears DTCs).
        let e = t.request(&[0x04], 500).unwrap_err();
        assert!(matches!(e, Error::Unsupported(_)), "{what}: {e}");
        assert_eq!(att_count(&h), 1);

        // The ECU's source address is reported, and named in the refusal.
        assert_eq!(d.kline_obd_source(), Some(0x10), "{what}");
        let fw = d.info().firmware.unwrap();
        assert!(
            fw.contains("KWP2000 refused, ECU source address 10)"),
            "{fw}"
        );
        let e = d.diag().unwrap().request(&[0x1A, 0x90], 500).unwrap_err();
        assert!(e.to_string().contains("(init_addr=10)"), "{e}");
        assert_eq!(att_count(&h), 1);
    }

    // session= is KWP2000: with an ISO 9141-2 wake the open fails, unsent.
    let cfg = kcfg(&[("init", "5baud"), ("init_addr", "33"), ("session", "81")]);
    let (d, h) = open(&cfg, iso9141_fake((0x08, 0x08)));
    let e = d.err().unwrap().to_string();
    assert!(e.contains("ECU woke as ISO 9141-2 OBD"), "{e}");
    assert_eq!(att_count(&h), 0);

    // An explicit header still wins: KWP2000 forced (what the bench sent
    // before keyword decoding: functional, silent).
    let cfg = kcfg(&[("init", "5baud"), ("init_addr", "33"), ("target", "33")]);
    let (d, h) = open(&cfg, iso9141_fake((0x08, 0x08)));
    let mut d = d.unwrap();
    assert!(d.info().firmware.unwrap().contains("forced"));
    let e = d.diag().unwrap().request(&[0x1A, 0x90], 200).unwrap_err();
    assert!(matches!(e, Error::Timeout(_)), "{e}");
    assert_eq!(host_frames(&h)[0][..3], [0xC2, 0x33, 0xF1]);

    // The ISO 9141-2 ECU does not wake at its physical address or by fast
    // init.
    for o in [&[("init", "5baud")][..], &[("init", "fast")]] {
        assert!(open(&kcfg(o), iso9141_fake((0x08, 0x08))).0.is_err());
    }
}

#[test]
fn the_session_option_goes_first_and_programming_sessions_are_refused() {
    let cfg = kcfg(&[("init", "5baud"), ("init_addr", "33"), ("session", "81")]);
    let (d, h) = open(&cfg, obd_fake(0xEF));
    let mut d = d.unwrap();
    assert!(d.read_ecu_id().unwrap().vin.is_some());
    let frames = host_frames(&h);
    assert_eq!(frames[0][..5], [0xC2, 0x33, 0xF1, 0x10, 0x81]);
    assert_eq!(frames[1][..5], [0xC2, 0x33, 0xF1, 0x1A, 0x80]);

    // An ECU refusing the session fails the open with its NRC.
    let (d, _) = open_with(
        &kcfg(&[("session", "89")]),
        FakeConfig::default(),
        Chunking::Whole,
        |dev| {
            dev.set_kline_ecu(Box::new(|req: &[u8]| match req {
                [0x10, ..] => vec![vec![0x7F, 0x10, 0x12]],
                _ => Vec::new(),
            }));
        },
    );
    let e = d.err().unwrap().to_string();
    assert!(e.contains("NRC 0x12"), "{e}");

    // Programming sessions: refused by the shared guard before the port.
    for prog in ["85", "02"] {
        let (d, h) = open(&kcfg(&[("session", prog)]), FakeConfig::default());
        let e = d.err().expect("refused");
        assert!(matches!(e, Error::Unsupported(_)), "{prog}: {e}");
        assert!(h.log().lines.is_empty(), "{prog}: nothing sent");
    }
    // Without the option no session request is sent.
    let (d, h) = open(&kcfg(&[]), FakeConfig::default());
    d.unwrap().read_ecu_id().unwrap();
    assert!(host_frames(&h).iter().all(|f| f[3] != 0x10));
}

// --- receive-only raw-CAN capture ------------------------------------------

fn monitor_fake(fake: FakeConfig) -> (OpenPort2Driver, FakeHandle) {
    let (dev, handle) = FakeOp2::new(fake, no_can_ecu());
    let mut d = OpenPort2Driver::new();
    d.inject_monitor_port(Box::new(FakePort::new(dev, Chunking::Bytes(7))));
    (d, handle)
}

fn opts(bitrate: u32, extra: &[(&str, &str)]) -> MonitorOpts {
    let mut o = MonitorOpts {
        bitrate,
        ..MonitorOpts::default()
    };
    for (k, v) in extra {
        o.options.insert((*k).into(), (*v).into());
    }
    o
}

#[test]
fn can_monitor_captures_both_id_widths_and_never_transmits() {
    let traffic = vec![
        (0x7E8, vec![0x02, 0x50, 0x03]),
        (0x18DA_F110, vec![0x03, 0x7F, 0x22, 0x31]),
        (0x123, vec![1, 2, 3, 4, 5, 6, 7, 8]),
        (0x1FFF_FFFF, vec![]),
    ];
    let fake = FakeConfig {
        bus_traffic: traffic.clone(),
        ..FakeConfig::default()
    };
    let (mut d, h) = monitor_fake(fake);
    let mon: &mut dyn CanMonitor = d.can_monitor().expect("openport2 captures");
    mon.start(&opts(500_000, &[])).unwrap();
    assert!(
        mon.describe().contains("receive-only"),
        "{}",
        mon.describe()
    );
    let mut got = Vec::new();
    while let Some(f) = mon.next_frame(300).unwrap() {
        got.push((f.id(), f.extended, f.data().to_vec()));
    }
    mon.stop().unwrap();
    let want: Vec<_> = traffic
        .iter()
        .map(|(id, d)| (*id, *id > 0x7FF, d.clone()))
        .collect();
    assert_eq!(got, want, "indication records dropped, both widths kept");

    let log = h.log();
    assert!(
        log.lines.iter().all(|l| !l.starts_with("att")
            && !l.starts_with("atm")
            && !l.starts_with("aty")
            && !l.starts_with("atw")
            && !l.starts_with("atv")
            && !l.starts_with("atx")),
        "no transmit ever reached the cable: {:?}",
        log.lines
    );
    assert!(log.transmits.is_empty() && log.bus.iter().all(|f| !f.from_host));
    assert_eq!(log.opened, vec![(5, 0, 500_000)]);
    assert!(log.lines.contains(&"atf5 1 0 4 4".to_string()));
    assert!(log.lines[log.lines.len() - 2].starts_with("atc5 "));
    assert!(log.open_channels.is_empty(), "released");
}

#[test]
fn can_monitor_filter_is_pass_all_and_options_are_checked() {
    let fake = FakeConfig {
        bus_traffic: vec![(0x18DB_33F1, vec![0x02, 0x01, 0x00])],
        ..FakeConfig::default()
    };
    let (mut d, h) = monitor_fake(fake);
    let mon = d.can_monitor().unwrap();
    mon.start(&opts(250_000, &[("can_ids", "29")])).unwrap();
    let f = mon.next_frame(300).unwrap().unwrap();
    assert_eq!((f.id(), f.extended), (0x18DB_33F1, true));
    mon.stop().unwrap();
    let log = h.log();
    assert_eq!(log.opened, vec![(5, 256, 250_000)]);
    assert!(log.lines.contains(&"atf5 1 256 4 4".to_string()));

    // Refused before anything is sent: listen-only demanded, a rate the
    // driver does not open, an unknown id mode.
    for (o, what) in [
        (opts(500_000, &[("listen_only", "require")]), "listen-only"),
        (opts(33_333, &[]), "bit rate"),
        (opts(500_000, &[("can_ids", "16")]), "can_ids"),
    ] {
        let (mut d, h) = monitor_fake(FakeConfig::default());
        let e = d.can_monitor().unwrap().start(&o).unwrap_err().to_string();
        assert!(e.contains(what), "{e}");
        assert!(h.log().lines.is_empty(), "{what}: nothing sent");
    }
    // next_frame before start.
    let (mut d, _) = monitor_fake(FakeConfig::default());
    assert!(d.can_monitor().unwrap().next_frame(10).is_err());
}

/// The EDC15C6 bench line (five-baud init at 0x12, key bytes EF 8F): the
/// default frames identify with the length in the format byte, and
/// `length=separate` really changes the bytes on the wire to the separate
/// length byte; the ECU answers both.
#[test]
fn length_option_changes_the_wire_form() {
    let fake = || FakeConfig {
        kline_ecu_addr: 0x12,
        kline_key_bytes: (0xEF, 0x8F),
        ..FakeConfig::default()
    };
    for (opts, first) in [
        (vec![("init", "5baud")], [0x82, 0x12, 0xF1, 0x1A, 0x80]),
        (
            vec![("init", "5baud"), ("length", "separate")],
            [0x80, 0x12, 0xF1, 0x02, 0x1A],
        ),
    ] {
        let mut cfg = kcfg(&opts);
        cfg.tx_id = 0x12;
        let (d, h) = open(&cfg, fake());
        let mut d = d.unwrap_or_else(|e| panic!("{opts:?}: {e}"));
        assert_eq!(
            d.read_ecu_id().unwrap().vin.as_deref(),
            Some("OTKLINEFAKE000001"),
            "{opts:?}"
        );
        assert_eq!(host_frames(&h)[0][..5], first, "{opts:?}");
    }
}
