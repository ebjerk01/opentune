//! # opentune-image
//!
//! An **addressed sparse-image** model plus **Intel HEX** and **Motorola SREC**
//! readers and writers for OpenTune.
//!
//! Flash I/O in OpenTune works with *explicit addressed segments* before it ever
//! flattens to a contiguous buffer: a container exposes several code/calibration
//! segments, a bench read covers only the pages requested, and a HEX/SREC tune
//! file touches just the bytes it changes. [`SparseImage`] keeps those segments
//! and the holes between them explicit, so no stage has to guess that a gap is
//! "probably `0xFF`". Materialising a flat buffer is a deliberate, separate step
//! ([`SparseImage::flatten`]) that names the fill byte and reports every range it
//! invented.
//!
//! ## What this crate does
//!
//! * [`SparseImage`] — ordered, non-overlapping [`Segment`]s with an
//!   overlap-policy insert ([`Overlap`]), byte/range queries that return `None`
//!   in a hole, segment and gap iteration, total span, and a hole-filling
//!   [`SparseImage::flatten`].
//! * [`hex`] — Intel HEX reader/writer (records 00/01/02/03/04/05; 16/20/32-bit
//!   addressing via the extension records; validated checksums; enforced EOF).
//! * [`srec`] — Motorola SREC reader/writer (S0/S1/S2/S3/S5/S6/S7/S8/S9;
//!   16/24/32-bit addresses; validated checksums; enforced termination).
//!
//! Errors are honest ([`ImageError`]): a bad checksum, a malformed or truncated
//! record, or an overlapping write is always reported, never silently dropped.
//!
//! ## Example
//!
//! ```
//! use opentune_image::{hex, srec, Overlap, SparseImage};
//!
//! let mut img = SparseImage::new();
//! img.insert(0x0800_0000, &[0xDE, 0xAD, 0xBE, 0xEF], Overlap::Error).unwrap();
//!
//! // Round-trip through Intel HEX.
//! let file = hex::HexFile { image: img.clone(), start: None };
//! let text = hex::write(&file, hex::WriteOptions::default()).unwrap();
//! assert_eq!(hex::read(&text).unwrap().image, img);
//!
//! // The same image expressed as SREC decodes identically.
//! let s = srec::SRecFile { header: None, image: img.clone(), start: None };
//! let stext = srec::write(&s, srec::WriteOptions::default()).unwrap();
//! assert_eq!(srec::read(&stext).unwrap().image, img);
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod error;
pub mod hex;
pub mod sparse;
pub mod srec;

pub use error::{ImageError, Result};
pub use sparse::{Flattened, InsertReport, Overlap, Range, Segment, SparseImage};

#[cfg(test)]
mod tests {
    use super::*;

    /// Insert helper that panics on error, for terse tests.
    fn img_from(parts: &[(u64, &[u8])]) -> SparseImage {
        let mut img = SparseImage::new();
        for (addr, bytes) in parts {
            img.insert(*addr, bytes, Overlap::Error).unwrap();
        }
        img
    }

    #[test]
    fn contiguous_inserts_coalesce() {
        let img = img_from(&[(0x10, &[1, 2]), (0x12, &[3, 4])]);
        assert_eq!(img.segment_count(), 1);
        assert_eq!(img.span(), Some((0x10, 0x14)));
        assert_eq!(img.range(0x10, 4), Some(vec![1, 2, 3, 4]));
    }

    #[test]
    fn queries_return_none_in_holes() {
        let img = img_from(&[(0x00, &[1, 2]), (0x10, &[9])]);
        assert_eq!(img.byte(0x00), Some(1));
        assert_eq!(img.byte(0x05), None);
        assert_eq!(img.byte(0x10), Some(9));
        // A range that crosses the hole is absent as a whole.
        assert_eq!(img.range(0x01, 2), None);
        assert_eq!(img.gaps(), vec![(0x02, 0x10)]);
    }

    #[test]
    fn overlap_error_policy_rejects_and_preserves() {
        let mut img = img_from(&[(0x00, &[1, 2, 3, 4])]);
        let err = img.insert(0x02, &[9, 9], Overlap::Error).unwrap_err();
        assert_eq!(err, ImageError::Overlap { start: 0x02, end: 0x04 });
        // Image is untouched by the rejected insert.
        assert_eq!(img.range(0x00, 4), Some(vec![1, 2, 3, 4]));
    }

