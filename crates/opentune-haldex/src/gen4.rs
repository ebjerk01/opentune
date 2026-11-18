//! Read-only engine for the **Haldex Generation 4** controller (VAG PQ35/PQ46).
//!
//! The PQ-era Haldex controller ("J492") is diagnosed with **KWP2000 over VW
//! TP2.0**, not UDS. Public facts used here (no code copied):
//!
//! * TP2.0 channel setup to broadcast id `0x200` with logical address **`0x0A`**,
//!   the controller answering on `0x200 + 0x0A` = `0x20A`; KWP session `0x89`;
//!   identification `0x1A 0x9B`; trouble codes `0x18 02 FF 00`; measuring
//!   blocks `0x21 <group>`. All of this is visible in the public capture
//!   `documents/vehicle-logs/vw-pq/0AY 554C - clear codes, oil temp, ...csv`
//!   in Forbes-Automotive/OpenHaldex-C6 (MIT), taken from an `0AY 907 554 C`
//!   unit identifying itself as "Haldex 4Motion".
//! * Gen4 measuring groups 1 (oil temperature, plate temperature, supply
//!   voltage) and 3 (oil pressure, estimated torque, clutch valve duty,
//!   clutch valve current) and the six value formulas below: OpenHaldex-C6
//!   README "Live Diagnostics" and `src/OpenHaldexC6_UDS.cpp`, which state
//!   they were checked against a scan tool on a Gen4 unit. **Community
//!   confidence**; other formulas are reported raw, never guessed.
//!
//! The engine only sends `0x10` (session), `0x1A` (identification), `0x18`
//! (read trouble codes) and `0x21` (measuring block). It never clears codes,
//! writes coding, adapts, runs routines or flashes. Validation state:
//! **synthetic-tested** (simulated unit); the first real read is planned in
//! `docs/FIRST-CAR-TEST.md`.
use opentune_core::protocol::diag::DiagTransport;
use opentune_core::protocol::kwp2000::Kwp2000Client;
use serde::Serialize;

/// TP2.0 logical address of the PQ Haldex (AWD) controller.
pub const TP20_LOGICAL_ADDRESS: u8 = 0x0A;
/// CAN id the tester broadcasts TP2.0 channel setup on.
pub const TP20_SETUP_ID: u32 = 0x200;
/// CAN id the controller answers the channel setup on (`0x200 + 0x0A`).
pub const TP20_SETUP_RESPONSE_ID: u32 = 0x200 + TP20_LOGICAL_ADDRESS as u32;
/// CAN id the tester asks the controller to send data on (as in the public
/// capture; any free id works).
pub const TP20_TESTER_RX_ID: u32 = 0x300;
/// KWP2000 diagnostic session used by VAG scan tools on this unit.
pub const KWP_SESSION: u8 = 0x89;
/// `readEcuIdentification` option for the VAG identification record.
pub const ID_OPTION: u8 = 0x9B;
/// Measuring groups read by default (1 and 3 carry the documented Gen4
/// values; 125/126 are read raw for characterisation).
pub const DEFAULT_GROUPS: [u8; 4] = [1, 3, 125, 126];

/// Haldex generation suggested by a controller part number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum GenerationHint {
    /// `1K0 907 554` / `7H0 907 554`.
    Gen2,
    /// `0AY 907 554` (except suffix F), `0BR/0BS/0BY 907 554`.
    Gen4,
    /// `0AY 907 554 F` — a PQ-fitted unit OpenHaldex-C6 treats as Gen5.
    Gen5Pq,
    /// `0CQ 907 554`.
    Gen5Mqb,
    /// Not a known Haldex controller number.
    Unknown,
}

/// Classify a controller part number (spaces ignored, case-insensitive).
pub fn classify_part(part: &str) -> GenerationHint {
    let p: String = part
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_uppercase();
    if p.starts_with("0AY907554F") {
        GenerationHint::Gen5Pq
    } else if ["0AY907554", "0BR907554", "0BS907554", "0BY907554"]
        .iter()
        .any(|x| p.starts_with(x))
    {
        GenerationHint::Gen4
    } else if p.starts_with("1K0907554") || p.starts_with("7H0907554") {
        GenerationHint::Gen2
    } else if p.starts_with("0CQ907554") {
        GenerationHint::Gen5Mqb
    } else {
        GenerationHint::Unknown
    }
}

