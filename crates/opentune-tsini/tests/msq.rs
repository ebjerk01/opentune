//! Page <-> constant mapping and the .msq format.
use opentune_tsini::*;
use std::path::PathBuf;

fn demo() -> Ini {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../samples/ts/opentune-demo.ini");
    load_ini(&p, &IniOptions::default()).unwrap()
}

fn s(v: f64) -> Value {
    Value::Scalar(v)
}

#[test]
fn page_mapping_every_type() {
    let ini = demo();
    let mut t = Tune::blank(&ini);
    t.set(&ini, "reqFuel", &s(8.5)).unwrap();
    t.set(&ini, "crankRPM", &s(400.0)).unwrap();
    t.set(&ini, "cltOffset", &s(-5.0)).unwrap();
    t.set(&ini, "injLayout", &Value::Bits(3)).unwrap();
    t.set(&ini, "nCyl", &Value::Bits(4)).unwrap();
    t.set(&ini, "flags", &Value::Bits(0xA)).unwrap();
    t.set(&ini, "injOpen", &s(-1.5)).unwrap();
    t.set(&ini, "boostGain", &s(0.75)).unwrap();
    t.set(&ini, "tuneName", &Value::Text("demo".into()))
        .unwrap();
    t.set(&ini, "wideU32", &s(3_000_000_000.0)).unwrap();
    t.set(&ini, "negS32", &s(-100.5)).unwrap();
    t.set(&ini, "idleTemp", &s(80.0)).unwrap();
    t.set(
        &ini,
        "wueBins",
        &Value::Array(vec![-40.0, 0.0, 20.0, 40.0, 60.0, 80.0]),
    )
    .unwrap();
    let p = &t.pages[0];
    assert_eq!(p[0], 85, "8.5 ms / 0.1");
    assert_eq!(&p[1..3], &[40, 0], "U16 little-endian, 400 / 10");
    assert_eq!(p[3], (-5i8) as u8);
    assert_eq!(p[4], 0b0001_0011, "two bit fields share byte 4");
    assert_eq!(&p[5..11], &[0, 40, 60, 80, 100, 120], "translate -40");
    assert_eq!(&p[17..19], &[0x00, 0x0A], "bits [8:11] of a U16");
    assert_eq!(&p[19..21], &(-1500i16).to_le_bytes());
    assert_eq!(&p[22..23], &[120]);
    assert_eq!(&p[24..28], &0.75f32.to_le_bytes());
    assert_eq!(&p[28..33], b"demo\0");
    assert_eq!(&p[44..48], &3_000_000_000u32.to_le_bytes());
    assert_eq!(&p[48..52], &(-201i32).to_le_bytes());

    for (name, want) in [
        ("reqFuel", s(8.5)),
        ("crankRPM", s(400.0)),
        ("cltOffset", s(-5.0)),
        ("injLayout", Value::Bits(3)),
        ("nCyl", Value::Bits(4)),
        ("injOpen", s(-1.5)),
        ("boostGain", s(0.75)),
        ("tuneName", Value::Text("demo".into())),
        ("negS32", s(-100.5)),
        ("idleTemp", s(80.0)),
    ] {
        match (t.get(&ini, name).unwrap(), &want) {
            (Value::Scalar(a), Value::Scalar(b)) => {
                assert!((a - b).abs() < 1e-9, "{name}: {a} vs {b}")
            }
            (got, want) => assert_eq!(&got, want, "{name}"),
        }
    }
}

#[test]
fn page_mapping_big_endian() {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../samples/ts/opentune-demo-ms.ini");
    let ini = load_ini(&p, &IniOptions::default()).unwrap();
    let mut t = Tune::blank(&ini);
    t.set(
        &ini,
        "rpmBins",
        &Value::Array(vec![500.0, 1000.0, 3000.0, 6500.0]),
    )
    .unwrap();
    t.set(&ini, "tempOff", &s(-2.5)).unwrap();
    t.set(&ini, "reqFuel", &s(12.345)).unwrap();
    let b = &t.pages[0];
    assert_eq!(&b[16..18], &[0x01, 0xF4]);
    assert_eq!(&b[30..32], &(-25i16).to_be_bytes());
    assert_eq!(&b[28..30], &12345u16.to_be_bytes());
}

