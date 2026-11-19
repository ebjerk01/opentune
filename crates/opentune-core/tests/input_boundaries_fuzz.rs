//! Deterministic seeded fuzzing of three input boundaries of the core that
//! take bytes or text from outside: the VAG measuring-block decoder
//! ([`vagmeas`], fed by an ECU), the privacy scanner and the provenance
//! checks (fed by contributed files). The invariants:
//!
//! * nothing panics, on any input;
//! * no silent wrong value: a known formula yields a finite value with a unit,
//!   anything else stays raw; a finding points at exactly the text it reports;
//! * planted identifiers are found (no false negative at a clean boundary),
//!   and clearly synthetic / reserved ones are not (no false positive).
//!
//! Same style as `tp20_fuzz.rs`: a small splitmix64 generator, no extra
//! dependencies, [`CASES`] cases per test in the normal `cargo test`.
use opentune_core::privacy::{self, PrivacyKind};
use opentune_core::protocol::vagmeas;
use opentune_core::provenance::{self, EVIDENCE_LEVELS};
use opentune_core::validate::Severity;

const CASES: u64 = 2000;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn byte(&mut self) -> u8 {
        self.next() as u8
    }
    fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[self.below(xs.len() as u64) as usize]
    }
}

// ---------------------------------------------------------------------------
// vagmeas
// ---------------------------------------------------------------------------

#[test]
fn every_formula_and_extreme_byte_scales_finitely_or_stays_raw() {
    let edges = [0u8, 1, 99, 100, 101, 127, 128, 129, 254, 255];
    for f in 0..=255u8 {
        for &a in &edges {
            for &b in &edges {
                match vagmeas::scale(f, a, b) {
                    Some((v, unit)) => {
                        assert!(vagmeas::KNOWN_FORMULAS.contains(&f), "0x{f:02X}");
                        assert!(v.is_finite(), "0x{f:02X} {a} {b} -> {v}");
                        assert!(!unit.is_empty());
                    }
                    None => assert!(!vagmeas::KNOWN_FORMULAS.contains(&f)),
                }
            }
        }
    }
    // Bounds of each known formula over the whole byte range (a value outside
    // them would be a scaling bug, not a reading).
    for f in vagmeas::KNOWN_FORMULAS {
        let (mut lo, mut hi) = (f64::MAX, f64::MIN);
        for a in 0..=255u8 {
            for b in 0..=255u8 {
                let (v, _) = vagmeas::scale(f, a, b).unwrap();
                lo = lo.min(v);
                hi = hi.max(v);
            }
        }
        let (want_lo, want_hi) = match f {
            0x06 | 0x18 => (0.0, 65.025),
            0x0E => (-255.0, 395.25),
            0x1A => (-255.0, 255.0),
            0x21 => (0.0, 650.25),
            0x5E => (-3264.0, 3238.5),
            _ => unreachable!(),
        };
        assert!(
            (lo - want_lo).abs() < 1e-9 && (hi - want_hi).abs() < 1e-9,
            "0x{f:02X}: {lo}..{hi}"
        );
    }
}

#[test]
fn records_of_any_length_split_into_whole_cells_only() {
    let mut r = Rng(0x7A6E_A500);
    for _ in 0..CASES {
        let n = r.below(64) as usize;
        let rec: Vec<u8> = (0..n).map(|_| r.byte()).collect();
        let cells = vagmeas::cells(&rec);
        assert_eq!(cells.len(), n / 3);
        for (i, c) in cells.iter().enumerate() {
            assert_eq!((c.formula, c.a, c.b), (rec[3 * i], rec[3 * i + 1], rec[3 * i + 2]));
            let s = vagmeas::scale(c.formula, c.a, c.b);
            assert_eq!(c.value, s.map(|x| x.0));
            assert_eq!(c.unit, s.map(|x| x.1).unwrap_or(""));
        }
    }
}

// ---------------------------------------------------------------------------
// privacy
// ---------------------------------------------------------------------------

const VIN_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPRSTUVWXYZ0123456789";

/// A VIN-shaped token the scanner must flag (not synthetic by its rules).
fn real_looking_vin(r: &mut Rng) -> String {
    loop {
        let mut v: Vec<u8> = (0..13).map(|_| r.pick(VIN_ALPHABET)).collect();
        v.extend((0..4).map(|_| b'0' + r.below(10) as u8));
        let v = String::from_utf8(v).unwrap();
        if privacy::is_vin_like(&v) && !privacy::is_synthetic_vin(&v) {
            return v;
        }
    }
}

fn word(r: &mut Rng) -> String {
    let n = 1 + r.below(10) as usize;
    (0..n)
        .map(|_| (b'a' + r.below(26) as u8) as char)
        .collect()
}

/// Random filler: words, punctuation, JSON noise and some non-ASCII text.
fn filler(r: &mut Rng) -> String {
    let mut s = String::new();
    for _ in 0..r.below(12) {
        match r.below(8) {
            0 => s.push_str("\n"),
            1 => s.push_str("\": \""),
            2 => s.push_str(" Ærø – ü ✓ "),
            3 => s.push_str(" 0x7E0 "),
            4 => s.push_str(&format!(" {} ", r.below(1_000_000))),
            _ => {
                s.push(' ');
                s.push_str(&word(r));
            }
        }
    }
    s.push(' ');
    s
}

fn byte_offset(text: &str, line: usize, column: usize) -> usize {
    let start: usize = text
        .split_inclusive('\n')
        .take(line - 1)
        .map(str::len)
        .sum();
    start + column - 1
}

