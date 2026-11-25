//! The bridge that turns a real `opentune-checksum` verification into a
//! [`ChecksumAttestation`] the flash layer trusts.
//!
//! The flash safety policy enforces that a payload carries a checksum
//! attestation **bound to its exact bytes**, but it deliberately does not
//! recompute OEM checksums itself (keeping `opentune-flash` independent of the
//! checksum engine). That leaves one honest way to get a *truthful*
//! attestation: compute it here, where both crates meet.
//!
//! [`attest_image`] runs the named checksum algorithm over the image and sets
//! the attestation's `satisfied` flag from the **actual** per-block result — not
//! a caller's claim. An image whose stored checksums do not already match the
//! recomputation yields `satisfied = false`, so the flash policy refuses the
//! write. This is the "verify == stored before write" gate.
//!
//! [`attest_image_with_reference`] additionally takes the owner's untouched
//! stock read as a reference, for algorithms (EDC17) whose blocks cannot all be
//! verified from the image alone; a block it still cannot verify keeps the
//! attestation unsatisfied.
//!
//! It does **not** correct checksums: correcting an image and attesting it are
//! separate, deliberate steps (`opentune-checksum`'s `correct_bad_only` first,
//! then attest the corrected bytes).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use opentune_flash::ChecksumAttestation;

/// Error producing an attestation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttestError {
    /// No checksum algorithm is registered under this id.
    UnknownAlgorithm(String),
    /// The algorithm found no checksum block to verify in this image, so no
    /// honest "satisfied" claim can be made. Never attested as satisfied.
    NoBlocksFound(String),
    /// The checksum engine failed to analyse the image.
    Checksum(String),
}

impl std::fmt::Display for AttestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttestError::UnknownAlgorithm(a) => write!(f, "unknown checksum algorithm '{a}'"),
            AttestError::NoBlocksFound(a) => {
                write!(f, "algorithm '{a}' found no checksum block in this image")
            }
            AttestError::Checksum(e) => write!(f, "checksum analysis failed: {e}"),
        }
    }
}

impl std::error::Error for AttestError {}

/// Verify `image` with the checksum algorithm registered under `algo_id` and
/// produce a [`ChecksumAttestation`] bound to `image`, whose `satisfied` flag
/// reflects the **actual** verification: `true` only if every checksum block the
/// algorithm recognises already matches the stored value.
///
/// Feed the returned attestation into `opentune-flash`'s checked flash path;
/// when it is unsatisfied the policy refuses the write rather than bricking an
/// ECU with an internally inconsistent image.
pub fn attest_image(algo_id: &str, image: &[u8]) -> Result<ChecksumAttestation, AttestError> {
    let algo = opentune_checksum::get(algo_id)
        .ok_or_else(|| AttestError::UnknownAlgorithm(algo_id.to_string()))?;
    attest_image_with(&*algo, algo_id, image)
}

/// [`attest_image`] with an already resolved algorithm, e.g. one an embedding
/// application supplies that is not in the public `opentune-checksum` set.
/// `algo_id` is recorded in the attestation as the algorithm's name.
pub fn attest_image_with(
    algo: &dyn opentune_checksum::ChecksumAlgo,
    algo_id: &str,
    image: &[u8],
) -> Result<ChecksumAttestation, AttestError> {
    let blocks =
        opentune_checksum::report(algo, image).map_err(|e| AttestError::Checksum(e.to_string()))?;

    if blocks.is_empty() {
        return Err(AttestError::NoBlocksFound(algo_id.to_string()));
    }

    let bad = blocks.iter().filter(|b| !b.ok).count();
    let satisfied = bad == 0;

    let mut att = ChecksumAttestation::for_payload(image, satisfied, algo_id);
    att.covered = blocks.iter().map(|b| b.region).collect();
    att.note = if satisfied {
        format!("verified {} checksum block(s), all ok", blocks.len())
    } else {
        format!(
            "{bad} of {} checksum block(s) do not match stored — NOT safe to flash",
            blocks.len()
        )
    };
    Ok(att)
}

