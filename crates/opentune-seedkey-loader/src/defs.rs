//! Parse and validate an UnlockECU-format definitions file.
//!
//! The format is taken from the public README of `jglim/UnlockECU` (MIT): each
//! definition is a JSON object with `EcuName`, `Aliases`, `AccessLevel`,
//! `SeedLength`, `KeyLength`, `Provider`, `Origin` and `Parameters` (a list of
//! `{Key, Value, DataType}`). OpenTune reads only that shape; it ships no
//! definition. See `docs/SEEDKEY-LOADER.md`.
//!
//! Structure errors (not JSON, no definition list, file too large, too many
//! entries) fail the whole file. A single entry that fails validation is
//! **rejected** (reported with its index and reason, never run), and an entry
//! in the immobiliser / anti-theft class is **refused** ([`crate::scope`]).
use crate::scope;
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::fmt;
use std::path::Path;

/// Largest definitions file read (bytes).
pub const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// Most definitions one file may hold.
pub const MAX_DEFINITIONS: usize = 50_000;
/// Longest seed or key (bytes).
pub const MAX_SEED_KEY_LEN: usize = 64;
/// Longest `EcuName` / alias / provider / origin / parameter key (chars).
pub const MAX_NAME_LEN: usize = 128;
/// Most aliases per definition.
pub const MAX_ALIASES: usize = 64;
/// Most parameters per definition.
pub const MAX_PARAMS: usize = 64;
/// Largest `ByteArray` / `String` parameter value (bytes / chars).
pub const MAX_PARAM_LEN: usize = 4096;
/// The access-level (request-seed sub-function) range accepted.
pub const ACCESS_LEVELS: std::ops::RangeInclusive<u32> = 0x01..=0x7E;

/// A parameter value. `Debug` never shows the value (it may be secret).
#[derive(Clone, PartialEq, Eq)]
pub enum ParamValue {
    /// `DataType: "ByteArray"` — the `Value` is hex text.
    ByteArray(Vec<u8>),
    /// `DataType: "Int32"` — a JSON number, or decimal / `0x`-hex text, that
    /// fits a signed or unsigned 32-bit integer.
    Int32(i64),
    /// `DataType: "String"` — free text.
    String(String),
}

impl ParamValue {
    /// The `DataType` name.
    pub fn type_name(&self) -> &'static str {
        match self {
            ParamValue::ByteArray(_) => "ByteArray",
            ParamValue::Int32(_) => "Int32",
            ParamValue::String(_) => "String",
        }
    }
}

impl fmt::Debug for ParamValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParamValue::ByteArray(b) => write!(f, "ByteArray(<redacted, {} bytes>)", b.len()),
            ParamValue::Int32(_) => write!(f, "Int32(<redacted>)"),
            ParamValue::String(s) => write!(f, "String(<redacted, {} chars>)", s.chars().count()),
        }
    }
}

/// One `{Key, Value, DataType}` parameter. `Debug` redacts the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parameter {
    /// `Key`.
    pub key: String,
    /// `Value`, typed by `DataType`.
    pub value: ParamValue,
}

/// One validated definition. `Debug` redacts parameter values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    /// Index in the file's definition list (0-based).
    pub index: usize,
    /// `EcuName`.
    pub ecu_name: String,
    /// `Aliases`.
    pub aliases: Vec<String>,
    /// `AccessLevel`: the request-seed (odd) SecurityAccess level it serves.
    pub access_level: u8,
    /// `SeedLength` in bytes.
    pub seed_length: usize,
    /// `KeyLength` in bytes.
    pub key_length: usize,
    /// `Provider`: the algorithm name the provider program implements.
    pub provider: String,
    /// `Origin` (free text, may be empty).
    pub origin: String,
    /// `Parameters`.
    pub parameters: Vec<Parameter>,
}