/// Decoded `0x1A 0x9B` identification record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Identity {
    /// VAG part number, e.g. `0AY907554C`.
    pub part_number: String,
    /// Software version (4 characters), e.g. `0041`.
    pub sw_version: String,
    /// Component text, e.g. `Haldex 4Motion`.
    pub component: String,
    /// The bytes between the version and the component text, hex.
    pub extra_hex: String,
    /// Generation suggested by the part number.
    pub generation: GenerationHint,
    /// The whole record, hex (for reports).
    pub raw_hex: String,
}

fn ascii(b: &[u8]) -> String {
    b.iter()
        .map(|&c| {
            if (0x20..0x7F).contains(&c) {
                c as char
            } else {
                ' '
            }
        })
        .collect::<String>()
        .trim()
        .to_string()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

/// Parse the identification record (the bytes after `5A 9B`). Layout as seen
/// in the public capture: part number (12 ASCII, space padded), software
/// version (4 ASCII), 10 binary bytes, component text (20 ASCII). A shorter
/// record keeps whatever fields it has.
pub fn parse_identity(rec: &[u8]) -> Identity {
    let part_number = ascii(rec.get(..12.min(rec.len())).unwrap_or_default());
    let sw_version = ascii(rec.get(12..16).unwrap_or_default());
    let extra = rec.get(16..26.min(rec.len().max(16))).unwrap_or_default();
    let component = ascii(rec.get(26..).unwrap_or_default());
    Identity {
        generation: classify_part(&part_number),
        part_number,
        sw_version,
        component,
        extra_hex: hex(extra),
        raw_hex: hex(rec),
    }
}

/// One measured value of a measuring block: a 3-byte `(formula, a, b)` cell.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MeasuredValue {
    pub formula: u8,
    pub a: u8,
    pub b: u8,
    /// Scaled value, `None` for a formula OpenTune does not know.
    pub value: Option<f64>,
    pub unit: &'static str,
    /// Gen4 meaning of this cell (groups 1/3), community confidence.
    pub label: Option<&'static str>,
}

/// Scale a measuring-block cell. Only formulas with a public, Gen4-checked
/// source (OpenHaldex-C6) are decoded; anything else is `None` (raw).
pub fn scale(formula: u8, a: u8, b: u8) -> Option<(f64, &'static str)> {
    let (a, b) = (a as f64, b as f64);
    Some(match formula {
        0x06 => (0.001 * a * b, "V"),
        0x0E => (0.01 * a * (b - 100.0), "bar"),
        0x18 => (0.001 * a * b, "A"),
        0x1A => (b - a, "°C"),
        0x21 => (0.01 * a * b, "%"),
        0x5E => (0.1 * a * (b - 128.0), "Nm"),
        _ => return None,
    })
}

/// Gen4 meaning of cell `idx` in `group` (OpenHaldex-C6, community).
pub fn gen4_label(group: u8, idx: usize) -> Option<&'static str> {
    match (group, idx) {
        (1, 0) => Some("oil temperature"),
        (1, 1) => Some("clutch plate temperature"),
        (1, 2) => Some("supply voltage"),
        (3, 0) => Some("oil pressure"),
        (3, 1) => Some("estimated torque"),
        (3, 2) => Some("clutch valve duty"),
        (3, 3) => Some("clutch valve current"),
        _ => None,
    }
}

/// A measuring block read.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Group {
    pub group: u8,
    pub values: Vec<MeasuredValue>,
    /// The record after the echoed group, hex.
    pub raw_hex: String,
    /// Set when the read failed (the unit may not implement the group).
    pub error: Option<String>,
}

