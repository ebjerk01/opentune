//! ISO 3779 VIN decoding and validation.
//!
//! A VIN is 17 characters split into three fields:
//!
//! | Positions | Field | Meaning                                              |
//! |-----------|-------|------------------------------------------------------|
//! | 1–3       | WMI   | World Manufacturer Identifier (make + region)        |
//! | 4–9       | VDS   | Vehicle Descriptor Section (model/body/engine + chk) |
//! | 10–17     | VIS   | Vehicle Identifier Section (year, plant, serial)     |
//!
//! Position 9 is the **check digit** (mandatory for North-American VINs, used
//! by some others, and left as a filler elsewhere). Position 10 is the **model
//! year**. The letters `I`, `O`, `Q` never appear in a valid VIN.
//!
//! [`decode_vin`] validates the shape and, for VINs that carry a numeric or
//! `X` check digit computed per the North-American algorithm, verifies it. It
//! then returns everything it could decode in a [`Vin`], flagging what it
//! could not. It never invents a model, body style or engine from the VDS —
//! those are manufacturer-specific and not decodable from the VIN alone.

use crate::wmi::{lookup_wmi, WmiInfo};

/// Why a string is not a decodable VIN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VinError {
    /// Not exactly 17 characters.
    BadLength(usize),
    /// Contains a character not allowed in a VIN (`I`, `O`, `Q`, or non
    /// alphanumeric). Carries the offending character and its 1-based position.
    IllegalChar(char, usize),
}

impl std::fmt::Display for VinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VinError::BadLength(n) => {
                write!(f, "VIN must be 17 characters, got {n}")
            }
            VinError::IllegalChar(c, pos) => {
                write!(f, "illegal character {c:?} at position {pos} (I/O/Q and non-alphanumerics are not allowed)")
            }
        }
    }
}

impl std::error::Error for VinError {}

/// A decoded model-year character (VIN position 10).
///
/// The same letter/digit repeats on a 30-year cycle, so a single character is
/// genuinely ambiguous. We surface **both** candidate years rather than pick
/// one. For North-American VINs the digit at position 7 disambiguates (alpha →
/// 2010+, numeric → pre-2010); that hint is applied when available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelYear {
    /// The raw position-10 character.
    pub code: char,
    /// All plausible calendar years for this code within 1980..=2039.
    pub candidates: Vec<u16>,
    /// Best single guess when the position-7 hint resolves the ambiguity,
    /// otherwise `None` (ambiguous — do not guess).
    pub resolved: Option<u16>,
}

/// A fully-parsed VIN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vin {
    /// The normalised 17-character VIN (upper-cased).
    pub vin: String,
    /// Positions 1–3.
    pub wmi: String,
    /// Positions 4–9 (includes the check digit at position 9).
    pub vds: String,
    /// Positions 10–17.
    pub vis: String,
    /// Manufacturer/country resolved from the WMI.
    pub wmi_info: WmiInfo,
    /// Position 9 as written.
    pub check_digit: char,
    /// `Some(true/false)` if the North-American check digit was verifiable,
    /// `None` if this VIN does not use a verifiable check digit (e.g. many
    /// European VINs put a non-computed filler here).
    pub check_digit_valid: Option<bool>,
    /// Decoded position-10 model year.
    pub model_year: ModelYear,
    /// Position 11: assembly plant code (manufacturer-specific, not decoded).
    pub plant_code: char,
    /// Positions 12–17: production serial number.
    pub serial: String,
    /// Positions 4–8: the descriptor part of the VDS (body/engine/model), which
    /// is manufacturer-specific and intentionally **not** interpreted here.
    pub vds_descriptor: String,
    /// Human-readable notes about anything uncertain or un-decodable.
    pub flags: Vec<String>,
}

/// Transliteration value of a VIN character for the check-digit calculation
/// (ISO 3779 / 49 CFR 565). Digits map to themselves; letters map per the
/// standard table (I/O/Q are excluded upstream).
fn translit(c: char) -> Option<u32> {
    match c {
        '0'..='9' => Some(c as u32 - '0' as u32),
        'A' | 'J' => Some(1),
        'B' | 'K' | 'S' => Some(2),
        'C' | 'L' | 'T' => Some(3),
        'D' | 'M' | 'U' => Some(4),
        'E' | 'N' | 'V' => Some(5),
        'F' | 'W' => Some(6),
        'G' | 'P' | 'X' => Some(7),
        'H' | 'Y' => Some(8),
        'R' | 'Z' => Some(9),
        _ => None,
    }
}

