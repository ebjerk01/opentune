//! Structural parsing of **VAG** and **Bosch** ECU part numbers.
//!
//! This is deliberately *shape-only*. It recognises the documented structure of
//! these numbering systems and extracts the groups that structure defines; it
//! does **not** map a group to a specific engine, model or ECU family, because
//! those mappings are not encoded in the number and inventing them would be
//! dishonest. Where a field's meaning is standard (VAG main group `906` =
//! engine electrics; a Bosch `0 281` family = diesel engine control), that is
//! reported as documented structural knowledge, with the un-derivable parts
//! (exact ECU generation, calibration, engine) explicitly flagged.
//!
//! ## VAG part numbers
//!
//! Shape: `TTT GGG SSS[V]` — commonly written `03G 906 021 LR`.
//!
//! * `TTT` — type/platform group (3 chars). For engine parts this group tends
//!   to track the engine-code family, but the number alone does not name the
//!   engine, so we report it without interpretation.
//! * `GGG` — main group (3 digits). `906` is the standard VAG main group for
//!   engine electrics / engine control units.
//! * `SSS` — subgroup / consecutive number (3 digits).
//! * `V` — version/revision index (0–3 trailing letters), e.g. `LR`, `E`.
//!
//! ## Bosch part numbers
//!
//! Shape: `D NNN NNN NNN` — 10 digits, e.g. `0 281 011 402`. The leading
//! 4-digit family (`0281`, `0261`, `0280`, …) identifies the product line;
//! the remaining 6 digits are a consecutive number. The family tells diesel vs
//! gasoline engine control but **not** the ECU generation (EDC15 vs 16 vs 17),
//! which is not recoverable from the number.

/// Which numbering system a part number matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartKind {
    /// VAG (Volkswagen Audi Group) spare-part number.
    Vag,
    /// Bosch 10-digit part number.
    Bosch,
    /// Shape not recognised as either.
    Unknown,
}

/// Result of parsing a part number. Fields that could not be determined are
/// `None`/empty; `flags` explains what is and isn't derivable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartNumber {
    /// The normalised input (whitespace/hyphens removed, upper-cased).
    pub normalized: String,
    /// Which system it matched.
    pub kind: PartKind,
    /// Supplier, when the number itself identifies one (Bosch numbers do; VAG
    /// numbers do not encode the supplier).
    pub supplier: Option<&'static str>,
    /// VAG type/platform group (`TTT`), e.g. `"03G"`. `None` for Bosch.
    pub vag_type_group: Option<String>,
    /// VAG main group (`GGG`), e.g. `"906"`. `None` for Bosch.
    pub vag_main_group: Option<String>,
    /// VAG subgroup/consecutive (`SSS`). `None` for Bosch.
    pub vag_subgroup: Option<String>,
    /// VAG version/revision index (trailing letters), e.g. `"LR"`. `None` for
    /// Bosch or when absent.
    pub vag_version: Option<String>,
    /// Bosch 4-digit family (`0281`, `0261`, …). `None` for VAG.
    pub bosch_family: Option<String>,
    /// Bosch 6-digit consecutive number. `None` for VAG.
    pub bosch_consecutive: Option<String>,
    /// Human-readable, standards-backed notes and what is NOT derivable.
    pub flags: Vec<String>,
}

impl PartNumber {
    fn unknown(norm: String) -> Self {
        PartNumber {
            normalized: norm,
            kind: PartKind::Unknown,
            supplier: None,
            vag_type_group: None,
            vag_main_group: None,
            vag_subgroup: None,
            vag_version: None,
            bosch_family: None,
            bosch_consecutive: None,
            flags: vec![
                "part-number shape not recognised as VAG or Bosch; no structural parse".to_string(),
            ],
        }
    }
}

/// Normalise: strip spaces, dots and hyphens, upper-case.
fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace() && *c != '-' && *c != '.')
        .collect::<String>()
        .to_ascii_uppercase()
}

/// What a known Bosch family prefix denotes (documented product lines only).
fn bosch_family_note(family: &str) -> Option<&'static str> {
    match family {
        "0281" => Some("diesel engine control (EDC family) / diesel components"),
        "0261" => Some("gasoline engine control (Motronic / ME / MED)"),
        "0280" => Some("gasoline fuel-injection components"),
        "0260" => Some("transmission control"),
        _ => None,
    }
}

