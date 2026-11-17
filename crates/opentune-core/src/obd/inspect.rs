//! OBD-II **inspection data** (SAE J1979 / ISO 15031-5): everything a
//! roadside / periodic emissions check reads, beside the mode-01 live values.
//!
//! - **Readiness** — mode `01` PID `01` (MIL, stored-DTC count, ignition type,
//!   continuous + non-continuous monitors, each supported / complete) and PID
//!   `41` (the same monitors for the current drive cycle): [`read_readiness`].
//! - **Freeze frame** — mode `02`: PID `02` names the DTC that stored the frame,
//!   then every supported PID of that frame decodes through the mode-01
//!   [`CHANNELS`](super::CHANNELS) table: [`read_freeze_frame`].
//! - **Vehicle information** — mode `09`: supported-infotype bitmap, VIN
//!   (`02`), calibration IDs (`04`, several), calibration verification
//!   numbers (`06`, several), ECU name (`0A`): [`read_vehicle_info`]. Both the
//!   ISO 15765-4 (CAN) single-message form and the legacy multi-message form
//!   (`49 it seq d d d d`, one message per 4 bytes) are decoded.
//! - **DTCs** — mode `03` (stored / confirmed), `07` (pending) and `0A`
//!   (permanent), decoded with [`opentune_dtc`]: [`read_dtcs`].
//!
//! A [`RawDiag`] talks to **one** module (the adapter is physically addressed),
//! so "several ECUs answering" is handled one level up: the caller runs
//! [`inspect`] once per address of [`OBD_ECU_IDS`] and keeps the modules whose
//! [`Inspection::responded`] is set.
//!
//! All decoders are public and pure, so they are testable from byte vectors;
//! [`super::sim::SimEcu`] answers every mode here for offline use.
use super::{channels_for_pid, hex, nrc_text, parse_support_bitmap, ObdValue};
use crate::diag::RawDiag;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// The ISO 15765-4 11-bit physical request / response CAN ids of the eight
/// possible emissions-related ECUs (`7E0/7E8` … `7E7/7EF`).
pub const OBD_ECU_IDS: [(u32, u32); 8] = [
    (0x7E0, 0x7E8),
    (0x7E1, 0x7E9),
    (0x7E2, 0x7EA),
    (0x7E3, 0x7EB),
    (0x7E4, 0x7EC),
    (0x7E5, 0x7ED),
    (0x7E6, 0x7EE),
    (0x7E7, 0x7EF),
];

/// Conventional role of the OBD module at request id `tx` (ISO 15765-4
/// assigns `7E0` to the engine and `7E1` usually to the transmission; the rest
/// are vehicle-specific).
pub fn obd_ecu_label(tx: u32) -> &'static str {
    match tx {
        0x7E0 => "engine",
        0x7E1 => "transmission",
        _ => "other",
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Send `req`; expect the positive id `req[0] + 0x40` followed by an echo of
/// `req[1..1 + echo]`. Returns the bytes after the echo.
fn exchange<D: RawDiag + ?Sized>(diag: &mut D, req: &[u8], echo: usize) -> Result<Vec<u8>, String> {
    let resp = diag.request(req)?;
    if resp.first() == Some(&0x7F) {
        return Err(nrc_text(&resp));
    }
    let pos = req[0].wrapping_add(0x40);
    if resp.first() != Some(&pos) || resp.len() < 1 + echo || resp[1..1 + echo] != req[1..1 + echo]
    {
        return Err(format!("unexpected response {}", hex(&resp)));
    }
    Ok(resp[1 + echo..].to_vec())
}

/// Walk `00`, `20`, `40`, … support bitmaps through `fetch(base)` (which
/// returns the 4 bitmap bytes), following each range's "next range" bit.
/// Range markers are not included; only a failing `00` is an error.
fn walk_bitmaps(
    mut fetch: impl FnMut(u8) -> Result<Vec<u8>, String>,
) -> Result<BTreeSet<u8>, String> {
    let mut set = BTreeSet::new();
    let mut base: u8 = 0x00;
    loop {
        match fetch(base) {
            Ok(d) => set.extend(parse_support_bitmap(base, &d)),
            Err(e) if base == 0 => return Err(e),
            Err(_) => break,
        }
        let next = base.wrapping_add(0x20);
        if base == 0xE0 || !set.contains(&next) {
            break;
        }
        base = next;
    }
    set.retain(|p| p % 0x20 != 0);
    Ok(set)
}

fn need(data: &[u8], n: usize, what: &str) -> Result<(), String> {
    if data.len() < n {
        Err(format!("{what}: {} byte(s), need {n}", data.len()))
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// DTCs: modes 03 / 07 / 0A
// ---------------------------------------------------------------------------

/// One decoded diagnostic trouble code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DtcEntry {
    /// Canonical 5-character code, e.g. `P0401`.
    pub code: String,
    /// Description from the bundled SAE J2012 table, or a generic
    /// "manufacturer-specific / unknown" label.
    pub description: String,
    /// `powertrain`, `chassis`, `body` or `network`.
    pub system: &'static str,
}

/// Decode the two ISO 15031-6 DTC bytes (`P0401` = `04 01`).
pub fn dtc_from_bytes(hi: u8, lo: u8) -> DtcEntry {
    let code = opentune_dtc::format_code(hi, lo, 0);
    let (description, system) = match opentune_dtc::lookup(&code) {
        Some(d) => (d.description, d.system),
        None => (
            "Manufacturer-specific / unknown code".to_string(),
            opentune_dtc::System::Powertrain,
        ),
    };
    let system = match system {
        opentune_dtc::System::Powertrain => "powertrain",
        opentune_dtc::System::Chassis => "chassis",
        opentune_dtc::System::Body => "body",
        opentune_dtc::System::Network => "network",
    };
    DtcEntry {
        code,
        description,
        system,
    }
}

/// Which DTC list to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DtcMode {
    /// Mode `03`: stored (confirmed) emissions-related DTCs — these light the MIL.
    Stored,
    /// Mode `07`: pending DTCs (detected in the current or last drive cycle,
    /// not yet confirmed).
    Pending,
    /// Mode `0A`: permanent DTCs (cannot be cleared by a scan tool; the ECU
    /// erases them itself after the monitor passes).
    Permanent,
}

impl DtcMode {
    /// Every mode, in display order.
    pub const ALL: [DtcMode; 3] = [DtcMode::Stored, DtcMode::Pending, DtcMode::Permanent];

    /// The request service id (`03`, `07`, `0A`).
    pub fn sid(self) -> u8 {
        match self {
            DtcMode::Stored => 0x03,
            DtcMode::Pending => 0x07,
            DtcMode::Permanent => 0x0A,
        }
    }

    /// JSON key (`stored`, `pending`, `permanent`).
    pub fn key(self) -> &'static str {
        match self {
            DtcMode::Stored => "stored",
            DtcMode::Pending => "pending",
            DtcMode::Permanent => "permanent",
        }
    }
}

