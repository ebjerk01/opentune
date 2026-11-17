//! The vehicle **profile**: the data that defines a virtual car (its modules,
//! addresses, identity DIDs, stored DTCs, coding / adaptation DIDs, routines,
//! IO-control channels and the test-only security mask). Scenarios are data, so
//! a new virtual vehicle is a new JSON file — see `profiles/demo.json` and
//! `docs/VIRTUAL-CAR.md`.
//!
//! Numbers that are conventionally hex (CAN ids, DIDs, routine ids, bytes) are
//! written as hex strings (`"7E0"`, `"0x7E0"`) or plain JSON numbers.
use opentune_core::{Error, Result};
use serde::{Deserialize, Deserializer};

/// The built-in demo profile (`profiles/demo.json`), compiled into the driver.
pub const DEMO_JSON: &str = include_str!("../profiles/demo.json");

/// Placeholder in a DID's `ascii` value that is replaced by the profile VIN.
pub const VIN_PLACEHOLDER: &str = "$VIN";

/// A whole virtual vehicle.
#[derive(Debug, Clone, Deserialize)]
pub struct Profile {
    /// Stable id, e.g. `"demo"`.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Free-text description / provenance note.
    #[serde(default)]
    pub description: String,
    /// The (fictional) VIN served by modules whose DID value is `$VIN`, and by
    /// OBD mode 09 PID 02 on the OBD-capable module.
    pub vin: String,
    /// Whether the engine starts out running (battery ~14.1 V, live rpm) or off
    /// (~12.6 V, rpm 0).
    #[serde(default = "yes")]
    pub engine_running: bool,
    /// The test-only SecurityAccess relationship shared by every module.
    #[serde(default)]
    pub security: SecurityProfile,
    /// The modules on the virtual bus.
    pub modules: Vec<ModuleProfile>,
}

fn yes() -> bool {
    true
}

/// The **test-only** seed→key relationship: `key[i] = seed[i] ^ xor_mask`.
///
/// A deliberately trivial algorithm for exercising SecurityAccess (0x27)
/// offline. It is not, and is not derived from, any manufacturer's algorithm.
#[derive(Debug, Clone, Deserialize)]
pub struct SecurityProfile {
    /// Must be `"test-xor"` (the only algorithm the virtual car knows).
    #[serde(default = "test_xor")]
    pub algorithm: String,
    /// The XOR mask.
    #[serde(default = "default_mask", deserialize_with = "de_u8")]
    pub xor_mask: u8,
    /// Free-text note.
    #[serde(default)]
    pub note: String,
}

fn test_xor() -> String {
    "test-xor".into()
}

fn default_mask() -> u8 {
    0x5A
}

impl Default for SecurityProfile {
    fn default() -> Self {
        Self {
            algorithm: test_xor(),
            xor_mask: default_mask(),
            note: String::new(),
        }
    }
}

/// One diagnostic module on the virtual bus.
#[derive(Debug, Clone, Deserialize)]
pub struct ModuleProfile {
    /// Display name.
    pub name: String,
    /// Logical diagnostic address (informational; routing uses the CAN ids).
    #[serde(default)]
    pub addr: u8,
    /// Physical request CAN id (11-bit).
    #[serde(deserialize_with = "de_u32")]
    pub req_id: u32,
    /// Response CAN id (11-bit).
    #[serde(deserialize_with = "de_u32")]
    pub resp_id: u32,
    /// Answers OBD-II (modes 01/03/04/07/09) on its physical id and on the
    /// functional broadcast id `0x7DF`.
    #[serde(default)]
    pub obd: bool,
    /// The seed this module issues for SecurityAccess (hex bytes).
    #[serde(default, deserialize_with = "de_opt_hex")]
    pub seed: Option<Vec<u8>>,
    /// Refuse WriteDataByIdentifier (NRC 0x33) until SecurityAccess is unlocked.
    #[serde(default)]
    pub require_security_for_write: bool,
    /// Data identifiers and their initial values.
    #[serde(default)]
    pub dids: Vec<DidProfile>,
    /// Stored DTCs.
    #[serde(default)]
    pub dtcs: Vec<DtcProfile>,
    /// RoutineControl (0x31) routines.
    #[serde(default)]
    pub routines: Vec<RoutineProfile>,
    /// InputOutputControlByIdentifier (0x2F) channels.
    #[serde(default)]
    pub io: Vec<IoProfile>,
}

/// One DID and its initial value: exactly one of `ascii` / `hex`.
#[derive(Debug, Clone, Deserialize)]
pub struct DidProfile {
    /// The data identifier.
    #[serde(deserialize_with = "de_u16")]
    pub did: u16,
    /// Text value (`$VIN` = the profile VIN).
    #[serde(default)]
    pub ascii: Option<String>,
    /// Raw value as hex bytes (whitespace allowed).
    #[serde(default)]
    pub hex: Option<String>,
}

