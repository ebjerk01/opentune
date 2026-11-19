//! **VAG DQ250 02E** (6-speed wet dual-clutch, Temic mechatronic) — a
//! **read-only** diagnostics engine.
//!
//! PQ-era VAG modules are diagnosed with **KWP2000 over a VW TP2.0 channel**;
//! the gearbox sits at TP2.0 logical address **`0x02`**. This crate mirrors
//! `opentune-haldex`'s Gen4 engine for that unit:
//!
//! * [`Dq250Reader::identify`] — `0x1A` readEcuIdentification: the VAG
//!   identification record (`0x9B`: part number, software version, component
//!   text) plus ISO 14230-3 options (hardware number `0x91`, system name
//!   `0x97`). Every option is optional; a refused one is recorded, not fatal.
//! * [`Dq250Reader::dtcs`] — `0x18` readDTCByStatus: the VAG 2-byte fault
//!   number and the KWP2000 status byte ([`DtcStatus`]).
//! * [`Dq250Reader::group`] — `0x21` readDataByLocalIdentifier: a measuring
//!   group as numbered 3-byte cells, scaled with the shared VAG formula table
//!   ([`opentune_core::protocol::vagmeas`]).
//!
//! **What each DSG measuring group means is deliberately not encoded.** No
//! public, licence-compatible source and no capture from a real 02E is
//! available yet, so every cell is reported as numbered, formula-decoded (or
//! raw) and [`MeaningState::Unconfirmed`]. Meanings are data: a
//! `kwp_measuring_block` entry in the controller definition's `live[]`
//! (`ctrldefs/vag-dq250-02e.json`) is attached to its cell by
//! [`GroupLabels::from_ctrldef`] — add them there once a capture confirms them.
//!
//! The reader only ever sends `0x10` (session start), `0x1A`, `0x18` and
//! `0x21`. There is no clear-codes, coding, adaptation, basic-setting,
//! security-access, routine or flash service in this crate. The TP2.0 channel
//! (setup, keep-alive, teardown) is the core's
//! [`Tp20Channel`](opentune_core::protocol::tp20channel::Tp20Channel), opened
//! by the raw-CAN drivers with [`dsg_connect_config`].
//!
//! Validation state: **synthetic-tested** (scripted responder over the core
//! TP2.0 simulator). Not yet tried on a car. See `docs/DSG.md`.
#![forbid(unsafe_code)]

pub mod labels;
pub mod sim;

pub use labels::{CellMeaning, GroupLabels};

use opentune_core::protocol::diag::DiagTransport;
use opentune_core::protocol::kwp2000::{sid, Kwp2000Client};
use opentune_core::protocol::tp20channel::{transport_kind, CanTransportKind};
use opentune_core::protocol::vagmeas;
use opentune_core::{ConnectConfig, DiagLayer, Protocol};
use serde::Serialize;

/// TP2.0 logical address of the gearbox control unit on PQ-era VAG cars.
pub const TP20_LOGICAL_ADDRESS: u8 = 0x02;
/// CAN id the tester broadcasts TP2.0 channel setup on.
pub const TP20_SETUP_ID: u32 = 0x200;
/// CAN id the unit answers the channel setup on (`0x200 + 0x02`).
pub const TP20_SETUP_RESPONSE_ID: u32 = TP20_SETUP_ID + TP20_LOGICAL_ADDRESS as u32;
/// CAN id the tester asks the unit to transmit on (the core default).
pub const TP20_TESTER_RX_ID: u32 = 0x300;
/// KWP2000 diagnostic session started before the reads: the one the Haldex
/// Gen4 path uses (seen in a public PQ capture). Not confirmed on an 02E.
pub const KWP_SESSION: u8 = 0x89;
/// The VAG identification record option of `0x1A`.
pub const ID_VAG_RECORD: u8 = 0x9B;
/// ISO 14230-3 `vehicleManufacturerECUHardwareNumber`.
pub const ID_HW_NUMBER: u8 = 0x91;
/// ISO 14230-3 system name.
pub const ID_SYSTEM_NAME: u8 = 0x97;
/// Identification options read by default.
pub const DEFAULT_ID_OPTIONS: [u8; 3] = [ID_VAG_RECORD, ID_HW_NUMBER, ID_SYSTEM_NAME];
/// The only KWP2000 services this crate sends.
pub const READ_SERVICES: [u8; 4] = [
    sid::START_DIAG_SESSION,
    sid::READ_ECU_ID,
    sid::READ_DTC_BY_STATUS,
    sid::READ_DATA_BY_LOCAL_ID,
];
/// Driver ids that can open a VW TP2.0 channel (raw-CAN adapters).
pub const TP20_DRIVERS: &[&str] = &["socketcan", "slcan", "gsusb"];