/// Positional weights for the check-digit sum (position 9 itself has weight 0).
const WEIGHTS: [u32; 17] = [8, 7, 6, 5, 4, 3, 2, 10, 0, 9, 8, 7, 6, 5, 4, 3, 2];

/// Model-year code → the year within a given 30-year window base.
/// The code sequence skips I, O, Q, U, Z and 0; it covers 1980..2009 then
/// repeats from 2010. See [`ModelYear`].
fn year_codes() -> &'static [(char, u16)] {
    // Base window 1980..=2009; add 30 for the next cycle.
    &[
        ('A', 1980),
        ('B', 1981),
        ('C', 1982),
        ('D', 1983),
        ('E', 1984),
        ('F', 1985),
        ('G', 1986),
        ('H', 1987),
        ('J', 1988),
        ('K', 1989),
        ('L', 1990),
        ('M', 1991),
        ('N', 1992),
        ('P', 1993),
        ('R', 1994),
        ('S', 1995),
        ('T', 1996),
        ('V', 1997),
        ('W', 1998),
        ('X', 1999),
        ('Y', 2000),
        ('1', 2001),
        ('2', 2002),
        ('3', 2003),
        ('4', 2004),
        ('5', 2005),
        ('6', 2006),
        ('7', 2007),
        ('8', 2008),
        ('9', 2009),
    ]
}

/// Decode the model-year character. `pos7_is_alpha` is the North-American
/// disambiguation hint: for the 2010+ cycle, position 7 is a letter; for the
/// pre-2010 cycle it is a digit. Pass `None` when the hint is unavailable.
fn decode_model_year(code: char, pos7_is_alpha: Option<bool>) -> ModelYear {
    let base = year_codes()
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, y)| *y);
    let mut candidates = Vec::new();
    if let Some(y0) = base {
        candidates.push(y0);
        candidates.push(y0 + 30); // next 30-year cycle
    }
    let resolved = match (base, pos7_is_alpha) {
        (Some(y0), Some(true)) => Some(y0 + 30), // alpha pos7 → later cycle
        (Some(y0), Some(false)) => Some(y0),     // numeric pos7 → earlier cycle
        _ => None,
    };
    ModelYear {
        code,
        candidates,
        resolved,
    }
}