/// A stored DTC.
#[derive(Debug, Clone, Deserialize)]
pub struct DtcProfile {
    /// Five-character code, e.g. `"P0101"`.
    pub code: String,
    /// ISO 14229-1 status byte (hex).
    #[serde(deserialize_with = "de_u8")]
    pub status: u8,
    /// Failure-type byte (third DTC byte, hex). Defaults to `00`.
    #[serde(default, deserialize_with = "de_u8")]
    pub failure_type: u8,
}

/// A RoutineControl routine.
#[derive(Debug, Clone, Deserialize)]
pub struct RoutineProfile {
    /// Routine identifier.
    #[serde(deserialize_with = "de_u16")]
    pub id: u16,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// Status record returned by every RoutineControl on it (hex).
    #[serde(default, deserialize_with = "de_opt_hex")]
    pub result: Option<Vec<u8>>,
}

/// An InputOutputControl channel.
#[derive(Debug, Clone, Deserialize)]
pub struct IoProfile {
    /// IO data identifier.
    #[serde(deserialize_with = "de_u16")]
    pub did: u16,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// The ECU's own (default) output state (hex).
    #[serde(deserialize_with = "de_hex")]
    pub default: Vec<u8>,
}

impl Profile {
    /// The built-in demo vehicle.
    pub fn demo() -> Self {
        // The demo is compiled in and covered by tests, so it always parses.
        Self::from_json(DEMO_JSON).expect("built-in demo profile is valid")
    }

    /// Parse and validate a profile from JSON.
    pub fn from_json(text: &str) -> Result<Self> {
        let p: Profile =
            serde_json::from_str(text).map_err(|e| Error::Config(format!("vcar profile: {e}")))?;
        p.validate()?;
        Ok(p)
    }

    /// Load a profile from a JSON file.
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("read vcar profile {}: {e}", path.display())))?;
        Self::from_json(&text)
    }

    /// Check the invariants the simulator relies on: a 17-character VIN, the
    /// `test-xor` algorithm, unique 11-bit request/response ids that do not use
    /// the functional id `0x7DF`, valid DID values and DTC codes.
    pub fn validate(&self) -> Result<()> {
        let bad = |m: String| Err(Error::Config(format!("vcar profile '{}': {m}", self.id)));
        if self.vin.len() != 17 || !self.vin.is_ascii() {
            return bad(format!("VIN '{}' must be 17 ASCII characters", self.vin));
        }
        if self.security.algorithm != "test-xor" {
            return bad(format!(
                "unknown security algorithm '{}' (only the test-only 'test-xor' exists)",
                self.security.algorithm
            ));
        }
        if self.modules.is_empty() {
            return bad("no modules".into());
        }
        let mut ids = std::collections::BTreeSet::new();
        for m in &self.modules {
            for id in [m.req_id, m.resp_id] {
                if id > 0x7FF {
                    return bad(format!("{}: CAN id {id:X} is not 11-bit", m.name));
                }
                if id == crate::FUNCTIONAL_ID {
                    return bad(format!("{}: 0x7DF is the functional broadcast id", m.name));
                }
                if !ids.insert(id) {
                    return bad(format!("{}: CAN id {id:X} is used twice", m.name));
                }
            }
            for d in &m.dids {
                d.value(&self.vin)
                    .map_err(|e| Error::Config(format!("{}: DID {:04X}: {e}", m.name, d.did)))?;
            }
            for d in &m.dtcs {
                parse_dtc(&d.code)
                    .map_err(|e| Error::Config(format!("{}: DTC {}: {e}", m.name, d.code)))?;
            }
        }
        Ok(())
    }
}

impl DidProfile {
    /// The DID's initial value bytes.
    pub fn value(&self, vin: &str) -> std::result::Result<Vec<u8>, String> {
        match (&self.ascii, &self.hex) {
            (Some(a), None) if a == VIN_PLACEHOLDER => Ok(vin.as_bytes().to_vec()),
            (Some(a), None) => Ok(a.as_bytes().to_vec()),
            (None, Some(h)) => parse_hex(h),
            _ => Err("give exactly one of 'ascii' / 'hex'".into()),
        }
    }
}

/// Encode a five-character DTC (`P0101`, `C0035`, `B1A00`, `U0100`) as its two
/// ISO 15031-6 / ISO 14229 bytes: letter in bits 7-6, first digit (0-3) in
/// bits 5-4, then three hex digits.
pub fn parse_dtc(code: &str) -> std::result::Result<[u8; 2], String> {
    let c: Vec<char> = code.trim().chars().collect();
    if c.len() != 5 {
        return Err("expected 5 characters, e.g. P0101".into());
    }
    let sys = match c[0].to_ascii_uppercase() {
        'P' => 0u8,
        'C' => 1,
        'B' => 2,
        'U' => 3,
        other => return Err(format!("unknown system letter '{other}'")),
    };
    let d1 = c[1]
        .to_digit(4)
        .ok_or_else(|| "second character must be 0-3".to_string())? as u8;
    let mut nib = [0u8; 3];
    for (i, ch) in c[2..].iter().enumerate() {
        nib[i] = ch
            .to_digit(16)
            .ok_or_else(|| format!("'{ch}' is not a hex digit"))? as u8;
    }
    Ok([(sys << 6) | (d1 << 4) | nib[0], (nib[1] << 4) | nib[2]])
}