/// Name of an identification option, or `None` when OpenTune has no source.
pub fn id_option_name(option: u8) -> Option<&'static str> {
    match option {
        ID_VAG_RECORD => Some("VAG identification record"),
        ID_HW_NUMBER => Some("hardware number (ISO 14230-3)"),
        ID_SYSTEM_NAME => Some("system name (ISO 14230-3)"),
        _ => None,
    }
}

/// Why a real read is refused with `driver`, or `None` when it can open TP2.0.
pub fn tp20_refusal(driver: &str) -> Option<String> {
    if TP20_DRIVERS.contains(&driver) {
        return None;
    }
    Some(format!(
        "driver '{driver}' cannot open a VW TP2.0 channel, which the DQ250 02E read needs \
         (KWP2000 over TP2.0 at logical address 0x02). Use a raw-CAN adapter: socketcan, \
         slcan (CANable) or gsusb (candleLight). The ELM327 drivers refuse TP2.0."
    ))
}

/// The connection for a read over a raw-CAN driver: `base` (port, bitrate,
/// extra options) with KWP2000 over a TP2.0 channel to
/// [`TP20_LOGICAL_ADDRESS`]. Options the caller set (`tp20_rx_id`,
/// `tp20_keepalive_ms`, ...) are kept; `transport=isotp` is refused.
pub fn dsg_connect_config(mut base: ConnectConfig) -> Result<ConnectConfig, String> {
    if matches!(transport_kind(&base), Ok(CanTransportKind::IsoTp))
        && base.options.contains_key("transport")
    {
        return Err(
            "this read uses VW TP2.0, not ISO-TP: drop transport=isotp (or set transport=tp20)"
                .into(),
        );
    }
    base.protocol = Protocol::Vwtp20;
    base.diag_layer = DiagLayer::Kwp2000;
    base.tx_id = TP20_LOGICAL_ADDRESS as u32;
    base.rx_id = TP20_TESTER_RX_ID;
    let o = &mut base.options;
    o.entry("transport".into()).or_insert_with(|| "tp20".into());
    o.entry("tp20_addr".into())
        .or_insert_with(|| format!("0x{TP20_LOGICAL_ADDRESS:02X}"));
    Ok(base)
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

/// Gearbox family suggested by a part number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FamilyHint {
    /// Gearbox code `02E` (DQ250, PQ).
    Dq250_02e,
    /// Gearbox code `0D9` (DQ250-MQB, UDS; not this crate's target).
    Dq250Mqb,
    /// Anything else.
    Unknown,
}

/// Classify a part number by its gearbox code (spaces ignored).
pub fn classify_part(part: &str) -> FamilyHint {
    let p: String = part
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_uppercase();
    if p.starts_with("02E") {
        FamilyHint::Dq250_02e
    } else if p.starts_with("0D9") {
        FamilyHint::Dq250Mqb
    } else {
        FamilyHint::Unknown
    }
}

/// One identification option the unit answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IdRecord {
    pub option: u8,
    pub name: Option<&'static str>,
    /// Printable ASCII of the record (non-printables as spaces, trimmed).
    pub ascii: String,
    pub hex: String,
}

/// One identification option that failed (refused, absent, malformed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IdError {
    pub option: u8,
    pub error: String,
}

/// The decoded identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Identity {
    /// VAG part number from the `0x9B` record (empty if that option failed).
    pub part_number: String,
    /// Software version (4 characters) from `0x9B`.
    pub sw_version: String,
    /// Component / system text from `0x9B`.
    pub component: String,
    /// The 10 binary bytes between version and component text, hex. Other
    /// VAG units carry coding/workshop data here, but the layout is not
    /// publicly confirmed, so it is not decoded.
    pub extra_hex: String,
    /// Hardware number (`0x91`), when answered.
    pub hardware_number: Option<String>,
    /// System name (`0x97`), when answered.
    pub system_name: Option<String>,
    /// Family suggested by the part number.
    pub family: FamilyHint,
}

/// Parse the `0x9B` record (the bytes after `5A 9B`): part number (12 ASCII),
/// software version (4 ASCII), 10 binary bytes, component text — the VAG
/// layout seen in a public PQ capture (see `opentune-haldex`). A shorter
/// record keeps whatever fields it has.
pub fn parse_vag_record(rec: &[u8]) -> Identity {
    let part_number = ascii(rec.get(..12.min(rec.len())).unwrap_or_default());
    Identity {
        family: classify_part(&part_number),
        part_number,
        sw_version: ascii(rec.get(12..16.min(rec.len().max(12))).unwrap_or_default()),
        extra_hex: hex(rec.get(16..26.min(rec.len().max(16))).unwrap_or_default()),
        component: ascii(rec.get(26..).unwrap_or_default()),
        hardware_number: None,
        system_name: None,
    }
}