/// Decode a mode `03` / `07` / `0A` response (`resp` includes the positive id
/// `sid + 0x40`). Handles:
///
/// - ISO 15765-4 (CAN): `43 n hi lo hi lo …` with a DTC count byte;
/// - legacy multi-message: several 7-byte messages `43 d1 d2 d3 d4 d5 d6`
///   (three DTCs each, zero-padded), concatenated;
/// - otherwise the lenient form the ELM327 driver uses (odd body = count byte).
///
/// All-zero pairs (padding) are dropped.
pub fn parse_dtc_response(sid: u8, resp: &[u8]) -> Result<Vec<DtcEntry>, String> {
    let pos = sid.wrapping_add(0x40);
    match resp.first() {
        Some(0x7F) => return Err(nrc_text(resp)),
        Some(p) if *p == pos => {}
        _ => return Err(format!("unexpected response {}", hex(resp))),
    }
    let pairs: Vec<u8> = if resp.len() >= 2 && resp[1] as usize * 2 == resp.len() - 2 {
        resp[2..].to_vec()
    } else if resp.len() % 7 == 0 && resp.chunks(7).all(|c| c[0] == pos) {
        resp.chunks(7)
            .flat_map(|c| c[1..].iter().copied())
            .collect()
    } else {
        let body = &resp[1..];
        if body.len() % 2 == 1 {
            body[1..].to_vec()
        } else {
            body.to_vec()
        }
    };
    Ok(pairs
        .chunks_exact(2)
        .filter(|p| p[0] != 0 || p[1] != 0)
        .map(|p| dtc_from_bytes(p[0], p[1]))
        .collect())
}

/// Read one DTC list.
pub fn read_dtcs<D: RawDiag + ?Sized>(
    diag: &mut D,
    mode: DtcMode,
) -> Result<Vec<DtcEntry>, String> {
    let resp = diag.request(&[mode.sid()])?;
    parse_dtc_response(mode.sid(), &resp)
}

/// Stored, pending and permanent DTCs of one module.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DtcReport {
    pub stored: Vec<DtcEntry>,
    pub pending: Vec<DtcEntry>,
    pub permanent: Vec<DtcEntry>,
    /// Why a list could not be read (key `stored` / `pending` / `permanent`);
    /// that list is then empty. Older ECUs typically lack mode `0A`.
    pub errors: BTreeMap<String, String>,
}

/// Read all three DTC lists; a failing mode is recorded, not fatal.
pub fn read_all_dtcs<D: RawDiag + ?Sized>(diag: &mut D) -> DtcReport {
    let mut r = DtcReport::default();
    for mode in DtcMode::ALL {
        match read_dtcs(diag, mode) {
            Ok(list) => match mode {
                DtcMode::Stored => r.stored = list,
                DtcMode::Pending => r.pending = list,
                DtcMode::Permanent => r.permanent = list,
            },
            Err(e) => {
                r.errors.insert(mode.key().into(), e);
            }
        }
    }
    r
}

// ---------------------------------------------------------------------------
// Readiness: mode 01 PID 01 / PID 41
// ---------------------------------------------------------------------------

/// Engine type reported in PID `01` byte B bit 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Ignition {
    /// Spark ignition (petrol / gas).
    Spark,
    /// Compression ignition (diesel).
    Compression,
}

/// One monitor's readiness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MonitorState {
    /// Supported (PID `41`: enabled this drive cycle) and the test has run.
    Complete,
    /// Supported / enabled but the test has not completed yet.
    Incomplete,
    /// Not supported by this ECU (PID `41`: disabled for this drive cycle).
    NotSupported,
}

/// One OBD monitor and its state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Monitor {
    /// Stable snake_case key (`misfire`, `catalyst`, `pm_filter`, …).
    pub key: &'static str,
    pub name: &'static str,
    /// Continuous monitors (misfire, fuel system, components) run all the time.
    pub continuous: bool,
    pub state: MonitorState,
}

type MonitorDef = (u8, &'static str, &'static str);

/// Continuous monitors: byte B bit n = supported, bit n+4 = incomplete.
const CONTINUOUS: [MonitorDef; 3] = [
    (0, "misfire", "Misfire"),
    (1, "fuel_system", "Fuel system"),
    (2, "components", "Comprehensive components"),
];

/// Non-continuous monitors of a spark-ignition engine: byte C bit = supported,
/// byte D bit = incomplete.
const SPARK: [MonitorDef; 8] = [
    (0, "catalyst", "Catalyst"),
    (1, "heated_catalyst", "Heated catalyst"),
    (2, "evap_system", "Evaporative system"),
    (3, "secondary_air", "Secondary air system"),
    (4, "ac_refrigerant", "A/C refrigerant"),
    (5, "o2_sensor", "Oxygen sensor"),
    (6, "o2_sensor_heater", "Oxygen sensor heater"),
    (7, "egr_vvt", "EGR / VVT system"),
];

/// Non-continuous monitors of a compression-ignition engine (bits 2 and 4 are
/// reserved).
const COMPRESSION: [MonitorDef; 6] = [
    (0, "nmhc_catalyst", "NMHC catalyst"),
    (1, "nox_scr", "NOx / SCR aftertreatment"),
    (3, "boost_pressure", "Boost pressure"),
    (5, "exhaust_gas_sensor", "Exhaust gas sensor"),
    (6, "pm_filter", "PM filter (DPF)"),
    (7, "egr_vvt", "EGR / VVT system"),
];

fn state(supported: bool, incomplete: bool) -> MonitorState {
    match (supported, incomplete) {
        (false, _) => MonitorState::NotSupported,
        (true, true) => MonitorState::Incomplete,
        (true, false) => MonitorState::Complete,
    }
}

/// Decode bytes B, C, D (shared by PID `01` and `41`) into the monitor list:
/// continuous monitors first, then the non-continuous set for `ignition`.
fn decode_monitors(b: u8, c: u8, d: u8, ignition: Ignition) -> Vec<Monitor> {
    let mut out: Vec<Monitor> = CONTINUOUS
        .iter()
        .map(|&(bit, key, name)| Monitor {
            key,
            name,
            continuous: true,
            state: state(b & (1 << bit) != 0, b & (1 << (bit + 4)) != 0),
        })
        .collect();
    let table: &[MonitorDef] = match ignition {
        Ignition::Spark => &SPARK,
        Ignition::Compression => &COMPRESSION,
    };
    out.extend(table.iter().map(|&(bit, key, name)| Monitor {
        key,
        name,
        continuous: false,
        state: state(c & (1 << bit) != 0, d & (1 << bit) != 0),
    }));
    out
}

