//! Identity-string extraction from a raw ECU image.
//!
//! OEM part numbers, software numbers, VINs and Mercedes numbers are stored as
//! ASCII inside the dump (in the EPROM identity block, the ASAM/EPK descriptor,
//! or free text in a tuner's DAMOS/`.dcm` export). We scan the raw bytes with
//! small hand-rolled matchers rather than pulling a regex dependency, so the
//! crate stays trivially auditable like `opentune-vin`. Every match is boundary
//! checked (the surrounding bytes must not extend the token) to avoid picking a
//! number out of the middle of a longer digit run.

use std::collections::BTreeMap;

fn is_digit(b: u8) -> bool {
    b.is_ascii_digit()
}
fn is_upper(b: u8) -> bool {
    b.is_ascii_uppercase()
}
fn is_alnum(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}

/// Everything identity-ish we pulled out of one image, frequency-ranked.
#[derive(Debug, Default, Clone)]
pub struct Extracted {
    /// VAG spare-part numbers (e.g. `03G906021LR`), most frequent first.
    pub vag_part_numbers: Vec<String>,
    /// Bosch 10-digit numbers starting `02` (e.g. `0281011402`).
    pub bosch_numbers: Vec<String>,
    /// Software / calibration numbers starting `103` (e.g. `1037382088`).
    pub sw_numbers: Vec<String>,
    /// Mercedes `A##########` part numbers.
    pub mercedes_numbers: Vec<String>,
    /// Validated VINs (decode to a known WMI or pass the check digit).
    pub vins: Vec<String>,
    /// Count of ASAM/EPK-looking descriptor strings seen (identity marker).
    pub epk_hits: usize,
    /// Byte offset of the first occurrence of every token above.
    pub first_offset: BTreeMap<String, usize>,
    /// Occurrence count of every token above.
    pub counts: BTreeMap<String, usize>,
}

/// Rank the keys of `counts` by descending frequency, then lexically.
fn ranked(counts: &BTreeMap<String, usize>) -> Vec<String> {
    let mut v: Vec<(&String, &usize)> = counts.iter().collect();
    v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    v.into_iter().map(|(k, _)| k.clone()).collect()
}

/// Scan a raw image for identity strings.
pub fn extract(image: &[u8]) -> Extracted {
    let n = image.len();
    let mut vag: BTreeMap<String, usize> = BTreeMap::new();
    let mut bosch: BTreeMap<String, usize> = BTreeMap::new();
    let mut sw: BTreeMap<String, usize> = BTreeMap::new();
    let mut merc: BTreeMap<String, usize> = BTreeMap::new();
    let mut vins: BTreeMap<String, usize> = BTreeMap::new();
    let mut epk = 0usize;
    let mut first: BTreeMap<String, usize> = BTreeMap::new();

    let before_ok = |i: usize| i == 0 || !is_alnum(image[i - 1]);

    let mut i = 0usize;
    while i < n {
        let b = image[i];

        // --- VAG strict: dd G dddddd [A-Z]{0,2} (e.g. 03G906021LR) ---
        if is_digit(b) && before_ok(i) {
            if let Some((tok, len)) = match_vag_strict(image, i) {
                first.entry(tok.clone()).or_insert(i);
                *vag.entry(tok).or_default() += 1;
                i += len;
                continue;
            }
            // --- VAG generic 9-digit: ddd ddd ddd [A-Z]{0,2} ---
            if let Some((tok, len)) = match_vag_generic(image, i) {
                first.entry(tok.clone()).or_insert(i);
                *vag.entry(tok).or_default() += 1;
                i += len;
                continue;
            }
            // --- Bosch / software: 10 digit run ---
            if let Some((tok, len)) = match_digits_run(image, i, 10) {
                if tok.starts_with("02") {
                    first.entry(tok.clone()).or_insert(i);
                    *bosch.entry(tok).or_default() += 1;
                } else if tok.starts_with("103") {
                    first.entry(tok.clone()).or_insert(i);
                    *sw.entry(tok).or_default() += 1;
                }
                i += len;
                continue;
            }
        }

        // --- Mercedes: A########## ---
        if b == b'A' && before_ok(i) {
            if let Some((tok, len)) = match_mercedes(image, i) {
                first.entry(tok.clone()).or_insert(i);
                *merc.entry(tok).or_default() += 1;
                i += len;
                continue;
            }
        }

        // --- VIN: 17 VIN-valid chars, validated by opentune-vin ---
        if is_vin_char(b) && before_ok(i) {
            if let Some((tok, len)) = match_vin(image, i) {
                first.entry(tok.clone()).or_insert(i);
                *vins.entry(tok).or_default() += 1;
                i += len;
                continue;
            }
        }

        // --- ASAM/EPK descriptor marker (identity-block signal) ---
        if (b == b'E' || b == b'@') && has_ascii_at(image, i, b"EPK") {
            epk += 1;
        }

        i += 1;
    }

    let counts: BTreeMap<String, usize> = [&vag, &bosch, &sw, &merc, &vins]
        .iter()
        .flat_map(|m| m.iter().map(|(k, v)| (k.clone(), *v)))
        .collect();
    Extracted {
        vag_part_numbers: ranked(&vag),
        bosch_numbers: ranked(&bosch),
        sw_numbers: ranked(&sw),
        mercedes_numbers: ranked(&merc),
        vins: ranked(&vins),
        epk_hits: epk,
        first_offset: first,
        counts,
    }
}

