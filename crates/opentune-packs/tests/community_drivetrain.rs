//! The `community-drivetrain` pack (the worked example of
//! `docs/PACK-AUTHORING.md`) loads cleanly and its controller definitions
//! decode **synthetic** frames correctly.
//!
//! The payloads are built here straight from the opendbc DBC lines
//! (`SG_ NAME : start|len@1+ (factor,offset)`, Intel byte order: bit `n` of the
//! frame is bit `n % 8` of byte `n / 8`), independently of the pack's JSON and
//! of `opentune-ctrldef`'s own bit helpers. A transcription slip in a start
//! bit, length or scale makes these tests fail.
//!
//! Validation state: synthetic-tested only. Nothing here comes from a car.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use opentune_ctrldef::decode::{summarize, RawFrame};
use opentune_ctrldef::{ControllerDef, DecodedFrame, Decoder, Value};
use opentune_packs::{load_pack, Pack, PackLibrary};

const PACK: &str = "community-drivetrain";

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn pack() -> Pack {
    load_pack(&repo().join("packs").join(PACK)).expect("pack opens")
}

fn def(id: &str) -> ControllerDef {
    pack()
        .controllers
        .into_iter()
        .find(|d| d.id == id)
        .unwrap_or_else(|| panic!("no controller {id}"))
}

/// Build a little-endian (DBC `@1`) payload from `(start, len, raw)` triples.
fn intel(fields: &[(u32, u32, u64)]) -> [u8; 8] {
    let mut word: u64 = 0;
    for &(start, len, v) in fields {
        assert!(start + len <= 64);
        assert!(
            len == 64 || v < (1u64 << len),
            "{v} does not fit {len} bits"
        );
        assert_eq!((word >> start) & ((1u64 << len) - 1), 0, "fields overlap");
        word |= v << start;
    }
    word.to_le_bytes()
}

fn val<'a>(f: &'a DecodedFrame, name: &str) -> &'a Value {
    &f.get(name)
        .unwrap_or_else(|| panic!("no signal {name} in {}", f.frame))
        .value
}

fn enum_name(f: &DecodedFrame, name: &str) -> Option<String> {
    match val(f, name) {
        Value::Enum(n) => n.clone(),
        v => panic!("{name} is {v:?}"),
    }
}

fn approx(f: &DecodedFrame, name: &str, want: f64) {
    let got = f
        .num(name)
        .unwrap_or_else(|| panic!("{name} is {:?}", val(f, name)));
    assert!((got - want).abs() < 1e-9, "{name}: got {got}, want {want}");
}

#[test]
fn pack_loads_cleanly_with_full_provenance() {
    let p = pack();
    assert!(p.report.is_clean(), "{:?}", p.report.notes);
    assert_eq!(p.manifest.id, PACK);
    assert_eq!(p.manifest.license, "GPL-3.0-or-later");
    assert_eq!(p.manifest.evidence, "synthetic-tested");
    assert!(p.manifest.sources.iter().any(|s| s.contains("opendbc")));
    let ids: BTreeSet<&str> = p.controllers.iter().map(|d| d.id.as_str()).collect();
    assert_eq!(
        ids,
        BTreeSet::from(["hyundai-kia-4wd", "hyundai-kia-tcu", "subaru-global-tcu"])
    );
    // Controllers only: the pack claims nothing it does not test.
    assert_eq!(p.entries.len(), 3);
    for d in &p.controllers {
        // No definition claims more than its frames were tested for, and every
        // source carries a URL and a licence.
        assert_eq!(d.validation.as_str(), "synthetic-tested", "{}", d.id);
        assert!(d.sources.iter().all(|s| !s.url.is_empty()), "{}", d.id);
        for diag in &d.diagnostics {
            assert_eq!(diag.validation.as_str(), "implemented", "{}", d.id);
        }
    }
}

