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
//! * the drive-off table has its documented shape (an 8-point axis 0..~1000
//!   stored in front of 12 rows x 8 rising cells that start in the idle band,
//!   the rows being four identical groups of three curves) — on the target
//!   reads and, at the documented per-build offsets, on every other 02E build;
//! * the sibling-calibration evidence the PowerPC maps rest on still holds: an
//!   A3 2.0 TDI and a TT 2.0 TFSI dataset of the same software differ inside
//!   **every** shipped map, and nothing outside the calibration sector differs;
//!   the drive-off table also differs in the third (Golf GTI) dataset and starts
//!   at a lower idle speed on the petrol calibrations than on the diesel one.
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

/// `n` u16 values at `addr` in the given byte order.
fn words(img: &[u8], addr: u32, n: usize, endian: Endian) -> Vec<u16> {
    let a = addr as usize;
    img[a..a + 2 * n]
        .chunks_exact(2)
        .map(|c| match endian {
            Endian::Big => u16::from_be_bytes([c[0], c[1]]),
            Endian::Little => u16::from_le_bytes([c[0], c[1]]),
        })
        .collect()
}

const DRIVE_OFF: &str = "drive-off engine speed vs pedal (inferred)";

/// The drive-off table shape: axis `0 .. 1000..=1100` strictly increasing,
/// 12 rows x 8 non-decreasing cells, every row starting in the idle band and
/// staying below 4000, rows `r` and `r + 3` identical (four groups of the same
/// three curves).
fn check_drive_off(label: &str, img: &[u8], axis: u32, data: u32, endian: Endian) {
    let ax = words(img, axis, 8, endian);
    assert_eq!(ax[0], 0, "{label}: axis {ax:?}");
    assert!(ax.windows(2).all(|w| w[0] < w[1]), "{label}: axis {ax:?}");
    assert!((1000..=1100).contains(&ax[7]), "{label}: axis {ax:?}");
    let c = words(img, data, 96, endian);
    let rows: Vec<&[u16]> = c.chunks(8).collect();
    for (r, row) in rows.iter().enumerate() {
        assert!(
            (600..=1100).contains(&row[0]),
            "{label}: row {r} not starting at idle: {row:?}"
        );
        assert!(
            row.windows(2).all(|w| w[0] <= w[1]) && row[7] > row[0] && row[7] < 4000,
            "{label}: row {r} not a rising engine-speed curve: {row:?}"
        );
        if r >= 3 {
            assert_eq!(
                *row,
                rows[r - 3],
                "{label}: row {r} breaks the 4 x 3 repeat"
            );
        }
    }
}

/// Decode without errors, and every shipped table has its documented shape.
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
        if m.name == DRIVE_OFF {
            assert_eq!((m.rows, m.cols), (12, 8));
            let ax = m
                .x_axis
                .as_ref()
                .and_then(|a| a.address)
                .expect("stored axis");
            assert_eq!(ax + 16, m.address, "axis directly in front of the data");
            check_drive_off(id, img, ax, m.address, m.endian);
            continue;
        }
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

#[test]
#[ignore = "needs private DQ250 02E fixtures via OPENTUNE_FIXTURES/tcu/dq250-02e"]
fn dq250_02e_drive_off_follows_the_engine() {
    let (Some(a3), Some(tt), Some(gti)) = (
        fixture(&["02E300041N"]),
        fixture(&["02E300046K"]),
        fixture(&["02E300042M"]),
    ) else {
        return;
    };
    let def = ecudef("temic-dq250-02e");
    let m = def.maps.iter().find(|m| m.name == DRIVE_OFF).expect("map");
    let (ca, ct, cg) = (cells(&a3, m), cells(&tt, m), cells(&gti, m));
    // Three calibrations of one code build, three different tables.
    assert_ne!(ca, ct);
    assert_ne!(ca, cg);
    assert_ne!(ct, cg);
    // Starts at the engine's idle band: the 2.0 TDI above both 2.0 TFSI, and
    // tops out lowest on the diesel (narrow engine-speed band).
    assert!(
        ca[0] > ct[0] && ca[0] > cg[0],
        "{} {} {}",
        ca[0],
        ct[0],
        cg[0]
    );
    let top = |c: &[u16]| c.iter().copied().max().unwrap_or(0);
    assert!(top(&ca) < top(&ct) && top(&ca) < top(&cg));
}

#[test]
#[ignore = "needs private DQ250 02E fixtures via OPENTUNE_FIXTURES/tcu/dq250-02e"]
fn dq250_02e_drive_off_documented_for_other_builds() {
    // docs/MAP-LIBRARY.md: the drive-off table (axis address) per build.
    for (needles, axis) in [
        (&["02E300042F"][..], 0x6_315C_u32),
        (&["02E300042G"][..], 0x6_315C),
        (&["R32"][..], 0x6_3664),
        (&["02E300011C"][..], 0x6_3644),
    ] {
        let Some(img) = fixture(needles) else {
            continue;
        };
        check_drive_off(needles[0], &img, axis, axis + 16, Endian::Big);
    }
}
