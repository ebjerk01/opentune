//! Diagnostic Trouble Code (DTC) database and decoder.
//!
//! This crate answers two questions that come up constantly when talking to an
//! engine ECU:
//!
//! 1. *What does a code like `P0101` mean?* — [`lookup`].
//! 2. *A UDS `ReadDTCInformation` response gave me raw bytes; what codes are
//!    stored?* — [`decode_uds`].
//!
//! The textual descriptions come from a bundled table of public SAE J2012 /
//! ISO 15031-6 powertrain (`P`) and network (`U`) codes (see `README.md` for
//! provenance). Codes that are not in the table still decode structurally to
//! their [`System`] and a generic "manufacturer-specific / unknown" label, so
//! the caller always gets a usable result for a well-formed code.
//!
//! The crate has no dependencies beyond `std`. The table is embedded at compile
//! time with [`include_str!`] and parsed once on first use.

use std::sync::OnceLock;

/// The DTC table, tab-separated `CODE<TAB>DESCRIPTION`, one per line.
/// Sorted by code and de-duplicated at build time (see `README.md`).
const DTC_TSV: &str = include_str!("../data/dtc.tsv");

/// The vehicle system a DTC belongs to, taken from the code's first letter per
/// ISO 15031-6 / SAE J2012.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum System {
    /// `P` — powertrain (engine, transmission, emissions).
    Powertrain,
    /// `C` — chassis (ABS, steering, suspension).
    Chassis,
    /// `B` — body (airbags, lighting, HVAC, comfort).
    Body,
    /// `U` — network / communication (CAN bus, modules).
    Network,
}

impl System {
    /// Classify from the leading letter of a DTC. Accepts upper or lower case.
    ///
    /// Returns `None` for any other character.
    pub fn from_letter(c: char) -> Option<System> {
        match c.to_ascii_uppercase() {
            'P' => Some(System::Powertrain),
            'C' => Some(System::Chassis),
            'B' => Some(System::Body),
            'U' => Some(System::Network),
            _ => None,
        }
    }

    /// The canonical letter for this system (`'P'`, `'C'`, `'B'`, `'U'`).
    pub fn letter(self) -> char {
        match self {
            System::Powertrain => 'P',
            System::Chassis => 'C',
            System::Body => 'B',
            System::Network => 'U',
        }
    }

    /// The two-bit code used in the first DTC byte (ISO 15031-6): `00`=P,
    /// `01`=C, `10`=B, `11`=U.
    fn from_bits(bits: u8) -> System {
        match bits & 0b11 {
            0b00 => System::Powertrain,
            0b01 => System::Chassis,
            0b10 => System::Body,
            _ => System::Network,
        }
    }
}

/// A single diagnostic trouble code and its human-readable description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dtc {
    /// The canonical code, e.g. `"P0101"`.
    pub code: String,
    /// Human-readable description, or a generic fallback if the code is not in
    /// the bundled table.
    pub description: String,
    /// The vehicle system, derived from the code's first letter.
    pub system: System,
}

/// One record parsed from a UDS `ReadDTCInformation` payload: a decoded DTC
/// plus its raw status byte (ISO 14229-1 `DTCStatusMask` bits).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DtcRecord {
    /// The decoded diagnostic trouble code.
    pub dtc: Dtc,
    /// The raw status byte accompanying the DTC in the UDS response.
    pub status: u8,
}

/// Parse and cache the embedded table as a sorted slice for binary search.
fn table() -> &'static [(&'static str, &'static str)] {
    static TABLE: OnceLock<Vec<(&'static str, &'static str)>> = OnceLock::new();
    TABLE
        .get_or_init(|| {
            let mut v: Vec<(&'static str, &'static str)> = DTC_TSV
                .lines()
                .filter_map(|line| {
                    let line = line.trim_end_matches('\r');
                    if line.is_empty() {
                        return None;
                    }
                    line.split_once('\t')
                        .map(|(code, desc)| (code.trim(), desc.trim()))
                })
                .filter(|(code, desc)| !code.is_empty() && !desc.is_empty())
                .collect();
            // The source is already sorted, but sort again so binary_search is
            // correct regardless of how the data file was produced.
            v.sort_unstable_by(|a, b| a.0.cmp(b.0));
            v.dedup_by(|a, b| a.0 == b.0);
            v
        })
        .as_slice()
}