impl Definition {
    /// Whether `name` is this definition's `EcuName` or one of its aliases
    /// (ASCII case-insensitive, surrounding blanks ignored).
    pub fn answers_to(&self, name: &str) -> bool {
        let name = name.trim();
        self.ecu_name.eq_ignore_ascii_case(name)
            || self.aliases.iter().any(|a| a.eq_ignore_ascii_case(name))
    }

    /// The parameter called `key` (exact match), if any.
    pub fn param(&self, key: &str) -> Option<&ParamValue> {
        self.parameters
            .iter()
            .find(|p| p.key == key)
            .map(|p| &p.value)
    }
}

/// A definition the refuse-list matched. Never run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// Index in the file's definition list.
    pub index: usize,
    /// Its `EcuName` (as far as it could be read).
    pub ecu_name: String,
    /// The field that matched (`EcuName`, `Aliases`, `Origin`, `Provider`).
    pub field: &'static str,
    /// The refuse-list term it matched.
    pub term: &'static str,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "definition #{} '{}': {} matches '{}' — {}",
            self.index,
            self.ecu_name,
            self.field,
            self.term,
            scope::SCOPE_NOTE
        )
    }
}

/// A definition that failed validation. Never run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    /// Index in the file's definition list.
    pub index: usize,
    /// Its `EcuName` (as far as it could be read).
    pub ecu_name: String,
    /// Why.
    pub reason: String,
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "definition #{} '{}': rejected: {}",
            self.index, self.ecu_name, self.reason
        )
    }
}

/// A loaded definitions file: the usable definitions plus what was refused
/// and rejected.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DefinitionSet {
    /// Validated, in-scope definitions.
    pub definitions: Vec<Definition>,
    /// Entries the immobiliser / anti-theft refuse-list matched.
    pub refused: Vec<Refusal>,
    /// Entries that failed validation.
    pub rejected: Vec<Rejection>,
}

impl DefinitionSet {
    /// Read and parse a definitions file (at most [`MAX_FILE_BYTES`]).
    pub fn load(path: &Path) -> Result<Self, String> {
        let meta = std::fs::metadata(path)
            .map_err(|e| format!("definitions file {}: {e}", path.display()))?;
        if !meta.is_file() {
            return Err(format!(
                "definitions file {}: not a regular file",
                path.display()
            ));
        }
        if meta.len() > MAX_FILE_BYTES {
            return Err(format!(
                "definitions file {}: {} bytes is over the {MAX_FILE_BYTES}-byte limit",
                path.display(),
                meta.len()
            ));
        }
        let raw =
            std::fs::read(path).map_err(|e| format!("definitions file {}: {e}", path.display()))?;
        if raw.len() as u64 > MAX_FILE_BYTES {
            return Err(format!(
                "definitions file {}: over the {MAX_FILE_BYTES}-byte limit",
                path.display()
            ));
        }
        let text = std::str::from_utf8(&raw)
            .map_err(|_| format!("definitions file {}: not UTF-8 text", path.display()))?;
        Self::parse(text).map_err(|e| format!("definitions file {}: {e}", path.display()))
    }

