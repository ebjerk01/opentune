//! Recognition of VAG DSG transmission control units (DQ250 02E / 0D9, DQ381,
//! DQ200, DL382): synthetic images, plus fixture-gated real reads under
//! `OPENTUNE_FIXTURES` (`tcu/dq250-02e/*.bin`, `tcu/dq381_full.bin`).

use opentune_core::ecudef;
use opentune_core::types::EcuId;
use opentune_scan::extract::extract;
use opentune_scan::family;
use opentune_scan::recognize::{recognize, Recognition, Tier};
use std::path::PathBuf;

fn run(img: &[u8]) -> Recognition {
    let det = family::detect(img);
    recognize(img, family::structural_detections(&det))
}

/// Pseudo-random fill so the image is "raw", not blank or text.
fn noise(len: usize) -> Vec<u8> {
    let mut x: u32 = 0x1234_5679;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            // keep it out of the printable range so no stray tokens appear
            (x as u8) | 0x80
        })
        .collect()
}

/// A synthetic 02E (PowerPC generation) read with the ident record and tag.
fn synthetic_02e() -> Vec<u8> {
    let mut img = noise(0x6_8000);
    let rec = b"0102E300041N  \x01GSG DSG         DCT";
    img[0x6_0008..0x6_0008 + rec.len()].copy_from_slice(rec);
    img[0x6_7FC0..0x6_7FE0].copy_from_slice(b"v069080102ea___getriebe_DSG_OS4A");
    img
}

fn ecudefs() -> Vec<ecudef::EcuDef> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ecudefs");
    ecudef::load_dir(&dir).expect("ecudefs load")
}

fn matched_def(img: &[u8]) -> Option<String> {
    let ex = extract(img);
    let id = EcuId {
        hw_number: ex.vag_part_numbers.first().cloned(),
        ..Default::default()
    };
    ecudef::match_ecu(&ecudefs(), &id).map(|d| d.id.clone())
}

#[test]
fn dq250_02e_ident_record_is_high_confidence() {
    let img = synthetic_02e();
    let r = run(&img);
    let p = r.primary.expect("02E recognised");
    assert_eq!(p.family, "DQ250");
    assert_eq!(p.supplier, "Temic");
    assert_eq!(p.variant.as_deref(), Some("02E"));
    assert_eq!(p.confidence, Tier::High);
    // The part-number evidence points at the part, not the dataset index.
    let part = p
        .evidence
        .iter()
        .find(|e| e.text.as_deref() == Some("02E300041N"))
        .expect("part evidence");
    assert_eq!(part.offset, Some(0x6_000A));
    assert!(p.evidence.iter().any(|e| e.marker.contains("0x68000 read")));
}

#[test]
fn dq250_02e_part_is_extracted_and_matches_the_definition() {
    let img = synthetic_02e();
    let ex = extract(&img);
    assert_eq!(ex.vag_part_numbers, vec!["02E300041N".to_string()]);
    assert_eq!(matched_def(&img).as_deref(), Some("temic-dq250-02e"));

    // The TriCore-generation software matches its own definition.
    let mut tc = noise(0xB_0000);
    let rec = b"0602E300013N  \x01\x00\x00\x00GSG DSG AG6     DCT";
    tc[0x3668..0x3668 + rec.len()].copy_from_slice(rec);
    assert_eq!(matched_def(&tc).as_deref(), Some("temic-dq250-02e-tricore"));
    let p = run(&tc).primary.expect("02E TriCore recognised");
    assert_eq!((p.family, p.confidence), ("DQ250", Tier::High));

    // MQB software matches the DQ250-MQB definition.
    let mut mqb = noise(0x18_0000);
    mqb[0x4_FFC0..0x4_FFCA].copy_from_slice(b"0D9300042M");
    assert_eq!(matched_def(&mqb).as_deref(), Some("temic-dq250-mqb"));
}

#[test]
fn dq200_and_dl382_part_numbers_are_medium_tier() {
    for (part, fam, variant) in [
        (&b"0AM927769G"[..], "DQ200", "0AM"),
        (b"0CW300014B", "DQ200", "0CW"),
        (b"0CK927156AA", "DL382", "0CK"),
    ] {
        let mut img = noise(0x10_0000);
        img[0x4_0000] = b' ';
        img[0x4_0001..0x4_0001 + part.len()].copy_from_slice(part);
        img[0x4_0001 + part.len()] = b' ';
        let p = run(&img)
            .primary
            .unwrap_or_else(|| panic!("{fam} recognised"));
        assert_eq!(p.family, fam);
        assert_eq!(p.variant.as_deref(), Some(variant));
        assert_eq!(p.confidence, Tier::Medium);
    }
}

#[test]
fn glued_or_malformed_tcu_parts_are_ignored() {
    for bad in [
        &b"X02E300041N "[..], // letter glued in front
        b"34502E300041N ",    // three digits in front: not a dataset index
        b" 02E300041NAB ",    // three index letters
        b" 02E301041N ",      // not a 300/927 group
        b" 02E30004 ",        // too short
    ] {
        let mut img = noise(0x10_0000);
        img[0x100..0x100 + bad.len()].copy_from_slice(bad);
        let r = run(&img);
        assert!(
            r.primary.as_ref().map_or(true, |p| p.family != "DQ250"),
            "{}: {r:#?}",
            String::from_utf8_lossy(bad)
        );
        assert!(extract(&img).vag_part_numbers.is_empty());
    }
}

fn fixtures_dir(rel: &str) -> Option<PathBuf> {
    let d = PathBuf::from(std::env::var("OPENTUNE_FIXTURES").ok()?).join(rel);
    d.exists().then_some(d)
}

#[test]
#[ignore = "needs private DSG fixtures via OPENTUNE_FIXTURES/tcu"]
fn real_dq250_02e_reads_are_recognised() {
    let Some(dir) = fixtures_dir("tcu/dq250-02e") else {
        return;
    };
    let mut n = 0;
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let img = std::fs::read(e.path()).unwrap();
        let r = run(&img);
        let p = r
            .primary
            .as_ref()
            .unwrap_or_else(|| panic!("{:?}: not recognised", e.path()));
        assert_eq!(p.family, "DQ250", "{:?}", e.path());
        assert_eq!(p.confidence, Tier::High, "{:?}", e.path());
        assert!(!r.contested, "{:?}: {r:#?}", e.path());
        let def = matched_def(&img).unwrap_or_else(|| panic!("{:?}: no ecudef", e.path()));
        assert!(def.starts_with("temic-dq250-02e"), "{:?}: {def}", e.path());
        n += 1;
    }
    assert!(n >= 9, "expected the nine 02E reads, got {n}");
}

#[test]
#[ignore = "needs private DSG fixtures via OPENTUNE_FIXTURES/tcu"]
fn real_dq381_read_is_recognised() {
    let Some(p) = fixtures_dir("tcu/dq381_full.bin") else {
        return;
    };
    let img = std::fs::read(p).unwrap();
    let r = run(&img);
    let d = r
        .primary
        .as_ref()
        .filter(|d| d.family == "DQ381")
        .or_else(|| r.others.iter().find(|d| d.family == "DQ381"))
        .unwrap_or_else(|| panic!("DQ381 not detected: {r:#?}"));
    assert_eq!(d.supplier, "Bosch");
    assert_eq!(d.confidence, Tier::High);
    assert_eq!(matched_def(&img).as_deref(), Some("bosch-dq381"));
}