/// Look up the description for a code from the bundled table.
///
/// Returns `None` only for a malformed code (empty, or one whose first letter
/// is not `P`/`C`/`B`/`U`). A well-formed code that is not in the table still
/// returns `Some(Dtc)` with a generic "manufacturer-specific / unknown"
/// description and the [`System`] decoded from its first letter.
///
/// Lookup is case-insensitive; the returned `code` is upper-cased.
///
/// ```
/// let dtc = opentune_dtc::lookup("p0101").unwrap();
/// assert_eq!(dtc.code, "P0101");
/// assert_eq!(dtc.system, opentune_dtc::System::Powertrain);
/// ```
pub fn lookup(code: &str) -> Option<Dtc> {
    let code = code.trim().to_ascii_uppercase();
    let first = code.chars().next()?;
    let system = System::from_letter(first)?;

    let description = match table().binary_search_by(|probe| probe.0.cmp(code.as_str())) {
        Ok(idx) => table()[idx].1.to_string(),
        Err(_) => "Manufacturer-specific / unknown code".to_string(),
    };

    Some(Dtc {
        code,
        description,
        system,
    })
}

/// Build the canonical 5-character DTC string from the three DTC bytes of a
/// UDS record, per ISO 15031-6 / SAE J2012.
///
/// Byte layout:
/// * `high` bits 7-6: letter (`00`=P, `01`=C, `10`=B, `11`=U)
/// * `high` bits 5-4: first digit (0-3)
/// * `high` bits 3-0: second digit (hex 0-F)
/// * `mid`  bits 7-4: third digit (hex 0-F)
/// * `mid`  bits 3-0: fourth digit (hex 0-F)
///
/// The `low` byte is the ISO 14229-1 failure-type / additional byte of the
/// 3-byte UDS DTC; it is not part of the 5-character SAE code and is carried in
/// the signature only because UDS records supply all three bytes.
///
/// ```
/// assert_eq!(opentune_dtc::format_code(0x01, 0x01, 0x00), "P0101");
/// ```
pub fn format_code(high: u8, mid: u8, low: u8) -> String {
    let _ = low; // failure-type byte, not part of the 5-char SAE code
    let letter = System::from_bits(high >> 6).letter();
    let d1 = (high >> 4) & 0b11; // 0-3
    let d2 = high & 0x0F; // hex
    let d3 = (mid >> 4) & 0x0F; // hex
    let d4 = mid & 0x0F; // hex
    format!("{letter}{d1}{d2:X}{d3:X}{d4:X}")
}

/// Decode a UDS `ReadDTCInformation` payload into records.
///
/// The input is the DTC-and-status portion of the response: a flat sequence of
/// 4-byte records, each `[high, mid, low, status]`, where `high`/`mid`/`low`
/// are the 3-byte DTC (see [`format_code`]) and `status` is the ISO 14229-1
/// status byte. Any trailing bytes that do not form a complete 4-byte record
/// are ignored.
///
/// Callers should strip the service id (`0x59`), the `ReadDTCInformation`
/// sub-function and the `DTCStatusAvailabilityMask` byte before calling this;
/// what remains is the record list this function expects.
///
/// ```
/// let payload = [0x01, 0x01, 0x00, 0x08]; // P0101, status 0x08
/// let recs = opentune_dtc::decode_uds(&payload);
/// assert_eq!(recs.len(), 1);
/// assert_eq!(recs[0].dtc.code, "P0101");
/// assert_eq!(recs[0].status, 0x08);
/// ```
pub fn decode_uds(raw: &[u8]) -> Vec<DtcRecord> {
    raw.chunks_exact(4)
        .map(|rec| {
            let code = format_code(rec[0], rec[1], rec[2]);
            // `code` is always well-formed here, so lookup never returns None.
            let dtc = lookup(&code).unwrap_or_else(|| Dtc {
                code,
                description: "Manufacturer-specific / unknown code".to_string(),
                system: System::Powertrain,
            });
            DtcRecord {
                dtc,
                status: rec[3],
            }
        })
        .collect()
}