/// Result of [`Dq250Reader::identify`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct IdentifyReport {
    /// Built when at least one known option answered.
    pub identity: Option<Identity>,
    /// Every option that answered, raw.
    pub records: Vec<IdRecord>,
    /// Every option that did not.
    pub errors: Vec<IdError>,
}

impl IdentifyReport {
    /// True when the unit answered at least one identification option.
    pub fn responded(&self) -> bool {
        !self.records.is_empty()
    }
}

/// KWP2000 storage state of a code (status byte bits 6..5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StorageState {
    /// `00`: no code stored.
    NotStored,
    /// `01`: stored, not present at the time of the request.
    StoredNotPresent,
    /// `10`: maturing / pending.
    Pending,
    /// `11`: present and stored.
    Present,
}

/// The KWP2000 DTC status byte, decoded as far as public sources go (the bit
/// layout rnd-ash/ecu_diagnostics uses for KWP2000; community confidence,
/// not confirmed for the 02E).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DtcStatus {
    /// Bits 6..5.
    pub storage: StorageState,
    /// Bit 7: the code requests the warning lamp.
    pub warning_lamp: bool,
    /// Bit 4: the readiness flag.
    pub readiness: bool,
    /// Bits 3..0, not decoded (VAG fault-symptom detail; no public table).
    pub low_bits: u8,
}

impl DtcStatus {
    pub fn from_byte(s: u8) -> Self {
        Self {
            storage: match (s >> 5) & 0x03 {
                0 => StorageState::NotStored,
                1 => StorageState::StoredNotPresent,
                2 => StorageState::Pending,
                _ => StorageState::Present,
            },
            warning_lamp: s & 0x80 != 0,
            readiness: s & 0x10 != 0,
            low_bits: s & 0x0F,
        }
    }
}

/// A stored trouble code: VAG modules report their 5-digit fault number as
/// the 16-bit code. Turning it into text is not implemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Dtc {
    pub code: u16,
    /// The code as the 5-digit decimal number scan tools print.
    pub vag_number: u32,
    pub status_byte: u8,
    pub status: DtcStatus,
}

/// Whether a cell's meaning is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MeaningState {
    /// No source says what this cell is.
    Unconfirmed,
    /// A controller definition names it (see the meaning's `source`).
    FromDefinition,
}

/// One cell of a measuring group.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GroupCell {
    /// 0-based position in the group (the ctrldef `cell` index).
    pub cell: u8,
    pub formula: u8,
    pub a: u8,
    pub b: u8,
    /// Scaled with the shared VAG formula table; `None` = raw only.
    pub value: Option<f64>,
    pub unit: &'static str,
    pub state: MeaningState,
    pub meaning: Option<CellMeaning>,
}

/// A measuring-group read.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Group {
    pub group: u8,
    pub cells: Vec<GroupCell>,
    /// The record after the echoed group, hex (includes a partial tail cell).
    pub raw_hex: String,
    /// Set when the read failed (the unit may not implement the group).
    pub error: Option<String>,
    /// What the decode could not account for (a partial trailing cell, cells
    /// past the 256 a cell number can name, a re-read after a lost session).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// Most cells a group can have: the cell number (and the controller
/// definition's `cell`) is a `u8`.
pub const MAX_CELLS: usize = 256;

/// Decode a measuring-group record (after the echoed group number).
pub fn decode_group(group: u8, rec: &[u8], labels: &GroupLabels) -> Group {
    let mut notes = Vec::new();
    let all = vagmeas::cells(rec);
    if all.len() > MAX_CELLS {
        notes.push(format!(
            "record has {} cells; only the first {MAX_CELLS} are decoded (see raw_hex)",
            all.len()
        ));
    }
    if rec.len() % 3 != 0 {
        notes.push(format!(
            "{} trailing byte(s) do not form a whole cell (kept in raw_hex)",
            rec.len() % 3
        ));
    }
    let cells = all
        .into_iter()
        .take(MAX_CELLS)
        .enumerate()
        .map(|(i, c)| {
            let cell = i as u8;
            let meaning = labels.get(group, cell).cloned();
            GroupCell {
                cell,
                formula: c.formula,
                a: c.a,
                b: c.b,
                value: c.value,
                unit: c.unit,
                state: if meaning.is_some() {
                    MeaningState::FromDefinition
                } else {
                    MeaningState::Unconfirmed
                },
                meaning,
            }
        })
        .collect();
    Group {
        group,
        cells,
        raw_hex: hex(rec),
        error: None,
        notes,
    }
}