/// PID `01`: monitor status since DTCs were cleared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MonitorStatus {
    /// Malfunction indicator lamp ("check engine") commanded on.
    pub mil: bool,
    /// Number of stored (confirmed) emissions DTCs.
    pub dtc_count: u8,
    pub ignition: Ignition,
    pub monitors: Vec<Monitor>,
}

impl MonitorStatus {
    /// Supported monitors that have not completed.
    pub fn incomplete(&self) -> impl Iterator<Item = &Monitor> {
        self.monitors
            .iter()
            .filter(|m| m.state == MonitorState::Incomplete)
    }
}

/// Decode PID `01` data (`A B C D`).
pub fn decode_monitor_status(data: &[u8]) -> Result<MonitorStatus, String> {
    need(data, 4, "PID 01")?;
    let ignition = if data[1] & 0x08 != 0 {
        Ignition::Compression
    } else {
        Ignition::Spark
    };
    Ok(MonitorStatus {
        mil: data[0] & 0x80 != 0,
        dtc_count: data[0] & 0x7F,
        ignition,
        monitors: decode_monitors(data[1], data[2], data[3], ignition),
    })
}

/// PID `41`: monitor status this drive cycle. `NotSupported` here means the
/// monitor is disabled for the rest of this drive cycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DriveCycleStatus {
    pub ignition: Ignition,
    pub monitors: Vec<Monitor>,
}

/// Decode PID `41` data (`A B C D`; `A` is reserved). `ignition` from PID `01`
/// wins over this PID's own byte-B bit 3 when given.
pub fn decode_drive_cycle(
    data: &[u8],
    ignition: Option<Ignition>,
) -> Result<DriveCycleStatus, String> {
    need(data, 4, "PID 41")?;
    let ignition = ignition.unwrap_or(if data[1] & 0x08 != 0 {
        Ignition::Compression
    } else {
        Ignition::Spark
    });
    Ok(DriveCycleStatus {
        ignition,
        monitors: decode_monitors(data[1], data[2], data[3], ignition),
    })
}

/// Readiness of one module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Readiness {
    /// PID `01`: since DTCs were last cleared.
    pub status: MonitorStatus,
    /// PID `41`: this drive cycle (`None` when the ECU does not report it).
    pub this_cycle: Option<DriveCycleStatus>,
    /// Why `this_cycle` is missing.
    pub this_cycle_error: Option<String>,
}

/// Read PID `01` (required) and PID `41` (optional).
pub fn read_readiness<D: RawDiag + ?Sized>(diag: &mut D) -> Result<Readiness, String> {
    let d =
        exchange(diag, &[0x01, 0x01], 1).map_err(|e| format!("PID 01 (monitor status): {e}"))?;
    let status = decode_monitor_status(&d)?;
    let (this_cycle, this_cycle_error) = match exchange(diag, &[0x01, 0x41], 1)
        .and_then(|d| decode_drive_cycle(&d, Some(status.ignition)))
    {
        Ok(c) => (Some(c), None),
        Err(e) => (None, Some(format!("PID 41 (this drive cycle): {e}"))),
    };
    Ok(Readiness {
        status,
        this_cycle,
        this_cycle_error,
    })
}

// ---------------------------------------------------------------------------
// Freeze frame: mode 02
// ---------------------------------------------------------------------------

/// One freeze-frame value.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FreezeValue {
    pub pid: u8,
    /// Channel key of the mode-01 table, or `pid_XX` for a PID it lacks.
    pub key: String,
    pub name: String,
    pub unit: String,
    pub value: Option<ObdValue>,
    /// Why `value` is missing (decode error); `None` for an undecoded raw PID.
    pub error: Option<String>,
    /// The PID's raw data bytes as hex.
    pub raw: String,
}

/// One freeze frame of one module.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FreezeFrame {
    pub frame: u8,
    /// `false` when PID `02` reports `0000` (no freeze frame stored).
    pub stored: bool,
    /// The DTC that caused the frame to be stored.
    pub dtc: Option<DtcEntry>,
    /// Mode-02 PIDs the ECU supports for this frame (range markers excluded).
    pub pids: Vec<u8>,
    /// Decoded values in PID order (several rows per multi-field PID).
    pub values: Vec<FreezeValue>,
    /// Per-PID read errors, key `pid_XX`.
    pub errors: BTreeMap<String, String>,
}

/// Decode one freeze-frame PID's `data` into rows: one per mode-01 channel of
/// the PID, or a single raw row when the table has no channel for it.
pub fn decode_freeze_pid(pid: u8, data: &[u8]) -> Vec<FreezeValue> {
    let raw = hex(data);
    let rows: Vec<FreezeValue> = channels_for_pid(pid)
        .map(|c| {
            let (value, error) = match c.decode.apply(data) {
                Ok(v) => (Some(v), None),
                Err(e) => (None, Some(e)),
            };
            FreezeValue {
                pid,
                key: c.key.into(),
                name: c.name.into(),
                unit: c.unit.into(),
                value,
                error,
                raw: raw.clone(),
            }
        })
        .collect();
    if !rows.is_empty() {
        return rows;
    }
    vec![FreezeValue {
        pid,
        key: format!("pid_{pid:02x}"),
        name: format!("PID {pid:02X}"),
        unit: String::new(),
        value: None,
        error: None,
        raw,
    }]
}

/// Read freeze frame `frame` (J1979 defines frame `0`): the support bitmaps,
/// the DTC that stored it (PID `02`), then every supported PID, one request
/// per PID (`02 pid frame` → `42 pid frame data…`).
pub fn read_freeze_frame<D: RawDiag + ?Sized>(
    diag: &mut D,
    frame: u8,
) -> Result<FreezeFrame, String> {
    let set = walk_bitmaps(|base| {
        let d = exchange(diag, &[0x02, base, frame], 2)?;
        need(&d, 4, "bitmap")?;
        Ok(d[..4].to_vec())
    })
    .map_err(|e| format!("mode 02 (freeze frame): {e}"))?;
    let mut ff = FreezeFrame {
        frame,
        stored: false,
        dtc: None,
        pids: set.iter().copied().filter(|&p| p != 0x02).collect(),
        values: Vec::new(),
        errors: BTreeMap::new(),
    };
    match exchange(diag, &[0x02, 0x02, frame], 2).and_then(|d| {
        need(&d, 2, "PID 02")?;
        Ok((d[0], d[1]))
    }) {
        Ok((0, 0)) => return Ok(ff),
        Ok((hi, lo)) => ff.dtc = Some(dtc_from_bytes(hi, lo)),
        // Some ECUs omit PID 02; the frame may still hold data.
        Err(e) => {
            ff.errors.insert("pid_02".into(), e);
        }
    }
    ff.stored = true;
    for pid in ff.pids.clone() {
        match exchange(diag, &[0x02, pid, frame], 2) {
            Ok(d) => ff.values.extend(decode_freeze_pid(pid, &d)),
            Err(e) => {
                ff.errors.insert(format!("pid_{pid:02x}"), e);
            }
        }
    }
    Ok(ff)
}