/// Parse a VAG or Bosch part number, or return an `Unknown` result.
pub fn parse_part_number(input: &str) -> PartNumber {
    let norm = normalize(input);

    // --- Bosch: exactly 10 digits ---
    if norm.len() == 10 && norm.chars().all(|c| c.is_ascii_digit()) {
        let family = &norm[0..4];
        let consecutive = &norm[4..10];
        let mut flags = Vec::new();
        match bosch_family_note(family) {
            Some(note) => flags.push(format!("Bosch family {family}: {note}")),
            None => flags.push(format!(
                "Bosch-shaped (10 digits) but family {family} not in known-line table"
            )),
        }
        flags.push(
            "ECU generation (e.g. EDC15/EDC16/EDC17) is NOT encoded in the Bosch number; confirm separately"
                .to_string(),
        );
        return PartNumber {
            normalized: norm.clone(),
            kind: PartKind::Bosch,
            supplier: Some("Bosch"),
            vag_type_group: None,
            vag_main_group: None,
            vag_subgroup: None,
            vag_version: None,
            bosch_family: Some(family.to_string()),
            bosch_consecutive: Some(consecutive.to_string()),
            flags,
        };
    }

    // --- VAG: TTT GGG SSS [V] ---
    // 9 leading characters: 3 (type, alnum) + 3 digits + 3 digits, then 0..=3
    // trailing letters. The type group is alphanumeric (e.g. "03G", "8E0").
    if norm.len() >= 9 && norm.len() <= 12 {
        let b = norm.as_bytes();
        let type_group = &norm[0..3];
        let main_group = &norm[3..6];
        let subgroup = &norm[6..9];
        let version = &norm[9..];
        let main_ok = main_group.bytes().all(|c| c.is_ascii_digit());
        let sub_ok = subgroup.bytes().all(|c| c.is_ascii_digit());
        let type_ok = type_group.bytes().all(|c| c.is_ascii_alphanumeric());
        let version_ok = version.bytes().all(|c| c.is_ascii_alphabetic());
        // Require at least one digit in the type group so plain words don't match.
        let type_has_digit = type_group.bytes().any(|c: u8| c.is_ascii_digit());
        if main_ok && sub_ok && type_ok && version_ok && type_has_digit && b.len() >= 9 {
            let mut flags = Vec::new();
            if main_group == "906" {
                flags.push(
                    "VAG main group 906 = engine electrics / engine control unit (documented)"
                        .to_string(),
                );
            } else {
                flags.push(format!(
                    "VAG main group {main_group} (not 906; not necessarily an engine ECU)"
                ));
            }
            flags.push(format!(
                "type group '{type_group}' is not mapped to a specific engine/platform here (not derivable from the number alone)"
            ));
            flags.push(
                "VAG numbers do not encode the ECU supplier; determine it separately".to_string(),
            );
            return PartNumber {
                normalized: norm.clone(),
                kind: PartKind::Vag,
                supplier: None,
                vag_type_group: Some(type_group.to_string()),
                vag_main_group: Some(main_group.to_string()),
                vag_subgroup: Some(subgroup.to_string()),
                vag_version: if version.is_empty() {
                    None
                } else {
                    Some(version.to_string())
                },
                bosch_family: None,
                bosch_consecutive: None,
                flags,
            };
        }
    }

    PartNumber::unknown(norm)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vag_engine_ecu() {
        let p = parse_part_number("03G 906 021 LR");
        assert_eq!(p.kind, PartKind::Vag);
        assert_eq!(p.vag_type_group.as_deref(), Some("03G"));
        assert_eq!(p.vag_main_group.as_deref(), Some("906"));
        assert_eq!(p.vag_subgroup.as_deref(), Some("021"));
        assert_eq!(p.vag_version.as_deref(), Some("LR"));
        assert!(p
            .flags
            .iter()
            .any(|f| f.contains("906") && f.contains("engine")));
        // VAG numbers don't name the supplier.
        assert_eq!(p.supplier, None);
    }

    #[test]
    fn vag_no_version_suffix() {
        let p = parse_part_number("03L906018");
        assert_eq!(p.kind, PartKind::Vag);
        assert_eq!(p.vag_type_group.as_deref(), Some("03L"));
        assert_eq!(p.vag_main_group.as_deref(), Some("906"));
        assert_eq!(p.vag_version, None);
    }

    #[test]
    fn bosch_diesel_edc() {
        let p = parse_part_number("0281011402");
        assert_eq!(p.kind, PartKind::Bosch);
        assert_eq!(p.supplier, Some("Bosch"));
        assert_eq!(p.bosch_family.as_deref(), Some("0281"));
        assert_eq!(p.bosch_consecutive.as_deref(), Some("011402"));
        assert!(p.flags.iter().any(|f| f.contains("diesel")));
        // The generation must be flagged as not-encoded.
        assert!(p.flags.iter().any(|f| f.contains("EDC15/EDC16/EDC17")));
    }

    #[test]
    fn bosch_with_spaces() {
        let p = parse_part_number("0 261 S04 ...");
        // Not 10 pure digits after normalising → not Bosch.
        assert_ne!(p.kind, PartKind::Bosch);
    }

    #[test]
    fn bosch_gasoline_family() {
        let p = parse_part_number("0261S04567"); // contains a letter → not 10 digits
        assert_eq!(p.kind, PartKind::Unknown);
        let p2 = parse_part_number("0261204567");
        assert_eq!(p2.kind, PartKind::Bosch);
        assert!(p2.flags.iter().any(|f| f.contains("gasoline")));
    }

    #[test]
    fn unknown_shape() {
        let p = parse_part_number("hello world");
        assert_eq!(p.kind, PartKind::Unknown);
        assert!(!p.flags.is_empty());
    }

    #[test]
    fn plain_word_is_not_vag() {
        // 9+ letters, no digit in type group → must not be mis-parsed as VAG.
        let p = parse_part_number("ABCDEFGHI");
        assert_eq!(p.kind, PartKind::Unknown);
    }
}
