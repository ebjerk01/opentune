//! # opentune-cbf
//!
//! A parser for Mercedes-Benz **CBF** (CAESAR diagnostic description) files.
//!
//! A `.cbf` file is a container shipped with Mercedes diagnostic software (DAS /
//! Xentry). It describes one or more ECU families: the diagnostic services they
//! support, their software variants, and — the focus of this crate — their
//! **Diagnostic Trouble Code (DTC)** definitions with human-readable
//! descriptions.
//!
//! ## What it extracts
//!
//! * [`Ecu::dtcs`] — the ECU-global DTC pool: every fault code and its
//!   description. This is the authoritative code→description source.
//! * [`Ecu::services`] — diagnostic services (metadata: qualifier, name, type).
//! * [`Ecu::variants`] — concrete ECU software variants, each listing the DTCs
//!   and services it exposes, plus the patterns that identify it on a vehicle.
//!
//! ## Quick start
//!
//! ```no_run
//! let cbf = opentune_cbf::parse_file("ECU.cbf").unwrap();
//! for ecu in &cbf.ecus {
//!     println!("{} — {} DTCs", ecu.qualifier, ecu.dtcs.len());
//! }
//! // Flat DTC code→description list, ready to feed a DTC database:
//! for entry in opentune_cbf::dtc_entries(&cbf) {
//!     println!("{}: {}", entry.code, entry.description);
//! }
//! // Or build the normalised, classified diagnostic view (one per ECU),
//! // with honest notes about anything missing or not decoded:
//! for diag in opentune_cbf::diagnostics(&cbf) {
//!     println!("{}: {} DTCs, {} services", diag.ecu.qualifier,
//!              diag.dtcs.len(), diag.services.len());
//!     for warning in &diag.warnings {
//!         eprintln!("note: {warning}");
//!     }
//! }
//! // Or export the whole model as JSON:
//! let json = opentune_cbf::to_json(&cbf).unwrap();
//! ```
//!
//! ## Provenance
//!
//! The binary format handling is adapted from the **CBFParser** component of
//! [OpenVehicleDiag](https://github.com/rnd-ash/OpenVehicleDiag) (GPL-3.0). See
//! the `NOTICE` file. OpenVehicleDiag's SMR-D parser (removed upstream under a
//! DMCA takedown) is **not** reimplemented here; this crate handles CBF only.

#![forbid(unsafe_code)]

mod diagnostics;
mod error;
mod header;
mod model;
mod parse;
mod raf;
mod reader;

pub use diagnostics::{
    diagnostics, ecu_diagnostics, CbfDiagnostics, CbfDtc, CbfEcuIdentity, CbfService, CbfVariant,
    Classification, ServiceKind,
};
pub use error::{CbfError, Result};
pub use model::{
    CbfFile, DiagService, Dtc, DtcEntry, Ecu, EcuVariant, Protocol, ServiceType, VariantPattern,
};
pub use parse::parse_bytes;

use std::path::Path;

/// Parse a CBF file from disk.
///
/// This is the small, CLI-friendly entry point: read the bytes and hand them to
/// [`parse_bytes`].
pub fn parse_file<P: AsRef<Path>>(path: P) -> Result<CbfFile> {
    let data = std::fs::read(path)?;
    parse_bytes(&data)
}

/// Flatten every ECU's DTC pool into a list of [`DtcEntry`] records.
///
/// Each entry carries the owning ECU qualifier so codes from multiple files can
/// be merged. The `code` field is the Mercedes-specific CBF qualifier, not an
/// SAE J2012 `P/C/B/U` code — callers populating `opentune-dtc` should store
/// these under a manufacturer ("MB") namespace rather than the SAE table.
pub fn dtc_entries(cbf: &CbfFile) -> Vec<DtcEntry> {
    let mut out = Vec::new();
    for ecu in &cbf.ecus {
        for dtc in &ecu.dtcs {
            out.push(DtcEntry {
                code: dtc.qualifier.clone(),
                description: dtc.description.clone().unwrap_or_default(),
                summary: dtc.reference.clone().unwrap_or_default(),
                ecu: ecu.qualifier.clone(),
            });
        }
    }
    out
}

