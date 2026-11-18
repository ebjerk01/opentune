//! Mercedes 722.6 (EGS51) decode against **synthetic** frames built from the
//! published Ultimate-NAG52 EGS51 layout.
//!
//! The frames are built with the upstream convention, independently of the
//! definition's DBC start bits: upstream reads the payload as a big-endian
//! `u64` and places a signal at `OFFSET` bits from the most-significant bit,
//! i.e. at bits `[64 - OFFSET - LEN, 63 - OFFSET]`. If the generator's
//! OFFSET -> DBC conversion were wrong, these tests would fail.
//!
//! Validation state: synthetic-tested only. Nothing here comes from a car.
use opentune_ctrldef::decode::{summarize, RawFrame};
use opentune_ctrldef::{ControllerDef, Decoder, Value};
use std::path::Path;

fn def() -> ControllerDef {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ctrldefs/mercedes-egs5x-7226.json");
    ControllerDef::from_json(&std::fs::read_to_string(p).unwrap()).unwrap()
}

/// Build a payload from `(OFFSET, LEN, value)` triples, upstream style.
fn nag(fields: &[(u32, u32, u64)]) -> [u8; 8] {
    let mut raw: u64 = 0;
    for &(off, len, v) in fields {
        let mask = if len == 64 {
            u64::MAX
        } else {
            (1u64 << len) - 1
        };
        assert!(v <= mask);
        raw |= v << (64 - off - len);
    }
    raw.to_be_bytes()
}

fn val<'a>(f: &'a opentune_ctrldef::DecodedFrame, name: &str) -> &'a Value {
    &f.get(name)
        .unwrap_or_else(|| panic!("no signal {name}"))
        .value
}

fn enum_name(f: &opentune_ctrldef::DecodedFrame, name: &str) -> Option<String> {
    match val(f, name) {
        Value::Enum(n) => n.clone(),
        v => panic!("{name} is {v:?}"),
    }
}

fn egs51(d: &ControllerDef) -> Decoder<'_> {
    Decoder::new(d, Some("egs51")).unwrap()
}

#[test]
fn only_the_egs51_set_is_shipped() {
    let d = def();
    let sets: Vec<&str> = d.buses.iter().map(|b| b.name.as_str()).collect();
    assert_eq!(sets, vec!["egs51"]);
    assert!(d.diagnostics.is_empty());
    // One set: no --bus needed, and other set names are refused.
    assert!(Decoder::new(&d, None).is_ok());
    assert!(Decoder::new(&d, Some("egs53")).is_err());
    assert!(Decoder::new(&d, Some("EGS51")).is_ok());
}

#[test]
fn gs_418_display_temperature_and_gears() {
    let d = def();
    let dec = egs51(&d);
    // FSC 'D', FPC 'S', T_GET 80 degC (raw 130), MECH small (1), GZC D4, GIC D3,
    // M_VERL 10 Nm (raw 40), WHST D (4), FMRAD 0x7FF.
    let data = nag(&[
        (0, 8, b'D' as u64),
        (8, 8, b'S' as u64),
        (16, 8, 130),
        (28, 2, 1),
        (32, 4, 4),
        (36, 4, 3),
        (40, 8, 40),
        (50, 3, 4),
        (53, 11, 0x7FF),
    ]);
    let f = dec.decode(0x418, false, &data).unwrap();
    assert_eq!(f.frame, "GS_418");
    assert_eq!(val(&f, "FSC"), &Value::Ascii("D".into()));
    assert_eq!(val(&f, "FPC"), &Value::Ascii("S".into()));
    assert_eq!(f.num("T_GET"), Some(80.0));
    assert_eq!(enum_name(&f, "MECH").as_deref(), Some("small NAG"));
    assert_eq!(enum_name(&f, "GZC").as_deref(), Some("D4"));
    assert_eq!(enum_name(&f, "GIC").as_deref(), Some("D3"));
    assert_eq!(f.num("M_VERL"), Some(10.0));
    assert_eq!(enum_name(&f, "WHST").as_deref(), Some("D"));
    assert_eq!(f.num("FMRAD"), Some(2047.0));
    let f = dec.decode(0x418, false, &nag(&[(40, 8, 0xFF)])).unwrap();
    assert_eq!(val(&f, "M_VERL"), &Value::Invalid);
    let f = dec.decode(0x418, false, &nag(&[(16, 8, 40)])).unwrap();
    assert_eq!(f.num("T_GET"), Some(-10.0));
}