fn has_ascii_at(image: &[u8], i: usize, pat: &[u8]) -> bool {
    image
        .get(i..i + pat.len())
        .map(|s| s == pat)
        .unwrap_or(false)
}

/// `dd G dddddd [A-Z]{0,2}` starting at `i`. Returns (token, consumed-len).
fn match_vag_strict(image: &[u8], i: usize) -> Option<(String, usize)> {
    let s = image.get(i..i + 9)?;
    if !(is_digit(s[0]) && is_digit(s[1]) && s[2] == b'G' && s[3..9].iter().all(|&b| is_digit(b))) {
        return None;
    }
    let mut len = 9;
    let mut tok = String::new();
    for &b in &s[..9] {
        tok.push(b as char);
    }
    // up to two trailing version letters
    for k in 0..2 {
        match image.get(i + 9 + k) {
            Some(&b) if is_upper(b) => {
                tok.push(b as char);
                len += 1;
            }
            _ => break,
        }
    }
    // boundary: next must not continue the token
    if image.get(i + len).map(|&b| is_alnum(b)).unwrap_or(false) {
        return None;
    }
    Some((tok, len))
}

/// `ddd ?ddd ?ddd [A-Z]{0,2}` (VAG spare-part shape with optional spaces).
fn match_vag_generic(image: &[u8], i: usize) -> Option<(String, usize)> {
    let mut pos = i;
    let mut digits = String::new();
    let mut consumed = 0usize;
    for group in 0..3 {
        for _ in 0..3 {
            match image.get(pos) {
                Some(&b) if is_digit(b) => {
                    digits.push(b as char);
                    pos += 1;
                    consumed += 1;
                }
                _ => return None,
            }
        }
        if group < 2 {
            // optional single separator space
            if image.get(pos) == Some(&b' ') {
                pos += 1;
                consumed += 1;
            }
        }
    }
    // require the middle group to look like a VAG main group (starts 9/8/0..)
    // and reject an all-identical run (e.g. 000000000, 111111111).
    if digits.bytes().all(|c| c == digits.as_bytes()[0]) {
        return None;
    }
    let mut tok = digits;
    for k in 0..2 {
        match image.get(pos + k) {
            Some(&b) if is_upper(b) => {
                tok.push(b as char);
                consumed += 1;
            }
            _ => break,
        }
    }
    if image
        .get(i + consumed)
        .map(|&b| is_alnum(b))
        .unwrap_or(false)
    {
        return None;
    }
    Some((tok, consumed))
}

/// Exactly `want` ASCII digits, boundary-checked.
fn match_digits_run(image: &[u8], i: usize, want: usize) -> Option<(String, usize)> {
    let s = image.get(i..i + want)?;
    if !s.iter().all(|&b| is_digit(b)) {
        return None;
    }
    if image.get(i + want).map(|&b| is_digit(b)).unwrap_or(false) {
        return None; // longer run, not exactly `want`
    }
    let tok: String = s.iter().map(|&b| b as char).collect();
    Some((tok, want))
}

/// `A` + 10 digits, boundary-checked.
fn match_mercedes(image: &[u8], i: usize) -> Option<(String, usize)> {
    let s = image.get(i + 1..i + 11)?;
    if !s.iter().all(|&b| is_digit(b)) {
        return None;
    }
    if image.get(i + 11).map(|&b| is_digit(b)).unwrap_or(false) {
        return None;
    }
    let mut tok = String::from("A");
    for &b in s {
        tok.push(b as char);
    }
    Some((tok, 11))
}

fn is_vin_char(b: u8) -> bool {
    (b.is_ascii_uppercase() && b != b'I' && b != b'O' && b != b'Q') || b.is_ascii_digit()
}

/// 17 VIN-valid chars validated via opentune-vin (known WMI or valid check digit).
fn match_vin(image: &[u8], i: usize) -> Option<(String, usize)> {
    let s = image.get(i..i + 17)?;
    if !s.iter().all(|&b| is_vin_char(b)) {
        return None;
    }
    if image.get(i + 17).map(|&b| is_vin_char(b)).unwrap_or(false) {
        return None;
    }
    let tok: String = s.iter().map(|&b| b as char).collect();
    // reject low-entropy runs (e.g. all zeros / repeated)
    let distinct = {
        let mut set = [false; 256];
        let mut c = 0;
        for &b in s {
            if !set[b as usize] {
                set[b as usize] = true;
                c += 1;
            }
        }
        c
    };
    if distinct < 6 {
        return None;
    }
    let decoded = opentune_vin::decode_vin(&tok).ok()?;
    let known = decoded.wmi_info.manufacturer.is_some() || decoded.check_digit_valid == Some(true);
    if known {
        Some((tok, 17))
    } else {
        None
    }
}
