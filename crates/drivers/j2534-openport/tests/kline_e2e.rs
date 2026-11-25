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
    KLineRxLayout,
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
            log.lines.contains(&"atw4 1 5".to_string()),
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
    assert_eq!(log.opened, vec![(3, 0, 10_400)]);
    assert!(
        log.lines.contains(&"atw3 51 5".to_string()),
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
    assert_eq!(h.log().opened, vec![(4, 0, 10_400)]);
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