/// The pack adds definitions the built-in `ctrldefs/` set does not have.
#[test]
fn pack_controllers_are_new() {
    let builtin: BTreeSet<String> = std::fs::read_dir(repo().join("ctrldefs"))
        .unwrap()
        .filter_map(|e| {
            let p = e.ok()?.path();
            (p.extension()? == "json").then(|| p.file_stem()?.to_str().map(str::to_string))?
        })
        .collect();
    assert!(builtin.len() >= 8);
    for d in &pack().controllers {
        assert!(!builtin.contains(&d.id), "{} duplicates ctrldefs/", d.id);
    }
    // ...and is found through the cross-pack library like any other pack.
    let lib = PackLibrary::load_all(&repo().join("packs"));
    for id in ["hyundai-kia-tcu", "hyundai-kia-4wd", "subaru-global-tcu"] {
        assert!(lib.controller_by_id(id).is_some(), "{id}");
    }
}

/// Every shipped frame is one these tests decode: adding a frame without a
/// synthetic test (and so claiming `synthetic-tested` for it) fails here.
#[test]
fn every_shipped_frame_is_exercised() {
    let tested: &[(&str, &[u32])] = &[
        ("hyundai-kia-tcu", &[0x111, 0x112, 0x113]),
        ("hyundai-kia-4wd", &[0x386, 0x428, 0x429, 0x42A]),
        ("subaru-global-tcu", &[0x048, 0x049, 0x13A]),
    ];
    for (id, ids) in tested {
        let d = def(id);
        let mut shipped: Vec<u32> = d
            .buses
            .iter()
            .flat_map(|b| b.frames.iter().map(|f| f.id))
            .collect();
        shipped.sort_unstable();
        assert_eq!(&shipped, ids, "{id}");
    }
}

#[test]
fn hyundai_tcu_frames() {
    let d = def("hyundai-kia-tcu");
    let dec = Decoder::new(&d, None).unwrap();

    // TCU11: selector 5, gear-select switch, torque request 50 % (128 x
    // 0.390625), ATF 90 degC (130 - 40), turbine 2000 rpm (8000 x 0.25).
    let f = dec
        .decode(
            0x111,
            false,
            &intel(&[
                (8, 4, 5),
                (19, 1, 1),
                (24, 8, 128),
                (32, 8, 130),
                (40, 16, 8000),
                (58, 2, 2),
                (60, 4, 0xA),
            ]),
        )
        .unwrap();
    assert_eq!(f.frame, "TCU11");
    approx(&f, "G_SEL_DISP", 5.0);
    assert_eq!(val(&f, "SWI_GS"), &Value::Bool(true));
    approx(&f, "TQI_TCU", 50.0);
    approx(&f, "TQI_TCU_INC", 0.0);
    approx(&f, "TEMP_AT", 90.0);
    approx(&f, "N_TC", 2000.0);
    approx(&f, "CF_Tcu_Alive1", 2.0);
    approx(&f, "CF_Tcu_ChkSum1", 10.0);
    let f = dec.decode(0x111, false, &intel(&[(32, 8, 0)])).unwrap();
    approx(&f, "TEMP_AT", -40.0);

    // TCU12: torque limit 200 Nm, gear D3, 87.5 km/h, fuel-cut inhibit,
    // zero spark retard (63 x 0.375 - 23.625), raw turbine 2001 rpm.
    let f = dec
        .decode(
            0x112,
            false,
            &intel(&[
                (0, 8, 100),
                (8, 4, 3),
                (16, 8, 87),
                (29, 1, 1),
                (32, 8, 63),
                (40, 16, 8004),
                (56, 8, 64),
            ]),
        )
        .unwrap();
    assert_eq!(f.frame, "TCU12");
    approx(&f, "ETL_TCU", 200.0);
    assert_eq!(enum_name(&f, "CUR_GR").as_deref(), Some("D3"));
    approx(&f, "VS_TCU", 87.0);
    approx(&f, "VS_TCU_DECIMAL", 0.5);
    assert_eq!(val(&f, "INH_FUEL_CUT"), &Value::Bool(true));
    assert_eq!(val(&f, "FUEL_CUT_TCU"), &Value::Bool(false));
    approx(&f, "SPK_RTD_TCU", 0.0);
    approx(&f, "N_TC_RAW", 2001.0);
    for (raw, name) in [(0, Some("P")), (14, Some("R")), (8, Some("D8")), (11, None)] {
        let f = dec.decode(0x112, false, &intel(&[(8, 4, raw)])).unwrap();
        assert_eq!(enum_name(&f, "CUR_GR").as_deref(), name, "raw {raw}");
    }

    // TCU13: lock-up target 1500 rpm, slope +2 %, target gear 4, torque
    // gradient 250 Nm/s, ISG target 800 rpm, sport ready.
    let f = dec
        .decode(
            0x113,
            false,
            &intel(&[
                (0, 8, 100),
                (8, 6, 36),
                (20, 4, 4),
                (32, 8, 25),
                (40, 8, 40),
                (48, 1, 1),
            ]),
        )
        .unwrap();
    assert_eq!(f.frame, "TCU13");
    approx(&f, "N_TGT_LUP", 1500.0);
    approx(&f, "SLOPE_TCU", 2.0);
    approx(&f, "CF_Tcu_TarGr", 4.0);
    approx(&f, "CF_Tcu_TqGrdLim", 250.0);
    approx(&f, "CR_Tcu_IsgTgtRPM", 800.0);
    assert_eq!(val(&f, "CF_Tcu_SptRdy"), &Value::Bool(true));
    let f = dec.decode(0x113, false, &intel(&[(8, 6, 0)])).unwrap();
    approx(&f, "SLOPE_TCU", -16.0);

    // A short payload is not decoded (the frames are 8 bytes).
    assert!(dec.decode(0x112, false, &[0; 7]).is_none());
    // Ids outside the definition are not decoded.
    assert!(dec.decode(0x110, false, &[0; 8]).is_none());
}

