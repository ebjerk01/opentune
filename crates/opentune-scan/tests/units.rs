//! Unit coverage for the pure, hardware-free building blocks of the scanner:
//! size classification, identity-string extraction, and filename parsing. These
//! had no tests despite being the parts most sensitive to malformed input.

use opentune_scan::extract::extract;
use opentune_scan::family::size_class;
use opentune_scan::filename;
use opentune_scan::Confidence;

#[test]
fn size_class_buckets_are_exact_at_the_boundaries() {
    assert_eq!(size_class(0), "<96K");
    assert_eq!(size_class(0x1_7FFF), "<96K");
    assert_eq!(size_class(0x1_8000), "128-256K");
    assert_eq!(size_class(0x4_FFFF), "128-256K");
    assert_eq!(size_class(0x8_0000), "~512K"); // 512 KiB
    assert_eq!(size_class(0x10_0000), "~1M"); // 1 MiB
    assert_eq!(size_class(0x20_0000), "~2M"); // 2 MiB
    assert_eq!(size_class(0x40_0000), "~4M"); // 4 MiB
    assert_eq!(size_class(0x80_0000), ">4M"); // 8 MiB
}

#[test]
fn confidence_labels_round_trip() {
    assert_eq!(Confidence::None.as_str(), "None");
    assert_eq!(Confidence::Low.as_str(), "Low");
    assert_eq!(Confidence::Medium.as_str(), "Medium");
    assert_eq!(Confidence::High.as_str(), "High");
}

#[test]
fn extract_finds_nothing_in_empty_or_uniform_bytes() {
    for img in [vec![0u8; 2048], vec![0xFFu8; 2048]] {
        let e = extract(&img);
        assert!(e.vag_part_numbers.is_empty());
        assert!(e.bosch_numbers.is_empty());
        assert!(e.sw_numbers.is_empty());
        assert!(e.mercedes_numbers.is_empty());
        assert!(e.vins.is_empty());
    }
}

#[test]
fn extract_recovers_an_embedded_vag_part_number() {
    // Surround the ASCII token with non-alphanumeric separators so the matcher
    // sees a clean word boundary, as it would inside a real dump.
    let mut img = vec![0u8; 64];
    let token = b"03G906021LR";
    img[16..16 + token.len()].copy_from_slice(token);
    let e = extract(&img);
    assert!(
        e.vag_part_numbers.iter().any(|p| p == "03G906021LR"),
        "expected the VAG part number to be extracted, got {:?}",
        e.vag_part_numbers
    );
}

#[test]
fn extract_does_not_invent_a_vin_from_random_digits() {
    // A 17-char run that is not a valid VIN must be rejected, not reported.
    let mut img = vec![0u8; 64];
    img[8..25].copy_from_slice(b"00000000000000000");
    let e = extract(&img);
    assert!(e.vins.is_empty(), "an invalid VIN must never be reported");
}

#[test]
fn filename_parse_reads_identity_from_a_sorted_tree() {
    let p = filename::parse("Volkswagen/Passat/1.9 TDI/dump.bin");
    assert!(p.has_identity(), "a well-named path should yield an identity");
    assert_eq!(p.make.as_deref(), Some("Volkswagen"));
}

#[test]
fn filename_parse_is_empty_for_an_opaque_name() {
    let p = filename::parse("misc/file1234.bin");
    assert!(!p.has_identity());
    assert!(!p.full_identity());
}