// ---------------------------------------------------------------------------
// Vehicle information: mode 09
// ---------------------------------------------------------------------------

/// Split a mode-09 response (`resp` incl. `49 it`) into data items of
/// `item_len` bytes. Accepts the ISO 15765-4 form `49 it NODI data…` (one
/// message, `NODI` items) and the legacy multi-message form `49 it seq d d d d`
/// repeated (`seq` = 1, 2, …), whose 4-byte payloads are concatenated.
pub fn parse_info_items(
    infotype: u8,
    item_len: usize,
    resp: &[u8],
) -> Result<Vec<Vec<u8>>, String> {
    match resp {
        [0x7F, ..] => return Err(nrc_text(resp)),
        [0x49, it, ..] if *it == infotype => {}
        _ => return Err(format!("unexpected response {}", hex(resp))),
    }
    if item_len == 0 {
        return Err("item length 0".into());
    }
    let can_items =
        |data: &[u8]| -> Vec<Vec<u8>> { data.chunks_exact(item_len).map(<[u8]>::to_vec).collect() };
    // CAN: exact NODI * item_len.
    if resp.len() >= 3 && resp.len() - 3 == resp[2] as usize * item_len {
        return Ok(can_items(&resp[3..]));
    }
    // Legacy: 7-byte messages with ascending sequence numbers.
    let legacy = resp.len() % 7 == 0
        && resp
            .chunks(7)
            .enumerate()
            .all(|(i, c)| c[0] == 0x49 && c[1] == infotype && c[2] as usize == i + 1);
    if legacy {
        let data: Vec<u8> = resp
            .chunks(7)
            .flat_map(|c| c[3..].iter().copied())
            .collect();
        if data.len() % item_len == 0 {
            return Ok(can_items(&data));
        }
        return Err(format!(
            "infotype {infotype:02X}: {} data bytes are not a multiple of {item_len}",
            data.len()
        ));
    }
    // Lenient CAN: a wrong NODI but whole items after it.
    if resp.len() > 3 && (resp.len() - 3) % item_len == 0 {
        return Ok(can_items(&resp[3..]));
    }
    Err(format!(
        "infotype {infotype:02X}: unexpected length {} ({})",
        resp.len(),
        hex(resp)
    ))
}

/// Text of a fixed-width ASCII field: NULs dropped, surrounding spaces
/// trimmed; a field with non-printable bytes is returned as hex instead.
pub fn info_text(bytes: &[u8]) -> String {
    let printable = bytes.iter().all(|&b| b == 0 || (0x20..=0x7E).contains(&b));
    if printable {
        bytes
            .iter()
            .filter(|&&b| b != 0)
            .map(|&b| b as char)
            .collect::<String>()
            .trim()
            .to_string()
    } else {
        hex(bytes)
    }
}

/// VIN from a mode `09` PID `02` response (CAN or legacy form): the last 17
/// alphanumeric characters of the payload, or `None`.
pub fn parse_vin(resp: &[u8]) -> Option<String> {
    let payload: Vec<u8> = match parse_info_items(0x02, 1, resp) {
        // item_len 1: CAN `49 02 01 <17>` fails the NODI check and falls into
        // the lenient path (all bytes after NODI); legacy concatenates.
        Ok(items) => items.into_iter().flatten().collect(),
        Err(_) => return None,
    };
    let ascii: String = payload
        .iter()
        .filter(|b| b.is_ascii_alphanumeric())
        .map(|&b| b as char)
        .collect();
    (ascii.len() >= 17).then(|| ascii[ascii.len() - 17..].to_string())
}

/// Mode-09 data of one module.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct VehicleInfo {
    /// Supported infotypes (range markers excluded); empty when `09 00` failed.
    pub supported: Vec<u8>,
    pub vin: Option<String>,
    /// Calibration IDs (infotype `04`), in ECU order.
    pub calids: Vec<String>,
    /// Calibration verification numbers (infotype `06`) as 8 hex digits, in the
    /// same order as `calids`. A CVN that differs from the value for the stock
    /// calibration means the calibration was modified.
    pub cvns: Vec<String>,
    /// ECU name (infotype `0A`), e.g. `ECM-EngineControl`.
    pub ecu_name: Option<String>,
    /// Why an item is missing (`supported`, `vin`, `calid`, `cvn`, `ecu_name`).
    pub errors: BTreeMap<String, String>,
}

/// Read the mode-09 support bitmap, then VIN, CALIDs, CVNs and ECU name for
/// the supported infotypes. Without a bitmap only the VIN is tried (mode 09,
/// then UDS `22 F190`).
pub fn read_vehicle_info<D: RawDiag + ?Sized>(diag: &mut D) -> VehicleInfo {
    let mut info = VehicleInfo::default();
    let set = walk_bitmaps(|base| {
        let d = exchange(diag, &[0x09, base], 1)?;
        // Legacy (and some CAN ECUs) prefix the bitmap with a message count.
        match d.len() {
            4 => Ok(d),
            5 => Ok(d[1..].to_vec()),
            n => Err(format!("bitmap: {n} byte(s), need 4")),
        }
    });
    let set = match set {
        Ok(s) => s,
        Err(e) => {
            info.errors.insert("supported".into(), e);
            info.vin = super::read_vin(diag);
            if info.vin.is_none() {
                info.errors.insert("vin".into(), "not reported".into());
            }
            return info;
        }
    };
    info.supported = set.iter().copied().collect();
    fn get<D: RawDiag + ?Sized>(diag: &mut D, it: u8, len: usize) -> Result<Vec<Vec<u8>>, String> {
        let resp = diag.request(&[0x09, it])?;
        parse_info_items(it, len, &resp)
    }
    if set.contains(&0x02) {
        match diag.request(&[0x09, 0x02]).map(|r| parse_vin(&r)) {
            Ok(Some(v)) => info.vin = Some(v),
            Ok(None) => {
                info.errors
                    .insert("vin".into(), "no valid VIN in response".into());
            }
            Err(e) => {
                info.errors.insert("vin".into(), e);
            }
        }
    }
    if set.contains(&0x04) {
        match get(diag, 0x04, 16) {
            Ok(items) => info.calids = items.iter().map(|i| info_text(i)).collect(),
            Err(e) => {
                info.errors.insert("calid".into(), e);
            }
        }
    }
    if set.contains(&0x06) {
        match get(diag, 0x06, 4) {
            Ok(items) => info.cvns = items.iter().map(|i| hex(i)).collect(),
            Err(e) => {
                info.errors.insert("cvn".into(), e);
            }
        }
    }
    if set.contains(&0x0A) {
        match get(diag, 0x0A, 20) {
            Ok(items) => info.ecu_name = items.first().map(|i| info_text(i)),
            Err(e) => {
                info.errors.insert("ecu_name".into(), e);
            }
        }
    }
    info
}

