use super::*;

/// Deterministic pseudo-random filler (looks like code/data noise).
fn noise(len: usize, seed: u32) -> Vec<u8> {
    let mut s = seed;
    (0..len)
        .map(|_| {
            s = s.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (s >> 16) as u8
        })
        .collect()
}

fn put16(img: &mut [u8], off: usize, words: &[i64], le: bool) -> usize {
    for (i, w) in words.iter().enumerate() {
        let b = if le {
            (*w as u16).to_le_bytes()
        } else {
            (*w as u16).to_be_bytes()
        };
        img[off + 2 * i..off + 2 * i + 2].copy_from_slice(&b);
    }
    off + 2 * words.len()
}

/// A smooth `rows x cols` surface (row-major).
fn surface(rows: usize, cols: usize, base: i64) -> Vec<i64> {
    let mut v = Vec::new();
    for r in 0..rows {
        for c in 0..cols {
            v.push(base + (r as i64) * 37 + (c as i64) * 55 + ((r * c) as i64 % 7));
        }
    }
    v
}

const RPM: [i64; 8] = [800, 1000, 1500, 2000, 2500, 3000, 3500, 4500];
const LOAD: [i64; 6] = [0, 100, 200, 400, 600, 800];

/// Lay down `[8][6][rpm][load][8x6 data]` at `off`; returns the data address.
fn counted_map(img: &mut [u8], off: usize, le: bool) -> usize {
    let mut p = put16(img, off, &[8, 6], le);
    p = put16(img, p, &RPM, le);
    p = put16(img, p, &LOAD, le);
    put16(img, p, &surface(8, 6, 1200), le);
    p
}

#[test]
fn finds_counted_big_endian_map_with_shape_axes_and_hints() {
    let mut img = noise(0x4000, 7);
    let data = counted_map(&mut img, 0x1000, false);
    let found = find_maps(&img, &FindOptions::default());
    let c = found
        .iter()
        .find(|c| c.address as usize == data)
        .expect("map found");
    assert_eq!(c.layout, Layout::CountedMap);
    assert_eq!((c.rows, c.cols), (8, 6));
    assert_eq!(c.cell_width, 2);
    assert_eq!(c.endian, Endian::Big);
    assert_eq!(c.header_address, Some(0x1000));
    assert_eq!(c.y_axis.as_ref().unwrap().kind, AxisKind::RpmLike);
    assert_eq!(c.x_axis.kind, AxisKind::LoadLike);
    assert!(c.score > 0.8, "score {}", c.score);
    assert!(c.hints.iter().any(|h| h.contains("unverified")));
    // It should be the top candidate: noise does not look like a map.
    assert_eq!(found[0].address as usize, data);
    // MapDef lowering keeps the shape and flags the candidate as unverified.
    let m = c.to_mapdef();
    assert_eq!(m.address as usize, data);
    assert_eq!((m.rows, m.cols), (8, 6));
    assert!(m.notes.starts_with("CANDIDATE"));
    assert_eq!(m.y_axis.unwrap().address, Some(0x1004));
    assert_eq!(m.x_axis.unwrap().address, Some(0x1004 + 16));
}

#[test]
fn finds_little_endian_map_and_counted_curve() {
    let mut img = noise(0x4000, 11);
    let data = counted_map(&mut img, 0x2000, true);
    // Curve: [6][temperature axis s16][data], LE.
    let mut p = put16(&mut img, 0x3000, &[6], true);
    p = put16(&mut img, p, &[-40, -10, 20, 50, 80, 110], true);
    let curve = p;
    put16(&mut img, p, &[900, 850, 700, 500, 400, 380], true);
    let found = find_maps(&img, &FindOptions::default());
    let m = found.iter().find(|c| c.address as usize == data).unwrap();
    assert_eq!(m.endian, Endian::Little);
    let c = found.iter().find(|c| c.address as usize == curve).unwrap();
    assert_eq!(c.layout, Layout::CountedCurve);
    assert_eq!((c.rows, c.cols), (1, 6));
    assert!(c.x_axis.signed);
    assert_eq!(c.x_axis.kind, AxisKind::TemperatureLike);
}