/// Parse whitespace-tolerant hex bytes (`"03 52"`, `"0352"`).
pub fn parse_hex(s: &str) -> std::result::Result<Vec<u8>, String> {
    let compact: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.len() % 2 != 0 {
        return Err(format!("hex '{s}' has an odd number of digits"));
    }
    (0..compact.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&compact[i..i + 2], 16).map_err(|_| format!("bad hex in '{s}'"))
        })
        .collect()
}

/// A JSON number, or a hex string with or without `0x`.
fn num(v: serde_json::Value) -> std::result::Result<u64, String> {
    match v {
        serde_json::Value::Number(n) => n.as_u64().ok_or_else(|| format!("bad number {n}")),
        serde_json::Value::String(s) => {
            let t = s.trim();
            let h = t
                .strip_prefix("0x")
                .or_else(|| t.strip_prefix("0X"))
                .unwrap_or(t);
            u64::from_str_radix(h, 16).map_err(|_| format!("bad hex number '{s}'"))
        }
        other => Err(format!("expected a number or hex string, got {other}")),
    }
}

fn de_num<'de, D: Deserializer<'de>>(d: D, max: u64) -> std::result::Result<u64, D::Error> {
    let v = num(serde_json::Value::deserialize(d)?).map_err(serde::de::Error::custom)?;
    if v > max {
        return Err(serde::de::Error::custom(format!(
            "{v:#X} is larger than {max:#X}"
        )));
    }
    Ok(v)
}

fn de_u32<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<u32, D::Error> {
    de_num(d, u32::MAX as u64).map(|v| v as u32)
}

fn de_u16<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<u16, D::Error> {
    de_num(d, u16::MAX as u64).map(|v| v as u16)
}

fn de_u8<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<u8, D::Error> {
    de_num(d, u8::MAX as u64).map(|v| v as u8)
}

fn de_hex<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Vec<u8>, D::Error> {
    let s = String::deserialize(d)?;
    parse_hex(&s).map_err(serde::de::Error::custom)
}

fn de_opt_hex<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Option<Vec<u8>>, D::Error> {
    de_hex(d).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_profile_is_valid() {
        let p = Profile::demo();
        assert_eq!(p.modules.len(), 6);
        assert_eq!(p.security.xor_mask, 0x5A);
        assert!(p.modules[0].obd);
        assert_eq!((p.modules[0].req_id, p.modules[0].resp_id), (0x7E0, 0x7E8));
    }

    #[test]
    fn dtc_codes_encode() {
        assert_eq!(parse_dtc("P0101").unwrap(), [0x01, 0x01]);
        assert_eq!(parse_dtc("C0035").unwrap(), [0x40, 0x35]);
        assert_eq!(parse_dtc("B1A00").unwrap(), [0x9A, 0x00]);
        assert_eq!(parse_dtc("U0100").unwrap(), [0xC1, 0x00]);
        assert!(parse_dtc("X0100").is_err());
        assert!(parse_dtc("P4100").is_err());
        assert!(parse_dtc("P010").is_err());
    }

    #[test]
    fn invalid_profiles_are_rejected() {
        let base = |modules: &str| {
            format!(r#"{{"id":"t","name":"t","vin":"TESTVCAR0D0000001","modules":[{modules}]}}"#)
        };
        assert!(
            Profile::from_json(&base(r#"{"name":"a","req_id":"7E0","resp_id":"7E8"}"#)).is_ok()
        );
        // Shared response id.
        assert!(Profile::from_json(&base(
            r#"{"name":"a","req_id":"7E0","resp_id":"7E8"},{"name":"b","req_id":"7E1","resp_id":"7E8"}"#
        ))
        .is_err());
        // The functional id.
        assert!(
            Profile::from_json(&base(r#"{"name":"a","req_id":"7DF","resp_id":"7E8"}"#)).is_err()
        );
        // Both ascii and hex.
        assert!(Profile::from_json(&base(
            r#"{"name":"a","req_id":"7E0","resp_id":"7E8","dids":[{"did":"F187","ascii":"x","hex":"00"}]}"#
        ))
        .is_err());
        // An unknown security algorithm.
        let bad_sec = r#"{"id":"t","name":"t","vin":"TESTVCAR0D0000001","security":{"algorithm":"oem"},
            "modules":[{"name":"a","req_id":"7E0","resp_id":"7E8"}]}"#;
        assert!(Profile::from_json(bad_sec).is_err());
    }
}