/// A read-only KWP2000 session on an open channel to the gearbox.
///
/// It wraps the core client but exposes only the read services in
/// [`READ_SERVICES`].
pub struct Dq250Reader<'a> {
    kwp: Kwp2000Client<'a>,
    /// `None` = session started (or not requested); `Some(err)` = it failed.
    /// The reads still run: some units answer them in the default session.
    pub session_error: Option<String>,
}

impl<'a> Dq250Reader<'a> {
    /// Start `session` (normally [`KWP_SESSION`]; `None` skips it) on
    /// `transport`, which must already be a TP2.0 channel to the gearbox (or
    /// a simulator).
    pub fn start(
        transport: &'a mut dyn DiagTransport,
        session: Option<u8>,
        timeout_ms: u64,
    ) -> Self {
        let mut kwp = Kwp2000Client::new(transport);
        kwp.timeout_ms = timeout_ms;
        let session_error = session.and_then(|s| kwp.start_session(s).err().map(|e| e.to_string()));
        Self { kwp, session_error }
    }

    /// Read identification `options` (each in `0x80..=0x9F`, the ISO
    /// identification range). Missing or refused options are recorded.
    pub fn identify(&mut self, options: &[u8]) -> IdentifyReport {
        let mut r = IdentifyReport::default();
        let mut id: Option<Identity> = None;
        let mut hw = None;
        let mut name = None;
        for &opt in options {
            if !(0x80..=0x9F).contains(&opt) {
                r.errors.push(IdError {
                    option: opt,
                    error: "not an identification option (0x80..=0x9F); not sent".into(),
                });
                continue;
            }
            let rec = match self.kwp.read_ecu_id(opt) {
                // The client strips the SID; the record follows the echoed option.
                Ok(resp) if resp.first() == Some(&opt) => resp[1..].to_vec(),
                Ok(resp) => {
                    r.errors.push(IdError {
                        option: opt,
                        error: format!("unexpected identification reply {}", hex(&resp)),
                    });
                    continue;
                }
                Err(e) => {
                    r.errors.push(IdError {
                        option: opt,
                        error: e.to_string(),
                    });
                    continue;
                }
            };
            match opt {
                ID_VAG_RECORD => id = Some(parse_vag_record(&rec)),
                ID_HW_NUMBER => hw = Some(ascii(&rec)),
                ID_SYSTEM_NAME => name = Some(ascii(&rec)),
                _ => {}
            }
            r.records.push(IdRecord {
                option: opt,
                name: id_option_name(opt),
                ascii: ascii(&rec),
                hex: hex(&rec),
            });
        }
        if id.is_none() && (hw.is_some() || name.is_some()) {
            id = Some(parse_vag_record(&[]));
        }
        if let Some(i) = id.as_mut() {
            i.hardware_number = hw;
            i.system_name = name;
        }
        r.identity = id;
        r
    }

    /// Stored trouble codes (`18 02 FF 00`: all stored codes, all groups).
    pub fn dtcs(&mut self) -> Result<Vec<Dtc>, String> {
        self.kwp
            .read_dtcs_by_status(0x02, 0xFF00)
            .map(|list| {
                list.into_iter()
                    .map(|d| Dtc {
                        code: d.code,
                        vag_number: d.code as u32,
                        status_byte: d.status,
                        status: DtcStatus::from_byte(d.status),
                    })
                    .collect()
            })
            .map_err(|e| e.to_string())
    }