#[test]
fn hyundai_4wd_frames() {
    let d = def("hyundai-kia-4wd");
    let dec = Decoder::new(&d, None).unwrap();

    // _4WD11: type 1, duty 40 %, tyre radius 320 mm (120 + 200), auto mode,
    // 850 Nm through the coupling.
    let f = dec
        .decode(
            0x428,
            false,
            &intel(&[
                (0, 2, 1),
                (16, 8, 40),
                (24, 8, 120),
                (43, 1, 1),
                (48, 16, 850),
            ]),
        )
        .unwrap();
    assert_eq!(f.frame, "_4WD11");
    approx(&f, "_4WD_TYPE", 1.0);
    approx(&f, "CLU_DUTY", 40.0);
    approx(&f, "R_TIRE", 320.0);
    assert_eq!(val(&f, "AUTO_ACT"), &Value::Bool(true));
    assert_eq!(val(&f, "LOCK_ACT"), &Value::Bool(false));
    assert_eq!(val(&f, "_4H_ACT"), &Value::Bool(false));
    approx(&f, "_4WD_TQC_CUR", 850.0);
    let f = dec.decode(0x428, false, &intel(&[(44, 1, 1)])).unwrap();
    assert_eq!(val(&f, "LOCK_ACT"), &Value::Bool(true));
    assert_eq!(val(&f, "AUTO_ACT"), &Value::Bool(false));

    // _4WD12: steering +45 deg (645 - 600), wheel speeds 50..53 km/h,
    // clutch pressure 12.5 bar (1000 x 0.0625 - 50).
    let f = dec
        .decode(
            0x429,
            false,
            &intel(&[
                (0, 16, 645),
                (16, 8, 50),
                (24, 8, 51),
                (32, 8, 52),
                (40, 8, 53),
                (48, 16, 1000),
            ]),
        )
        .unwrap();
    approx(&f, "Ster_Pos", 45.0);
    approx(&f, "FRSS", 50.0);
    approx(&f, "FLSS", 51.0);
    approx(&f, "RRSS", 52.0);
    approx(&f, "RLSS", 53.0);
    approx(&f, "CLU_PRES", 12.5);

    // _4WD13 is a 6-byte frame: 25 A (64 x 0.390625), actuator 45 deg
    // (2880 / 64), thermal stress 37 %, status 3.
    let b = intel(&[(0, 8, 64), (8, 16, 2880), (24, 8, 37), (32, 8, 3)]);
    let f = dec.decode(0x42A, false, &b[..6]).unwrap();
    approx(&f, "_4WD_CURRENT", 25.0);
    approx(&f, "_4WD_POSITION", 45.0);
    approx(&f, "_4WD_CLU_THERM_STR", 37.0);
    approx(&f, "_4WD_STATUS", 3.0);
    assert!(dec.decode(0x42A, false, &b[..5]).is_none(), "min_len 6");

    // WHL_SPD11: 14-bit speeds, 1/32 km/h. The alive-counter / checksum
    // bits interleaved between them must not leak into the speeds.
    let f = dec
        .decode(
            0x386,
            false,
            &intel(&[
                (0, 14, 1600),
                (14, 2, 3),
                (16, 14, 1632),
                (30, 2, 3),
                (32, 14, 1664),
                (46, 2, 3),
                (48, 14, 1696),
                (62, 2, 3),
            ]),
        )
        .unwrap();
    approx(&f, "WHL_SPD_FL", 50.0);
    approx(&f, "WHL_SPD_FR", 51.0);
    approx(&f, "WHL_SPD_RL", 52.0);
    approx(&f, "WHL_SPD_RR", 53.0);
}

