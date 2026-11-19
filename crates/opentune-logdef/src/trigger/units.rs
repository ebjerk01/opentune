//! Units for trigger comparisons: alias folding plus the few conversions a
//! threshold needs (temperature, speed, pressure). Anything else compares
//! only when the spellings fold to the same unit.

/// Canonical spelling of a unit: lower case, no spaces or brackets, aliases
/// of the same unit folded (`1/min` = `rpm`, `°C` = `degc`, `mbar` = `hpa`,
/// `° BTDC` = `deg`, …).
pub fn canonical_unit(u: &str) -> String {
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
        "°c" | "degc" | "deg.c" | "celsius" | "c" => "degc",
        "°f" | "degf" | "fahrenheit" | "f" => "degf",
        "k" | "kelvin" => "k",
        "km/h" | "kmh" | "kph" => "km/h",
        "mph" => "mph",
        "m/s" => "m/s",
        "kpa" => "kpa",
        "hpa" | "mbar" => "hpa",
        "pa" => "pa",
        "bar" => "bar",
        "psi" => "psi",
        "%" | "pct" | "percent" => "%",
        "°" | "deg" | "grad" | "°btdc" | "degbtdc" | "°kw" | "°ca" | "degca" | "°crk" => "deg",
        "v" | "volt" | "volts" => "v",
        other => other,
    };
    canon.to_string()
}

/// `(family, scale, offset)` mapping a canonical unit onto its family's base
/// unit: `base = value * scale + offset`.
fn linear(c: &str) -> Option<(&'static str, f64, f64)> {
    Some(match c {
        "degc" => ("temp", 1.0, 0.0),
        "degf" => ("temp", 5.0 / 9.0, -32.0 * 5.0 / 9.0),
        "k" => ("temp", 1.0, -273.15),
        "km/h" => ("speed", 1.0, 0.0),
        "mph" => ("speed", 1.609_344, 0.0),
        "m/s" => ("speed", 3.6, 0.0),
        "kpa" => ("pressure", 1.0, 0.0),
        "hpa" => ("pressure", 0.1, 0.0),
        "pa" => ("pressure", 0.001, 0.0),
        "bar" => ("pressure", 100.0, 0.0),
        "psi" => ("pressure", 6.894_757_293_168_361, 0.0),
        _ => return None,
    })
}

/// Convert `v` from unit `from` to unit `to`. `None` when the units differ
/// and no conversion is known. Equal (folded) units return `v` unchanged.
pub fn convert_units(v: f64, from: &str, to: &str) -> Option<f64> {
    let (a, b) = (canonical_unit(from), canonical_unit(to));
    if a == b {
        return Some(v);
    }
    let (fa, sa, oa) = linear(&a)?;
    let (fb, sb, ob) = linear(&b)?;
    if fa != fb {
        return None;
    }
    Some((v * sa + oa - ob) / sb)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_and_converts() {
        assert_eq!(canonical_unit("[°C]"), "degc");
        assert_eq!(canonical_unit("° BTDC"), "deg");
        assert_eq!(canonical_unit("1/min"), "rpm");
        assert_eq!(convert_units(3000.0, "1/min", "rpm"), Some(3000.0));
        let f = convert_units(100.0, "°C", "°F").unwrap();
        assert!((f - 212.0).abs() < 1e-9);
        let c = convert_units(212.0, "degF", "°C").unwrap();
        assert!((c - 100.0).abs() < 1e-9);
        let k = convert_units(0.0, "°C", "K").unwrap();
        assert!((k - 273.15).abs() < 1e-9);
        let kmh = convert_units(60.0, "mph", "km/h").unwrap();
        assert!((kmh - 96.56064).abs() < 1e-9);
        assert_eq!(convert_units(1.5, "bar", "kPa"), Some(150.0));
        assert!((convert_units(1013.0, "mbar", "kPa").unwrap() - 101.3).abs() < 1e-9);
        assert_eq!(convert_units(1.0, "bar", "°C"), None);
        assert_eq!(convert_units(1.0, "rpm", "%"), None);
        assert_eq!(convert_units(1.0, "g/s", "kg/h"), None);
    }
}