#[test]
fn gs_218_torque_request_gear_and_flags() {
    let d = def();
    let dec = egs51(&d);
    // Torque request byte 0 (raw * 3 Nm), request enabled, gear coding
    // R = 6 / P = 8, big gearbox, TCC slipping.
    let data = nag(&[
        (0, 8, 40),
        (15, 1, 1),
        (16, 4, 8),
        (20, 4, 6),
        (27, 1, 1),
        (24, 1, 1),
    ]);
    let f = dec.decode(0x218, false, &data).unwrap();
    assert_eq!(f.num("TORQUE_REQ"), Some(120.0));
    assert_eq!(val(&f, "TORQUE_REQ_EN"), &Value::Bool(true));
    assert_eq!(enum_name(&f, "GZC").as_deref(), Some("P"));
    assert_eq!(enum_name(&f, "GIC").as_deref(), Some("R"));
    assert_eq!(val(&f, "GEARBOX_BIG"), &Value::Bool(true));
    assert_eq!(val(&f, "TCC_SLIPPING"), &Value::Bool(true));
    assert_eq!(val(&f, "TCC_CLOSED"), &Value::Bool(false));
    let f = dec.decode(0x218, false, &nag(&[(0, 8, 0xFE)])).unwrap();
    assert_eq!(val(&f, "TORQUE_REQ"), &Value::Invalid, "0xFE = no request");
    // A value outside the EGS51 gear table is not a known gear.
    let f = dec.decode(0x218, false, &nag(&[(20, 4, 11)])).unwrap();
    assert_eq!(enum_name(&f, "GIC"), None);
}

#[test]
fn engine_esp_and_selector_frames() {
    let d = def();
    let dec = egs51(&d);
    // MS_308: NMOT 2500 rpm, T_OEL 90 degC (raw 130), KUEB_O_A.
    let f = dec
        .decode(
            0x308,
            false,
            &nag(&[(1, 1, 1), (8, 16, 2500), (40, 8, 130)]),
        )
        .unwrap();
    assert_eq!(f.num("NMOT"), Some(2500.0));
    assert_eq!(f.num("T_OEL"), Some(90.0));
    assert_eq!(val(&f, "KUEB_O_A"), &Value::Bool(true));
    // MS_210: pedal 100 % (raw 250), torque byte 0xFF (n/a).
    let f = dec
        .decode(0x210, false, &nag(&[(16, 8, 250), (48, 8, 0xFF)]))
        .unwrap();
    assert_eq!(f.num("PW"), Some(100.0));
    assert_eq!(val(&f, "M_ESP"), &Value::Invalid);
    // MS_310: indicated 150 Nm (raw 50), drag 30 Nm (10), max 0xFF (n/a).
    let f = dec
        .decode(
            0x310,
            false,
            &nag(&[(32, 8, 50), (40, 8, 10), (48, 8, 0xFF)]),
        )
        .unwrap();
    assert_eq!(f.num("IND_TORQUE"), Some(150.0));
    assert_eq!(f.num("DRG_TORQUE"), Some(30.0));
    assert_eq!(val(&f, "MAX_TORQUE"), &Value::Invalid);
    // MS_608: coolant 85 degC, intake 0xFF (n/a).
    let f = dec
        .decode(0x608, false, &nag(&[(0, 8, 125), (8, 8, 255)]))
        .unwrap();
    assert_eq!(f.num("T_MOT"), Some(85.0));
    assert_eq!(val(&f, "T_LUFT"), &Value::Invalid);
    // BS_200: brake pressed; BS_208: rear wheels 800 rpm left, right n/a.
    let f = dec.decode(0x200, false, &nag(&[(14, 2, 1)])).unwrap();
    assert_eq!(enum_name(&f, "BLS").as_deref(), Some("pressed"));
    let f = dec
        .decode(
            0x208,
            false,
            &nag(&[(48, 2, 1), (50, 14, 1600), (34, 14, 0x3FFF)]),
        )
        .unwrap();
    assert_eq!(f.num("DHL"), Some(800.0));
    assert_eq!(enum_name(&f, "DRTGHL").as_deref(), Some("forward"));
    assert_eq!(val(&f, "DHR"), &Value::Invalid);
    // EWM_230 is one byte: kickdown + selector D (5).
    let b = nag(&[(2, 1, 1), (4, 4, 5)]);
    let f = dec.decode(0x230, false, &b[..1]).unwrap();
    assert_eq!(val(&f, "KD"), &Value::Bool(true));
    assert_eq!(enum_name(&f, "WHC").as_deref(), Some("D"));
    // KLA_410: A/C compressor 12 Nm (raw 48).
    let f = dec.decode(0x410, false, &nag(&[(16, 8, 48)])).unwrap();
    assert_eq!(f.num("M_KOMP"), Some(12.0));
    // Ids outside the EGS51 set are not decoded.
    for id in [0x212, 0x300, 0x312, 0x338] {
        assert!(dec.decode(id, false, &[0; 8]).is_none(), "0x{id:X}");
    }
}

