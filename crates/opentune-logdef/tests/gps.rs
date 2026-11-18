//! End to end: a recorded (synthetic) NMEA route replayed into a live log
//! against the UDS simulator, through `LiveSession::run_csv_gps`.

use opentune_core::ecudef::Endian;
use opentune_core::CoreDiag;
use opentune_logdef::opentune_gps::ReplayFeed;
use opentune_logdef::{
    Addressing, Conversion, ConversionFit, EcuGroup, FastLogConfig, GpsLogger, LiveSession, LogDef,
    LogParam, ProtocolGroup, ProtocolKind, SourceFormat, ValueKind, SPEED_ERROR_ID,
};
use opentune_udssim::server::UdsServer;
use opentune_udssim::transport::SimTransport;
use std::collections::HashMap;

fn param(id: &str, name: &str, did: u16, units: &str) -> LogParam {
    LogParam {
        id: id.into(),
        name: name.into(),
        description: String::new(),
        units: units.into(),
        protocol: ProtocolKind::UdsMode22,
        addressing: Addressing::UdsDid { did },
        storagetype: None,
        endian: Endian::Big,
        value_kind: ValueKind::Number,
        conversion: Some(Conversion {
            units: units.into(),
            expr: "X".into(),
            format: None,
            fit: ConversionFit::Linear {
                factor: 1.0,
                offset: 0.0,
            },
            gauge_min: None,
            gauge_max: None,
            gauge_step: None,
        }),
        conversions: Vec::new(),
        min: None,
        max: None,
        alternates: Vec::new(),
        depends_on: Vec::new(),
        convert_group: None,
        enabled: None,
    }
}

fn def(params: Vec<LogParam>) -> LogDef {
    LogDef {
        format: SourceFormat::RomRaiderLogger,
        version: None,
        convert_factors: Vec::new(),
        protocols: vec![ProtocolGroup {
            id: "uds".into(),
            kind: ProtocolKind::UdsMode22,
            baud: None,
            ecus: vec![EcuGroup {
                id: None,
                name: None,
                ecu_type: None,
                params,
            }],
        }],
        warnings: Vec::new(),
    }
}

fn fixture(name: &str) -> Vec<u8> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../opentune-gps/tests/fixtures")
        .join(name);
    std::fs::read(p).unwrap()
}

/// `(round, param_id) -> (value, note)` from the CSV.
fn rows(csv: &str) -> HashMap<(usize, String), (String, String)> {
    let mut lines = csv.lines();
    assert_eq!(
        lines.next(),
        Some("round,param_id,name,raw,value,units,note,text,ts")
    );
    lines
        .map(|l| {
            let f: Vec<&str> = l.split(',').collect();
            assert_eq!(f.len(), 9, "{l}");
            (
                (f[0].parse().unwrap(), f[1].to_string()),
                (f[4].to_string(), f[6].to_string()),
            )
        })
        .collect()
}

#[test]
fn replayed_route_adds_gps_channels_and_speed_error() {
    let mut server = UdsServer::new();
    server.set_did(0x1000, vec![0x00, 0x64]); // vehicle speed 100 km/h
    server.set_did(0x1001, vec![0x0B, 0xB8]); // rpm 3000
    let mut sim = SimTransport::new(server);
    let mut diag = CoreDiag::new(&mut sim, 1000);

    let d = def(vec![
        param("rpm", "Engine speed", 0x1001, "rpm"),
        param("vss", "Vehicle speed", 0x1000, "km/h"),
    ]);
    let mut session = LiveSession::new(&d, &[], FastLogConfig::default()).unwrap();
    session.start(&mut diag);
    let feed = ReplayFeed::from_bytes("route", &fixture("route_gn.nmea"));
    let mut gps = GpsLogger::new(Box::new(feed), session.params(), None).unwrap();
    assert_eq!(gps.speed_channel(), Some(1));

    // 22 rounds over a 20-epoch replay: the last two rounds are stale.
    let mut out = Vec::new();
    session
        .run_csv_gps(&mut diag, 22, 0, Some(&mut gps), &mut out)
        .unwrap();
    let csv = String::from_utf8(out).unwrap();
    // Header + 22 rounds x (2 ECU + 8 GPS + 1 speed error).
    assert_eq!(csv.lines().count(), 1 + 22 * 11);
    let r = rows(&csv);

    // Round 0: GPS at standstill -> no speed error, but a good fix.
    assert_eq!(r[&(0, "gps_lat".into())].0, "56.5");
    assert_eq!(r[&(0, "gps_fix".into())].0, "1");
    assert_eq!(r[&(0, "gps_sats".into())].0, "9");
    assert_eq!(r[&(0, SPEED_ERROR_ID.into())].0, "");
    assert!(r[&(0, SPEED_ERROR_ID.into())].1.contains("below"));
    // Round i: GPS speed 6*i km/h, vehicle 100 -> error 100 - 6i.
    for i in 1..15usize {
        let gps_kmh: f64 = r[&(i, "gps_speed_kmh".into())].0.parse().unwrap();
        assert!((gps_kmh - 6.0 * i as f64).abs() < 0.01);
        let err: f64 = r[&(i, SPEED_ERROR_ID.into())].0.parse().unwrap();
        assert!((err - (100.0 - 6.0 * i as f64)).abs() < 0.01, "round {i}");
        assert_eq!(r[&(i, "gps_lat".into())].1, "", "no note while good");
    }
    // Past the end of the replay: empty values, flagged.
    for id in ["gps_lat", "gps_speed_kmh", "gps_sats", SPEED_ERROR_ID] {
        let (v, note) = &r[&(21, id.to_string())];
        assert_eq!(v, "");
        assert!(note.starts_with("replay ended"), "{id}: {note}");
    }
    // ECU channels are untouched by the GPS rows.
    assert_eq!(r[&(21, "rpm".into())].0, "3000");
    assert_eq!(gps.good_ticks(), (20, 22));
    assert_eq!(gps.stats().rmc, 20);
}

#[test]
fn broken_nmea_never_stops_the_log() {
    let mut server = UdsServer::new();
    server.set_did(0x1001, vec![0x0B, 0xB8]);
    let mut sim = SimTransport::new(server);
    let mut diag = CoreDiag::new(&mut sim, 1000);
    let d = def(vec![param("rpm", "Engine speed", 0x1001, "rpm")]);
    let mut session = LiveSession::new(&d, &[], FastLogConfig::default()).unwrap();
    session.start(&mut diag);
    let feed = ReplayFeed::from_bytes("broken", &fixture("broken.nmea"));
    let mut gps = GpsLogger::new(Box::new(feed), session.params(), None).unwrap();
    assert_eq!(gps.speed_channel(), None);

    let mut out = Vec::new();
    session
        .run_csv_gps(&mut diag, 10, 0, Some(&mut gps), &mut out)
        .unwrap();
    let csv = String::from_utf8(out).unwrap();
    assert_eq!(csv.lines().count(), 1 + 10 * 9);
    let r = rows(&csv);
    // The outage (epochs 4-5): position empty with the reason, status kept.
    assert_eq!(r[&(4, "gps_lat".into())].0, "");
    assert!(r[&(4, "gps_lat".into())].1.starts_with("no fix"));
    assert_eq!(r[&(4, "gps_fix".into())].0, "0");
    assert_eq!(r[&(4, "gps_sats".into())].0, "4");
    assert!(!r[&(6, "gps_lat".into())].0.is_empty());
    let st = gps.stats();
    assert_eq!(st.rejected(), 6);
}