    /// Parse definitions JSON: a top-level array of definitions, or an object
    /// with exactly one array-valued member holding them.
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.len() as u64 > MAX_FILE_BYTES {
            return Err(format!("over the {MAX_FILE_BYTES}-byte limit"));
        }
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let root: Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
        let list = match &root {
            Value::Array(a) => a,
            Value::Object(o) => {
                let arrays: Vec<&Vec<Value>> = o
                    .values()
                    .filter_map(|v| match v {
                        Value::Array(a) => Some(a),
                        _ => None,
                    })
                    .collect();
                match arrays.as_slice() {
                    [one] => *one,
                    [] => {
                        return Err(
                            "no definition list (expected a JSON array of definitions)".into()
                        )
                    }
                    _ => {
                        return Err(
                            "ambiguous: the top-level object has more than one array member".into(),
                        )
                    }
                }
            }
            _ => return Err("expected a JSON array of definitions".into()),
        };
        if list.len() > MAX_DEFINITIONS {
            return Err(format!(
                "{} definitions is over the limit of {MAX_DEFINITIONS}",
                list.len()
            ));
        }
        let mut set = DefinitionSet::default();
        let mut seen: HashSet<(String, u8)> = HashSet::new();
        for (index, v) in list.iter().enumerate() {
            let name = v
                .get("EcuName")
                .and_then(Value::as_str)
                .map(|s| printable(s, MAX_NAME_LEN))
                .unwrap_or_else(|| "?".into());
            if let Some((field, term)) = refusal(v) {
                set.refused.push(Refusal {
                    index,
                    ecu_name: name,
                    field,
                    term,
                });
                continue;
            }
            match definition(index, v) {
                Ok(d) => {
                    // One definition per (name or alias, level): a later
                    // duplicate would make the choice ambiguous.
                    let keys: Vec<(String, u8)> = std::iter::once(&d.ecu_name)
                        .chain(&d.aliases)
                        .map(|n| (n.to_ascii_lowercase(), d.access_level))
                        .collect();
                    if let Some((n, _)) = keys.iter().find(|k| seen.contains(*k)) {
                        set.rejected.push(Rejection {
                            index,
                            ecu_name: name,
                            reason: format!(
                                "duplicate: '{n}' at AccessLevel 0x{:02X} is already defined",
                                d.access_level
                            ),
                        });
                        continue;
                    }
                    seen.extend(keys);
                    set.definitions.push(d);
                }
                Err(reason) => set.rejected.push(Rejection {
                    index,
                    ecu_name: name,
                    reason,
                }),
            }
        }
        Ok(set)
    }

    /// The definitions that answer to `name` (EcuName or alias).
    pub fn lookup(&self, name: &str) -> Vec<&Definition> {
        self.definitions
            .iter()
            .filter(|d| d.answers_to(name))
            .collect()
    }
}

/// The first refuse-list match among EcuName, Aliases, Origin and Provider,
/// read leniently (a refused entry is refused even when malformed).
fn refusal(v: &Value) -> Option<(&'static str, &'static str)> {
    for k in ["EcuName", "Origin", "Provider"] {
        if let Some(t) = v
            .get(k)
            .and_then(Value::as_str)
            .and_then(scope::refused_term)
        {
            return Some((k, t));
        }
    }
    v.get("Aliases")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .find_map(scope::refused_term)
        .map(|t| ("Aliases", t))
}