#[test]
fn refuses_values_that_do_not_fit() {
    let ini = demo();
    let mut t = Tune::blank(&ini);
    t.set(&ini, "veTable", &Value::Array(vec![50.0; 48]))
        .unwrap();
    let before = t.clone();
    // 300 does not fit U08; nothing in the array is written.
    let mut v = vec![60.0; 48];
    v[47] = 300.0;
    assert!(t.set(&ini, "veTable", &Value::Array(v)).is_err());
    assert!(t.set(&ini, "veTable", &Value::Array(vec![1.0; 3])).is_err());
    assert!(t.set(&ini, "injLayout", &Value::Bits(4)).is_err());
    assert!(t.set(&ini, "cltOffset", &s(-129.0)).is_err());
    assert!(t
        .set(&ini, "tuneName", &Value::Text("x".repeat(17)))
        .is_err());
    assert!(t.set(&ini, "reqFuel", &Value::Bits(1)).is_err());
    assert!(matches!(
        t.set(&ini, "nope", &s(1.0)),
        Err(Error::Unknown(_))
    ));
    assert_eq!(t, before, "failed writes leave the pages untouched");
}

#[test]
fn expression_scale_uses_pc_variables() {
    let ini = demo();
    let mut t = Tune::blank(&ini);
    // Without stoich the scale cannot be evaluated: refused, not guessed.
    assert!(t.set(&ini, "afrTarget", &s(1.0)).is_err());
    t.pc.insert("stoich".into(), s(14.7));
    t.set(&ini, "afrTarget", &s(1.0)).unwrap();
    assert_eq!(t.pages[0][21], 147);
}

/// Fill pages with deterministic pseudo-random bytes.
fn noisy(ini: &Ini) -> Tune {
    let mut t = Tune::blank(ini);
    let mut x: u32 = 0x1234_5678;
    for p in &mut t.pages {
        for b in p.iter_mut() {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            *b = x as u8;
        }
    }
    t.pc.insert("stoich".into(), s(14.7));
    t.pc.insert("tsCanId".into(), Value::Bits(0));
    // Keep the float and the text printable.
    t.set(ini, "boostGain", &s(-12.3456)).unwrap();
    t.set(ini, "tuneName", &Value::Text("noisy tune".into()))
        .unwrap();
    t
}

#[test]
fn msq_round_trip_is_lossless() {
    let ini = demo();
    let t = noisy(&ini);
    let (m, skipped) = Msq::from_tune(
        &ini,
        &t,
        &MsqMeta {
            author: "test".into(),
            ..MsqMeta::default()
        },
    );
    assert!(skipped.is_empty(), "{skipped:?}");
    let xml = m.to_xml();
    assert!(xml.contains("signature=\"opentune demo 1\""));
    assert!(
        !xml.contains("unusedTail"),
        "noMsqSave constants are not saved"
    );
    let back = Msq::parse(&xml).unwrap();
    assert_eq!(back, m, "parse(to_xml(m)) == m");
    let (t2, rep) = back
        .to_tune(&ini, Tune::blank(&ini), MsqLoadOptions::default())
        .unwrap();
    assert!(rep.skipped.is_empty() && rep.unknown.is_empty(), "{rep:?}");
    for c in ini.constants.iter().filter(|c| !c.no_msq_save) {
        assert_eq!(
            t.get(&ini, &c.name).unwrap(),
            t2.get(&ini, &c.name).unwrap(),
            "{}",
            c.name
        );
        if !matches!(c.kind, Kind::Bits { .. }) {
            let r = c.offset..c.offset + c.byte_len();
            let pg = c.page.unwrap();
            assert_eq!(t.pages[pg][r.clone()], t2.pages[pg][r], "{} bytes", c.name);
        }
    }
    assert_eq!(t2.pc.get("stoich"), Some(&s(14.7)));
}

#[test]
fn duplicate_bit_labels_stay_exact() {
    let ini = demo();
    let mut t = Tune::blank(&ini);
    t.set(&ini, "flags", &Value::Bits(2)).unwrap(); // label "b" == label of 1
    t.set(&ini, "injLayout", &Value::Bits(2)).unwrap(); // "INVALID"
    let (m, _) = Msq::from_tune(&ini, &t, &MsqMeta::default());
    assert_eq!(m.entry("flags").unwrap().text, "2");
    assert_eq!(m.entry("injLayout").unwrap().text, "\"INVALID\"");
    assert_eq!(m.entry("nCyl").unwrap().text, "\"INVALID\"");
    let (t2, _) = m
        .to_tune(&ini, Tune::blank(&ini), MsqLoadOptions::default())
        .unwrap();
    assert_eq!(t2.get(&ini, "flags").unwrap(), Value::Bits(2));
}