/// On what basis a block was judged by [`attest_image_with_reference`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// The algorithm's own check (stored vs computed / library constant).
    Algorithm,
    /// Checked against a target constant recovered from the reference image
    /// (EDC17 top-level blocks without a library constant).
    ReferenceConstant,
    /// Byte-identical to the reference image, so its checksum holds exactly as
    /// it did there (EDC17 nested sub-blocks, whose rule is unknown).
    UnchangedFromReference,
    /// The block cannot be verified at all (unknown rule and changed). Never
    /// counted as satisfied.
    Unverifiable,
}

/// One block's verdict in a [`ReferenceAttestation`].
#[derive(Debug, Clone)]
pub struct BlockVerdict {
    /// Block name as the algorithm reports it.
    pub name: String,
    /// Region the block covers.
    pub region: opentune_core::MemoryRegion,
    /// How it was judged.
    pub basis: Basis,
    /// Whether it verified.
    pub ok: bool,
}

/// The result of [`attest_image_with_reference`]: the attestation the flash
/// policy consumes plus the per-block verdicts behind it.
#[derive(Debug, Clone)]
pub struct ReferenceAttestation {
    /// Bound to the image; `satisfied` only when every block verified.
    pub attestation: ChecksumAttestation,
    /// Every block, in the algorithm's order.
    pub blocks: Vec<BlockVerdict>,
}

impl ReferenceAttestation {
    /// Blocks that could not be verified at all ([`Basis::Unverifiable`]).
    pub fn unverifiable(&self) -> Vec<&BlockVerdict> {
        self.blocks
            .iter()
            .filter(|b| b.basis == Basis::Unverifiable)
            .collect()
    }

    /// Blocks that were checked and do NOT match (a real checksum failure).
    pub fn failed(&self) -> Vec<&BlockVerdict> {
        self.blocks
            .iter()
            .filter(|b| !b.ok && b.basis != Basis::Unverifiable)
            .collect()
    }
}

