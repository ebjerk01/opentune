//! Structural ECU-family detection — the most reliable signal, because it works
//! even on a no-string dump. Each family's structural fingerprint lives in
//! `opentune-checksum`; we run every detector, record which fired, and resolve a
//! single primary family (directory-based markers beat the weaker EDC15 field
//! scan; the shared EDC16/EDC17 marker pair is disambiguated by size).

use serde::Serialize;

use crate::recognize::{supplier, Detection, Tier, STRUCTURAL};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum EcuFamily {
    Edc15,
    Edc16,
    Edc17,
    Pcr21,
    Unknown,
}

impl EcuFamily {
    pub fn as_str(self) -> &'static str {
        match self {
            EcuFamily::Edc15 => "EDC15",
            EcuFamily::Edc16 => "EDC16",
            EcuFamily::Edc17 => "EDC17",
            EcuFamily::Pcr21 => "PCR2.1",
            EcuFamily::Unknown => "Unknown",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FamilyDetection {
    pub family: EcuFamily,
    /// Every structural signal that fired, with what it was (for transparency).
    pub how: Vec<String>,
    /// Size bucket, used as a tiebreak and reported for Unknown dumps.
    pub size_class: &'static str,
}

/// Coarse size class of a dump.
pub fn size_class(len: usize) -> &'static str {
    match len {
        0..=0x1_7FFF => "<96K",
        0x1_8000..=0x4_FFFF => "128-256K",
        0x5_0000..=0xB_FFFF => "~512K",
        0xC_0000..=0x1C_FFFF => "~1M",
        0x1D_0000..=0x37_FFFF => "~2M",
        0x38_0000..=0x5F_FFFF => "~4M",
        _ => ">4M",
    }
}

/// Run every structural detector and resolve a primary family.
pub fn detect(image: &[u8]) -> FamilyDetection {
    let sc = size_class(image.len());
    let mut how: Vec<String> = Vec::new();

    let edc17_dir = opentune_checksum::edc17::parse_directory(image);
    let edc16_dir = opentune_checksum::edc16::parse_directory(image);
    let pcr21_dir = opentune_checksum::pcr21::find_directory(image);

    if !edc17_dir.is_empty() {
        how.push(format!(
            "EDC17: {} FADECAFE/CAFEAFFE LE checksum-directory record(s)",
            edc17_dir.len()
        ));
    }
    if !edc16_dir.is_empty() {
        how.push(format!(
            "EDC16: {} FADECAFE/CAFEAFFE BE checksum-directory record(s)",
            edc16_dir.len()
        ));
    }
    if pcr21_dir.is_some() {
        how.push("PCR2.1: zero-init/CRC/area checksum directory found".to_string());
    }

    // EDC15: use the checksum engine's verify surface. A VAG TDI v4.1v2 image
    // yields "vag-region-*" statuses; a Mercedes EDC15C6 image yields
    // "edc15c6-block-*" (recognised) or "edc15c6-field-*" (located only)
    // statuses (require several to avoid coincidental markers).
    let (edc15_vag, edc15c6_fields) = edc15_signal(image);
    if edc15_vag {
        how.push("EDC15: VAG TDI v4.1v2 checksum regions located".to_string());
    }
    if edc15c6_fields >= 4 {
        how.push(format!(
            "EDC15C6: {edc15c6_fields} located 0xC058 checksum block(s)/field(s)"
        ));
    }

    // --- resolve a single primary family ---
    let family = if pcr21_dir.is_some() {
        EcuFamily::Pcr21
    } else if !edc17_dir.is_empty() && !edc16_dir.is_empty() {
        // Shared marker pair: disambiguate by size (EDC17 is 2M/4M, EDC16 ≤1M).
        if image.len() >= 0x18_0000 {
            how.push("tiebreak: both EDC16+EDC17 markers; size≥1.5M ⇒ EDC17".into());
            EcuFamily::Edc17
        } else {
            how.push("tiebreak: both EDC16+EDC17 markers; size<1.5M ⇒ EDC16".into());
            EcuFamily::Edc16
        }
    } else if !edc17_dir.is_empty() {
        EcuFamily::Edc17
    } else if !edc16_dir.is_empty() {
        EcuFamily::Edc16
    } else if edc15_vag || edc15c6_fields >= 4 {
        EcuFamily::Edc15
    } else {
        EcuFamily::Unknown
    };

    FamilyDetection {
        family,
        how,
        size_class: sc,
    }
}

/// Returns (is_vag_tdi_regions, count_of_edc15c6_fields) from the EDC15 engine.
fn edc15_signal(image: &[u8]) -> (bool, usize) {
    let Some(algo) = opentune_checksum::get("edc15") else {
        return (false, 0);
    };
    match algo.verify(image) {
        Ok(blocks) => {
            let vag = blocks.iter().any(|b| b.name.starts_with("vag-region"));
            let c6 = blocks
                .iter()
                .filter(|b| b.name.starts_with("edc15c6-"))
                .count();
            (vag, c6)
        }
        Err(_) => (false, 0),
    }
}

/// The checksum-structure result as a recognition [`Detection`] (high tier:
/// a parsed checksum directory / located checksum fields is structure, not a
/// string coincidence). Empty for `Unknown`.
pub fn structural_detections(det: &FamilyDetection) -> Vec<Detection> {
    let (sup, fam) = match det.family {
        EcuFamily::Edc15 => (supplier::BOSCH, "EDC15"),
        EcuFamily::Edc16 => (supplier::BOSCH, "EDC16"),
        EcuFamily::Edc17 => (supplier::BOSCH, "EDC17"),
        EcuFamily::Pcr21 => (supplier::CONTINENTAL, "PCR2.1"),
        EcuFamily::Unknown => return Vec::new(),
    };
    let mut d = Detection::new(sup, fam, Tier::High, STRUCTURAL);
    for h in &det.how {
        d = d.ev(h.clone(), None, None);
    }
    vec![d]
}