#[test]
fn summary_tracks_live_channels() {
    let d = def();
    let dec = egs51(&d);
    let frames: Vec<(f64, u32, [u8; 8])> = vec![
        (0.00, 0x418, nag(&[(16, 8, 110), (36, 4, 1)])),
        (0.02, 0x308, nag(&[(8, 16, 900)])),
        (0.04, 0x418, nag(&[(16, 8, 120), (36, 4, 2)])),
        (0.06, 0x308, nag(&[(8, 16, 3100)])),
        (0.08, 0x7E8, [0; 8]),
    ];
    let (s, decoded) = summarize(
        &dec,
        "egs51",
        frames.iter().map(|(t, id, data)| RawFrame {
            ts: Some(*t),
            id: *id,
            extended: false,
            data,
        }),
    );
    assert_eq!((s.frames, s.decoded, decoded.len()), (5, 4, 4));
    let rpm = &s.live["engine speed"];
    assert_eq!((rpm.min, rpm.max), (Some(900.0), Some(3100.0)));
    assert!(
        !s.live.contains_key("actual gear"),
        "GS_218 is not in this capture"
    );
    let gs418 = &s.per_frame["GS_418"];
    assert_eq!(gs418.count, 2);
    assert_eq!(gs418.signals["T_GET"].max, Some(70.0));
    assert_eq!(gs418.signals["GIC"].counts["D1"], 1);
    assert_eq!(gs418.first_ts, Some(0.0));
    assert_eq!(gs418.last_ts, Some(0.04));
}

#[test]
fn gear_ratios_and_tables() {
    let d = def();
    let small96 = d
        .gearsets
        .iter()
        .find(|g| g.name.starts_with("small NAG (W5A 280"))
        .unwrap();
    assert_eq!(small96.gears[0].ratio, 3.932);
    assert_eq!(small96.gears.len(), 7);
    let up = d.table("upshift schedule").unwrap();
    assert_eq!((up.rows, up.cols), (4, 11));
    assert_eq!(up.y_axis.as_ref().unwrap().labels[0], "1-2");
    assert!(d
        .tables
        .iter()
        .all(|t| t.confidence == opentune_ctrldef::Confidence::StructureOnly));
    assert!(d.checksum.is_none() && d.flash.is_none() && d.ecudefs.is_empty());
}

#[test]
fn every_signal_round_trips_through_insert() {
    // Write a value into each signal with the DBC layout and read it back:
    // catches out-of-frame layouts in the generated file.
    let d = def();
    for bus in &d.buses {
        for f in &bus.frames {
            for s in &f.signals {
                let mut data = vec![0u8; f.dlc.unwrap_or(8) as usize];
                let v = 1u64 << (s.len - 1);
                assert!(
                    opentune_ctrldef::signal::insert(&mut data, s.start, s.len, s.byte_order, v),
                    "{}/{}/{}",
                    bus.name,
                    f.name,
                    s.name
                );
                assert_eq!(s.raw(&data), Some(v));
            }
        }
    }
}