#[test]
fn finds_headerless_axis_pair() {
    let mut img = noise(0x4000, 3);
    let mut p = put16(&mut img, 0x800, &RPM, false);
    p = put16(&mut img, p, &[50, 100, 200, 300, 400], false);
    let data = p;
    put16(&mut img, p, &surface(8, 5, 900), false);
    let found = find_maps(&img, &FindOptions::default());
    let c = found.iter().find(|c| c.address as usize == data).unwrap();
    assert_eq!(c.layout, Layout::AxisPair);
    assert_eq!(c.header_address, None);
    assert_eq!(c.cells(), 40);
}

#[test]
fn finds_u8_counted_map() {
    let mut img = noise(0x2000, 5);
    let mut bytes = vec![6u8, 5, 10, 20, 40, 60, 80, 100, 0, 30, 60, 90, 120];
    for r in 0..6u8 {
        for c in 0..5u8 {
            bytes.push(20 + r * 6 + c * 4);
        }
    }
    img[0x400..0x400 + bytes.len()].copy_from_slice(&bytes);
    let found = find_maps(&img, &FindOptions::default());
    let c = found.iter().find(|c| c.address == 0x400 + 13).unwrap();
    assert_eq!(c.cell_width, 1);
    assert_eq!(c.cells(), 30);
}

#[test]
fn pure_noise_and_erased_flash_yield_no_strong_candidates() {
    let img = noise(0x10000, 99);
    let strong = find_maps(&img, &FindOptions::default())
        .into_iter()
        .filter(|c| c.score >= 0.75)
        .count();
    assert_eq!(strong, 0);
    // A valid header + axes followed by erased flash is not a map.
    let mut img = vec![0xFFu8; 0x1000];
    let p = put16(&mut img, 0x100, &[4, 4, 1, 2, 3, 4, 10, 20, 30, 40], false);
    assert!(find_maps(&img, &FindOptions::default())
        .iter()
        .all(|c| c.address as usize != p));
}

#[test]
fn orientation_follows_the_smoother_reading() {
    // Data stored with axis1 varying fastest: smooth only when transposed.
    let mut img = noise(0x2000, 21);
    let mut p = put16(&mut img, 0x200, &[8, 6], false);
    p = put16(&mut img, p, &RPM, false);
    p = put16(&mut img, p, &LOAD, false);
    let data = p;
    // 6 rows (load) x 8 cols (rpm), row-major: rpm (stored first) is fastest.
    let s: Vec<i64> = (0..48)
        .map(|i| 400 + (i % 8) * 100 + (i / 8) * 10)
        .collect();
    put16(&mut img, data, &s, false);
    let c = find_maps(&img, &FindOptions::default())
        .into_iter()
        .find(|c| c.address as usize == data)
        .unwrap();
    assert_eq!((c.rows, c.cols), (6, 8));
    assert_eq!(c.x_axis.kind, AxisKind::RpmLike);
}

#[test]
fn classify_axis_hints() {
    assert_eq!(
        classify_axis(&[2431, 2731, 3031, 3331], 2, false).0,
        AxisKind::TemperatureLike
    );
    assert_eq!(
        classify_axis(&[0, 250, 500, 750, 1000], 2, false).0,
        AxisKind::PercentLike
    );
    assert_eq!(
        classify_axis(&[600, 1000, 2000, 4000, 5000], 2, false).0,
        AxisKind::RpmLike
    );
    assert_eq!(
        classify_axis(&[900, 1300, 1800, 2500], 2, false).0,
        AxisKind::PressureLike
    );
    assert_eq!(classify_axis(&[1, 2, 3], 1, false).0, AxisKind::Unknown);
}

#[test]
fn match_defs_reports_exact_axes_and_missed() {
    let mut img = noise(0x4000, 7);
    let data = counted_map(&mut img, 0x1000, false) as u32;
    let found = find_maps(&img, &FindOptions::default());
    let mut exact = found
        .iter()
        .find(|c| c.address == data)
        .unwrap()
        .to_mapdef();
    exact.name = "exact".into();
    let mut shifted = exact.clone();
    shifted.name = "axes".into();
    shifted.address += 4;
    let mut missing = exact.clone();
    missing.name = "missing".into();
    missing.address = 0x3F00;
    missing.rows = 1;
    missing.cols = 2;
    missing.x_axis = None;
    missing.y_axis = None;
    let m = match_defs(&found, &[exact, shifted, missing]);
    assert_eq!(m[0].kind, MatchKind::Exact);
    assert_eq!(m[1].kind, MatchKind::Axes);
    assert_eq!(m[2].kind, MatchKind::Missed);
}
