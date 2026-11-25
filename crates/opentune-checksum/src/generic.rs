//! Generic, ECU-agnostic checksum examples.
use crate::{crc32, BlockStatus, ChecksumAlgo, Fix};
use opentune_core::{Error, MemoryRegion, Result};

/// The last 4 bytes of the image hold a little-endian CRC32 of everything before.
///
/// Layout-agnostic and claims no ECU family, so correction is **match-first**:
/// standalone [`correct`](ChecksumAlgo::correct) writes nothing and refuses a
/// mismatch (an edited crc32-tail image cannot be told from an image whose
/// last 4 bytes are not a whole-image CRC32 at all, such as a normal OEM read).
/// Correct an edited image with
/// [`correct_with_reference`](ChecksumAlgo::correct_with_reference) against a
/// same-length stock image whose own tail validates.
pub struct Crc32Tail;

/// `(stored, computed, split)` of a crc32-tail image.
fn tail_eval(image: &[u8]) -> Result<(u32, u32, usize)> {
    if image.len() < 5 {
        return Err(Error::Config("image too small".into()));
    }
    let split = image.len() - 4;
    let stored = u32::from_le_bytes([
        image[split],
        image[split + 1],
        image[split + 2],
        image[split + 3],
    ]);
    Ok((stored, crc32(&image[..split]), split))
}

fn tail_status(image: &[u8], name: &str) -> Result<Vec<BlockStatus>> {
    let (stored, computed, split) = tail_eval(image)?;
    Ok(vec![BlockStatus {
        name: name.into(),
        region: MemoryRegion::new(0, split as u32),
        stored: stored as u64,
        computed: computed as u64,
        ok: stored == computed,
    }])
}

/// Rewrite a stale tail (no checks: callers gate it).
fn tail_fix(image: &mut [u8], name: &str) -> Result<Vec<Fix>> {
    let (old, computed, split) = tail_eval(image)?;
    let mut fixes = Vec::new();
    if old != computed {
        image[split..].copy_from_slice(&computed.to_le_bytes());
        fixes.push(Fix {
            name: name.into(),
            offset: split as u32,
            old: old as u64,
            new: computed as u64,
        });
    }
    Ok(fixes)
}

impl ChecksumAlgo for Crc32Tail {
    fn id(&self) -> &str {
        "crc32-tail"
    }

    fn verify(&self, image: &[u8]) -> Result<Vec<BlockStatus>> {
        tail_status(image, "crc32-tail")
    }

    fn correct(&self, image: &mut [u8]) -> Result<Vec<Fix>> {
        let (stored, computed, _) = tail_eval(image)?;
        if stored != computed {
            return Err(Error::Unsupported(
                "the last 4 bytes are not a CRC32 of the rest of the image, so it cannot be \
                 told whether this is an edited crc32-tail image or not one at all; refusing \
                 to write. Correct against your own untouched stock image instead (correct \
                 with a reference)"
                    .into(),
            ));
        }
        Ok(Vec::new())
    }

    fn correct_with_reference(&self, image: &mut [u8], reference: &[u8]) -> Result<Vec<Fix>> {
        let ok_ref =
            reference.len() == image.len() && tail_eval(reference).is_ok_and(|(s, c, _)| s == c);
        if !ok_ref {
            return Err(Error::Unsupported(
                "the reference is not a same-length image whose crc32-tail validates; it \
                 cannot prove the layout, refusing to correct"
                    .into(),
            ));
        }
        tail_fix(image, "crc32-tail")
    }
}

/// TEST FIXTURE ONLY: a crc32-tail that rewrites the tail unconditionally, for
/// tests across the workspace that need a self-contained algorithm whose
/// `correct` always fixes. Compiled only for this crate's tests or with the
/// `test-algos` feature (dev-dependencies), resolvable only as
/// `test-crc32-tail`, and never listed in [`crate::algorithms`], so no user
/// can select it for a real image.
#[cfg(any(test, feature = "test-algos"))]
pub struct TestCrc32Tail;

#[cfg(any(test, feature = "test-algos"))]
impl ChecksumAlgo for TestCrc32Tail {
    fn id(&self) -> &str {
        "test-crc32-tail"
    }

    fn verify(&self, image: &[u8]) -> Result<Vec<BlockStatus>> {
        tail_status(image, "test-crc32-tail")
    }

    fn correct(&self, image: &mut [u8]) -> Result<Vec<Fix>> {
        tail_fix(image, "test-crc32-tail")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid(len: usize) -> Vec<u8> {
        let mut img: Vec<u8> = (0..len).map(|i| (i as u8).wrapping_mul(13)).collect();
        let split = len - 4;
        let c = crc32(&img[..split]);
        img[split..].copy_from_slice(&c.to_le_bytes());
        img
    }

    /// Match-first: a valid tail needs nothing; a mismatch (an edited image,
    /// or not a crc32-tail image at all) is refused and nothing is written;
    /// the validating stock image as the reference corrects it.
    #[test]
    fn crc32_tail_is_match_first() {
        let algo = Crc32Tail;
        let stock = valid(64);
        assert!(algo.correct(&mut stock.clone()).unwrap().is_empty());
        let mut edited = stock.clone();
        edited[10] ^= 0xFF;
        let before = edited.clone();
        assert!(matches!(
            algo.correct(&mut edited).unwrap_err(),
            Error::Unsupported(_)
        ));
        assert_eq!(edited, before, "a refusal writes nothing");
        // An OEM-like image whose tail is not a CRC32: refused, untouched.
        let mut oem = vec![0xA5u8; 64];
        assert!(algo.correct(&mut oem).is_err());
        assert_eq!(oem, vec![0xA5u8; 64]);
        // Against the stock image it is corrected.
        let fixes = algo.correct_with_reference(&mut edited, &stock).unwrap();
        assert_eq!(fixes.len(), 1);
        assert!(algo.verify(&edited).unwrap().iter().all(|b| b.ok));
        // A reference that does not validate, or of another length: refused.
        let mut e2 = before.clone();
        assert!(algo.correct_with_reference(&mut e2, &before).is_err());
        assert!(algo.correct_with_reference(&mut e2, &stock[..32]).is_err());
    }

    #[test]
    fn test_fixture_fixes_unconditionally() {
        let algo = TestCrc32Tail;
        let mut img = vec![0u8; 16];
        let fixes = algo.correct(&mut img).unwrap();
        assert_eq!(fixes.len(), 1);
        assert!(algo.verify(&img).unwrap().iter().all(|b| b.ok));
    }
}