#[test]
fn subaru_transmission_frames() {
    let d = def("subaru-global-tcu");
    let dec = Decoder::new(&d, Some("global")).unwrap();

    // Transmission: counter 7, gear D (121), speed field 1234.
    let f = dec
        .decode(
            0x048,
            false,
            &intel(&[(0, 8, 0x5A), (8, 4, 7), (24, 8, 121), (40, 15, 1234)]),
        )
        .unwrap();
    assert_eq!(f.frame, "Transmission");
    approx(&f, "CHECKSUM", 90.0);
    approx(&f, "COUNTER", 7.0);
    assert_eq!(enum_name(&f, "Gear").as_deref(), Some("D"));
    approx(&f, "RPM", 1234.0);
    for (raw, name) in [
        (2, Some("N")),
        (3, Some("R")),
        (4, Some("P")),
        (137, Some("1")),
        (153, Some("3")),
        (177, Some("6")),
        (0, None),
    ] {
        let f = dec.decode(0x048, false, &intel(&[(24, 8, raw)])).unwrap();
        assert_eq!(enum_name(&f, "Gear").as_deref(), name, "raw {raw}");
    }
    // Bit 55 (top of byte 6) is outside the 15-bit RPM field.
    let f = dec
        .decode(0x048, false, &intel(&[(40, 15, 0x7FFF), (55, 1, 1)]))
        .unwrap();
    approx(&f, "RPM", 32767.0);

    // CVT: gear byte 5.
    let f = dec
        .decode(0x049, false, &intel(&[(8, 4, 1), (24, 8, 5)]))
        .unwrap();
    approx(&f, "CVT_Gear", 5.0);

    // Wheel_Speeds: 13-bit, x 0.057 km/h, in the DBC's FR, RR, RL, FL order.
    let f = dec
        .decode(
            0x13A,
            false,
            &intel(&[
                (8, 4, 1),
                (12, 13, 1000),
                (25, 13, 1000),
                (38, 13, 2000),
                (51, 13, 0x1FFF),
            ]),
        )
        .unwrap();
    approx(&f, "FR", 57.0);
    approx(&f, "RR", 57.0);
    approx(&f, "RL", 114.0);
    approx(&f, "FL", 8191.0 * 0.057);
}

/// A synthetic drive through P, R, N, D as `opentune controller decode`
/// would summarise it: the live channels see every gear once.
#[test]
fn subaru_gear_sequence_summary() {
    let d = def("subaru-global-tcu");
    let dec = Decoder::new(&d, None).unwrap();
    let frames: Vec<(f64, u32, [u8; 8])> = [4u64, 3, 2, 121]
        .iter()
        .enumerate()
        .map(|(i, g)| (i as f64 * 0.1, 0x048, intel(&[(24, 8, *g)])))
        .chain(std::iter::once((0.5, 0x13A, intel(&[(51, 13, 100)]))))
        .chain(std::iter::once((0.6, 0x7E8, [0; 8])))
        .collect();
    let (s, decoded) = summarize(
        &dec,
        "global",
        frames.iter().map(|(t, id, data)| RawFrame {
            ts: Some(*t),
            id: *id,
            extended: false,
            data,
        }),
    );
    assert_eq!((s.frames, s.decoded, decoded.len()), (6, 5, 5));
    let gear = &s.live["gear"];
    for g in ["P", "R", "N", "D"] {
        assert_eq!(gear.counts.get(g), Some(&1), "{g}: {:?}", gear.counts);
    }
    let fl = &s.live["wheel speed FL"];
    assert!((fl.max.unwrap() - 5.7).abs() < 1e-9);
}