const TS_STYLE: &str = r#"<?xml version="1.0" encoding="ISO-8859-1"?>
<msq xmlns="http://www.msefi.com/:msq">
<bibliography author="hand written" tuneComment="a &amp; b" writeDate="today"/>
<versionInfo fileFormat="5.0" firmwareInfo="" nPages="2" signature="opentune demo 1"/>
<page>
<pcVariable digits="1" name="stoich" units="AFR">14.7</pcVariable>
</page>
<page number="0" size="64">
<constant digits="1" name="reqFuel" units="ms">8.5</constant>
<constant name="injLayout">"Sequential"</constant>
<constant name="nCyl">"4"</constant>
<constant cols="1" digits="0" name="wueBins" rows="6" units="C">
         -40.0 
         0.0 
         20.0 
         40.0 
         60.0 
         80.0 
      </constant>
<constant digits="3" name="afrTarget" units="lambda">0.98</constant>
<constant name="tuneName">"street"</constant>
<constant digits="0" name="crankRPM" units="rpm">2000.0</constant>
<constant name="fromNewerFirmware">1.0</constant>
<constant cols="8" name="veTable" rows="6">1 2 3</constant>
</page>
<settings Comment="project settings">
<setting name="FAHRENHEIT" value="FAHRENHEIT"/>
</settings>
<userComments Comment="These are user comments."><userComment name="reqFuel" value="from calc"/></userComments>
</msq>
"#;

#[test]
fn reads_tunerstudio_style_files() {
    let ini = demo();
    let m = Msq::parse(TS_STYLE).unwrap();
    assert_eq!(m.signature(), Some("opentune demo 1"));
    assert_eq!(m.bibliography[1].1, "a & b");
    assert_eq!(
        m.settings_conditions().into_iter().collect::<Vec<_>>(),
        ["FAHRENHEIT"]
    );
    assert_eq!(m.extra.len(), 1, "userComments kept verbatim");
    // Lossless file round trip.
    assert_eq!(Msq::parse(&m.to_xml()).unwrap(), m);

    let (t, rep) = m
        .to_tune(&ini, Tune::blank(&ini), MsqLoadOptions::default())
        .unwrap();
    assert_eq!(rep.unknown, vec!["fromNewerFirmware"]);
    assert_eq!(rep.skipped.len(), 1, "{rep:?}");
    assert_eq!(rep.skipped[0].0, "veTable");
    assert_eq!(rep.out_of_range, vec!["crankRPM = 2000 outside 100..1000"]);
    assert_eq!(t.get(&ini, "reqFuel").unwrap(), s(8.5));
    assert_eq!(t.get(&ini, "injLayout").unwrap(), Value::Bits(3));
    assert_eq!(t.get(&ini, "nCyl").unwrap(), Value::Bits(4));
    assert_eq!(
        t.get(&ini, "tuneName").unwrap(),
        Value::Text("street".into())
    );
    // afrTarget's scale needs stoich from the pcVariable page.
    assert_eq!(t.pages[0][21], 144, "0.98 lambda * 14.7 / 0.1");
}

#[test]
fn signature_mismatch_is_refused_unless_forced() {
    let ini = demo();
    let other = TS_STYLE.replace("opentune demo 1", "speeduino 202504");
    let m = Msq::parse(&other).unwrap();
    let e = m
        .to_tune(&ini, Tune::blank(&ini), MsqLoadOptions::default())
        .unwrap_err();
    assert!(matches!(e, Error::SignatureMismatch { .. }), "{e}");
    let (_, rep) = m
        .to_tune(
            &ini,
            Tune::blank(&ini),
            MsqLoadOptions {
                force_signature: true,
            },
        )
        .unwrap();
    assert!(rep.notes[0].contains("forced"));
}

#[test]
fn base_tune_bytes_survive() {
    let ini = demo();
    let base = noisy(&ini);
    let m = Msq::parse(TS_STYLE).unwrap();
    let (t, _) = m
        .to_tune(&ini, base.clone(), MsqLoadOptions::default())
        .unwrap();
    // veTable was skipped (bad shape): its bytes are the base's.
    assert_eq!(t.pages[1][..48], base.pages[1][..48]);
    // The gap at byte 23 is not a constant: untouched.
    assert_eq!(t.pages[0][23], base.pages[0][23]);
    let changed = base.diff_ranges(&t);
    assert!(changed.iter().all(|(p, _, _)| *p == 0), "{changed:?}");
}