/// Number of entries in the bundled DTC table.
pub fn table_len() -> usize {
    table().len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_code_example() {
        assert_eq!(format_code(0x01, 0x01, 0x00), "P0101");
    }

    #[test]
    fn format_code_systems_and_hex() {
        // Letter bits: 00=P, 01=C, 10=B, 11=U.
        assert_eq!(format_code(0x00, 0x00, 0x00), "P0000");
        assert_eq!(format_code(0x41, 0x23, 0x00), "C0123");
        assert_eq!(format_code(0x81, 0x34, 0x00), "B0134");
        assert_eq!(format_code(0xC1, 0x01, 0x00), "U0101");
        // Hex digits above 9 render A-F.
        assert_eq!(format_code(0x1A, 0xBC, 0x00), "P1ABC");
        // First digit maxes at 3 (two bits).
        assert_eq!(format_code(0x30, 0x00, 0x00), "P3000");
        // low byte does not affect the 5-char code.
        assert_eq!(format_code(0x01, 0x01, 0x1C), "P0101");
    }

    #[test]
    fn lookup_known_codes() {
        let p0101 = lookup("P0101").unwrap();
        assert_eq!(p0101.code, "P0101");
        assert_eq!(p0101.system, System::Powertrain);
        assert!(p0101
            .description
            .to_lowercase()
            .contains("mass or volume air"));

        let p0234 = lookup("P0234").unwrap();
        assert_eq!(p0234.code, "P0234");
        assert!(p0234.description.to_lowercase().contains("overboost"));

        let u0101 = lookup("U0101").unwrap();
        assert_eq!(u0101.code, "U0101");
        assert_eq!(u0101.system, System::Network);
        assert!(u0101.description.to_lowercase().contains("tcm"));
    }

    #[test]
    fn lookup_is_case_insensitive() {
        assert_eq!(lookup("p0101").unwrap().code, "P0101");
        assert_eq!(lookup("  P0101  ").unwrap().code, "P0101");
    }

    #[test]
    fn lookup_unknown_but_wellformed_falls_back() {
        // A valid chassis code that is not in the P/U-only table.
        let dtc = lookup("C1234").unwrap();
        assert_eq!(dtc.code, "C1234");
        assert_eq!(dtc.system, System::Chassis);
        assert!(dtc.description.to_lowercase().contains("unknown"));
    }

    #[test]
    fn lookup_malformed_returns_none() {
        assert!(lookup("").is_none());
        assert!(lookup("X0101").is_none());
        assert!(lookup("12345").is_none());
    }

    #[test]
    fn system_classification() {
        assert_eq!(System::from_letter('P'), Some(System::Powertrain));
        assert_eq!(System::from_letter('c'), Some(System::Chassis));
        assert_eq!(System::from_letter('B'), Some(System::Body));
        assert_eq!(System::from_letter('u'), Some(System::Network));
        assert_eq!(System::from_letter('Z'), None);
        assert_eq!(System::Powertrain.letter(), 'P');
        assert_eq!(System::Network.letter(), 'U');
    }

    #[test]
    fn decode_uds_sample_payload() {
        // Two records: P0101 (status 0x08) and U0101 (status 0x2F).
        let payload = [0x01, 0x01, 0x00, 0x08, 0xC1, 0x01, 0x00, 0x2F];
        let recs = decode_uds(&payload);
        assert_eq!(recs.len(), 2);

        assert_eq!(recs[0].dtc.code, "P0101");
        assert_eq!(recs[0].dtc.system, System::Powertrain);
        assert_eq!(recs[0].status, 0x08);
        assert!(recs[0]
            .dtc
            .description
            .to_lowercase()
            .contains("mass or volume air"));

        assert_eq!(recs[1].dtc.code, "U0101");
        assert_eq!(recs[1].dtc.system, System::Network);
        assert_eq!(recs[1].status, 0x2F);
    }

    #[test]
    fn decode_uds_ignores_trailing_partial_record() {
        let payload = [0x01, 0x01, 0x00, 0x08, 0xAB]; // 1 full record + stray byte
        let recs = decode_uds(&payload);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].dtc.code, "P0101");
    }

    #[test]
    fn decode_uds_empty() {
        assert!(decode_uds(&[]).is_empty());
    }

    #[test]
    fn table_is_populated() {
        // Sanity check the embedded table loaded and is a reasonable size.
        assert!(table_len() > 10_000, "table_len = {}", table_len());
    }
}