fn check_findings_point_at_their_value(text: &str) -> Vec<privacy::PrivacyFinding> {
    let f = privacy::scan(text, &[]);
    for x in &f {
        let off = byte_offset(text, x.line, x.column);
        assert_eq!(
            text.get(off..off + x.value.len()),
            Some(x.value.as_str()),
            "{x:?} in {text:?}"
        );
        assert!(!x.message().contains(&x.value), "message leaks the value");
    }
    f
}

#[test]
fn arbitrary_bytes_never_panic_and_findings_are_exact() {
    let mut r = Rng(0x9817_AC00);
    for _ in 0..CASES {
        let n = r.below(200) as usize;
        let bytes: Vec<u8> = (0..n)
            .map(|_| match r.below(6) {
                0 => r.pick(b"@.-_:=#/\"' \n"),
                1 => r.pick(VIN_ALPHABET),
                _ => r.byte(),
            })
            .collect();
        let text = String::from_utf8_lossy(&bytes);
        check_findings_point_at_their_value(&text);
    }
}

#[test]
fn planted_identifiers_are_found_and_placeholders_are_not() {
    let mut r = Rng(0x0D47_A000);
    let tlds = ["de", "no", "com", "org", "co"];
    let reserved = ["example.com", "example.org", "mail.example", "x.test", "a.invalid"];
    for case in 0..CASES {
        let vin = real_looking_vin(&mut r);
        let email = format!("{}.{}@{}.{}", word(&mut r), word(&mut r), word(&mut r), r.pick(&tlds));
        let fake = format!("{}@{}", word(&mut r), r.pick(&reserved));
        let serial = format!("{}{}", word(&mut r), 100_000 + r.below(900_000));
        let text = format!(
            "{}{vin}{}{email}{}{fake}{}serial number: {serial}{}",
            filler(&mut r),
            filler(&mut r),
            filler(&mut r),
            filler(&mut r),
            filler(&mut r)
        );
        let f = check_findings_point_at_their_value(&text);
        let has = |k: PrivacyKind, v: &str| f.iter().any(|x| x.kind == k && x.value == v);
        assert!(has(PrivacyKind::Vin, &vin), "case {case}: VIN missed in {text:?}");
        assert!(has(PrivacyKind::Email, &email), "case {case}: e-mail missed");
        assert!(
            !f.iter().any(|x| x.value.contains(&fake)),
            "case {case}: reserved-domain address flagged"
        );
        assert!(
            f.iter()
                .any(|x| x.kind == PrivacyKind::Serial && x.value.eq_ignore_ascii_case(&serial)),
            "case {case}: serial missed in {text:?}"
        );
        // The allow list removes exactly what it names.
        let allowed = privacy::scan(&text, &[vin.clone(), email.clone()]);
        assert!(!allowed.iter().any(|x| x.value == vin || x.value == email));
        assert_eq!(allowed.len() + 2, f.len());
    }
}

#[test]
fn vin_embedded_in_a_longer_token_is_not_a_vin() {
    let mut r = Rng(0x1E57);
    for _ in 0..200 {
        let vin = real_looking_vin(&mut r);
        for t in [format!("{vin}0"), format!("A{vin}"), format!("{vin}{vin}")] {
            assert!(
                !privacy::scan(&t, &[]).iter().any(|f| f.kind == PrivacyKind::Vin),
                "{t}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// provenance
// ---------------------------------------------------------------------------

#[test]
fn provenance_checks_never_panic_and_classify_consistently() {
    let mut r = Rng(0x9A0E_0000);
    let pieces = [
        "GPL-3.0-or-later",
        "MIT",
        "AND",
        "OR",
        "WITH",
        "(",
        ")",
        ",",
        "CC-BY-NC-4.0",
        "Proprietary",
        "?",
        "EUPL-1.2",
        "Ærø",
        "  ",
        "\t",
        "nc",
    ];
    let sources = ["own read", "ISO 14229", "WinOLS", "x.clb", "kessler notes", "", "ödis"];
    for _ in 0..CASES {
        let lic: String = (0..r.below(6))
            .map(|_| format!("{} ", r.pick(&pieces)))
            .collect();
        let src: Vec<String> = (0..r.below(4))
            .map(|_| r.pick(&sources).to_string())
            .collect();
        let ev = if r.below(3) == 0 {
            word(&mut r)
        } else {
            r.pick(&EVIDENCE_LEVELS).to_string()
        };
        let issues = provenance::check(&src, &lic, &ev, "p");
        for i in &issues {
            assert!(i.location.starts_with("p."), "{i:?}");
        }
        let errs: Vec<&str> = issues
            .iter()
            .filter(|i| i.severity == Severity::Error)
            .map(|i| i.code)
            .collect();
        assert_eq!(errs.contains(&"provenance.sources_missing"), src.is_empty());
        assert_eq!(
            errs.contains(&"provenance.license_missing"),
            lic.trim().is_empty()
        );
        assert_eq!(
            errs.contains(&"provenance.evidence_unknown")
                || errs.contains(&"provenance.evidence_missing"),
            !provenance::is_evidence_level(ev.trim())
        );
        let commercial = src
            .iter()
            .filter(|s| ["WinOLS", "x.clb"].contains(&s.as_str()))
            .count();
        assert_eq!(
            errs.iter()
                .filter(|c| **c == "provenance.source_commercial")
                .count(),
            commercial,
            "{src:?}"
        );
    }
}