/// Validate and decode a VIN. Input may be any case and may contain surrounding
/// whitespace; it is normalised first.
pub fn decode_vin(input: &str) -> Result<Vin, VinError> {
    let vin: String = input.trim().to_ascii_uppercase();
    let chars: Vec<char> = vin.chars().collect();
    if chars.len() != 17 {
        return Err(VinError::BadLength(chars.len()));
    }
    for (i, &c) in chars.iter().enumerate() {
        let ok = c.is_ascii_digit() || (c.is_ascii_uppercase() && !matches!(c, 'I' | 'O' | 'Q'));
        if !ok {
            return Err(VinError::IllegalChar(c, i + 1));
        }
    }

    let wmi: String = chars[0..3].iter().collect();
    let vds: String = chars[3..9].iter().collect();
    let vis: String = chars[9..17].iter().collect();
    let vds_descriptor: String = chars[3..8].iter().collect();
    let check_digit = chars[8];
    let plant_code = chars[10];
    let serial: String = chars[11..17].iter().collect();

    let mut flags: Vec<String> = Vec::new();

    // --- check digit (position 9) ---
    // Compute the North-American check digit. If the written value matches, the
    // VIN is self-consistent. A mismatch for a filler char (common on European
    // VINs) is reported as "not a verifiable check digit" rather than invalid.
    let mut sum = 0u32;
    let mut computable = true;
    for (i, &c) in chars.iter().enumerate() {
        match translit(c) {
            Some(v) => sum += v * WEIGHTS[i],
            None => {
                computable = false;
                break;
            }
        }
    }
    let check_digit_valid = if computable {
        let rem = sum % 11;
        let expected = if rem == 10 {
            'X'
        } else {
            (b'0' + rem as u8) as char
        };
        if check_digit == expected {
            Some(true)
        } else if check_digit == 'X' || check_digit.is_ascii_digit() {
            // A numeric/X check digit that does not match is a genuine failure.
            flags.push(format!(
                "check digit mismatch: position 9 is '{check_digit}', North-American algorithm expects '{expected}'"
            ));
            Some(false)
        } else {
            // A letter in position 9 that isn't 'X': this VIN does not carry a
            // computed check digit (legal for non-North-American VINs).
            flags.push(
                "position 9 is a letter other than 'X': VIN does not use a verifiable check digit"
                    .to_string(),
            );
            None
        }
    } else {
        None
    };

    // --- model year (position 10) with the position-7 hint ---
    let pos7 = chars[6];
    let pos7_is_alpha = Some(pos7.is_ascii_alphabetic());
    let model_year = decode_model_year(chars[9], pos7_is_alpha);
    if model_year.candidates.is_empty() {
        flags.push(format!(
            "model-year character '{}' (position 10) is not a recognised year code",
            chars[9]
        ));
    } else if model_year.resolved.is_none() {
        flags.push(format!(
            "model year ambiguous: '{}' could be {:?} (30-year VIN cycle; no position-7 hint)",
            chars[9], model_year.candidates
        ));
    }

    let wmi_info = lookup_wmi(&wmi);
    if wmi_info.manufacturer.is_none() {
        flags.push(format!("WMI '{wmi}' not in manufacturer table"));
    }
    if wmi_info.country.is_none() {
        flags.push(format!("country not derivable from WMI '{wmi}'"));
    }

    Ok(Vin {
        vin,
        wmi,
        vds,
        vis,
        wmi_info,
        check_digit,
        check_digit_valid,
        model_year,
        plant_code,
        serial,
        vds_descriptor,
        flags,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_length() {
        assert_eq!(decode_vin("WVWZZZ").unwrap_err(), VinError::BadLength(6));
    }

    #[test]
    fn rejects_illegal_char() {
        // 'O' at position 4.
        let e = decode_vin("WVWOZZZ1J2345678X").unwrap_err();
        assert_eq!(e, VinError::IllegalChar('O', 4));
    }

    #[test]
    fn na_check_digit_valid() {
        // Canonical NHTSA worked example: 1M8GDM9AXKP042788, check digit 'X'.
        let v = decode_vin("1M8GDM9AXKP042788").unwrap();
        assert_eq!(v.check_digit, 'X');
        assert_eq!(v.check_digit_valid, Some(true));
    }

    #[test]
    fn na_check_digit_invalid_when_corrupted() {
        // Flip a character so the check digit no longer matches.
        let v = decode_vin("1M8GDM9AXKP042789").unwrap();
        assert_eq!(v.check_digit_valid, Some(false));
    }

    #[test]
    fn real_vw_vin_fields() {
        // A real-form VW Golf VIN (check digit position is a filler letter on
        // many EU VINs; here position 9 'Z' → no verifiable check digit).
        let v = decode_vin("WVWZZZ1KZ8W123456").unwrap();
        assert_eq!(v.wmi, "WVW");
        assert_eq!(v.wmi_info.manufacturer, Some("Volkswagen (passenger)"));
        assert_eq!(v.wmi_info.country, Some("Germany"));
        assert_eq!(v.vds, "ZZZ1KZ");
        assert_eq!(v.vis, "8W123456");
        assert_eq!(v.plant_code, 'W');
        assert_eq!(v.serial, "123456");
        // position 9 'Z' is a letter other than X → no verifiable check digit.
        assert_eq!(v.check_digit_valid, None);
        // Model year code '8' → 2008 (pos 7 is '1', numeric → earlier cycle).
        assert!(v.model_year.candidates.contains(&2008));
        assert_eq!(v.model_year.resolved, Some(2008));
    }

    #[test]
    fn mercedes_vin_wmi() {
        // Mercedes-Benz WDD (passenger). Synthetic but well-formed VIN.
        let v = decode_vin("WDD2040081A123456").unwrap();
        assert_eq!(v.wmi_info.manufacturer, Some("Mercedes-Benz (passenger)"));
        assert_eq!(v.wmi_info.country, Some("Germany"));
    }

    #[test]
    fn bmw_vin_wmi() {
        let v = decode_vin("WBAVA71050VT12345").unwrap();
        assert_eq!(v.wmi_info.manufacturer, Some("BMW (passenger)"));
    }

    #[test]
    fn model_year_ambiguous_without_hint_is_flagged() {
        // Force the pos-7 hint to numeric vs alpha via the helper directly.
        let my = decode_model_year('L', None);
        assert_eq!(my.candidates, vec![1990, 2020]);
        assert_eq!(my.resolved, None);
        let my2 = decode_model_year('L', Some(true));
        assert_eq!(my2.resolved, Some(2020));
        let my3 = decode_model_year('L', Some(false));
        assert_eq!(my3.resolved, Some(1990));
    }

    #[test]
    fn year_code_zero_is_unknown() {
        let my = decode_model_year('0', None);
        assert!(my.candidates.is_empty());
        assert_eq!(my.resolved, None);
    }
}