    #[test]
    fn overlap_overwrite_and_reject_policies() {
        let mut ow = img_from(&[(0x00, &[1, 2, 3, 4])]);
        let r = ow.insert(0x02, &[0xA, 0xB, 0xC], Overlap::Overwrite).unwrap();
        assert_eq!(r.conflicts, vec![(0x02, 0x04)]);
        assert_eq!(ow.range(0x00, 5), Some(vec![1, 2, 0xA, 0xB, 0xC]));

        let mut rj = img_from(&[(0x00, &[1, 2, 3, 4])]);
        let r = rj.insert(0x02, &[0xA, 0xB, 0xC], Overlap::Reject).unwrap();
        assert_eq!(r.conflicts, vec![(0x02, 0x04)]);
        // Existing bytes win in the overlap; only the hole extension is new.
        assert_eq!(rj.range(0x00, 5), Some(vec![1, 2, 3, 4, 0xC]));
    }

    #[test]
    fn insert_can_bridge_a_gap() {
        let mut img = img_from(&[(0x00, &[1]), (0x04, &[5])]);
        assert_eq!(img.segment_count(), 2);
        img.insert(0x01, &[2, 3, 4], Overlap::Error).unwrap();
        assert_eq!(img.segment_count(), 1);
        assert_eq!(img.range(0x00, 5), Some(vec![1, 2, 3, 4, 5]));
    }

    #[test]
    fn flatten_fills_holes_and_reports_them() {
        let img = img_from(&[(0x00, &[1, 2]), (0x06, &[7, 8])]);
        let flat = img.flatten(0xFF).unwrap();
        assert_eq!(flat.base, 0x00);
        assert_eq!(flat.bytes, vec![1, 2, 0xFF, 0xFF, 0xFF, 0xFF, 7, 8]);
        assert_eq!(flat.fills, vec![(0x02, 0x06)]);
    }

    #[test]
    fn hex_roundtrip_with_extended_linear_and_a_hole() {
        // Two segments either side of a 64 KiB boundary → forces type-04 records.
        let img = img_from(&[
            (0x0000_F000, &[0x11; 32]),
            (0x0001_0000, &[0x22; 48]),
        ]);
        let file = hex::HexFile {
            image: img.clone(),
            start: Some(hex::StartAddress::Linear(0x0001_0000)),
        };
        let text = hex::write(&file, hex::WriteOptions { record_len: 16 }).unwrap();
        // Crossing the 64 KiB boundary forces a type-04 extended-linear record.
        assert!(text.contains(":020000040001"), "expected an extended-linear record");
        let back = hex::read(&text).unwrap();
        assert_eq!(back.image, img);
        assert_eq!(back.start, Some(hex::StartAddress::Linear(0x0001_0000)));
        // The gap between the two segments survives the round trip.
        assert_eq!(back.image.segment_count(), 2);
    }

    #[test]
    fn hex_rejects_bad_checksum() {
        let img = img_from(&[(0x0010, &[0xDE, 0xAD, 0xBE, 0xEF])]);
        let text = hex::write(
            &hex::HexFile { image: img, start: None },
            hex::WriteOptions::default(),
        )
        .unwrap();
        assert!(hex::read(&text).is_ok());
        // Corrupt the checksum nibble of the first (data) record.
        let mut lines: Vec<String> = text.lines().map(String::from).collect();
        let last = lines[0].pop().unwrap();
        lines[0].push(if last == '0' { '1' } else { '0' });
        let bad = lines.join("\n");
        assert!(matches!(hex::read(&bad), Err(ImageError::Checksum { line: 1, .. })));
    }

    #[test]
    fn hex_requires_eof_record() {
        let img = img_from(&[(0x0010, &[0xDE, 0xAD, 0xBE, 0xEF])]);
        let text = hex::write(
            &hex::HexFile { image: img, start: None },
            hex::WriteOptions::default(),
        )
        .unwrap();
        // Drop the trailing end-of-file record.
        let no_eof: String = text
            .lines()
            .filter(|l| !l.starts_with(":00000001"))
            .map(|l| format!("{l}\n"))
            .collect();
        assert!(matches!(hex::read(&no_eof), Err(ImageError::Truncated { .. })));
    }