/// [`attest_image_with`], with a known-good `reference` image (the owner's own
/// untouched stock read of the same ECU) available to judge blocks the
/// algorithm alone cannot.
///
/// For EDC17 (`algo.id() == "edc17"`) this uses
/// `opentune_checksum::edc17::verify_with_reference`: library-constant blocks
/// are checked as usual, other top-level blocks against the constant recovered
/// from the reference (the model `correct_with_reference` writes), and nested
/// sub-blocks of unknown rule only when byte-identical to the reference —
/// otherwise they are [`Basis::Unverifiable`] and the attestation is NOT
/// satisfied. Every other algorithm is self-contained and is attested exactly
/// like [`attest_image_with`] (the reference is not used).
///
/// The attestation is satisfied only when every block verified; an
/// unverifiable block is never counted as verified.
pub fn attest_image_with_reference(
    algo: &dyn opentune_checksum::ChecksumAlgo,
    algo_id: &str,
    image: &[u8],
    reference: &[u8],
) -> Result<ReferenceAttestation, AttestError> {
    use opentune_checksum::edc17::{self, RefBasis};
    let blocks: Vec<BlockVerdict> = if algo.id() == "edc17" {
        edc17::verify_with_reference(image, reference)
            .map_err(|e| AttestError::Checksum(e.to_string()))?
            .into_iter()
            .map(|b| BlockVerdict {
                name: b.status.name,
                region: b.status.region,
                basis: match b.basis {
                    RefBasis::LibraryConstant => Basis::Algorithm,
                    RefBasis::ReferenceConstant => Basis::ReferenceConstant,
                    RefBasis::UnchangedFromReference => Basis::UnchangedFromReference,
                    RefBasis::Unverifiable => Basis::Unverifiable,
                },
                ok: b.status.ok,
            })
            .collect()
    } else {
        opentune_checksum::report(algo, image)
            .map_err(|e| AttestError::Checksum(e.to_string()))?
            .into_iter()
            .map(|b| BlockVerdict {
                name: b.name,
                region: b.region,
                basis: Basis::Algorithm,
                ok: b.ok,
            })
            .collect()
    };
    if blocks.is_empty() {
        return Err(AttestError::NoBlocksFound(algo_id.to_string()));
    }
    let total = blocks.len();
    let count = |basis: Basis| blocks.iter().filter(|b| b.ok && b.basis == basis).count();
    let (by_algo, by_k, by_identity) = (
        count(Basis::Algorithm),
        count(Basis::ReferenceConstant),
        count(Basis::UnchangedFromReference),
    );
    let unverifiable: Vec<&str> = blocks
        .iter()
        .filter(|b| b.basis == Basis::Unverifiable)
        .map(|b| b.name.as_str())
        .collect();
    let failed: Vec<&str> = blocks
        .iter()
        .filter(|b| !b.ok && b.basis != Basis::Unverifiable)
        .map(|b| b.name.as_str())
        .collect();
    let satisfied = unverifiable.is_empty() && failed.is_empty();

    let mut att = ChecksumAttestation::for_payload(image, satisfied, algo_id);
    att.covered = blocks.iter().filter(|b| b.ok).map(|b| b.region).collect();
    let mut parts = vec![format!("{by_algo} by algorithm")];
    if by_k > 0 {
        parts.push(format!("{by_k} against the reference constant"));
    }
    if by_identity > 0 {
        parts.push(format!("{by_identity} byte-identical to the reference"));
    }
    let mut note = format!(
        "verified {} of {total} checksum block(s) ({})",
        total - unverifiable.len() - failed.len(),
        parts.join(", ")
    );
    if !failed.is_empty() {
        note.push_str(&format!(
            "; {} do not match stored: {}",
            failed.len(),
            failed.join(", ")
        ));
    }
    if !unverifiable.is_empty() {
        note.push_str(&format!(
            "; {} cannot be verified (unknown rule, changed vs the reference): {}",
            unverifiable.len(),
            unverifiable.join(", ")
        ));
    }
    if !satisfied {
        note.push_str(" — NOT safe to flash");
    }
    att.note = note;
    Ok(ReferenceAttestation {
        attestation: att,
        blocks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// EDC17-shaped synthetic image: top-level block [0x100, 0x1FF] (no library
    /// constant) containing a nested sub-block [0x140, 0x17F].
    fn edc17_stock() -> Vec<u8> {
        use opentune_checksum::edc17::forge_last_dword;
        let mut img = vec![0u8; 0x2000];
        for (at, s, e) in [(0x40usize, 0x100u32, 0x1FFu32), (0x50, 0x140, 0x17F)] {
            img[at..at + 4].copy_from_slice(&(0x8000_0000 | s).to_le_bytes());
            img[at + 4..at + 8].copy_from_slice(&(0x8000_0000 | e).to_le_bytes());
            img[at + 8..at + 12].copy_from_slice(&0xFADE_CAFEu32.to_le_bytes());
            img[at + 12..at + 16].copy_from_slice(&0xCAFE_AFFEu32.to_le_bytes());
        }
        for (i, b) in img[0x100..0x1FC].iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(29) ^ 0x11;
        }
        let x = forge_last_dword(&img[0x100..0x200], 0x0BAD_F00D);
        img[0x1FC..0x200].copy_from_slice(&x);
        img
    }

    #[test]
    fn edc17_plain_attestation_is_never_satisfied_without_a_reference() {
        // Blocks without a library constant cannot be confirmed alone.
        let att = attest_image("edc17", &edc17_stock()).unwrap();
        assert!(!att.satisfied);
    }

    #[test]
    fn edc17_reference_attestation() {
        let stock = edc17_stock();
        let algo = opentune_checksum::get("edc17").unwrap();
        // Stock against itself: satisfied.
        let r = attest_image_with_reference(&*algo, "edc17", &stock, &stock).unwrap();
        assert!(r.attestation.satisfied, "{}", r.attestation.note);
        assert_eq!(r.attestation.covered.len(), 2);

        // Tune outside the nested block, corrected with the reference: satisfied.
        let mut t = stock.clone();
        t[0x1A0] ^= 0x42;
        algo.correct_with_reference(&mut t, &stock).unwrap();
        let r = attest_image_with_reference(&*algo, "edc17", &t, &stock).unwrap();
        assert!(r.attestation.satisfied, "{}", r.attestation.note);
        assert!(r.attestation.verify_for(&t).is_ok());

        // Uncorrected: a real failure, not "unverifiable".
        let mut u = stock.clone();
        u[0x1A0] ^= 0x42;
        let r = attest_image_with_reference(&*algo, "edc17", &u, &stock).unwrap();
        assert!(!r.attestation.satisfied);
        assert_eq!(r.failed().len(), 1);
        assert!(r.unverifiable().is_empty());

        // Tune inside the nested block: unverifiable, never satisfied.
        let mut n = stock.clone();
        n[0x150] ^= 0x42;
        algo.correct_with_reference(&mut n, &stock).unwrap();
        let r = attest_image_with_reference(&*algo, "edc17", &n, &stock).unwrap();
        assert!(!r.attestation.satisfied);
        assert_eq!(r.unverifiable().len(), 1);
        assert!(r.failed().is_empty());
        assert!(r.attestation.note.contains("cannot be verified"));
    }

    #[test]
    fn other_algorithms_ignore_the_reference() {
        let algo = opentune_checksum::get("test-crc32-tail").unwrap();
        let mut img: Vec<u8> = (0..256u32).map(|i| i as u8).collect();
        algo.correct(&mut img).unwrap();
        let junk = vec![0xFFu8; 7];
        let r = attest_image_with_reference(&*algo, "test-crc32-tail", &img, &junk).unwrap();
        assert!(r.attestation.satisfied);
        assert!(r.blocks.iter().all(|b| b.basis == Basis::Algorithm));
        img[3] ^= 1;
        let r = attest_image_with_reference(&*algo, "test-crc32-tail", &img, &junk).unwrap();
        assert!(!r.attestation.satisfied);
        assert_eq!(r.failed().len(), 1);
    }

    /// Pick any real registered algorithm to exercise the bridge end-to-end.
    fn an_algorithm() -> String {
        opentune_checksum::algorithms()
            .into_iter()
            .next()
            .expect("at least one checksum algorithm is registered")
            .id
            .to_string()
    }

    #[test]
    fn unknown_algorithm_is_rejected() {
        let err = attest_image("definitely-not-an-algo", &[0u8; 16]).unwrap_err();
        assert!(matches!(err, AttestError::UnknownAlgorithm(_)));
    }

    #[test]
    fn a_corrected_image_attests_satisfied_and_binds_to_its_bytes() {
        let id = an_algorithm();
        let algo = opentune_checksum::get(&id).unwrap();
        // Start from arbitrary bytes and let the engine make the checksums
        // consistent, so we have a genuinely-valid image to attest.
        let mut img = vec![0u8; 4096];
        for (i, b) in img.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let fixes = opentune_checksum::correct_bad_only(&*algo, &mut img);
        let fixes = match fixes {
            Ok(f) => f,
            // Some algorithms need a recognised image to locate blocks; if this
            // one cannot operate on synthetic bytes, skip — other tests cover
            // the bridge logic.
            Err(_) => return,
        };
        let _ = fixes;

        let att = match attest_image(&id, &img) {
            Ok(a) => a,
            Err(AttestError::NoBlocksFound(_)) => return, // nothing to verify here
            Err(e) => panic!("unexpected error: {e}"),
        };
        assert!(att.satisfied, "a corrected image must attest satisfied");
        // Binding: the attestation verifies for these exact bytes only.
        assert!(att.verify_for(&img).is_ok());
        let mut tampered = img.clone();
        tampered[0] ^= 0xFF;
        assert!(
            att.verify_for(&tampered).is_err(),
            "attestation must not verify for different bytes"
        );
    }

    #[test]
    fn a_tampered_image_attests_unsatisfied() {
        let id = an_algorithm();
        let algo = opentune_checksum::get(&id).unwrap();
        let mut img = vec![0u8; 4096];
        for (i, b) in img.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        if opentune_checksum::correct_bad_only(&*algo, &mut img).is_err() {
            return;
        }
        // Corrupt a covered byte: the block no longer matches its stored value.
        let att_ok = match attest_image(&id, &img) {
            Ok(a) if a.satisfied => a,
            _ => return,
        };
        let region = match att_ok.covered.first() {
            Some(r) => *r,
            None => return,
        };
        img[region.address as usize] ^= 0xFF;
        let att = attest_image(&id, &img).expect("re-attest");
        assert!(
            !att.satisfied,
            "an image with a broken checksum block must attest unsatisfied"
        );
    }
}