/// `s` with control characters replaced, at most `max` chars (for messages).
fn printable(s: &str, max: usize) -> String {
    s.chars()
        .take(max)
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

fn obj(v: &Value) -> Result<&Map<String, Value>, String> {
    v.as_object().ok_or_else(|| "not a JSON object".to_string())
}

fn name_field(o: &Map<String, Value>, key: &str, required: bool) -> Result<String, String> {
    let s = match o.get(key) {
        None | Some(Value::Null) if !required => return Ok(String::new()),
        None => return Err(format!("{key} is missing")),
        Some(Value::String(s)) => s.trim(),
        Some(_) => return Err(format!("{key} must be a string")),
    };
    if required && s.is_empty() {
        return Err(format!("{key} is empty"));
    }
    if s.chars().count() > MAX_NAME_LEN {
        return Err(format!("{key} is longer than {MAX_NAME_LEN} characters"));
    }
    if s.chars().any(char::is_control) {
        return Err(format!("{key} contains control characters"));
    }
    Ok(s.to_string())
}

fn uint_field(o: &Map<String, Value>, key: &str) -> Result<u64, String> {
    o.get(key)
        .ok_or_else(|| format!("{key} is missing"))?
        .as_u64()
        .ok_or_else(|| format!("{key} must be a non-negative integer"))
}

fn definition(index: usize, v: &Value) -> Result<Definition, String> {
    let o = obj(v)?;
    let ecu_name = name_field(o, "EcuName", true)?;
    let aliases = match o.get("Aliases") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => {
            if a.len() > MAX_ALIASES {
                return Err(format!("more than {MAX_ALIASES} Aliases"));
            }
            a.iter()
                .map(|x| match x {
                    Value::String(s) if !s.trim().is_empty() => {
                        let s = s.trim();
                        if s.chars().count() > MAX_NAME_LEN || s.chars().any(char::is_control) {
                            Err("an alias is too long or contains control characters".to_string())
                        } else {
                            Ok(s.to_string())
                        }
                    }
                    _ => Err("Aliases must be non-empty strings".to_string()),
                })
                .collect::<Result<_, _>>()?
        }
        Some(_) => return Err("Aliases must be an array of strings".into()),
    };
    let level = uint_field(o, "AccessLevel")?;
    if !u32::try_from(level).is_ok_and(|l| ACCESS_LEVELS.contains(&l)) {
        return Err(format!(
            "AccessLevel {level} is outside 0x{:02X}..=0x{:02X}",
            ACCESS_LEVELS.start(),
            ACCESS_LEVELS.end()
        ));
    }
    let mut lens = [0usize; 2];
    for (slot, key) in lens.iter_mut().zip(["SeedLength", "KeyLength"]) {
        let n = uint_field(o, key)?;
        if n == 0 || n > MAX_SEED_KEY_LEN as u64 {
            return Err(format!("{key} {n} is outside 1..={MAX_SEED_KEY_LEN}"));
        }
        *slot = n as usize;
    }
    let provider = name_field(o, "Provider", true)?;
    if !provider
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return Err("Provider may hold only ASCII letters, digits, '_', '-' and '.'".into());
    }
    let origin = name_field(o, "Origin", false)?;
    let parameters = match o.get("Parameters") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => {
            if a.len() > MAX_PARAMS {
                return Err(format!("more than {MAX_PARAMS} Parameters"));
            }
            let mut keys = HashSet::new();
            let mut out = Vec::with_capacity(a.len());
            for (i, p) in a.iter().enumerate() {
                let p = parameter(p).map_err(|e| format!("Parameters[{i}]: {e}"))?;
                if !keys.insert(p.key.clone()) {
                    return Err(format!("Parameters[{i}]: duplicate Key '{}'", p.key));
                }
                out.push(p);
            }
            out
        }
        Some(_) => return Err("Parameters must be an array".into()),
    };
    Ok(Definition {
        index,
        ecu_name,
        aliases,
        access_level: level as u8,
        seed_length: lens[0],
        key_length: lens[1],
        provider,
        origin,
        parameters,
    })
}

fn parameter(v: &Value) -> Result<Parameter, String> {
    let o = obj(v)?;
    let key = name_field(o, "Key", true)?;
    let dtype = match o.get("DataType") {
        Some(Value::String(s)) => s.trim(),
        Some(_) => return Err("DataType must be a string".into()),
        None => return Err("DataType is missing".into()),
    };
    let raw = o.get("Value").ok_or("Value is missing")?;
    // Errors name the key and type only, never the value.
    let value = match dtype {
        "ByteArray" => {
            let s = raw.as_str().ok_or("a ByteArray Value must be hex text")?;
            let b = parse_hex(s).map_err(|e| format!("ByteArray '{key}': {e}"))?;
            if b.len() > MAX_PARAM_LEN {
                return Err(format!(
                    "ByteArray '{key}' is longer than {MAX_PARAM_LEN} bytes"
                ));
            }
            ParamValue::ByteArray(b)
        }
        "Int32" => ParamValue::Int32(
            int32(raw).ok_or_else(|| format!("Int32 '{key}': not a 32-bit integer"))?,
        ),
        "String" => {
            let s = raw.as_str().ok_or("a String Value must be text")?;
            if s.chars().count() > MAX_PARAM_LEN {
                return Err(format!(
                    "String '{key}' is longer than {MAX_PARAM_LEN} characters"
                ));
            }
            ParamValue::String(s.to_string())
        }
        other => {
            return Err(format!(
                "unsupported DataType '{}' (supported: ByteArray, Int32, String)",
                printable(other, 32)
            ))
        }
    };
    Ok(Parameter { key, value })
}