// ---------------------------------------------------------------------------
// One-module inspection
// ---------------------------------------------------------------------------

/// Which parts [`inspect`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sections {
    pub readiness: bool,
    pub dtcs: bool,
    pub freeze: bool,
    pub info: bool,
}

impl Sections {
    /// Everything.
    pub const ALL: Sections = Sections {
        readiness: true,
        dtcs: true,
        freeze: true,
        info: true,
    };

    /// From names `readiness`, `dtcs`, `freeze`, `info` (or `all`). Empty =
    /// everything.
    pub fn parse<S: AsRef<str>>(names: &[S]) -> Result<Sections, String> {
        if names.is_empty() {
            return Ok(Sections::ALL);
        }
        let mut s = Sections {
            readiness: false,
            dtcs: false,
            freeze: false,
            info: false,
        };
        for n in names {
            match n.as_ref().trim() {
                "all" => s = Sections::ALL,
                "readiness" => s.readiness = true,
                "dtcs" => s.dtcs = true,
                "freeze" => s.freeze = true,
                "info" => s.info = true,
                other => {
                    return Err(format!(
                        "unknown section '{other}' (readiness, dtcs, freeze, info, all)"
                    ))
                }
            }
        }
        Ok(s)
    }
}

/// Inspection result of one module. A section not requested, or one that
/// failed outright, is `None`; the failure is in `errors` under the section
/// name (`readiness`, `freeze`).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Inspection {
    /// Whether the module answered anything at all (a negative response counts).
    pub responded: bool,
    pub readiness: Option<Readiness>,
    pub dtcs: Option<DtcReport>,
    pub freeze_frame: Option<FreezeFrame>,
    pub info: Option<VehicleInfo>,
    pub errors: BTreeMap<String, String>,
}

/// Counts answers so [`inspect`] can tell a silent module from a refusing one.
struct Probe<'a, D: RawDiag + ?Sized> {
    inner: &'a mut D,
    answered: bool,
}

impl<D: RawDiag + ?Sized> RawDiag for Probe<'_, D> {
    fn request(&mut self, req: &[u8]) -> Result<Vec<u8>, String> {
        let r = self.inner.request(req);
        self.answered |= r.is_ok();
        r
    }
}

/// Read the requested sections from one module. A module that does not
/// answer the first section at all is reported as `responded: false` and the
/// rest is skipped (so a scan over [`OBD_ECU_IDS`] does not wait out every
/// request of an absent module).
pub fn inspect<D: RawDiag + ?Sized>(diag: &mut D, sections: Sections, frame: u8) -> Inspection {
    let mut p = Probe {
        inner: diag,
        answered: false,
    };
    let mut out = Inspection::default();
    let mut first = true;
    let mut silent = |p: &Probe<'_, D>, out: &mut Inspection| -> bool {
        let was_first = std::mem::replace(&mut first, false);
        if was_first && !p.answered {
            out.errors
                .insert("module".into(), "no response from this module".into());
            return true;
        }
        false
    };
    if sections.readiness {
        match read_readiness(&mut p) {
            Ok(r) => out.readiness = Some(r),
            Err(e) => {
                out.errors.insert("readiness".into(), e);
            }
        }
        if silent(&p, &mut out) {
            return out;
        }
    }
    if sections.dtcs {
        out.dtcs = Some(read_all_dtcs(&mut p));
        if silent(&p, &mut out) {
            out.dtcs = None;
            return out;
        }
    }
    if sections.freeze {
        match read_freeze_frame(&mut p, frame) {
            Ok(f) => out.freeze_frame = Some(f),
            Err(e) => {
                out.errors.insert("freeze".into(), e);
            }
        }
        if silent(&p, &mut out) {
            return out;
        }
    }
    if sections.info {
        let info = read_vehicle_info(&mut p);
        if silent(&p, &mut out) {
            return out;
        }
        out.info = Some(info);
    }
    out.responded = p.answered;
    out
}

#[cfg(test)]
mod tests {
    use super::super::sim::{SimEcu, SimVehicle};
    use super::*;

    fn codes(v: &[DtcEntry]) -> Vec<&str> {
        v.iter().map(|d| d.code.as_str()).collect()
    }