/// Decode a measuring-block record (after the echoed group) into cells. A
/// trailing partial cell is ignored (kept in `raw_hex`).
pub fn decode_group(group: u8, rec: &[u8]) -> Group {
    let values = rec
        .chunks_exact(3)
        .enumerate()
        .map(|(i, c)| {
            let s = scale(c[0], c[1], c[2]);
            MeasuredValue {
                formula: c[0],
                a: c[1],
                b: c[2],
                value: s.map(|(v, _)| v),
                unit: s.map(|(_, u)| u).unwrap_or(""),
                label: gen4_label(group, i),
            }
        })
        .collect();
    Group {
        group,
        values,
        raw_hex: hex(rec),
        error: None,
    }
}

/// A stored trouble code. VAG controllers report their 5-digit fault number
/// as the 16-bit code; turning it into text is not implemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Dtc {
    pub code: u16,
    /// The code as the 5-digit decimal number scan tools print.
    pub vag_number: u32,
    pub status: u8,
}

/// Options for [`read_gen4`].
#[derive(Debug, Clone)]
pub struct Gen4ReadOptions {
    /// Measuring groups to read (default [`DEFAULT_GROUPS`]).
    pub groups: Vec<u8>,
    /// Read stored trouble codes.
    pub read_dtcs: bool,
    /// Per-request timeout.
    pub timeout_ms: u64,
}

impl Default for Gen4ReadOptions {
    fn default() -> Self {
        Self {
            groups: DEFAULT_GROUPS.to_vec(),
            read_dtcs: true,
            timeout_ms: 2000,
        }
    }
}

/// Everything one read-only pass collected. Each part records its own error so
/// a unit that refuses one request still yields the rest.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Gen4Report {
    /// `None` = session opened; `Some(err)` = it failed (the read continues).
    pub session_error: Option<String>,
    pub identity: Option<Identity>,
    pub identity_error: Option<String>,
    pub dtcs: Option<Vec<Dtc>>,
    pub dtc_error: Option<String>,
    pub groups: Vec<Group>,
}

impl Gen4Report {
    /// True when the unit answered at least the identification.
    pub fn responded(&self) -> bool {
        self.identity.is_some()
    }
}

