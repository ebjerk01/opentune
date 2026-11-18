//! Synthetic tests for `opentune_mapfind::relocate`: sibling-software images
//! built from noise with tables moved, recalibrated, duplicated or changed.
use opentune_core::ecudef::{AxisDef, DataType, EcuDef, Endian, MapDef};
use opentune_mapfind::relocate::{derive_def, relocate, RelocateOptions, RelocationStatus};

fn noise(len: usize, seed: u32) -> Vec<u8> {
    let mut s = seed;
    (0..len)
        .map(|_| {
            s = s.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (s >> 16) as u8
        })
        .collect()
}

fn put16(img: &mut [u8], off: usize, words: &[u16]) -> usize {
    for (i, w) in words.iter().enumerate() {
        img[off + 2 * i..off + 2 * i + 2].copy_from_slice(&w.to_be_bytes());
    }
    off + 2 * words.len()
}

fn surface(rows: usize, cols: usize, base: u16, gain: u16) -> Vec<u16> {
    (0..rows * cols)
        .map(|i| base + gain * ((i / cols) as u16 * 3 + (i % cols) as u16 * 5) + (i as u16 % 3))
        .collect()
}

const RPM: [u16; 6] = [800, 1200, 2000, 2800, 3600, 4400];
const LOAD: [u16; 5] = [0, 150, 300, 600, 900];

/// `[6][5][rpm][load][6x5 cells]` at `off`; returns the data address.
fn counted(img: &mut [u8], off: usize, x: &[u16], cells: &[u16]) -> usize {
    let mut p = put16(img, off, &[6, 5]);
    p = put16(img, p, &RPM);
    p = put16(img, p, x);
    put16(img, p, cells);
    p
}

fn counted_def(name: &str, data: usize) -> MapDef {
    let ax = |count: u32, at: usize| AxisDef {
        count,
        address: Some(at as u32),
        data_type: DataType::U16,
        endian: Endian::Big,
        ..Default::default()
    };
    MapDef {
        name: name.into(),
        address: data as u32,
        data_type: DataType::U16,
        endian: Endian::Big,
        rows: 6,
        cols: 5,
        y_axis: Some(ax(6, data - 22)),
        x_axis: Some(ax(5, data - 10)),
        ..Default::default()
    }
}

fn plain_def(name: &str, data: usize, rows: u32, cols: u32) -> MapDef {
    MapDef {
        name: name.into(),
        address: data as u32,
        data_type: DataType::U16,
        endian: Endian::Big,
        rows,
        cols,
        ..Default::default()
    }
}

#[test]
fn structured_map_moves_with_recalibrated_cells() {
    let mut a = noise(0x8000, 1);
    let da = counted(&mut a, 0x2000, &LOAD, &surface(6, 5, 1000, 4));
    let mut b = noise(0x8000, 2);
    // Same axes/header, different (scaled) calibration, 0x1234 later.
    let db = counted(&mut b, 0x3234, &LOAD, &surface(6, 5, 1100, 5));
    let r = relocate(
        &a,
        &[counted_def("boost", da)],
        &b,
        &RelocateOptions::default(),
    );
    let m = &r.maps[0];
    assert_eq!(m.status, RelocationStatus::Relocated, "{m:?}");
    assert_eq!(m.address, Some(db as u32));
    assert_eq!(m.shift, Some(0x1234));
    assert!(m.confidence >= 0.8, "{}", m.confidence);
    assert!(m.cells_identical < 0.5);
    assert!(m.signature.starts_with("header + 2"), "{}", m.signature);
    assert!(m.evidence[0].contains("header"), "{:?}", m.evidence);
}

#[test]
fn changed_axis_is_refused_with_the_closest_partial_match() {
    let mut a = noise(0x8000, 3);
    let cells = surface(6, 5, 1000, 4);
    let da = counted(&mut a, 0x2000, &LOAD, &cells);
    let mut b = noise(0x8000, 4);
    // Identical cells, but one load breakpoint moved: axes must match.
    counted(&mut b, 0x4000, &[0, 150, 300, 650, 900], &cells);
    let r = relocate(
        &a,
        &[counted_def("boost", da)],
        &b,
        &RelocateOptions::default(),
    );
    let m = &r.maps[0];
    assert_eq!(m.status, RelocationStatus::Refused);
    let why = m.reason.as_deref().unwrap();
    assert!(why.contains("not found intact"), "{why}");
    assert!(why.contains("y-axis"), "{why}");
}

#[test]
fn identical_twins_are_placed_by_position() {
    // Two byte-identical 4x8 tables back to back plus a distinct third, in a
    // block that moved by 0x300 in the sibling; the surroundings are shared.
    let mut a = noise(0x6000, 5);
    let t = surface(4, 8, 2000, 7);
    let u = surface(4, 8, 500, 3);
    put16(&mut a, 0x2000, &t);
    put16(&mut a, 0x2040, &t);
    put16(&mut a, 0x2080, &u);
    let mut b = noise(0x6000, 6);
    b[0x1300..0x3300].copy_from_slice(&a[0x1000..0x3000]);
    let maps = [
        plain_def("twin 1", 0x2000, 4, 8),
        plain_def("twin 2", 0x2040, 4, 8),
        plain_def("third", 0x2080, 4, 8),
    ];
    let r = relocate(&a, &maps, &b, &RelocateOptions::default());
    for (m, want) in r.maps.iter().zip([0x2300u32, 0x2340, 0x2380]) {
        assert_eq!(m.address, Some(want), "{m:?}");
    }
    assert!(r.maps[0]
        .evidence
        .iter()
        .any(|e| e.contains("placed by position")));
    assert!(r.anchors > 0);
}

