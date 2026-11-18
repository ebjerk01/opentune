//! Fixture-gated validation of the DQ250 DSG definitions (`temic-dq250-02e`,
//! `temic-dq250-02e-tricore`) against real 02E reads — see `docs/DSG.md`.
//!
//! No stock/tuned pair of any DSG software was available, so instead of the
//! "tune changes fall inside maps" check of `fleet_maps_fixtures.rs` this asserts:
//!
//! * every map decodes on each real read without validator errors;
//! * every shipped shift table has the documented shape (4 rows of 12
//!   non-decreasing cells in the engine-speed band) on every read of the
//!   software the addresses target;
//! * the sibling-calibration evidence the PowerPC maps rest on still holds: an
//!   A3 2.0 TDI and a TT 2.0 TFSI dataset of the same software differ inside
//!   **every** shipped map, and nothing outside the calibration sector differs.
//!
//! The dumps are private and never committed; point `OPENTUNE_FIXTURES` at the
//! fixture tree (`tcu/dq250-02e/*.bin`) and run the ignored tests:
//!
//! ```text
//! OPENTUNE_FIXTURES=/path/to/opentune-fixtures \
//!   cargo test -p opentune-core --test dsg_fixtures -- --ignored
//! ```
use opentune_core::ecudef::{EcuDef, Endian, MapDef};
use opentune_core::validate::Severity;
use std::path::PathBuf;

fn ecudef(id: &str) -> EcuDef {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../ecudefs")
        .join(format!("{id}.json"));
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    EcuDef::from_json(&text).unwrap_or_else(|e| panic!("{id}: {e}"))
}

/// The first `tcu/dq250-02e` fixture whose file name contains every `needle`.
/// File names carry the software part number, so the test does not hard-code
/// the private names.
fn fixture(needles: &[&str]) -> Option<Vec<u8>> {
    let dir = std::env::var("OPENTUNE_FIXTURES").ok()?;
    let rd = std::fs::read_dir(format!("{dir}/tcu/dq250-02e")).ok()?;
    let mut names: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    names.sort();
    let p = names.into_iter().find(|p| {
        let n = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        needles.iter().all(|k| n.contains(k))
    })?;
    std::fs::read(p).ok()
}

fn cells(img: &[u8], m: &MapDef) -> Vec<u16> {
    let a = m.address as usize;
    img[a..a + m.byte_len() as usize]
        .chunks_exact(2)
        .map(|c| {
            if m.endian == Endian::Big {
                u16::from_be_bytes([c[0], c[1]])
            } else {
                u16::from_le_bytes([c[0], c[1]])
            }
        })
        .collect()
}

/// Decode without errors, and every shift table has the documented shape.
fn check_shape(id: &str, img: &[u8]) {
    let def = ecudef(id);
    assert_eq!(img.len(), def.flash_size as usize, "{id}: read size");
    let errs: Vec<_> = def
        .validate_against_image(img)
        .into_iter()
        .filter(|i| i.severity == Severity::Error)
        .collect();
    assert!(errs.is_empty(), "{id}: decode errors: {errs:#?}");
    assert!(!def.maps.is_empty());
    for m in &def.maps {
        assert!(m.name.starts_with("shift schedule"), "{}", m.name);
        assert_eq!((m.rows, m.cols), (4, 12), "{}", m.name);
        let c = cells(img, m);
        // Downshift-into-first rows start near standstill (tens), every table
        // tops out in the engine-speed band.
        let max = c.iter().copied().max().unwrap_or(0);
        assert!(
            c.iter().all(|&v| v <= 8000) && (1000..=8000).contains(&max),
            "{}: cells outside the engine-speed band: {c:?}",
            m.name
        );
        for row in c.chunks(12) {
            assert!(
                row.windows(2).all(|w| w[0] <= w[1]),
                "{}: row not non-decreasing: {row:?}",
                m.name
            );
        }
    }
}

#[test]
#[ignore = "needs private DQ250 02E fixtures via OPENTUNE_FIXTURES/tcu/dq250-02e"]
fn dq250_02e_maps_decode_on_every_read_of_the_target_software() {
    let mut seen = 0;
    for pn in ["02E300041N", "02E300046K", "02E300042M"] {
        let Some(img) = fixture(&[pn]) else {
            continue;
        };
        check_shape("temic-dq250-02e", &img);
        seen += 1;
    }
    if seen > 0 {
        assert_eq!(seen, 3, "expected all three target-software reads");
    }
}

#[test]
#[ignore = "needs private DQ250 02E fixtures via OPENTUNE_FIXTURES/tcu/dq250-02e"]
fn dq250_02e_sibling_calibrations_differ_inside_every_map() {
    let (Some(a3), Some(tt)) = (fixture(&["02E300041N"]), fixture(&["02E300046K"])) else {
        return;
    };
    let def = ecudef("temic-dq250-02e");
    // Same software: the code sectors are byte-identical, only CAL differs.
    assert_eq!(a3[..0x6_0000], tt[..0x6_0000]);
    for m in &def.maps {
        assert_ne!(
            cells(&a3, m),
            cells(&tt, m),
            "{}: identical in both datasets — not calibration",
            m.name
        );
    }
    // Writes go through the 02E sector-table checksum (its verify/correct round
    // trip on these reads is covered by opentune-checksum's fixture test).
    assert_eq!(def.checksum.as_deref(), Some("dq250-02e"));
}

#[test]
#[ignore = "needs private DQ250 02E fixtures via OPENTUNE_FIXTURES/tcu/dq250-02e"]
fn dq250_02e_tricore_maps_decode() {
    let mut seen = 0;
    for tag in ["GTI", "R.bin"] {
        let Some(img) = fixture(&["02E300013N", tag]) else {
            continue;
        };
        check_shape("temic-dq250-02e-tricore", &img);
        seen += 1;
    }
    if seen > 0 {
        assert_eq!(seen, 2);
    }
}

#[test]
#[ignore = "needs private DQ250 02E fixtures via OPENTUNE_FIXTURES/tcu/dq250-02e"]
fn dq250_02e_layout_shift_documented_for_0918_software() {
    // docs/MAP-LIBRARY.md: 02E300042F/G hold set A 0x15A6 bytes later.
    let Some(img) = fixture(&["02E300042G"]) else {
        return;
    };
    let def = ecudef("temic-dq250-02e");
    for m in def
        .maps
        .iter()
        .filter(|m| m.name.starts_with("shift schedule A"))
    {
        let mut moved = m.clone();
        moved.address += 0x15A6;
        let c = cells(&img, &moved);
        assert!(c.iter().all(|&v| v <= 8000), "{}", m.name);
        for row in c.chunks(12) {
            assert!(row.windows(2).all(|w| w[0] <= w[1]), "{}", m.name);
        }
    }
}
