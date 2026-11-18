//! CAN ids, DIDs and addresses read as either a JSON number or a `"0x…"`
//! string, and written back as hex strings so the files stay readable.
use serde::{Deserialize, Deserializer, Serializer};

#[derive(Deserialize)]
#[serde(untagged)]
enum NumOrStr {
    Num(u64),
    Str(String),
}

fn parse(v: NumOrStr) -> Result<u64, String> {
    match v {
        NumOrStr::Num(n) => Ok(n),
        NumOrStr::Str(s) => {
            let t = s.trim();
            if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                u64::from_str_radix(h, 16).map_err(|e| format!("bad hex '{s}': {e}"))
            } else {
                t.parse::<u64>()
                    .map_err(|e| format!("bad number '{s}': {e} (use 0x… for hex)"))
            }
        }
    }
}

/// Parse a hex string (`0x2C0`) or decimal string into a number.
pub fn parse_str(s: &str) -> Result<u64, String> {
    parse(NumOrStr::Str(s.to_string()))
}

/// `u32` as number or hex string; serialized as `"0x…"`.
pub mod u32_hex {
    use super::*;
    pub fn serialize<S: Serializer>(v: &u32, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("0x{v:X}"))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
        let v = parse(NumOrStr::deserialize(d)?).map_err(serde::de::Error::custom)?;
        u32::try_from(v)
            .map_err(|_| serde::de::Error::custom(format!("{v} does not fit in 32 bits")))
    }
}

/// `Option<u32>` as number or hex string; serialized as `"0x…"` or `null`.
pub mod opt_u32_hex {
    use super::*;
    pub fn serialize<S: Serializer>(v: &Option<u32>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(v) => s.serialize_str(&format!("0x{v:X}")),
            None => s.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
        match Option::<NumOrStr>::deserialize(d)? {
            None => Ok(None),
            Some(v) => {
                let v = parse(v).map_err(serde::de::Error::custom)?;
                u32::try_from(v)
                    .map(Some)
                    .map_err(|_| serde::de::Error::custom(format!("{v} does not fit in 32 bits")))
            }
        }
    }
}

/// `Option<u8>` as number or hex string; serialized as `"0x…"` or `null`.
pub mod opt_u8_hex {
    use super::*;
    pub fn serialize<S: Serializer>(v: &Option<u8>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(v) => s.serialize_str(&format!("0x{v:02X}")),
            None => s.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u8>, D::Error> {
        match Option::<NumOrStr>::deserialize(d)? {
            None => Ok(None),
            Some(v) => {
                let v = parse(v).map_err(serde::de::Error::custom)?;
                u8::try_from(v)
                    .map(Some)
                    .map_err(|_| serde::de::Error::custom(format!("{v} does not fit in 8 bits")))
            }
        }
    }
}

/// `u16` as number or hex string; serialized as `"0x…"`.
pub mod u16_hex {
    use super::*;
    pub fn serialize<S: Serializer>(v: &u16, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("0x{v:04X}"))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u16, D::Error> {
        let v = parse(NumOrStr::deserialize(d)?).map_err(serde::de::Error::custom)?;
        u16::try_from(v)
            .map_err(|_| serde::de::Error::custom(format!("{v} does not fit in 16 bits")))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_hex_and_decimal() {
        assert_eq!(super::parse_str("0x2C0").unwrap(), 0x2C0);
        assert_eq!(super::parse_str("704").unwrap(), 704);
        assert!(super::parse_str("2C0").is_err());
    }
}