/// Serialise a parsed [`CbfFile`] to pretty-printed JSON.
pub fn to_json(cbf: &CbfFile) -> Result<String> {
    Ok(serde_json::to_string_pretty(cbf)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_cbf_input() {
        let err = parse_bytes(b"not a cbf file at all, just random bytes......").unwrap_err();
        assert!(matches!(
            err,
            CbfError::BadMagic(_) | CbfError::UnexpectedEof
        ));
    }

    #[test]
    fn rejects_truncated_stub() {
        let err = parse_bytes(&[0u8; 16]).unwrap_err();
        assert!(matches!(err, CbfError::UnexpectedEof));
    }

    /// Build the minimal fixed stub header that `check_stub_header` accepts, so a
    /// test can then append a (possibly hostile) CFF body and exercise the body
    /// parser. STUB_HEADER_SIZE is 0x410.
    fn valid_stub(tail: &[u8]) -> Vec<u8> {
        let mut v = vec![0u8; 0x410];
        let magic = b"CBF-TRANSLATOR-VERSION:04.00";
        v[..magic.len()].copy_from_slice(magic);
        v[0x401] = 3; // required magic id
        v.extend_from_slice(tail);
        v
    }

    #[test]
    fn valid_stub_then_garbage_is_clean_error() {
        // A valid stub followed by random/hostile CFF bytes must error cleanly,
        // never panic or allocate wildly.
        for tail in [
            vec![],
            vec![0xff; 4],                // negative cff_header_size
            vec![0x00, 0x00, 0x00, 0x00], // zero header size, then EOF
            vec![0xff; 64],
            0x7fff_ffffi32.to_le_bytes().to_vec(), // huge header size
        ] {
            let buf = valid_stub(&tail);
            let _ = parse_bytes(&buf); // must not panic
        }
    }

    #[test]
    fn fuzz_cbf_bodies_never_panic() {
        // Deterministic fuzz over the CBF body parser with a valid stub header.
        // Hostile counts/offsets must be clamped/saturated, not panic, OOM or
        // hang. Iteration count kept modest for CI.
        let mut state: u64 = 0x4342_4600_2026_1116;
        let mut rng = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..256 {
            let len = (rng() % 512) as usize;
            let mut tail = Vec::with_capacity(len);
            for _ in 0..len {
                tail.push((rng() & 0xff) as u8);
            }
            let buf = valid_stub(&tail);
            let _ = parse_bytes(&buf);
        }
        // Also fuzz completely random buffers (most fail the stub check).
        for _ in 0..256 {
            let len = (rng() % 2048) as usize;
            let mut buf = Vec::with_capacity(len);
            for _ in 0..len {
                buf.push((rng() & 0xff) as u8);
            }
            let _ = parse_bytes(&buf);
        }
    }

    #[test]
    fn dtc_entries_flattens_pool() {
        let cbf = CbfFile {
            cbf_version: "CBF-TRANSLATOR-VERSION:04.00.01".into(),
            source_xml: String::new(),
            ecus: vec![Ecu {
                qualifier: "TESTECU".into(),
                name: Some("Test ECU".into()),
                description: None,
                dtcs: vec![
                    Dtc {
                        qualifier: "D_FAULT_A".into(),
                        description: Some("Sensor A short to ground".into()),
                        reference: Some("ref-1".into()),
                        pool_index: 0,
                    },
                    Dtc {
                        qualifier: "D_FAULT_B".into(),
                        description: None,
                        reference: None,
                        pool_index: 1,
                    },
                ],
                services: vec![],
                variants: vec![],
            }],
        };

        let entries = dtc_entries(&cbf);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].code, "D_FAULT_A");
        assert_eq!(entries[0].description, "Sensor A short to ground");
        assert_eq!(entries[0].summary, "ref-1");
        assert_eq!(entries[0].ecu, "TESTECU");
        // Missing description/reference become empty strings, not None.
        assert_eq!(entries[1].code, "D_FAULT_B");
        assert_eq!(entries[1].description, "");
        assert_eq!(entries[1].summary, "");
    }

    #[test]
    fn json_roundtrips() {
        let cbf = CbfFile {
            cbf_version: "v".into(),
            source_xml: "x".into(),
            ecus: vec![Ecu {
                qualifier: "E".into(),
                ..Default::default()
            }],
        };
        let json = to_json(&cbf).unwrap();
        let back: CbfFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.ecus.len(), 1);
        assert_eq!(back.ecus[0].qualifier, "E");
    }

    #[test]
    fn service_type_mapping() {
        assert_eq!(ServiceType::from_raw(5), ServiceType::Data);
        assert_eq!(ServiceType::from_raw(23), ServiceType::Routine);
        assert_eq!(ServiceType::from_raw(999), ServiceType::Unknown);
    }

    /// Opt-in test against a real CBF file.
    ///
    /// Set `OPENTUNE_CBF_SAMPLE=/path/to/file.cbf` to run it. When the variable
    /// is unset the test is a no-op, so CI stays hermetic. **Validation
    /// status: run this with a real Mercedes `.cbf` to confirm live parsing.**
    #[test]
    fn parses_real_sample_if_provided() {
        let Ok(path) = std::env::var("OPENTUNE_CBF_SAMPLE") else {
            eprintln!("OPENTUNE_CBF_SAMPLE unset — skipping real-file validation");
            return;
        };
        let cbf = parse_file(&path).expect("sample CBF should parse");
        assert!(!cbf.ecus.is_empty(), "expected at least one ECU");
        let entries = dtc_entries(&cbf);
        eprintln!(
            "{}: {} ECUs, {} DTC entries",
            path,
            cbf.ecus.len(),
            entries.len()
        );

        // Exercise the normalised diagnostic view on the real file too.
        let diags = diagnostics(&cbf);
        assert_eq!(diags.len(), cbf.ecus.len());
        for diag in &diags {
            // Flattened DTCs must match the ECU pool one-for-one.
            let ecu = cbf
                .ecus
                .iter()
                .find(|e| e.qualifier == diag.ecu.qualifier)
                .expect("diagnostics ECU must come from the file");
            assert_eq!(diag.dtcs.len(), ecu.dtcs.len());
            assert_eq!(diag.services.len(), ecu.services.len());
            eprintln!(
                "  {} — {} services classified, {} warning(s)",
                diag.ecu.qualifier,
                diag.services.len(),
                diag.warnings.len()
            );
        }
    }
}