#[test]
fn duplicate_location_without_context_is_ambiguous() {
    let mut a = noise(0x6000, 7);
    let t = surface(4, 8, 3000, 9);
    put16(&mut a, 0x2000, &t);
    let mut b = noise(0x6000, 8);
    put16(&mut b, 0x1000, &t);
    put16(&mut b, 0x4000, &t);
    let r = relocate(
        &a,
        &[plain_def("t", 0x2000, 4, 8)],
        &b,
        &RelocateOptions::default(),
    );
    let m = &r.maps[0];
    assert_eq!(m.status, RelocationStatus::Refused);
    assert!(
        m.reason.as_deref().unwrap().starts_with("ambiguous"),
        "{m:?}"
    );
}

#[test]
fn unrelated_target_is_refused() {
    let mut a = noise(0x6000, 9);
    put16(&mut a, 0x2000, &surface(4, 8, 3000, 9));
    let b = noise(0x6000, 10);
    let r = relocate(
        &a,
        &[plain_def("t", 0x2000, 4, 8)],
        &b,
        &RelocateOptions::default(),
    );
    assert_eq!(r.maps[0].status, RelocationStatus::Refused);
    assert_eq!(r.relocated(), 0);
}

#[test]
fn recalibrated_cells_only_map_needs_aligned_surroundings() {
    let mut a = noise(0x6000, 11);
    put16(&mut a, 0x2000, &surface(4, 8, 3000, 9));
    let mut b = noise(0x6000, 12);
    b[0x1100..0x3100].copy_from_slice(&a[0x1000..0x3000]);
    // Sibling calibration: same shape, the upper two rows changed.
    let mut cal = surface(4, 8, 3000, 9);
    for v in &mut cal[16..] {
        *v += 40;
    }
    put16(&mut b, 0x2100, &cal);
    let maps = [plain_def("t", 0x2000, 4, 8)];
    let r = relocate(&a, &maps, &b, &RelocateOptions::default());
    let m = &r.maps[0];
    assert_eq!(m.address, Some(0x2100), "{m:?}");
    assert!(m.evidence.iter().any(|e| e.starts_with("bracketed")));
    // A stricter caller refuses the same placement.
    let strict = RelocateOptions {
        min_confidence: 0.95,
        ..Default::default()
    };
    let r = relocate(&a, &maps, &b, &strict);
    assert_eq!(r.maps[0].status, RelocationStatus::Refused);
    assert!(r.maps[0]
        .reason
        .as_deref()
        .unwrap()
        .starts_with("low confidence"));
}

#[test]
fn odd_offset_between_reads_is_noted() {
    let mut a = noise(0x6000, 13);
    put16(&mut a, 0x2000, &surface(4, 8, 3000, 9));
    // The target is the same read missing its first byte.
    let b = a[1..].to_vec();
    let r = relocate(
        &a,
        &[plain_def("t", 0x2000, 4, 8)],
        &b,
        &RelocateOptions::default(),
    );
    assert_eq!(r.maps[0].address, Some(0x1FFF));
    assert!(r.notes.iter().any(|n| n.contains("missing its first byte")));
}

#[test]
fn derived_definition_keeps_relocated_maps_with_moved_axes() {
    let mut a = noise(0x8000, 14);
    let da = counted(&mut a, 0x2000, &LOAD, &surface(6, 5, 1000, 4));
    let mut b = noise(0x8000, 15);
    let db = counted(&mut b, 0x2800, &LOAD, &surface(6, 5, 1000, 4));
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../ecudefs/bosch-me7.json");
    let mut def = EcuDef::from_json(&std::fs::read_to_string(path).unwrap()).unwrap();
    def.maps = vec![counted_def("boost", da), plain_def("gone", 0x5000, 4, 8)];
    let r = relocate(&a, &def.maps, &b, &RelocateOptions::default());
    let d = derive_def(&def, &r, "src-relocated", "a.bin", "b.bin");
    assert_eq!(d.id, "src-relocated");
    assert_eq!(d.maps.len(), 1);
    let m = &d.maps[0];
    assert_eq!(m.address, db as u32);
    assert_eq!(m.x_axis.as_ref().unwrap().address, Some(db as u32 - 10));
    assert_eq!(m.y_axis.as_ref().unwrap().address, Some(db as u32 - 22));
    assert!(
        m.notes.starts_with("CONFIDENCE: LOW (relocated"),
        "{}",
        m.notes
    );
    assert!(m.notes.contains("RELOCATED from 0x"));
    assert!(d.notes.contains("from definition 'bosch-me7'"));
    assert!(d.notes.contains("gone"));
    let errs: Vec<_> = d
        .validate()
        .into_iter()
        .filter(|i| i.severity == opentune_core::Severity::Error)
        .collect();
    assert!(errs.is_empty(), "{errs:?}");
}