    fn by_key<'a>(ms: &'a [Monitor], key: &str) -> &'a Monitor {
        ms.iter().find(|m| m.key == key).unwrap()
    }

    #[test]
    fn dtc_bytes_decode() {
        let d = dtc_from_bytes(0x04, 0x01);
        assert_eq!(d.code, "P0401");
        assert_eq!(d.system, "powertrain");
        assert!(!d.description.is_empty());
        assert_eq!(dtc_from_bytes(0xC1, 0x00).code, "U0100");
        assert_eq!(dtc_from_bytes(0xC1, 0x00).system, "network");
        assert_eq!(dtc_from_bytes(0x41, 0x23).code, "C0123");
        assert_eq!(dtc_from_bytes(0x81, 0x34).system, "body");
    }

    #[test]
    fn dtc_response_can_form() {
        // 43 02 | 04 01 | 20 02 -> P0401, P2002.
        let r = parse_dtc_response(0x03, &[0x43, 0x02, 0x04, 0x01, 0x20, 0x02]).unwrap();
        assert_eq!(codes(&r), ["P0401", "P2002"]);
        // No DTCs.
        assert!(parse_dtc_response(0x03, &[0x43, 0x00]).unwrap().is_empty());
        // Pending / permanent use their own positive ids.
        let r = parse_dtc_response(0x07, &[0x47, 0x01, 0x02, 0x99]).unwrap();
        assert_eq!(codes(&r), ["P0299"]);
        let r = parse_dtc_response(0x0A, &[0x4A, 0x01, 0xC1, 0x00]).unwrap();
        assert_eq!(codes(&r), ["U0100"]);
        // Wrong positive id / negative response.
        assert!(parse_dtc_response(0x07, &[0x43, 0x00]).is_err());
        let e = parse_dtc_response(0x0A, &[0x7F, 0x0A, 0x11]).unwrap_err();
        assert!(e.contains("NRC 11"), "{e}");
    }

    #[test]
    fn dtc_response_legacy_multi_message() {
        // Two legacy messages: 3 DTCs + 1 DTC and zero padding.
        let r = parse_dtc_response(
            0x03,
            &[
                0x43, 0x01, 0x01, 0x03, 0x00, 0x04, 0x20, //
                0x43, 0xC1, 0x00, 0x00, 0x00, 0x00, 0x00,
            ],
        )
        .unwrap();
        assert_eq!(codes(&r), ["P0101", "P0300", "P0420", "U0100"]);
        // A single all-padding message.
        assert!(parse_dtc_response(0x03, &[0x43, 0, 0, 0, 0, 0, 0])
            .unwrap()
            .is_empty());
        // Lenient: no count byte, even body.
        let r = parse_dtc_response(0x03, &[0x43, 0x01, 0x01, 0x04, 0x20]).unwrap();
        assert_eq!(codes(&r), ["P0101", "P0420"]);
    }

    #[test]
    fn monitor_status_compression() {
        // A=82: MIL on, 2 DTCs. B=0F: CI, misfire/fuel/components supported,
        // all complete. C=EB / D=42: NOx + PM filter incomplete.
        let s = decode_monitor_status(&[0x82, 0x0F, 0xEB, 0x42]).unwrap();
        assert!(s.mil);
        assert_eq!(s.dtc_count, 2);
        assert_eq!(s.ignition, Ignition::Compression);
        assert_eq!(s.monitors.len(), 3 + 6);
        assert_eq!(by_key(&s.monitors, "misfire").state, MonitorState::Complete);
        assert!(by_key(&s.monitors, "misfire").continuous);
        assert_eq!(
            by_key(&s.monitors, "nmhc_catalyst").state,
            MonitorState::Complete
        );
        assert_eq!(
            by_key(&s.monitors, "nox_scr").state,
            MonitorState::Incomplete
        );
        assert_eq!(
            by_key(&s.monitors, "boost_pressure").state,
            MonitorState::Complete
        );
        assert_eq!(
            by_key(&s.monitors, "pm_filter").state,
            MonitorState::Incomplete
        );
        assert_eq!(by_key(&s.monitors, "egr_vvt").state, MonitorState::Complete);
        let inc: Vec<&str> = s.incomplete().map(|m| m.key).collect();
        assert_eq!(inc, ["nox_scr", "pm_filter"]);
    }

    #[test]
    fn monitor_status_spark() {
        // A=00: MIL off, 0 DTCs. B=37: SI; misfire+fuel+components supported,
        // misfire + fuel incomplete. C=65 (cat, evap, O2, O2 heater), D=21
        // (cat, O2 incomplete).
        let s = decode_monitor_status(&[0x00, 0x37, 0x65, 0x21]).unwrap();
        assert!(!s.mil);
        assert_eq!(s.dtc_count, 0);
        assert_eq!(s.ignition, Ignition::Spark);
        assert_eq!(s.monitors.len(), 3 + 8);
        assert_eq!(
            by_key(&s.monitors, "misfire").state,
            MonitorState::Incomplete
        );
        assert_eq!(
            by_key(&s.monitors, "fuel_system").state,
            MonitorState::Incomplete
        );
        assert_eq!(
            by_key(&s.monitors, "components").state,
            MonitorState::Complete
        );
        assert_eq!(
            by_key(&s.monitors, "catalyst").state,
            MonitorState::Incomplete
        );
        assert_eq!(
            by_key(&s.monitors, "heated_catalyst").state,
            MonitorState::NotSupported
        );
        assert_eq!(
            by_key(&s.monitors, "evap_system").state,
            MonitorState::Complete
        );
        assert_eq!(
            by_key(&s.monitors, "secondary_air").state,
            MonitorState::NotSupported
        );
        assert_eq!(
            by_key(&s.monitors, "o2_sensor").state,
            MonitorState::Incomplete
        );
        assert_eq!(
            by_key(&s.monitors, "o2_sensor_heater").state,
            MonitorState::Complete
        );
        assert_eq!(
            by_key(&s.monitors, "egr_vvt").state,
            MonitorState::NotSupported
        );
        // 127 DTCs max, MIL bit separate.
        let s = decode_monitor_status(&[0xFF, 0, 0, 0]).unwrap();
        assert!(s.mil && s.dtc_count == 127);
        assert!(decode_monitor_status(&[0x00, 0x00, 0x00]).is_err());
    }

    #[test]
    fn drive_cycle_status() {
        let c = decode_drive_cycle(&[0x00, 0x0F, 0xEB, 0x4A], None).unwrap();
        assert_eq!(c.ignition, Ignition::Compression);
        assert_eq!(
            by_key(&c.monitors, "boost_pressure").state,
            MonitorState::Incomplete
        );
        // PID 01's ignition type wins.
        let c = decode_drive_cycle(&[0x00, 0x07, 0x01, 0x00], Some(Ignition::Compression)).unwrap();
        assert_eq!(c.ignition, Ignition::Compression);
        assert_eq!(
            by_key(&c.monitors, "nmhc_catalyst").state,
            MonitorState::Complete
        );
        let c = decode_drive_cycle(&[0x00, 0x07, 0x01, 0x01], None).unwrap();
        assert_eq!(c.ignition, Ignition::Spark);
        assert_eq!(
            by_key(&c.monitors, "catalyst").state,
            MonitorState::Incomplete
        );
    }

    #[test]
    fn readiness_states_serialise_snake_case() {
        let s = decode_monitor_status(&[0x80, 0x08, 0x40, 0x40]).unwrap();
        let j = serde_json::to_value(&s).unwrap();
        assert_eq!(j["ignition"], "compression");
        assert_eq!(j["mil"], true);
        let pm = j["monitors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["key"] == "pm_filter")
            .unwrap();
        assert_eq!(pm["state"], "incomplete");
        let cat = j["monitors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["key"] == "nox_scr")
            .unwrap();
        assert_eq!(cat["state"], "not_supported");
    }

    #[test]
    fn freeze_pid_rows() {
        let rows = decode_freeze_pid(0x0C, &[0x1A, 0xF8]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, "rpm");
        assert_eq!(rows[0].value, Some(ObdValue::Scalar(1726.0)));
        assert_eq!(rows[0].raw, "1AF8");
        // Multi-field PID: one row per channel.
        let rows = decode_freeze_pid(0x7C, &[0x03, 0x0E, 0x10, 0x0D, 0x48, 0, 0, 0, 0]);
        assert_eq!(rows.len(), 2);
        // Unknown to the table: raw row only.
        let rows = decode_freeze_pid(0x03, &[0x02, 0x00]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, "pid_03");
        assert_eq!(rows[0].value, None);
        assert_eq!(rows[0].error, None);
        assert_eq!(rows[0].raw, "0200");
        // Short data is a per-row error.
        let rows = decode_freeze_pid(0x0C, &[0x1A]);
        assert!(rows[0].error.is_some());
    }

    #[test]
    fn info_items_can_form() {
        // CALID: 2 items of 16 bytes.
        let mut r = vec![0x49, 0x04, 0x02];
        r.extend(b"CAL-A\0\0\0\0\0\0\0\0\0\0\0");
        r.extend(b"CAL-B-0123456789");
        let items = parse_info_items(0x04, 16, &r).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(info_text(&items[0]), "CAL-A");
        assert_eq!(info_text(&items[1]), "CAL-B-0123456789");
        // CVN: 2 items of 4 bytes.
        let items = parse_info_items(
            0x06,
            4,
            &[
                0x49, 0x06, 0x02, 0x1A, 0x2B, 0x3C, 0x4D, 0x00, 0x00, 0xBE, 0xEF,
            ],
        )
        .unwrap();
        assert_eq!(hex(&items[0]), "1A2B3C4D");
        assert_eq!(hex(&items[1]), "0000BEEF");
        // ECU name.
        let mut r = vec![0x49, 0x0A, 0x01];
        r.extend(b"ECM\0-EngineControl\0\0");
        let items = parse_info_items(0x0A, 20, &r).unwrap();
        assert_eq!(info_text(&items[0]), "ECM-EngineControl");
        // NRC / wrong infotype / bad length.
        assert!(parse_info_items(0x04, 16, &[0x7F, 0x09, 0x12])
            .unwrap_err()
            .contains("NRC 12"));
        assert!(parse_info_items(0x04, 16, &[0x49, 0x06, 0x01, 1, 2, 3, 4]).is_err());
        assert!(parse_info_items(0x04, 16, &[0x49, 0x04, 0x01, 1, 2, 3]).is_err());
    }

    #[test]
    fn info_items_legacy_multi_message() {
        // CVNs: one message per CVN, seq 1, 2.
        let items = parse_info_items(
            0x06,
            4,
            &[
                0x49, 0x06, 0x01, 0x1A, 0x2B, 0x3C, 0x4D, //
                0x49, 0x06, 0x02, 0x00, 0x00, 0xBE, 0xEF,
            ],
        )
        .unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(hex(&items[1]), "0000BEEF");
        // One CALID = four messages.
        let cal = b"OT-DEMO-CAL-0001";
        let mut r = Vec::new();
        for (i, c) in cal.chunks(4).enumerate() {
            r.extend([0x49, 0x04, i as u8 + 1]);
            r.extend(c);
        }
        let items = parse_info_items(0x04, 16, &r).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(info_text(&items[0]), "OT-DEMO-CAL-0001");
        // Out-of-order sequence numbers are not legacy (and not CAN).
        let mut bad = r.clone();
        bad[2] = 3;
        assert!(parse_info_items(0x04, 16, &bad).is_err());
    }

    #[test]
    fn vin_both_forms() {
        let mut can = vec![0x49, 0x02, 0x01];
        can.extend(b"1HGCM82633A004352");
        assert_eq!(parse_vin(&can).as_deref(), Some("1HGCM82633A004352"));
        // Legacy: 3 zero pad bytes + 17 chars in 5 messages.
        let mut data = vec![0u8; 3];
        data.extend(b"1HGCM82633A004352");
        let mut leg = Vec::new();
        for (i, c) in data.chunks(4).enumerate() {
            leg.extend([0x49, 0x02, i as u8 + 1]);
            leg.extend(c);
        }
        assert_eq!(parse_vin(&leg).as_deref(), Some("1HGCM82633A004352"));
        assert_eq!(parse_vin(&[0x7F, 0x09, 0x12]), None);
        assert_eq!(parse_vin(&[0x49, 0x02, 0x01, b'A']), None);
    }

    #[test]
    fn info_text_falls_back_to_hex() {
        assert_eq!(info_text(b"  ABC \0\0"), "ABC");
        assert_eq!(info_text(&[0x01, 0xFF]), "01FF");
    }

    #[test]
    fn sections_parse() {
        assert_eq!(Sections::parse::<&str>(&[]).unwrap(), Sections::ALL);
        let s = Sections::parse(&["readiness", "info"]).unwrap();
        assert!(s.readiness && s.info && !s.dtcs && !s.freeze);
        assert!(Sections::parse(&["bogus"]).is_err());
    }

    // ---- sim-backed end to end ---------------------------------------------

    #[test]
    fn sim_readiness() {
        let mut ecu = SimEcu::demo(0);
        let r = read_readiness(&mut ecu).unwrap();
        assert!(r.status.mil);
        assert_eq!(r.status.dtc_count, 2);
        assert_eq!(r.status.ignition, Ignition::Compression);
        let inc: Vec<&str> = r.status.incomplete().map(|m| m.key).collect();
        assert_eq!(inc, ["nox_scr", "pm_filter"]);
        let c = r.this_cycle.unwrap();
        assert_eq!(
            by_key(&c.monitors, "boost_pressure").state,
            MonitorState::Incomplete
        );
        // Without PID 41 the cycle part is optional.
        ecu.pids.remove(&0x41);
        let r = read_readiness(&mut ecu).unwrap();
        assert!(r.this_cycle.is_none());
        assert!(r.this_cycle_error.unwrap().contains("PID 41"));
        // Without PID 01 readiness fails.
        ecu.pids.remove(&0x01);
        assert!(read_readiness(&mut ecu).is_err());
    }

    #[test]
    fn sim_dtcs_all_modes() {
        let mut ecu = SimEcu::demo(0);
        let r = read_all_dtcs(&mut ecu);
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(codes(&r.stored), ["P0401", "P2002"]);
        assert_eq!(codes(&r.pending), ["P0299"]);
        assert_eq!(codes(&r.permanent), ["P0401"]);
        // Legacy framing gives the same lists.
        ecu.inspect.legacy = true;
        let l = read_all_dtcs(&mut ecu);
        assert_eq!(l, r);
        // An ECU without mode 0A.
        ecu.inspect.permanent = None;
        let r = read_all_dtcs(&mut ecu);
        assert!(r.permanent.is_empty());
        assert!(r.errors["permanent"].contains("NRC 11"));
        assert_eq!(codes(&r.stored), ["P0401", "P2002"]);
    }

    #[test]
    fn sim_freeze_frame() {
        let mut ecu = SimEcu::demo(0);
        let ff = read_freeze_frame(&mut ecu, 0).unwrap();
        assert!(ff.stored);
        assert_eq!(ff.dtc.as_ref().unwrap().code, "P0401");
        assert!(ff.errors.is_empty(), "{:?}", ff.errors);
        assert!(!ff.pids.contains(&0x02));
        let v = |k: &str| ff.values.iter().find(|r| r.key == k).unwrap();
        assert_eq!(v("rpm").value, Some(ObdValue::Scalar(1726.0)));
        assert_eq!(v("coolant_temp").value, Some(ObdValue::Scalar(83.0)));
        assert_eq!(v("speed").value, Some(ObdValue::Scalar(80.0)));
        assert_eq!(v("pid_03").raw, "0200");
        // Every request carried frame 0.
        assert!(ecu
            .log
            .iter()
            .filter(|r| r[0] == 0x02)
            .all(|r| r.len() == 3 && r[2] == 0));
        // Frame 1 does not exist.
        assert!(read_freeze_frame(&mut ecu, 1).is_err());
        // No frame stored: PID 02 = 0000, no values read.
        ecu.inspect.freeze.as_mut().unwrap().dtc = [0, 0];
        ecu.log.clear();
        let ff = read_freeze_frame(&mut ecu, 0).unwrap();
        assert!(!ff.stored && ff.dtc.is_none() && ff.values.is_empty());
        // Mode 02 unsupported.
        ecu.inspect.freeze = None;
        assert!(read_freeze_frame(&mut ecu, 0)
            .unwrap_err()
            .contains("mode 02"));
    }

    #[test]
    fn sim_vehicle_info_can_and_legacy() {
        let mut ecu = SimEcu::demo(0);
        let i = read_vehicle_info(&mut ecu);
        assert!(i.errors.is_empty(), "{:?}", i.errors);
        assert_eq!(i.supported, vec![0x02, 0x04, 0x06, 0x0A]);
        assert_eq!(i.vin.as_deref(), Some("WVWZZZ3CZ9E000000"));
        assert_eq!(i.calids, ["OT-DEMO-CAL-0001", "OT-DEMO-DPF-0001"]);
        assert_eq!(i.cvns, ["1A2B3C4D", "0000BEEF"]);
        assert_eq!(i.ecu_name.as_deref(), Some("ECM-EngineControl"));
        ecu.inspect.legacy = true;
        assert_eq!(read_vehicle_info(&mut ecu), i);
        // The dashboard's read_vin also understands the legacy form.
        assert_eq!(
            super::super::read_vin(&mut ecu).as_deref(),
            Some("WVWZZZ3CZ9E000000")
        );
    }

    #[test]
    fn vehicle_info_without_mode09_falls_back_to_uds_vin() {
        let mut uds = |req: &[u8]| -> Result<Vec<u8>, String> {
            match req {
                [0x22, 0xF1, 0x90] => {
                    let mut r = vec![0x62, 0xF1, 0x90];
                    r.extend(b"1HGCM82633A004352");
                    Ok(r)
                }
                _ => Ok(vec![0x7F, req[0], 0x11]),
            }
        };
        let i = read_vehicle_info(&mut uds);
        assert_eq!(i.vin.as_deref(), Some("1HGCM82633A004352"));
        assert!(i.errors["supported"].contains("NRC 11"));
        assert!(i.calids.is_empty() && i.supported.is_empty());
    }

    #[test]
    fn inspect_all_sections_and_silent_module() {
        let mut v = SimVehicle::demo(0);
        let engine = v.ecus.get_mut(&0x7E0).unwrap();
        let i = inspect(engine, Sections::ALL, 0);
        assert!(i.responded);
        assert!(i.errors.is_empty(), "{:?}", i.errors);
        assert!(i.readiness.unwrap().status.mil);
        assert_eq!(i.dtcs.unwrap().stored.len(), 2);
        assert!(i.freeze_frame.unwrap().stored);
        assert_eq!(i.info.unwrap().calids.len(), 2);

        let tcm = v.ecus.get_mut(&0x7E1).unwrap();
        let i = inspect(tcm, Sections::ALL, 0);
        assert!(i.responded);
        let info = i.info.unwrap();
        assert_eq!(info.ecu_name.as_deref(), Some("TCM-TransmissionCtl"));
        assert!(info.vin.is_none());
        assert!(i.dtcs.unwrap().stored.is_empty());
        assert!(!i.freeze_frame.unwrap().stored);

        // A silent address: one request, then give up.
        let mut calls = 0;
        let mut dead = |_: &[u8]| -> Result<Vec<u8>, String> {
            calls += 1;
            Err("NO DATA".into())
        };
        let i = inspect(&mut dead, Sections::ALL, 0);
        assert!(!i.responded);
        assert!(i.readiness.is_none() && i.dtcs.is_none() && i.info.is_none());
        assert!(i.errors.contains_key("module"));
        assert_eq!(calls, 1);

        // Only one section requested.
        let engine = v.ecus.get_mut(&0x7E0).unwrap();
        engine.log.clear();
        let i = inspect(
            engine,
            Sections {
                readiness: false,
                dtcs: true,
                freeze: false,
                info: false,
            },
            0,
        );
        assert!(i.readiness.is_none() && i.info.is_none() && i.dtcs.is_some());
        assert_eq!(engine.log, vec![vec![0x03], vec![0x07], vec![0x0A]]);
        let j = serde_json::to_value(&i).unwrap();
        assert_eq!(j["dtcs"]["stored"][0]["code"], "P0401");
        assert!(j["readiness"].is_null());
    }

    #[test]
    fn ecu_ids_and_labels() {
        assert_eq!(OBD_ECU_IDS[0], (0x7E0, 0x7E8));
        assert!(OBD_ECU_IDS.iter().all(|(t, r)| r - t == 8));
        assert_eq!(obd_ecu_label(0x7E0), "engine");
        assert_eq!(obd_ecu_label(0x7E1), "transmission");
        assert_eq!(obd_ecu_label(0x7E5), "other");
    }
}
