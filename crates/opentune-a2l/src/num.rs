//! Numeric parsing helpers shared across the A2L importer.
//!
//! A2L numbers appear as decimals (`16`, `-3.2768`, `1.295360e-01`) and as
//! `0x`-prefixed hex (`0x3A566`). Addresses are hex or decimal; limits are
//! floating point.

/// Parse a decimal or `0x`-prefixed hex integer into `u64`.
pub fn parse_u64(s: &str) -> Option<u64> {
    let t = s.trim();
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        u64::from_str_radix(h, 16).ok()
    } else {
        t.parse::<u64>()
            .ok()
            .or_else(|| t.parse::<f64>().ok().map(|v| v as u64))
    }
}

/// Parse a possibly-hex unsigned 32-bit value (an ECU address), saturating.
pub fn parse_u32(s: &str) -> Option<u32> {
    parse_u64(s).map(|v| v.min(u32::MAX as u64) as u32)
}

/// Parse a floating-point number (also accepts `0x`-prefixed hex integers).
pub fn parse_f64(s: &str) -> Option<f64> {
    let t = s.trim();
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return u64::from_str_radix(h, 16).ok().map(|v| v as f64);
    }
    t.parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        assert_eq!(parse_u32("0x3A566"), Some(0x3A566));
        assert_eq!(parse_u32("16"), Some(16));
        assert_eq!(parse_f64("-3.276800"), Some(-3.2768));
        assert_eq!(parse_f64("1.295360e-01"), Some(0.129536));
        assert_eq!(parse_u64("0X10"), Some(16));
    }
}
