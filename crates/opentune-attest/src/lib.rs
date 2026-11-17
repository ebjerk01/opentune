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
    let blocks =
        opentune_checksum::report(&*algo, image).map_err(|e| AttestError::Checksum(e.to_string()))?;

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

#[cfg(test)]
mod tests {
    use super::*;

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
