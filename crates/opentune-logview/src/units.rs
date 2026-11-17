//! Comparing a log channel's unit with a map axis's unit.
//!
//! Units are **compared, never converted**. A small table maps spellings of
//! the *same* unit onto one canonical form (`1/min` = `rpm`, `mbar` = `hPa`,
//! `mg/Hub` = `mg/stroke`, …); anything else that differs is a mismatch the
//! caller must report. When either side has no unit the check is `Unknown`.

use serde::Serialize;

/// Outcome of [`check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitCheck {
    /// Same unit (possibly spelled differently).
    Match,
    /// Both sides state a unit and they differ.
    Mismatch,
    /// One side (or both) has no unit, so nothing can be checked.
    Unknown,
}

/// Canonical spelling of `u` for comparison: lower case, no spaces, aliases
/// of exactly the same unit folded together.
pub fn normalize(u: &str) -> String {
    let s: String = u
        .trim()
        .trim_start_matches(['[', '('])
        .trim_end_matches([']', ')'])
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_lowercase()
        .replace('º', "°");
    let canon = match s.as_str() {
        "rpm" | "1/min" | "u/min" | "min-1" | "min^-1" | "/min" | "r/min" => "rpm",
        "mg/stroke" | "mg/str" | "mg/stk" | "mg/hub" | "mg/hüb" | "mg/inj" => "mg/stroke",
        "hpa" | "mbar" => "hpa",
        "°c" | "degc" | "deg.c" | "celsius" | "°celsius" => "degc",
        "km/h" | "kmh" | "kph" => "km/h",
        "%" | "pct" | "percent" => "%",
        "°" | "deg" | "grad" => "deg",
        "°kw" | "degkw" | "°crk" | "degcrk" | "°ca" | "degca" => "degca",
        "v" | "volt" | "volts" => "v",
        "kg/h" | "kgh" => "kg/h",
        "mg/cyc" | "mg/cycle" => "mg/cyc",
        other => other,
    };
    canon.to_string()
}

/// Compare a channel unit with an axis unit.
pub fn check(channel_unit: &str, axis_unit: &str) -> UnitCheck {
    let a = normalize(channel_unit);
    let b = normalize(axis_unit);
    if a.is_empty() || b.is_empty() {
        UnitCheck::Unknown
    } else if a == b {
        UnitCheck::Match
    } else {
        UnitCheck::Mismatch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_match_and_real_differences_do_not() {
        assert_eq!(check("1/min", "rpm"), UnitCheck::Match);
        assert_eq!(check("mbar", "hPa"), UnitCheck::Match);
        assert_eq!(check("mg/Hub", "mg/stroke"), UnitCheck::Match);
        assert_eq!(check("[°C]", "degC"), UnitCheck::Match);
        assert_eq!(check("kPa", "hPa"), UnitCheck::Mismatch);
        assert_eq!(check("bar", "hPa"), UnitCheck::Mismatch);
        assert_eq!(check("mg/stroke", "rpm"), UnitCheck::Mismatch);
        assert_eq!(check("", "rpm"), UnitCheck::Unknown);
        assert_eq!(check("rpm", ""), UnitCheck::Unknown);
    }
}