fn int32(v: &Value) -> Option<i64> {
    let n = match v {
        Value::Number(n) => n.as_i64()?,
        Value::String(s) => {
            let s = s.trim();
            let (neg, body) = match s.strip_prefix('-') {
                Some(b) => (true, b),
                None => (false, s),
            };
            let mag = match body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
                Some(h) => i64::from_str_radix(h, 16).ok()?,
                None => body.parse::<i64>().ok()?,
            };
            if neg {
                -mag
            } else {
                mag
            }
        }
        _ => return None,
    };
    (i64::from(i32::MIN)..=i64::from(u32::MAX))
        .contains(&n)
        .then_some(n)
}

/// Hex text to bytes: whitespace is ignored; an even number of hex digits is
/// required. The error never echoes the text.
pub fn parse_hex(s: &str) -> Result<Vec<u8>, String> {
    let digits: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if digits.len() % 2 != 0 {
        return Err("odd number of hex digits".into());
    }
    digits
        .chunks(2)
        .map(|p| {
            let hi = (p[0] as char).to_digit(16);
            let lo = (p[1] as char).to_digit(16);
            match (hi, lo) {
                (Some(h), Some(l)) => Ok((h * 16 + l) as u8),
                _ => Err("not hex".to_string()),
            }
        })
        .collect()
}