    #[test]
    fn srec_s3_roundtrip() {
        let img = img_from(&[(0x0800_0000, &[0xAB; 40]), (0x0800_1000, &[0xCD; 10])]);
        let file = srec::SRecFile {
            header: Some(b"OPENTUNE".to_vec()),
            image: img.clone(),
            start: Some(0x0800_0000),
        };
        let text = srec::write(&file, srec::WriteOptions::default()).unwrap();
        assert!(text.contains("\nS3"), "expected S3 data records");
        assert!(text.starts_with("S0"), "expected an S0 header");
        assert!(text.contains("\nS7"), "expected an S7 terminator");
        let back = srec::read(&text).unwrap();
        assert_eq!(back.image, img);
        assert_eq!(back.start, Some(0x0800_0000));
        assert_eq!(back.header.as_deref(), Some(&b"OPENTUNE"[..]));
    }

    #[test]
    fn srec_rejects_bad_checksum() {
        let file = srec::SRecFile {
            header: None,
            image: img_from(&[(0x1000, &[0xDE, 0xAD, 0xBE, 0xEF])]),
            start: Some(0x1000),
        };
        let text = srec::write(&file, srec::WriteOptions::default()).unwrap();
        assert!(srec::read(&text).is_ok());
        // Flip the last hex digit of the first data line's checksum.
        let mut lines: Vec<String> = text.lines().map(String::from).collect();
        let data_idx = lines.iter().position(|l| l.starts_with("S1")).unwrap();
        let line = &mut lines[data_idx];
        let last = line.pop().unwrap();
        let flipped = if last == '0' { '1' } else { '0' };
        line.push(flipped);
        let corrupted = lines.join("\n");
        assert!(matches!(srec::read(&corrupted), Err(ImageError::Checksum { .. })));
    }

    #[test]
    fn srec_rejects_missing_terminator() {
        let file = srec::SRecFile {
            header: None,
            image: img_from(&[(0x0010, &[0xDE, 0xAD, 0xBE, 0xEF])]),
            start: None,
        };
        let text = srec::write(&file, srec::WriteOptions::default()).unwrap();
        assert!(srec::read(&text).is_ok());
        // Strip the S7/S8/S9 termination record.
        let no_term: String = text
            .lines()
            .filter(|l| !(l.starts_with("S7") || l.starts_with("S8") || l.starts_with("S9")))
            .map(|l| format!("{l}\n"))
            .collect();
        assert!(matches!(srec::read(&no_term), Err(ImageError::Truncated { .. })));
    }

    #[test]
    fn hex_and_srec_describe_the_same_image() {
        let img = img_from(&[
            (0x0000_F800, &[0x5A; 64]),
            (0x0001_0040, &[0xA5; 20]),
        ]);
        let hex_text = hex::write(
            &hex::HexFile { image: img.clone(), start: None },
            hex::WriteOptions { record_len: 32 },
        )
        .unwrap();
        let srec_text = srec::write(
            &srec::SRecFile { header: None, image: img.clone(), start: None },
            srec::WriteOptions { record_len: 24, width: None, emit_count: true },
        )
        .unwrap();
        let from_hex = hex::read(&hex_text).unwrap().image;
        let from_srec = srec::read(&srec_text).unwrap().image;
        assert_eq!(from_hex, from_srec);
        assert_eq!(from_hex, img);
    }

    #[test]
    fn srec_width_override_too_narrow_errors() {
        let file = srec::SRecFile {
            header: None,
            image: img_from(&[(0x0001_0000, &[1, 2, 3])]),
            start: None,
        };
        let opts = srec::WriteOptions {
            record_len: 16,
            width: Some(srec::AddressWidth::Bits16),
            emit_count: false,
        };
        assert!(matches!(
            srec::write(&file, opts),
            Err(ImageError::AddressWidth { .. })
        ));
    }
}
