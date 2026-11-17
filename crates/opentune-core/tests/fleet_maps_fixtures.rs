//! Fixture-gated validation of the shipped map libraries against real
//! stock/tuned reference reads (see `docs/MAP-LIBRARY.md`).
//!
//! For each ECU definition, every region a reference tune changed must fall
//! inside a defined map (so the viewer/compare highlights real tuned cells), and
//! every map must decode against the stock read without errors (in range, sane
//! stored axes).
//!
//! The dumps are private and never committed; point `OPENTUNE_FIXTURES` at the
//! fixture tree and run the ignored tests:
//!
//! ```text
//! OPENTUNE_FIXTURES=/path/to/opentune-fixtures \
//!   cargo test -p opentune-core --test fleet_maps_fixtures -- --ignored
//! ```
use opentune_core::ecudef::EcuDef;
use opentune_core::validate::Severity;
use std::path::PathBuf;

fn ecudef(id: &str) -> EcuDef {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../ecudefs")
        .join(format!("{id}.json"));
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    EcuDef::from_json(&text).unwrap_or_else(|e| panic!("{id}: {e}"))
}

fn fixture(rel: &str) -> Option<Vec<u8>> {
    let dir = std::env::var("OPENTUNE_FIXTURES").ok()?;
    std::fs::read(format!("{dir}/{rel}")).ok()
}

/// Cluster byte differences into regions, merging changes separated by at most
/// `gap` unchanged bytes (the same clustering `opentune findmaps` uses).
fn changed_regions(a: &[u8], b: &[u8], gap: usize) -> Vec<(usize, usize)> {
    let n = a.len().min(b.len());
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        if a[i] == b[i] {
            i += 1;
            continue;
        }
        let start = i;
        let mut end = i + 1;
        let mut since = 0usize;
        let mut j = i + 1;
        while j < n && since <= gap {
            if a[j] != b[j] {
                end = j + 1;
                since = 0;
            } else {
                since += 1;
            }
            j += 1;
        }
        out.push((start, end));
        i = end;
    }
    out
}

/// Names of the maps whose data block fully contains `[s, e)`.
fn containing_maps(def: &EcuDef, s: usize, e: usize) -> Vec<String> {
    def.maps
        .iter()
        .filter(|m| {
            let a = m.address as usize;
            a <= s && e <= a + m.byte_len() as usize
        })
        .map(|m| m.name.clone())
        .collect()
}

/// Assert the definition decodes cleanly on the stock read, and that every
/// changed region of the tune (outside `ignore`, after folding calibration
/// mirrors at `mirrors` back onto the first copy) lies inside a defined map.
/// Returns the set of map names the tune touched.
fn check_pair(
    id: &str,
    stock: &[u8],
    tuned: &[u8],
    mirrors: &[usize],
    ignore: &[(usize, usize)],
) -> Vec<String> {
    let def = ecudef(id);
    let errs: Vec<_> = def
        .validate_against_image(stock)
        .into_iter()
        .filter(|i| i.severity == Severity::Error)
        .collect();
    assert!(errs.is_empty(), "{id}: decode errors on stock: {errs:#?}");

    let mut touched: Vec<String> = Vec::new();
    for (s, e) in changed_regions(stock, tuned, 16) {
        if ignore.iter().any(|&(a, b)| a <= s && e <= b) {
            continue;
        }
        let hit = std::iter::once(0usize)
            .chain(mirrors.iter().copied())
            .filter(|&off| s >= off)
            .map(|off| containing_maps(&def, s - off, e - off))
            .find(|v| !v.is_empty());
        let Some(names) = hit else {
            panic!("{id}: tune changed 0x{s:06X}..0x{e:06X} outside every defined map");
        };
        for n in names {
            if !touched.contains(&n) {
                touched.push(n);
            }
        }
    }
    touched
}

#[test]
#[ignore = "needs private EDC16U34 fixtures via OPENTUNE_FIXTURES/golf-real"]
fn edc16u34_tune_changes_fall_inside_maps() {
    let (Some(stock), Some(tuned)) = (
        fixture("golf-real/stock.bin"),
        fixture("golf-real/tuned_uncorrected.bin"),
    ) else {
        return;
    };
    let touched = check_pair("bosch-edc16u34", &stock, &tuned, &[], &[]);
    assert!(touched
        .iter()
        .any(|n| n.starts_with("injection quantity limiter")));
    assert!(touched.iter().any(|n| n.starts_with("torque limiter")));
    assert_eq!(touched.len(), 2, "touched: {touched:?}");
}

#[test]
#[ignore = "needs private PCR2.1 fixtures via OPENTUNE_FIXTURES/pcr21"]
fn pcr21_tune_changes_fall_inside_maps() {
    let (Some(stock), Some(tuned)) = (
        fixture("pcr21/golf90_stock.bin"),
        fixture("pcr21/golf90_tuned.bin"),
    ) else {
        return;
    };
    // The uncorrected tune keeps the old CRC; nothing outside the maps changed.
    let touched = check_pair("siemens-pcr21", &stock, &tuned, &[], &[]);
    assert_eq!(
        touched.len(),
        47,
        "every shipped PCR2.1 map is diff-confirmed"
    );
}

#[test]
#[ignore = "needs private EDC15C6 fixtures via OPENTUNE_FIXTURES/edc15"]
fn edc15c6_tune_changes_fall_inside_maps() {
    let (Some(stock), Some(tuned)) = (
        fixture("edc15/MB_sprinter_2.2CDI_edc15c6_351686_ori.bin"),
        fixture("edc15/MB_sprinter_2.2CDI_edc15c6_351686_mod.bin"),
    ) else {
        return;
    };
    // The read holds the calibration four times; the 4-byte words at the end of
    // each copy are its checksum (handled by the checksum engine, not a map).
    let mirrors = [0xC000, 0x18000, 0xE0000];
    let ignore: Vec<(usize, usize)> = std::iter::once(0usize)
        .chain(mirrors)
        .map(|off| (0x1BFF8 + off, 0x1C000 + off))
        .collect();
    let touched = check_pair("bosch-edc15c6", &stock, &tuned, &mirrors, &ignore);
    assert!(touched
        .iter()
        .any(|n| n.starts_with("injection quantity limiter")));
    assert_eq!(touched.len(), 8, "touched: {touched:?}");
}

/// A sibling MB EDC15C6 software with the same layout: its tune changed the
/// boost target that the Sprinter definition ships at the identical address.
#[test]
#[ignore = "needs private EDC15C6 fixtures via OPENTUNE_FIXTURES/edc15"]
fn edc15c6_sibling_tune_confirms_boost_target() {
    let (Some(stock), Some(tuned)) = (
        fixture("edc15/mb_c220_351637_ori.bin"),
        fixture("edc15/mb_c220_351637_mod.bin"),
    ) else {
        return;
    };
    let def = ecudef("bosch-edc15c6");
    let m = def
        .maps
        .iter()
        .find(|m| m.name == "boost pressure target")
        .expect("boost target map");
    let (a, len) = (m.address as usize, m.byte_len() as usize);
    assert_ne!(
        stock[a..a + len],
        tuned[a..a + len],
        "sibling tune changed it"
    );
    // Same header (input ids + dims) in front of the data in both software.
    let hdr = a - (4 + 2 * 12) * 2;
    assert_eq!(stock[hdr..hdr + 4], [0x94, 0xC6, 12, 0]);
}