/// Bytes to upper-case hex.
pub fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every definition here is synthetic, written for the tests.
    fn def(name: &str, level: u32) -> String {
        format!(
            r#"{{"EcuName":"{name}","Aliases":["{name}-ALT"],"AccessLevel":{level},"SeedLength":4,"KeyLength":4,"Provider":"DemoXor","Origin":"synthetic test","Parameters":[{{"Key":"Mask","Value":"A5A5A5A5","DataType":"ByteArray"}},{{"Key":"N","Value":"0x10","DataType":"Int32"}}]}}"#
        )
    }

    #[test]
    fn parses_the_readme_shape() {
        let text = format!("[{},{}]", def("SIM-ECU", 17), def("SIM-ECU", 3));
        let set = DefinitionSet::parse(&text).unwrap();
        assert_eq!(set.definitions.len(), 2);
        assert!(set.refused.is_empty() && set.rejected.is_empty());
        let d = &set.definitions[0];
        assert_eq!(d.access_level, 0x11);
        assert_eq!((d.seed_length, d.key_length), (4, 4));
        assert_eq!(d.param("Mask"), Some(&ParamValue::ByteArray(vec![0xA5; 4])));
        assert_eq!(d.param("N"), Some(&ParamValue::Int32(16)));
        assert_eq!(set.lookup("sim-ecu-alt").len(), 2);
        assert!(set.lookup("other").is_empty());
        // An object wrapping exactly one list works too.
        let wrapped = format!("{{\"Whatever\":[{}],\"Version\":1}}", def("A", 1));
        assert_eq!(DefinitionSet::parse(&wrapped).unwrap().definitions.len(), 1);
    }

    #[test]
    fn debug_output_redacts_parameter_values() {
        let set = DefinitionSet::parse(&format!("[{}]", def("SIM-ECU", 17))).unwrap();
        let dbg = format!("{:?}", set);
        assert!(dbg.contains("<redacted, 4 bytes>"), "{dbg}");
        assert!(
            !dbg.contains("165") && !dbg.to_uppercase().contains("A5A5"),
            "{dbg}"
        );
        assert!(!dbg.contains("Int32(16)"), "{dbg}");
    }

    #[test]
    fn malformed_files_fail_whole() {
        for bad in [
            "",
            "not json",
            "42",
            "\"text\"",
            "{}",
            "{\"a\":[],\"b\":[]}",
            "[1,2",
        ] {
            assert!(DefinitionSet::parse(bad).is_err(), "{bad:?}");
        }
        let huge = format!("[{}]", vec!["{}"; MAX_DEFINITIONS + 1].join(","));
        let e = DefinitionSet::parse(&huge).unwrap_err();
        assert!(e.contains("over the limit"), "{e}");
    }

    #[test]
    fn malformed_entries_are_rejected_with_a_reason() {
        let good = def("OK", 1);
        let cases = [
            (r#"5"#, "not a JSON object"),
            (r#"{"AccessLevel":1}"#, "EcuName is missing"),
            (
                r#"{"EcuName":"","AccessLevel":1,"SeedLength":4,"KeyLength":4,"Provider":"P"}"#,
                "EcuName is empty",
            ),
            (
                r#"{"EcuName":"X","AccessLevel":0,"SeedLength":4,"KeyLength":4,"Provider":"P"}"#,
                "AccessLevel 0",
            ),
            (
                r#"{"EcuName":"X","AccessLevel":127,"SeedLength":4,"KeyLength":4,"Provider":"P"}"#,
                "AccessLevel 127",
            ),
            (
                r#"{"EcuName":"X","AccessLevel":-1,"SeedLength":4,"KeyLength":4,"Provider":"P"}"#,
                "AccessLevel must be",
            ),
            (
                r#"{"EcuName":"X","AccessLevel":1,"SeedLength":0,"KeyLength":4,"Provider":"P"}"#,
                "SeedLength 0",
            ),
            (
                r#"{"EcuName":"X","AccessLevel":1,"SeedLength":4,"KeyLength":65,"Provider":"P"}"#,
                "KeyLength 65",
            ),
            (
                r#"{"EcuName":"X","AccessLevel":1,"SeedLength":"4","KeyLength":4,"Provider":"P"}"#,
                "SeedLength must be",
            ),
            (
                r#"{"EcuName":"X","AccessLevel":1,"SeedLength":4,"KeyLength":4}"#,
                "Provider is missing",
            ),
            (
                r#"{"EcuName":"X","AccessLevel":1,"SeedLength":4,"KeyLength":4,"Provider":"a b"}"#,
                "Provider may hold",
            ),
            (
                r#"{"EcuName":"X","AccessLevel":1,"SeedLength":4,"KeyLength":4,"Provider":"P","Aliases":[1]}"#,
                "Aliases must be",
            ),
            (
                r#"{"EcuName":"X","AccessLevel":1,"SeedLength":4,"KeyLength":4,"Provider":"P","Parameters":{}}"#,
                "Parameters must be",
            ),
            (
                r#"{"EcuName":"X","AccessLevel":1,"SeedLength":4,"KeyLength":4,"Provider":"P","Parameters":[{"Key":"K","Value":"ABC","DataType":"ByteArray"}]}"#,
                "odd number",
            ),
            (
                r#"{"EcuName":"X","AccessLevel":1,"SeedLength":4,"KeyLength":4,"Provider":"P","Parameters":[{"Key":"K","Value":"ZZ","DataType":"ByteArray"}]}"#,
                "not hex",
            ),
            (
                r#"{"EcuName":"X","AccessLevel":1,"SeedLength":4,"KeyLength":4,"Provider":"P","Parameters":[{"Key":"K","Value":"99999999999","DataType":"Int32"}]}"#,
                "not a 32-bit",
            ),
            (
                r#"{"EcuName":"X","AccessLevel":1,"SeedLength":4,"KeyLength":4,"Provider":"P","Parameters":[{"Key":"K","Value":"1","DataType":"Float"}]}"#,
                "unsupported DataType 'Float'",
            ),
            (
                r#"{"EcuName":"X","AccessLevel":1,"SeedLength":4,"KeyLength":4,"Provider":"P","Parameters":[{"Key":"K","Value":"01"}]}"#,
                "DataType is missing",
            ),
            (
                r#"{"EcuName":"X","AccessLevel":1,"SeedLength":4,"KeyLength":4,"Provider":"P","Parameters":[{"Key":"K","Value":"01","DataType":"ByteArray"},{"Key":"K","Value":"02","DataType":"ByteArray"}]}"#,
                "duplicate Key",
            ),
        ];
        for (entry, want) in cases {
            let set = DefinitionSet::parse(&format!("[{good},{entry}]")).unwrap();
            assert_eq!(set.definitions.len(), 1, "{entry}");
            assert_eq!(set.rejected.len(), 1, "{entry}");
            let r = &set.rejected[0];
            assert_eq!(r.index, 1);
            assert!(r.reason.contains(want), "{entry}: {}", r.reason);
        }
        // A rejection never echoes a parameter value.
        let set = DefinitionSet::parse(
            r#"[{"EcuName":"X","AccessLevel":1,"SeedLength":4,"KeyLength":4,"Provider":"P","Parameters":[{"Key":"K","Value":"SECRETZZ","DataType":"ByteArray"}]}]"#,
        )
        .unwrap();
        assert!(!set.rejected[0].to_string().contains("SECRET"));
    }

    #[test]
    fn duplicates_are_rejected() {
        let text = format!("[{},{},{}]", def("A", 1), def("a", 1), def("A", 3));
        let set = DefinitionSet::parse(&text).unwrap();
        assert_eq!(set.definitions.len(), 2);
        assert_eq!(set.rejected.len(), 1);
        assert!(set.rejected[0].reason.contains("duplicate"));
    }

    #[test]
    fn refuse_list_applies_to_every_field_even_when_malformed() {
        let cases = [
            (
                r#"{"EcuName":"EZS167","AccessLevel":1,"SeedLength":4,"KeyLength":4,"Provider":"P"}"#,
                "EcuName",
                "ezs",
            ),
            (
                r#"{"EcuName":"X","Aliases":["ok","ELV"],"AccessLevel":1,"SeedLength":4,"KeyLength":4,"Provider":"P"}"#,
                "Aliases",
                "elv",
            ),
            (
                r#"{"EcuName":"X","Origin":"immobilizer box","AccessLevel":1,"SeedLength":4,"KeyLength":4,"Provider":"P"}"#,
                "Origin",
                "immobili",
            ),
            (
                r#"{"EcuName":"X","AccessLevel":1,"SeedLength":4,"KeyLength":4,"Provider":"KessyAlgo"}"#,
                "Provider",
                "kessy",
            ),
            // Malformed (no lengths) and still refused, not just rejected.
            (r#"{"EcuName":"WFS"}"#, "EcuName", "wfs"),
        ];
        for (entry, field, term) in cases {
            let set = DefinitionSet::parse(&format!("[{entry}]")).unwrap();
            assert!(
                set.definitions.is_empty() && set.rejected.is_empty(),
                "{entry}"
            );
            assert_eq!(set.refused.len(), 1, "{entry}");
            let r = &set.refused[0];
            assert_eq!((r.field, r.term), (field, term), "{entry}");
            let msg = r.to_string();
            assert!(
                msg.contains("CONTRIBUTING.md") && msg.contains("immobiliser"),
                "{msg}"
            );
        }
    }

    #[test]
    fn loads_files_with_a_size_cap() {
        let dir = std::env::temp_dir().join(format!("ot-skl-defs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ok = dir.join("defs.json");
        std::fs::write(&ok, format!("\u{feff}[{}]", def("A", 1))).unwrap();
        assert_eq!(DefinitionSet::load(&ok).unwrap().definitions.len(), 1);
        let big = dir.join("big.json");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(MAX_FILE_BYTES + 1).unwrap();
        let e = DefinitionSet::load(&big).unwrap_err();
        assert!(e.contains("limit"), "{e}");
        assert!(DefinitionSet::load(&dir.join("missing.json")).is_err());
        assert!(DefinitionSet::load(&dir)
            .unwrap_err()
            .contains("not a regular file"));
        let bin = dir.join("bin.json");
        std::fs::write(&bin, [0xFFu8, 0xFE, 0x00]).unwrap();
        assert!(DefinitionSet::load(&bin).unwrap_err().contains("UTF-8"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