    /// One measuring group; a refusal is recorded in [`Group::error`].
    pub fn group(&mut self, group: u8, labels: &GroupLabels) -> Group {
        match self.kwp.read_local_id(group) {
            Ok(rec) => decode_group(group, &rec, labels),
            Err(e) => Group {
                group,
                cells: Vec::new(),
                raw_hex: String::new(),
                error: Some(e.to_string()),
                notes: Vec::new(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_config_targets_the_gearbox() {
        assert!(tp20_refusal("gsusb").is_none());
        assert!(tp20_refusal("elm327obd").unwrap().contains("0x02"));
        let mut base = ConnectConfig::default();
        base.options
            .insert("tp20_keepalive_ms".into(), "250".into());
        let c = dsg_connect_config(base).unwrap();
        assert_eq!(c.protocol, Protocol::Vwtp20);
        assert_eq!(c.options["tp20_addr"], "0x02");
        let o = opentune_core::protocol::tp20channel::Tp20Options::from_config(&c)
            .unwrap()
            .unwrap();
        assert_eq!((o.addr, o.rx_id, o.keepalive_ms), (0x02, 0x300, 250));
        let mut iso = ConnectConfig::default();
        iso.options.insert("transport".into(), "isotp".into());
        assert!(dsg_connect_config(iso).is_err());
    }

    #[test]
    fn vag_record_layout_and_short_records() {
        let mut rec = b"02E000000X  0000".to_vec();
        rec.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        rec.extend_from_slice(b"SIMULATED DSG       ");
        let id = parse_vag_record(&rec);
        assert_eq!(id.part_number, "02E000000X");
        assert_eq!(id.sw_version, "0000");
        assert_eq!(id.extra_hex, "0102030405060708090A");
        assert_eq!(id.component, "SIMULATED DSG");
        assert_eq!(id.family, FamilyHint::Dq250_02e);
        let s = parse_vag_record(b"02E0000");
        assert_eq!(s.part_number, "02E0000");
        assert!(s.sw_version.is_empty() && s.extra_hex.is_empty() && s.component.is_empty());
        let s = parse_vag_record(b"0D9000000X  00");
        assert_eq!(
            (s.sw_version.as_str(), s.family),
            ("00", FamilyHint::Dq250Mqb)
        );
        assert_eq!(parse_vag_record(&[]).family, FamilyHint::Unknown);
    }

    #[test]
    fn dtc_status_bits() {
        let s = DtcStatus::from_byte(0xE3);
        assert_eq!(s.storage, StorageState::Present);
        assert!(s.warning_lamp && !s.readiness);
        assert_eq!(s.low_bits, 3);
        let s = DtcStatus::from_byte(0x30);
        assert_eq!(s.storage, StorageState::StoredNotPresent);
        assert!(!s.warning_lamp && s.readiness);
        assert_eq!(DtcStatus::from_byte(0x40).storage, StorageState::Pending);
        assert_eq!(DtcStatus::from_byte(0x0F).storage, StorageState::NotStored);
    }

    #[test]
    fn groups_are_unconfirmed_without_a_meaning() {
        let g = decode_group(7, &[0x1A, 0, 50, 0x77, 1, 2, 0x21], &GroupLabels::default());
        assert_eq!(g.cells.len(), 2);
        assert_eq!(g.cells[0].value, Some(50.0));
        assert_eq!(g.cells[1].value, None);
        assert!(g
            .cells
            .iter()
            .all(|c| c.state == MeaningState::Unconfirmed && c.meaning.is_none()));
        assert_eq!(g.raw_hex, "1A003277010221");

        let mut labels = GroupLabels::default();
        labels.insert(
            7,
            1,
            CellMeaning {
                name: "x".into(),
                unit: String::new(),
                notes: String::new(),
                source: "test".into(),
            },
        );
        let g = decode_group(7, &[0x1A, 0, 50, 0x77, 1, 2], &labels);
        assert_eq!(g.cells[0].state, MeaningState::Unconfirmed);
        assert_eq!(g.cells[1].state, MeaningState::FromDefinition);
        assert_eq!(g.cells[1].meaning.as_ref().unwrap().name, "x");
        assert!(g.notes.is_empty());
    }

    #[test]
    fn oversized_and_ragged_records_are_noted_not_renumbered() {
        // Regression: the cell number was `i as u8`, so cell 256 of an
        // oversized record became cell 0 again and took cell 0's meaning.
        let mut labels = GroupLabels::default();
        labels.insert(
            3,
            0,
            CellMeaning {
                name: "first".into(),
                unit: String::new(),
                notes: String::new(),
                source: "test".into(),
            },
        );
        let rec: Vec<u8> = (0..(MAX_CELLS + 2) * 3 + 1).map(|i| i as u8).collect();
        let g = decode_group(3, &rec, &labels);
        assert_eq!(g.cells.len(), MAX_CELLS);
        assert_eq!(g.cells.last().unwrap().cell, 255);
        assert_eq!(
            g.cells.iter().filter(|c| c.meaning.is_some()).count(),
            1,
            "only the real cell 0 carries its meaning"
        );
        assert_eq!(g.notes.len(), 2, "{:?}", g.notes);
        assert!(g.notes[0].contains("258 cells") && g.notes[1].contains("1 trailing"));
        assert_eq!(g.raw_hex.len(), rec.len() * 2);
    }
}