/// Run one read-only pass over `transport`, which must already be a VW TP2.0
/// channel to logical address [`TP20_LOGICAL_ADDRESS`] (or a simulator).
pub fn read_gen4(transport: &mut dyn DiagTransport, opts: &Gen4ReadOptions) -> Gen4Report {
    let mut kwp = Kwp2000Client::new(transport);
    kwp.timeout_ms = opts.timeout_ms;
    let mut r = Gen4Report {
        session_error: kwp.start_session(KWP_SESSION).err().map(|e| e.to_string()),
        ..Default::default()
    };
    match kwp.read_ecu_id(ID_OPTION) {
        // The client strips the SID; the record starts after the echoed option.
        Ok(resp) if resp.first() == Some(&ID_OPTION) => {
            r.identity = Some(parse_identity(&resp[1..]));
        }
        Ok(resp) => {
            r.identity_error = Some(format!("unexpected identification reply {}", hex(&resp)))
        }
        Err(e) => r.identity_error = Some(e.to_string()),
    }
    if opts.read_dtcs {
        match kwp.read_dtcs_by_status(0x02, 0xFF00) {
            Ok(list) => {
                r.dtcs = Some(
                    list.into_iter()
                        .map(|d| Dtc {
                            code: d.code,
                            vag_number: d.code as u32,
                            status: d.status,
                        })
                        .collect(),
                )
            }
            Err(e) => r.dtc_error = Some(e.to_string()),
        }
    }
    for &g in &opts.groups {
        r.groups.push(match kwp.read_local_id(g) {
            Ok(rec) => decode_group(g, &rec),
            Err(e) => Group {
                group: g,
                values: Vec::new(),
                raw_hex: String::new(),
                error: Some(e.to_string()),
            },
        });
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_parts() {
        assert_eq!(classify_part("0AY 907 554 C"), GenerationHint::Gen4);
        assert_eq!(classify_part("0ay907554h"), GenerationHint::Gen4);
        assert_eq!(classify_part("0AY907554F"), GenerationHint::Gen5Pq);
        assert_eq!(classify_part("0BS907554B"), GenerationHint::Gen4);
        assert_eq!(classify_part("1K0907554B"), GenerationHint::Gen2);
        assert_eq!(classify_part("0CQ907554D"), GenerationHint::Gen5Mqb);
        assert_eq!(classify_part("03G906021"), GenerationHint::Unknown);
    }

    #[test]
    fn identity_layout() {
        let mut rec = b"0AY907554C  0041".to_vec();
        rec.extend_from_slice(&[3, 0, 0, 5, 0, 0, 0, 0, 0, 0]);
        rec.extend_from_slice(b"Haldex 4Motion      ");
        let id = parse_identity(&rec);
        assert_eq!(id.part_number, "0AY907554C");
        assert_eq!(id.sw_version, "0041");
        assert_eq!(id.component, "Haldex 4Motion");
        assert_eq!(id.extra_hex, "03000005000000000000");
        assert_eq!(id.generation, GenerationHint::Gen4);
        // Short record: keeps what it has, no panic.
        let short = parse_identity(b"0AY90");
        assert_eq!(short.part_number, "0AY90");
        assert!(short.sw_version.is_empty() && short.component.is_empty());
    }

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// KWP replies reassembled from the public TP2.0 captures of a real
    /// `0AY 907 554 C` in Forbes-Automotive/OpenHaldex-C6 (MIT),
    /// `documents/vehicle-logs/vw-pq/0AY 554C - ...csv`. The second file's name
    /// records the scan-tool readings: oil pressure 27/5/4 bar, estimated torque
    /// 2032/464/384 Nm, valve duty 40/21/18 %, valve current 1.910/0.860/0.81 A.
    #[test]
    fn decodes_public_real_capture() {
        let id = unhex(concat!(
            "304159393037353534432020303034310300000500000000000048616c",
            "64657820344d6f74696f6e202020202020"
        ));
        let id = parse_identity(&id);
        assert_eq!(id.part_number, "0AY907554C");
        assert_eq!(id.sw_version, "0041");
        assert_eq!(id.component, "Haldex 4Motion");
        assert_eq!(id.generation, GenerationHint::Gen4);

        let expect = [
            ("0e1bc85ea0ff216428180abf", [27.0, 2032.0, 40.0, 1.910]),
            ("0e05c85ea09d216415180a56", [5.0, 464.0, 21.0, 0.860]),
            ("0e04c85ea098216412180a51", [4.0, 384.0, 18.0, 0.810]),
        ];
        for (rec, want) in expect {
            let g = decode_group(3, &unhex(rec));
            let got: Vec<f64> = g.values.iter().map(|v| v.value.unwrap()).collect();
            for (g, w) in got.iter().zip(want) {
                assert!((g - w).abs() < 1e-6, "{rec}: {got:?} vs {want:?}");
            }
        }
        // Group 1 from the first capture: oil 23 °C, plate 41 °C, then an
        // unknown formula 0x25 cell kept raw.
        let g = decode_group(1, &unhex("1a00171a0029066402250000"));
        assert_eq!(g.values[0].value, Some(23.0));
        assert_eq!(g.values[1].value, Some(41.0));
        assert_eq!(g.values[3].value, None);
    }

    #[test]
    fn formulas_match_documented_examples() {
        // Examples given next to each formula in OpenHaldex-C6.
        assert_eq!(scale(0x1A, 0, 24), Some((24.0, "°C")));
        let (v, u) = scale(0x18, 10, 191).unwrap();
        assert!((v - 1.910).abs() < 1e-9 && u == "A");
        let (v, _) = scale(0x21, 100, 40).unwrap();
        assert!((v - 40.0).abs() < 1e-9);
        let (v, _) = scale(0x0E, 27, 200).unwrap();
        assert!((v - 27.0).abs() < 1e-9);
        let (v, _) = scale(0x5E, 160, 255).unwrap();
        assert!((v - 2032.0).abs() < 1e-9);
        assert_eq!(scale(0x01, 1, 2), None);
        let g = decode_group(3, &[0x0E, 27, 200, 0x5E, 160, 255, 0x77, 1, 2, 0x21]);
        assert_eq!(g.values.len(), 3);
        assert_eq!(g.values[0].label, Some("oil pressure"));
        assert_eq!(g.values[2].value, None);
        assert_eq!(g.raw_hex, "0E1BC85EA0FF77010221");
    }
}
