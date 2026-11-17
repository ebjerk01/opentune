//! Generic OBD-II (SAE J1979 / ISO 15031-5) **mode 0x01** live data.
//!
//! - [`CHANNELS`]: a data-driven, `const` table of the standard PIDs a live
//!   dashboard shows (load, temperatures, MAP/boost, MAF, EGT, DPF, torque, …),
//!   each with its data length, key, unit, gauge range, suggested thresholds and
//!   decode formula.
//! - [`ObdSession`]: supported-PID discovery (`0100`/`0120`/… bitmaps, cached),
//!   efficient polling with up to six PIDs per request (the CAN OBD maximum) and
//!   an automatic, remembered fall-back to single-PID requests for ECUs that
//!   reject or mis-handle multi-PID requests.
//! - [`read_vin`]: VIN via mode `0x09` PID `0x02` (UDS `22 F190` fallback).
//! - [`sim::SimEcu`]: a small offline mode-01 ECU used by the tests, the CLI
//!   `--driver obdsim` and the desktop demo driver.
//!
//! Everything runs over the [`RawDiag`] request→response seam, so any driver
//! that exposes a diagnostic transport (wrapped in [`CoreDiag`](crate::CoreDiag))
//! can drive it. A 6-PID request is 7 bytes (`01` + 6 PIDs), which still fits an
//! ELM327's single-frame request limit.
//!
//! Formulas follow SAE J1979 as summarised in the public "OBD-II PIDs" table. The
//! decoder never clamps: `min`/`max` are a sensible gauge range, not the encoding
//! range. `warn`/`crit` are suggested **high-side** thresholds (value ≥ threshold)
//! and are `None` where no single upper limit is meaningful (trims, voltage, …).
use crate::diag::RawDiag;
use std::collections::{BTreeMap, BTreeSet};

/// Maximum PIDs in one mode-01 request on CAN (ISO 15765-4).
pub const MAX_PIDS_PER_REQUEST: usize = 6;

/// Dashboard grouping of a channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Group {
    Engine,
    Air,
    Fuel,
    Exhaust,
    Electrical,
    Vehicle,
}

impl Group {
    /// Lower-case name used in JSON (`"engine"`, `"air"`, …).
    pub fn as_str(self) -> &'static str {
        match self {
            Group::Engine => "engine",
            Group::Air => "air",
            Group::Fuel => "fuel",
            Group::Exhaust => "exhaust",
            Group::Electrical => "electrical",
            Group::Vehicle => "vehicle",
        }
    }
}

/// How a channel's value is computed from the PID's data bytes (`A`, `B`, …).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Decode {
    /// `A * mul + add`.
    U8 { mul: f64, add: f64 },
    /// `(256A + B) * mul + add`.
    U16 { mul: f64, add: f64 },
    /// Support-byte PID (`A` = per-slot "supported" bits, then 16-bit slots):
    /// the one 16-bit slot `slot` (bit `slot` of `A`), optionally two's
    /// complement. Scalar.
    Slot {
        slot: u8,
        mul: f64,
        add: f64,
        signed: bool,
    },
    /// Support-byte PID: the first supported of `slots` 16-bit slots. Scalar.
    FirstSlot { slots: u8, mul: f64, add: f64 },
    /// Support-byte PID: all `slots` slots, positionally, `None` for a slot
    /// whose support bit is clear (e.g. EGT sensors 1–4). Array.
    Slots { slots: u8, mul: f64, add: f64 },
    /// Computed from other channels after a read (e.g. boost = MAP − baro).
    Derived,
}

/// A decoded value: a number, or one number per reported sensor.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(untagged)]
pub enum ObdValue {
    Scalar(f64),
    /// Positional; `None` (JSON `null`) for a sensor the ECU does not report.
    Array(Vec<Option<f64>>),
}

impl ObdValue {
    /// The scalar, or the first reported array element.
    pub fn first(&self) -> Option<f64> {
        match self {
            ObdValue::Scalar(v) => Some(*v),
            ObdValue::Array(a) => a.iter().flatten().next().copied(),
        }
    }
}

impl Decode {
    /// Decode `data` (the bytes after the echoed PID).
    pub fn apply(&self, data: &[u8]) -> Result<ObdValue, String> {
        let need = |n: usize| {
            if data.len() < n {
                Err(format!("short data: {} byte(s), need {n}", data.len()))
            } else {
                Ok(())
            }
        };
        let slot = |i: usize, signed: bool| {
            let raw = u16::from_be_bytes([data[1 + 2 * i], data[2 + 2 * i]]);
            if signed {
                raw as i16 as f64
            } else {
                raw as f64
            }
        };
        match *self {
            Decode::U8 { mul, add } => {
                need(1)?;
                Ok(ObdValue::Scalar(data[0] as f64 * mul + add))
            }
            Decode::U16 { mul, add } => {
                need(2)?;
                let raw = u16::from_be_bytes([data[0], data[1]]) as f64;
                Ok(ObdValue::Scalar(raw * mul + add))
            }
            Decode::Slot {
                slot: s,
                mul,
                add,
                signed,
            } => {
                let s = s as usize;
                need(3 + 2 * s)?;
                if data[0] & (1 << s) == 0 {
                    return Err("sensor not reported by ECU".into());
                }
                Ok(ObdValue::Scalar(slot(s, signed) * mul + add))
            }
            Decode::FirstSlot { slots, mul, add } => {
                need(1 + 2 * slots as usize)?;
                (0..slots as usize)
                    .find(|&i| data[0] & (1 << i) != 0)
                    .map(|i| ObdValue::Scalar(slot(i, false) * mul + add))
                    .ok_or_else(|| "no sensor reported by ECU".into())
            }
            Decode::Slots { slots, mul, add } => {
                need(1 + 2 * slots as usize)?;
                let vals: Vec<Option<f64>> = (0..slots as usize)
                    .map(|i| (data[0] & (1 << i) != 0).then(|| slot(i, false) * mul + add))
                    .collect();
                if vals.iter().all(Option::is_none) {
                    Err("no sensor reported by ECU".into())
                } else {
                    Ok(ObdValue::Array(vals))
                }
            }
            Decode::Derived => Err("derived channel has no raw decoding".into()),
        }
    }
}

/// One dashboard channel (a PID, or one field of a multi-field PID).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Channel {
    /// Mode-01 PID. For a derived channel, the PID that typically sources it.
    pub pid: u8,
    /// Data bytes after the echoed PID (0 for derived channels).
    pub len: u8,
    /// Stable snake_case key.
    pub key: &'static str,
    pub name: &'static str,
    pub unit: &'static str,
    /// Suggested gauge range (not clamped).
    pub min: f64,
    pub max: f64,
    /// Suggested high-side thresholds.
    pub warn: Option<f64>,
    pub crit: Option<f64>,
    pub group: Group,
    pub decode: Decode,
}

impl Channel {
    pub fn is_derived(&self) -> bool {
        matches!(self.decode, Decode::Derived)
    }
}

const PCT: f64 = 100.0 / 255.0;

#[allow(clippy::too_many_arguments)]
const fn ch(
    pid: u8,
    len: u8,
    key: &'static str,
    name: &'static str,
    unit: &'static str,
    min: f64,
    max: f64,
    warn: Option<f64>,
    crit: Option<f64>,
    group: Group,
    decode: Decode,
) -> Channel {
    Channel {
        pid,
        len,
        key,
        name,
        unit,
        min,
        max,
        warn,
        crit,
        group,
        decode,
    }
}

const fn u8d(mul: f64, add: f64) -> Decode {
    Decode::U8 { mul, add }
}
const fn u16d(mul: f64, add: f64) -> Decode {
    Decode::U16 { mul, add }
}
const fn slot(slot: u8, mul: f64, add: f64) -> Decode {
    Decode::Slot {
        slot,
        mul,
        add,
        signed: false,
    }
}

use Group::*;

/// The mode-01 channel table, ordered by PID.
#[rustfmt::skip]
pub const CHANNELS: &[Channel] = &[
    ch(0x04, 1, "engine_load", "Calculated engine load", "%", 0.0, 100.0, None, None, Engine, u8d(PCT, 0.0)),
    ch(0x05, 1, "coolant_temp", "Coolant temperature", "°C", -40.0, 130.0, Some(105.0), Some(115.0), Engine, u8d(1.0, -40.0)),
    ch(0x06, 1, "stft_b1", "Short-term fuel trim bank 1", "%", -25.0, 25.0, None, None, Fuel, u8d(100.0 / 128.0, -100.0)),
    ch(0x07, 1, "ltft_b1", "Long-term fuel trim bank 1", "%", -25.0, 25.0, None, None, Fuel, u8d(100.0 / 128.0, -100.0)),
    ch(0x0B, 1, "intake_map", "Intake manifold pressure", "kPa", 0.0, 255.0, None, None, Air, u8d(1.0, 0.0)),
    ch(0x0B, 0, "boost_kpa", "Boost pressure (MAP − baro)", "kPa", -100.0, 250.0, None, None, Air, Decode::Derived),
    ch(0x0B, 0, "boost_bar", "Boost pressure (MAP − baro)", "bar", -1.0, 2.5, None, None, Air, Decode::Derived),
    ch(0x0C, 2, "rpm", "Engine speed", "rpm", 0.0, 8000.0, None, None, Engine, u16d(0.25, 0.0)),
    ch(0x0D, 1, "speed", "Vehicle speed", "km/h", 0.0, 255.0, None, None, Vehicle, u8d(1.0, 0.0)),
    ch(0x0E, 1, "timing_advance", "Timing advance", "°", -64.0, 63.5, None, None, Engine, u8d(0.5, -64.0)),
    ch(0x0F, 1, "intake_air_temp", "Intake air temperature", "°C", -40.0, 100.0, Some(60.0), Some(75.0), Air, u8d(1.0, -40.0)),
    ch(0x10, 2, "maf", "Mass air flow", "g/s", 0.0, 400.0, None, None, Air, u16d(0.01, 0.0)),
    ch(0x11, 1, "throttle_pos", "Throttle position", "%", 0.0, 100.0, None, None, Air, u8d(PCT, 0.0)),
    ch(0x1F, 2, "run_time", "Run time since engine start", "s", 0.0, 65535.0, None, None, Engine, u16d(1.0, 0.0)),
    ch(0x21, 2, "distance_mil", "Distance with MIL on", "km", 0.0, 65535.0, None, None, Vehicle, u16d(1.0, 0.0)),
    ch(0x22, 2, "fuel_rail_pressure_rel", "Fuel rail pressure (rel. manifold)", "kPa", 0.0, 5177.265, None, None, Fuel, u16d(0.079, 0.0)),
    ch(0x23, 2, "fuel_rail_pressure", "Fuel rail pressure (gauge)", "kPa", 0.0, 250000.0, None, None, Fuel, u16d(10.0, 0.0)),
    ch(0x2C, 1, "commanded_egr", "Commanded EGR", "%", 0.0, 100.0, None, None, Exhaust, u8d(PCT, 0.0)),
    ch(0x2F, 1, "fuel_level", "Fuel tank level", "%", 0.0, 100.0, None, None, Fuel, u8d(PCT, 0.0)),
    ch(0x31, 2, "distance_since_clear", "Distance since codes cleared", "km", 0.0, 65535.0, None, None, Vehicle, u16d(1.0, 0.0)),
    ch(0x33, 1, "baro_pressure", "Barometric pressure", "kPa", 0.0, 255.0, None, None, Air, u8d(1.0, 0.0)),
    ch(0x42, 2, "module_voltage", "Control module voltage", "V", 0.0, 16.0, None, None, Electrical, u16d(0.001, 0.0)),
    ch(0x43, 2, "abs_load", "Absolute load value", "%", 0.0, 200.0, None, None, Engine, u16d(PCT, 0.0)),
    ch(0x44, 2, "commanded_lambda", "Commanded air-fuel equivalence ratio", "λ", 0.0, 2.0, None, None, Fuel, u16d(2.0 / 65536.0, 0.0)),
    ch(0x45, 1, "rel_throttle", "Relative throttle position", "%", 0.0, 100.0, None, None, Air, u8d(PCT, 0.0)),
    ch(0x46, 1, "ambient_temp", "Ambient air temperature", "°C", -40.0, 60.0, None, None, Vehicle, u8d(1.0, -40.0)),
    ch(0x49, 1, "accel_pedal_d", "Accelerator pedal position D", "%", 0.0, 100.0, None, None, Vehicle, u8d(PCT, 0.0)),
    ch(0x4C, 1, "commanded_throttle", "Commanded throttle actuator", "%", 0.0, 100.0, None, None, Air, u8d(PCT, 0.0)),
    ch(0x5C, 1, "oil_temp", "Engine oil temperature", "°C", -40.0, 160.0, Some(130.0), Some(145.0), Engine, u8d(1.0, -40.0)),
    ch(0x5E, 2, "fuel_rate", "Engine fuel rate", "L/h", 0.0, 100.0, None, None, Fuel, u16d(0.05, 0.0)),
    ch(0x61, 1, "torque_demand", "Driver's demand engine torque", "%", -125.0, 130.0, None, None, Engine, u8d(1.0, -125.0)),
    ch(0x62, 1, "torque_actual", "Actual engine torque", "%", -125.0, 130.0, None, None, Engine, u8d(1.0, -125.0)),
    ch(0x63, 2, "torque_ref", "Engine reference torque", "Nm", 0.0, 1000.0, None, None, Engine, u16d(1.0, 0.0)),
    ch(0x70, 10, "boost_control", "Boost pressure sensor A (absolute)", "kPa", 0.0, 350.0, None, None, Air, slot(1, 0.03125, 0.0)),
    ch(0x70, 10, "boost_control_cmd", "Commanded boost pressure A (absolute)", "kPa", 0.0, 350.0, None, None, Air, slot(0, 0.03125, 0.0)),
    ch(0x73, 5, "exhaust_pressure", "Exhaust pressure (bank 1, else bank 2)", "kPa", 0.0, 400.0, None, None, Exhaust, Decode::FirstSlot { slots: 2, mul: 0.01, add: 0.0 }),
    ch(0x78, 9, "egt_b1", "Exhaust gas temperature bank 1", "°C", 0.0, 1000.0, Some(750.0), Some(850.0), Exhaust, Decode::Slots { slots: 4, mul: 0.1, add: -40.0 }),
    ch(0x79, 9, "egt_b2", "Exhaust gas temperature bank 2", "°C", 0.0, 1000.0, Some(750.0), Some(850.0), Exhaust, Decode::Slots { slots: 4, mul: 0.1, add: -40.0 }),
    ch(0x7A, 7, "dpf_dp_b1", "DPF differential pressure bank 1", "kPa", 0.0, 50.0, None, None, Exhaust, Decode::Slot { slot: 0, mul: 0.01, add: 0.0, signed: true }),
    ch(0x7B, 7, "dpf_dp_b2", "DPF differential pressure bank 2", "kPa", 0.0, 50.0, None, None, Exhaust, Decode::Slot { slot: 0, mul: 0.01, add: 0.0, signed: true }),
    ch(0x7C, 9, "dpf_temp", "DPF inlet temperature (first reported sensor)", "°C", 0.0, 900.0, Some(700.0), Some(800.0), Exhaust, Decode::FirstSlot { slots: 4, mul: 0.1, add: -40.0 }),
    ch(0x7C, 9, "dpf_temp_out", "DPF outlet temperature bank 1", "°C", 0.0, 900.0, Some(700.0), Some(800.0), Exhaust, slot(1, 0.1, -40.0)),
    ch(0x87, 5, "intake_map_abs", "Intake manifold absolute pressure", "kPa", 0.0, 400.0, None, None, Air, Decode::FirstSlot { slots: 2, mul: 0.03125, add: 0.0 }),
];

/// Every channel row, `CHANNELS` order.
pub fn channels() -> &'static [Channel] {
    CHANNELS
}

/// The channel with `key`.
pub fn channel(key: &str) -> Option<&'static Channel> {
    CHANNELS.iter().find(|c| c.key == key)
}

/// Raw (non-derived) channels decoded from `pid`.
pub fn channels_for_pid(pid: u8) -> impl Iterator<Item = &'static Channel> {
    CHANNELS
        .iter()
        .filter(move |c| c.pid == pid && !c.is_derived())
}

/// Data length (bytes after the PID echo) for a PID this module knows,
/// including the `0x00/0x20/…` support bitmaps (4 bytes).
pub fn pid_len(pid: u8) -> Option<usize> {
    if pid % 0x20 == 0 {
        return Some(4);
    }
    channels_for_pid(pid).next().map(|c| c.len as usize)
}

/// Decode a PID's data into `(key, value)` for each of its channels.
pub fn decode_pid(pid: u8, data: &[u8]) -> Vec<(&'static str, Result<ObdValue, String>)> {
    channels_for_pid(pid)
        .map(|c| (c.key, c.decode.apply(data)))
        .collect()
}

/// Parse a 4-byte support bitmap returned for range PID `base` (`0x00`, `0x20`,
/// …) into the supported PIDs (`base+1 ..= base+0x20`). MSB of `A` = `base+1`.
pub fn parse_support_bitmap(base: u8, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for (j, byte) in data.iter().take(4).enumerate() {
        for k in 0..8 {
            if byte & (0x80 >> k) != 0 {
                out.push(base.wrapping_add(1 + (j * 8 + k) as u8));
            }
        }
    }
    out
}

/// Parse the body of a (multi-)PID mode-01 positive response (bytes after
/// `0x41`): `pid, data…, pid, data…`. Returns the PIDs parsed, plus an error if
/// parsing had to stop early (unknown PID length or truncated data).
pub fn parse_multi_response(body: &[u8]) -> (BTreeMap<u8, Vec<u8>>, Option<String>) {
    let mut out = BTreeMap::new();
    let mut i = 0;
    while i < body.len() {
        let pid = body[i];
        let Some(len) = pid_len(pid) else {
            return (
                out,
                Some(format!(
                    "unknown PID {pid:02X} in response; cannot continue"
                )),
            );
        };
        if i + 1 + len > body.len() {
            return (out, Some(format!("PID {pid:02X}: truncated data")));
        }
        out.insert(pid, body[i + 1..i + 1 + len].to_vec());
        i += 1 + len;
    }
    (out, None)
}

/// Explain a negative response `7F sid nrc`.
fn nrc_text(resp: &[u8]) -> String {
    let nrc = resp.get(2).copied().unwrap_or(0);
    format!(
        "ECU rejected request: NRC {nrc:02X} ({})",
        crate::protocol::nrc_name(nrc)
    )
}

/// Send `01 pid` and return the PID's data bytes.
fn read_single<D: RawDiag + ?Sized>(diag: &mut D, pid: u8) -> Result<Vec<u8>, String> {
    let len = pid_len(pid).ok_or_else(|| format!("unknown PID {pid:02X}"))?;
    let resp = diag.request(&[0x01, pid])?;
    match resp.as_slice() {
        [0x41, p, data @ ..] if *p == pid => {
            if data.len() < len {
                Err(format!(
                    "short response: {} byte(s), need {len}",
                    data.len()
                ))
            } else {
                Ok(data[..len].to_vec())
            }
        }
        [0x7F, ..] => Err(nrc_text(&resp)),
        _ => Err(format!("unexpected response {}", hex(&resp))),
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

const NOT_SUPPORTED: &str = "not supported by ECU";

/// Result of [`ObdSession::read`].
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct Readout {
    /// One entry per requested channel key (`null` on failure), plus derived
    /// `boost_kpa`/`boost_bar` when MAP and baro are both available.
    pub values: BTreeMap<String, Option<ObdValue>>,
    /// Why a key is `null`. Unknown requested PIDs appear as `pid_XX`.
    pub errors: BTreeMap<String, String>,
}

/// Per-connection OBD state: discovered PIDs and learned ECU quirks. Cheap to
/// clone, so an app can keep one per adapter between polls.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObdSession {
    supported: Option<BTreeSet<u8>>,
    multi: Option<bool>,
    absent: BTreeSet<u8>,
}

impl ObdSession {
    pub fn new() -> Self {
        Self::default()
    }

    /// PIDs found by the last [`discover`](Self::discover), if run.
    pub fn supported(&self) -> Option<&BTreeSet<u8>> {
        self.supported.as_ref()
    }

    /// Whether multi-PID requests are known to work (`None` = not yet learned).
    pub fn multi_pid(&self) -> Option<bool> {
        self.multi
    }

    /// Walk the `0100`, `0120`, `0140`, … support bitmaps (following each
    /// range's "next range supported" bit, up to `01E0`) and cache the result.
    /// Range markers (`0x20`, `0x40`, …) are not included. A later range that
    /// does not answer ends discovery; only a failing `0100` is an error.
    pub fn discover<D: RawDiag + ?Sized>(&mut self, diag: &mut D) -> Result<BTreeSet<u8>, String> {
        let mut set = BTreeSet::new();
        let mut base: u8 = 0x00;
        loop {
            match read_single(diag, base) {
                Ok(data) => set.extend(parse_support_bitmap(base, &data)),
                Err(e) if base == 0 => return Err(format!("PID 00 (supported PIDs): {e}")),
                Err(_) => break,
            }
            let next = base.wrapping_add(0x20);
            if base == 0xE0 || !set.contains(&next) {
                break;
            }
            base = next;
        }
        set.retain(|p| p % 0x20 != 0);
        self.supported = Some(set.clone());
        self.absent.clear();
        Ok(set)
    }

    /// Fetch raw data for `pids` (deduplicated), batching up to
    /// [`MAX_PIDS_PER_REQUEST`] per request when the ECU allows it.
    pub fn read_pids<D: RawDiag + ?Sized>(
        &mut self,
        diag: &mut D,
        pids: &[u8],
    ) -> BTreeMap<u8, Result<Vec<u8>, String>> {
        let mut out = BTreeMap::new();
        let mut want = Vec::new();
        for &p in pids {
            if out.contains_key(&p) || want.contains(&p) {
                continue;
            }
            if pid_len(p).is_none() {
                out.insert(p, Err(format!("unknown PID {p:02X}")));
            } else if self.absent.contains(&p)
                || self.supported.as_ref().is_some_and(|s| !s.contains(&p))
            {
                out.insert(p, Err(NOT_SUPPORTED.into()));
            } else {
                want.push(p);
            }
        }

        let mut singles: Vec<u8> = Vec::new();
        // PIDs a multi request left out or failed for: a single success for one
        // of these means the ECU does not handle multi-PID requests properly.
        let mut suspect: BTreeSet<u8> = BTreeSet::new();
        if self.multi == Some(false) || want.len() < 2 {
            singles = want;
        } else {
            for chunk in want.chunks(MAX_PIDS_PER_REQUEST) {
                if chunk.len() == 1 {
                    singles.push(chunk[0]);
                    continue;
                }
                let mut req = vec![0x01];
                req.extend_from_slice(chunk);
                match diag.request(&req) {
                    Ok(resp) if resp.first() == Some(&0x41) => {
                        let (parsed, _err) = parse_multi_response(&resp[1..]);
                        let got = chunk.iter().filter(|p| parsed.contains_key(p)).count();
                        if got >= 2 {
                            self.multi = Some(true);
                        }
                        for &p in chunk {
                            match parsed.get(&p) {
                                Some(d) => {
                                    out.insert(p, Ok(d.clone()));
                                }
                                None if self.multi == Some(true) => {
                                    self.absent.insert(p);
                                    out.insert(p, Err(NOT_SUPPORTED.into()));
                                }
                                None => {
                                    suspect.insert(p);
                                    singles.push(p);
                                }
                            }
                        }
                    }
                    // Negative response, garbage or no answer: try one by one.
                    _ => {
                        suspect.extend(chunk.iter().copied());
                        singles.extend_from_slice(chunk);
                    }
                }
            }
        }

        for p in singles {
            let r = read_single(diag, p);
            if suspect.contains(&p) {
                if r.is_ok() {
                    self.multi = Some(false);
                } else {
                    // Omitted from the multi reply *and* no single answer.
                    self.absent.insert(p);
                }
            }
            out.insert(p, r);
        }
        out
    }

    /// Read the channels of `pids` and decode them into a [`Readout`]. When a
    /// MAP PID (`0x0B`/`0x87`) is requested, baro (`0x33`) is fetched too so the
    /// derived boost can be computed (its value is only reported if requested).
    pub fn read<D: RawDiag + ?Sized>(&mut self, diag: &mut D, pids: &[u8]) -> Readout {
        let mut fetch: Vec<u8> = pids.to_vec();
        let wants_map = pids.iter().any(|p| *p == 0x0B || *p == 0x87);
        if wants_map && !pids.contains(&0x33) {
            fetch.push(0x33);
        }
        let raw = self.read_pids(diag, &fetch);

        let mut ro = Readout::default();
        let mut decoded: BTreeMap<&'static str, ObdValue> = BTreeMap::new();
        for (&pid, res) in &raw {
            let requested = pids.contains(&pid);
            let mut known = false;
            for c in channels_for_pid(pid) {
                known = true;
                let v = res
                    .as_ref()
                    .map_err(|e| e.clone())
                    .and_then(|d| c.decode.apply(d));
                match v {
                    Ok(v) => {
                        decoded.insert(c.key, v.clone());
                        if requested {
                            ro.values.insert(c.key.into(), Some(v));
                        }
                    }
                    Err(e) if requested => {
                        ro.values.insert(c.key.into(), None);
                        ro.errors.insert(c.key.into(), e);
                    }
                    Err(_) => {}
                }
            }
            if !known && requested {
                let msg = res
                    .as_ref()
                    .err()
                    .cloned()
                    .unwrap_or_else(|| "unknown PID".into());
                ro.errors.insert(format!("pid_{pid:02x}"), msg);
            }
        }

        let map = decoded
            .get("intake_map_abs")
            .or_else(|| decoded.get("intake_map"))
            .and_then(ObdValue::first);
        let baro = decoded.get("baro_pressure").and_then(ObdValue::first);
        if let (Some(map), Some(baro)) = (map, baro) {
            let kpa = map - baro;
            ro.values
                .insert("boost_kpa".into(), Some(ObdValue::Scalar(kpa)));
            ro.values
                .insert("boost_bar".into(), Some(ObdValue::Scalar(kpa / 100.0)));
        }
        ro
    }
}

/// Read the VIN via OBD mode `09` PID `02`, falling back to UDS `22 F190`.
/// Returns `None` if neither yields a plausible 17-character VIN.
pub fn read_vin<D: RawDiag + ?Sized>(diag: &mut D) -> Option<String> {
    let pick = |payload: &[u8]| -> Option<String> {
        let ascii: String = payload
            .iter()
            .filter(|b| b.is_ascii_alphanumeric())
            .map(|&b| b as char)
            .collect();
        (ascii.len() >= 17).then(|| ascii[ascii.len() - 17..].to_string())
    };
    if let Ok(r) = diag.request(&[0x09, 0x02]) {
        if r.len() > 2 && r[0] == 0x49 && r[1] == 0x02 {
            // CAN: `49 02 01 <17 bytes>`; skip the data-item count byte.
            if let Some(v) = pick(&r[3.min(r.len())..]) {
                return Some(v);
            }
        }
    }
    if let Ok(r) = diag.request(&[0x22, 0xF1, 0x90]) {
        if r.len() > 3 && r[0] == 0x62 && r[1] == 0xF1 && r[2] == 0x90 {
            return pick(&r[3..]);
        }
    }
    None
}

/// An offline mode-01 ECU for tests, demos and UI development.
pub mod sim {
    use super::*;

    /// How the simulated ECU treats multi-PID requests.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum MultiMode {
        /// Answers every supported PID of the request (J1979 on CAN).
        Full,
        /// Rejects multi-PID requests with NRC `0x12`.
        Reject,
        /// Non-compliant: answers only the first PID.
        FirstOnly,
    }

    /// Simulated ECU answering mode `01` (incl. support bitmaps) and `09 02`.
    #[derive(Debug, Clone)]
    pub struct SimEcu {
        pub pids: BTreeMap<u8, Vec<u8>>,
        pub multi: MultiMode,
        pub vin: Option<String>,
        /// Every request received, in order.
        pub log: Vec<Vec<u8>>,
    }

    impl SimEcu {
        pub fn new(multi: MultiMode) -> Self {
            SimEcu {
                pids: BTreeMap::new(),
                multi,
                vin: None,
                log: Vec::new(),
            }
        }

        /// Set a PID's raw data bytes.
        pub fn set(&mut self, pid: u8, data: &[u8]) -> &mut Self {
            self.pids.insert(pid, data.to_vec());
            self
        }

        /// A plausible turbo-diesel at `phase` (any counter; values wobble with
        /// it so a polling UI looks alive). Single bank: no `0x79`/`0x7B`.
        pub fn demo(phase: u64) -> Self {
            let f = ((phase % 6283) as f64 / 10.0).sin();
            let u16b = |v: f64| (v.round().clamp(0.0, 65535.0) as u16).to_be_bytes();
            let mut s = SimEcu::new(MultiMode::Full);
            s.vin = Some("WVWZZZ3CZ9E000000".into());
            let rpm = 1800.0 + 600.0 * f;
            let map = 150.0 + 40.0 * f;
            let egt = 450.0 + 80.0 * f;
            let [r0, r1] = u16b(rpm * 4.0);
            let [m0, m1] = u16b((40.0 + 15.0 * f) * 100.0);
            let [p0, p1] = u16b(map * 32.0);
            let [c0, c1] = u16b((map + 3.0) * 32.0);
            let [e0, e1] = u16b((egt + 40.0) * 10.0);
            s.set(0x04, &[(115.0 + 35.0 * f) as u8])
                .set(0x05, &[128])
                .set(0x06, &[130])
                .set(0x07, &[124])
                .set(0x0B, &[map as u8])
                .set(0x0C, &[r0, r1])
                .set(0x0D, &[(80.0 + 10.0 * f) as u8])
                .set(0x0E, &[138])
                .set(0x0F, &[75])
                .set(0x10, &[m0, m1])
                .set(0x11, &[77])
                .set(0x1F, &[0x02, 0x58])
                .set(0x21, &[0, 0])
                .set(0x23, &[0x2E, 0xE0])
                .set(0x2C, &[51])
                .set(0x2F, &[158])
                .set(0x31, &[0x04, 0xD2])
                .set(0x33, &[101])
                .set(0x42, &[0x37, 0x14])
                .set(0x43, &[0x00, 128])
                .set(0x44, &[0xC0, 0x00])
                .set(0x45, &[64])
                .set(0x46, &[52])
                .set(0x49, &[77])
                .set(0x4C, &[204])
                .set(0x5C, &[135])
                .set(0x5E, &[0x00, 130])
                .set(0x61, &[165])
                .set(0x62, &[163])
                .set(0x63, &[0x01, 0x40])
                .set(0x70, &[0x03, c0, c1, p0, p1, 0, 0, 0, 0, 0])
                .set(0x73, &[0x01, 0x2C, 0xEC, 0, 0])
                .set(0x78, &[0x03, e0, e1, 0x10, 0x68, 0, 0, 0, 0])
                .set(0x7A, &[0x01, 0x03, 0x52, 0, 0, 0, 0])
                .set(0x7C, &[0x03, 0x0E, 0x10, 0x0D, 0x48, 0, 0, 0, 0])
                .set(0x87, &[0x01, p0, p1, 0, 0]);
            s
        }

        fn bitmap(&self, base: u8) -> Option<Vec<u8>> {
            let hi = base as u16 + 0x20;
            if base != 0 && !self.pids.keys().any(|&p| p as u16 > base as u16) {
                return None;
            }
            let mut bits = 0u32;
            for &p in self.pids.keys() {
                let p = p as u16;
                if p > base as u16 && p <= hi {
                    bits |= 1 << (32 - (p - base as u16));
                }
            }
            if self.pids.keys().any(|&p| p as u16 > hi) {
                bits |= 1; // next range supported
            }
            Some(bits.to_be_bytes().to_vec())
        }

        fn answer(&self, pid: u8) -> Option<Vec<u8>> {
            if pid % 0x20 == 0 {
                self.bitmap(pid)
            } else {
                self.pids.get(&pid).cloned()
            }
        }
    }

    impl RawDiag for SimEcu {
        fn request(&mut self, req: &[u8]) -> Result<Vec<u8>, String> {
            self.log.push(req.to_vec());
            match req {
                [0x01] => Ok(vec![0x7F, 0x01, 0x13]),
                [0x01, pids @ ..] => {
                    if pids.len() > MAX_PIDS_PER_REQUEST {
                        return Ok(vec![0x7F, 0x01, 0x13]);
                    }
                    let pids = match (pids.len(), self.multi) {
                        (n, MultiMode::Reject) if n > 1 => return Ok(vec![0x7F, 0x01, 0x12]),
                        (_, MultiMode::FirstOnly) => &pids[..1],
                        _ => pids,
                    };
                    let mut resp = vec![0x41];
                    for &p in pids {
                        if let Some(d) = self.answer(p) {
                            resp.push(p);
                            resp.extend(d);
                        }
                    }
                    if resp.len() == 1 {
                        Err("NO DATA".into())
                    } else {
                        Ok(resp)
                    }
                }
                [0x09, 0x02] => match &self.vin {
                    Some(v) => {
                        let mut r = vec![0x49, 0x02, 0x01];
                        r.extend(v.bytes());
                        Ok(r)
                    }
                    None => Ok(vec![0x7F, 0x09, 0x12]),
                },
                [sid, ..] => Ok(vec![0x7F, *sid, 0x11]),
                [] => Err("empty request".into()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::sim::{MultiMode, SimEcu};
    use super::*;

    fn val(key: &str, data: &[u8]) -> ObdValue {
        channel(key).unwrap().decode.apply(data).unwrap()
    }

    fn sc(key: &str, data: &[u8]) -> f64 {
        match val(key, data) {
            ObdValue::Scalar(v) => v,
            ObdValue::Array(a) => panic!("{key}: expected scalar, got {a:?}"),
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn table_is_well_formed() {
        let mut keys = BTreeSet::new();
        for c in CHANNELS {
            assert!(keys.insert(c.key), "duplicate key {}", c.key);
            assert!(c.min < c.max, "{}: min < max", c.key);
            if let (Some(w), Some(k)) = (c.warn, c.crit) {
                assert!(w < k, "{}: warn < crit", c.key);
            }
            assert!(
                c.key
                    .chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_'),
                "{} is snake_case",
                c.key
            );
            // Every row of a PID agrees on its data length.
            if !c.is_derived() {
                assert_eq!(pid_len(c.pid), Some(c.len as usize), "{}", c.key);
            }
        }
        for pid in [
            0x04, 0x05, 0x06, 0x07, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10, 0x11, 0x1F, 0x21, 0x22,
            0x23, 0x2C, 0x2F, 0x31, 0x33, 0x42, 0x43, 0x44, 0x45, 0x46, 0x49, 0x4C, 0x5C, 0x5E,
            0x61, 0x62, 0x63, 0x70, 0x73, 0x78, 0x79, 0x7A, 0x7B, 0x7C, 0x87,
        ] {
            assert!(
                channels_for_pid(pid).next().is_some(),
                "PID {pid:02X} covered"
            );
        }
    }

    #[test]
    fn single_byte_formulas() {
        assert!(close(sc("engine_load", &[0xFF]), 100.0));
        assert!(close(sc("engine_load", &[0x80]), 50.196078));
        assert!(close(sc("coolant_temp", &[0x7B]), 83.0));
        assert!(close(sc("coolant_temp", &[0x00]), -40.0));
        assert!(close(sc("stft_b1", &[0x80]), 0.0));
        assert!(close(sc("stft_b1", &[0x00]), -100.0));
        assert!(close(sc("ltft_b1", &[0xFF]), 99.21875));
        assert!(close(sc("intake_map", &[0xB4]), 180.0));
        assert!(close(sc("speed", &[0x50]), 80.0));
        assert!(close(sc("timing_advance", &[0x80]), 0.0));
        assert!(close(sc("timing_advance", &[0x8A]), 5.0));
        assert!(close(sc("intake_air_temp", &[0x46]), 30.0));
        assert!(close(sc("throttle_pos", &[0x33]), 20.0));
        assert!(close(sc("commanded_egr", &[0xFF]), 100.0));
        assert!(close(sc("fuel_level", &[0x66]), 40.0));
        assert!(close(sc("baro_pressure", &[0x65]), 101.0));
        assert!(close(sc("rel_throttle", &[0x00]), 0.0));
        assert!(close(sc("ambient_temp", &[0x34]), 12.0));
        assert!(close(sc("accel_pedal_d", &[0xCC]), 80.0));
        assert!(close(sc("commanded_throttle", &[0x99]), 60.0));
        assert!(close(sc("oil_temp", &[0x87]), 95.0));
        assert!(close(sc("torque_demand", &[0x7D]), 0.0));
        assert!(close(sc("torque_actual", &[0xFF]), 130.0));
        assert!(close(sc("torque_actual", &[0x00]), -125.0));
    }

    #[test]
    fn two_byte_formulas() {
        assert!(close(sc("rpm", &[0x1A, 0xF8]), 1726.0));
        assert!(close(sc("rpm", &[0xFF, 0xFF]), 16383.75));
        assert!(close(sc("maf", &[0x01, 0x2C]), 3.0));
        assert!(close(sc("run_time", &[0x01, 0x00]), 256.0));
        assert!(close(sc("distance_mil", &[0x00, 0x0A]), 10.0));
        assert!(close(sc("fuel_rail_pressure_rel", &[0x00, 0x64]), 7.9));
        assert!(close(sc("fuel_rail_pressure", &[0x00, 0x64]), 1000.0));
        assert!(close(sc("distance_since_clear", &[0x04, 0xD2]), 1234.0));
        assert!(close(sc("module_voltage", &[0x36, 0xB0]), 14.0));
        assert!(close(sc("abs_load", &[0x00, 0xFF]), 100.0));
        assert!(close(sc("commanded_lambda", &[0x80, 0x00]), 1.0));
        assert!(close(sc("fuel_rate", &[0x00, 0x64]), 5.0));
        assert!(close(sc("torque_ref", &[0x01, 0x40]), 320.0));
    }

    #[test]
    fn support_byte_formulas() {
        // PID 70: commanded A = 5920/32 = 185 kPa, actual A = 5760/32 = 180.
        let d70 = [0x03, 0x17, 0x20, 0x16, 0x80, 0, 0, 0, 0, 0];
        assert!(close(sc("boost_control_cmd", &d70), 185.0));
        assert!(close(sc("boost_control", &d70), 180.0));
        // Actual sensor not flagged as supported.
        let only_cmd = [0x01, 0x17, 0x20, 0x16, 0x80, 0, 0, 0, 0, 0];
        assert!(channel("boost_control")
            .unwrap()
            .decode
            .apply(&only_cmd)
            .is_err());
        // PID 73: 11500 * 0.01 = 115 kPa bank 1; bank 2 used when 1 is absent.
        assert!(close(
            sc("exhaust_pressure", &[0x01, 0x2C, 0xEC, 0x00, 0x00]),
            115.0
        ));
        assert!(close(
            sc("exhaust_pressure", &[0x02, 0, 0, 0x27, 0x10]),
            100.0
        ));
        // PID 7A: signed delta pressure.
        assert!(close(sc("dpf_dp_b1", &[0x01, 0x03, 0x52, 0, 0, 0, 0]), 8.5));
        assert!(close(
            sc("dpf_dp_b2", &[0x01, 0xFF, 0x9C, 0, 0, 0, 0]),
            -1.0
        ));
        // PID 7C: (raw/10) - 40.
        let d7c = [0x03, 0x0E, 0x10, 0x0D, 0x48, 0, 0, 0, 0];
        assert!(close(sc("dpf_temp", &d7c), 320.0));
        assert!(close(sc("dpf_temp_out", &d7c), 300.0));
        // PID 87: sensor A, or B when A is unsupported.
        assert!(close(
            sc("intake_map_abs", &[0x01, 0x16, 0xD0, 0, 0]),
            182.5
        ));
        assert!(close(
            sc("intake_map_abs", &[0x02, 0, 0, 0x0C, 0x80]),
            100.0
        ));
        assert!(channel("intake_map_abs")
            .unwrap()
            .decode
            .apply(&[0, 0, 0, 0, 0])
            .is_err());
    }

    #[test]
    fn egt_multi_sensor() {
        let d = [0x03, 0x13, 0x24, 0x10, 0x68, 0, 0, 0, 0];
        assert_eq!(
            val("egt_b1", &d),
            ObdValue::Array(vec![Some(450.0), Some(380.0), None, None])
        );
        // Sensors 1 and 3 only: positional, garbage in slot 2 ignored.
        let d = [0x05, 0x13, 0x24, 0xFF, 0xFF, 0x10, 0x68, 0, 0];
        assert_eq!(
            val("egt_b2", &d),
            ObdValue::Array(vec![Some(450.0), None, Some(380.0), None])
        );
        // All four.
        let d = [0x0F, 0x01, 0x90, 0x03, 0x20, 0x04, 0xB0, 0x06, 0x40];
        assert_eq!(
            val("egt_b1", &d),
            ObdValue::Array(vec![Some(0.0), Some(40.0), Some(80.0), Some(120.0)])
        );
        // None flagged.
        assert!(channel("egt_b1").unwrap().decode.apply(&[0; 9]).is_err());
        // Short data.
        assert!(channel("egt_b1")
            .unwrap()
            .decode
            .apply(&[0x01, 0x13])
            .is_err());
    }

    #[test]
    fn short_data_is_an_error() {
        assert!(channel("rpm").unwrap().decode.apply(&[0x1A]).is_err());
        assert!(channel("coolant_temp").unwrap().decode.apply(&[]).is_err());
    }

    #[test]
    fn bitmap_parsing() {
        // Classic example: 0100 -> BE 1F A8 13.
        let p = parse_support_bitmap(0x00, &[0xBE, 0x1F, 0xA8, 0x13]);
        assert_eq!(
            p,
            vec![
                0x01, 0x03, 0x04, 0x05, 0x06, 0x07, 0x0C, 0x0D, 0x0E, 0x0F, 0x10, 0x11, 0x13, 0x15,
                0x1C, 0x1F, 0x20
            ]
        );
        assert_eq!(
            parse_support_bitmap(0x40, &[0x80, 0, 0, 0x01]),
            vec![0x41, 0x60]
        );
        assert!(parse_support_bitmap(0x20, &[0, 0, 0, 0]).is_empty());
    }

    #[test]
    fn multi_response_parsing() {
        // 0C (2) + 05 (1) + 78 (9) concatenated.
        let body = [
            0x0C, 0x1A, 0xF8, 0x05, 0x7B, 0x78, 0x01, 0x13, 0x24, 0, 0, 0, 0, 0, 0,
        ];
        let (m, err) = parse_multi_response(&body);
        assert!(err.is_none());
        assert_eq!(m[&0x0C], vec![0x1A, 0xF8]);
        assert_eq!(m[&0x05], vec![0x7B]);
        assert_eq!(m[&0x78].len(), 9);
        // Truncated tail keeps what parsed.
        let (m, err) = parse_multi_response(&[0x05, 0x7B, 0x0C, 0x1A]);
        assert_eq!(m.len(), 1);
        assert!(err.unwrap().contains("truncated"));
        // Unknown PID stops parsing.
        let (m, err) = parse_multi_response(&[0x05, 0x7B, 0x02, 0x00, 0x0D, 0x10]);
        assert_eq!(m.len(), 1);
        assert!(err.is_some());
    }

    #[test]
    fn discovery_walks_ranges() {
        let mut ecu = SimEcu::demo(0);
        let mut s = ObdSession::new();
        let set = s.discover(&mut ecu).unwrap();
        assert!(set.contains(&0x0C) && set.contains(&0x87) && set.contains(&0x5C));
        assert!(!set.contains(&0x79) && !set.contains(&0x20) && !set.contains(&0x40));
        assert_eq!(set.len(), ecu.pids.len());
        // 00,20,40,60,80 asked; 0x80 range has no successor (no PID > 0x87).
        let asked: Vec<u8> = ecu.log.iter().map(|r| r[1]).collect();
        assert_eq!(asked, vec![0x00, 0x20, 0x40, 0x60, 0x80]);
        assert_eq!(s.supported(), Some(&set));
    }

    #[test]
    fn discovery_handles_silent_range() {
        // ECU claims 0x20 range support but never answers 0120.
        let mut calls = 0;
        let mut diag = |req: &[u8]| -> Result<Vec<u8>, String> {
            calls += 1;
            match req {
                [0x01, 0x00] => Ok(vec![0x41, 0x00, 0x18, 0x00, 0x00, 0x01]),
                _ => Err("NO DATA".into()),
            }
        };
        let set = ObdSession::new().discover(&mut diag).unwrap();
        assert_eq!(set.into_iter().collect::<Vec<_>>(), vec![0x04, 0x05]);
        assert_eq!(calls, 2);
        // No answer to 0100 at all is an error.
        let mut dead = |_: &[u8]| -> Result<Vec<u8>, String> { Err("NO DATA".into()) };
        assert!(ObdSession::new().discover(&mut dead).is_err());
    }

    #[test]
    fn multi_pid_read_batches_and_reports_missing() {
        let mut ecu = SimEcu::demo(0);
        let mut s = ObdSession::new();
        // 7 PIDs incl. unsupported 0x79 -> two requests (6 + 1).
        let pids = [0x0C, 0x05, 0x0F, 0x79, 0x10, 0x78, 0x42];
        let ro = s.read(&mut ecu, &pids);
        assert_eq!(ecu.log.len(), 2);
        assert_eq!(ecu.log[0].len(), 7);
        assert_eq!(s.multi_pid(), Some(true));
        assert!(close(
            ro.values["rpm"].as_ref().unwrap().first().unwrap(),
            1800.0
        ));
        assert!(close(
            ro.values["coolant_temp"].as_ref().unwrap().first().unwrap(),
            88.0
        ));
        assert!(close(
            ro.values["module_voltage"]
                .as_ref()
                .unwrap()
                .first()
                .unwrap(),
            14.1
        ));
        assert_eq!(
            ro.values["egt_b1"],
            Some(ObdValue::Array(vec![Some(450.0), Some(380.0), None, None]))
        );
        assert_eq!(ro.values["egt_b2"], None);
        assert_eq!(ro.errors["egt_b2"], NOT_SUPPORTED);
        // Second poll skips the learned-absent PID entirely.
        ecu.log.clear();
        let ro = s.read(&mut ecu, &pids);
        assert_eq!(ecu.log.len(), 1); // the 6 remaining PIDs fit one request
        assert!(!ecu.log.iter().any(|r| r.contains(&0x79)));
        assert_eq!(ro.errors["egt_b2"], NOT_SUPPORTED);
    }

    #[test]
    fn multi_pid_rejected_falls_back_to_singles() {
        let mut ecu = SimEcu::demo(0);
        ecu.multi = MultiMode::Reject;
        let mut s = ObdSession::new();
        let ro = s.read(&mut ecu, &[0x0C, 0x0D, 0x05]);
        assert_eq!(s.multi_pid(), Some(false));
        assert_eq!(ro.values.len(), 3);
        assert!(ro.errors.is_empty());
        assert_eq!(ecu.log.len(), 4); // 1 multi + 3 singles
        ecu.log.clear();
        s.read(&mut ecu, &[0x0C, 0x0D, 0x05]);
        assert!(ecu.log.iter().all(|r| r.len() == 2), "{:?}", ecu.log);
    }

    #[test]
    fn first_only_ecu_is_detected() {
        let mut ecu = SimEcu::demo(0);
        ecu.multi = MultiMode::FirstOnly;
        let mut s = ObdSession::new();
        let ro = s.read(&mut ecu, &[0x0C, 0x0D, 0x05]);
        assert_eq!(s.multi_pid(), Some(false));
        assert!(ro.errors.is_empty(), "{:?}", ro.errors);
        assert!(close(
            ro.values["speed"].as_ref().unwrap().first().unwrap(),
            80.0
        ));
    }

    #[test]
    fn supported_set_filters_requests() {
        let mut ecu = SimEcu::demo(0);
        let mut s = ObdSession::new();
        s.discover(&mut ecu).unwrap();
        ecu.log.clear();
        let ro = s.read(&mut ecu, &[0x0C, 0x7B, 0x01]);
        assert_eq!(ro.errors["dpf_dp_b2"], NOT_SUPPORTED);
        assert!(ro.errors["pid_01"].contains("unknown PID"));
        assert_eq!(ecu.log, vec![vec![0x01, 0x0C]]);
    }

    #[test]
    fn derived_boost() {
        let mut ecu = SimEcu::demo(0);
        let mut s = ObdSession::new();
        // MAP via 0x0B only: baro is fetched implicitly, not reported.
        let ro = s.read(&mut ecu, &[0x0B]);
        assert!(close(
            ro.values["boost_kpa"].as_ref().unwrap().first().unwrap(),
            49.0
        ));
        assert!(close(
            ro.values["boost_bar"].as_ref().unwrap().first().unwrap(),
            0.49
        ));
        assert!(!ro.values.contains_key("baro_pressure"));
        // 0x87 (higher resolution) wins over 0x0B.
        let ro = s.read(&mut ecu, &[0x0B, 0x87, 0x33]);
        assert!(close(
            ro.values["boost_kpa"].as_ref().unwrap().first().unwrap(),
            49.0
        ));
        assert!(close(
            ro.values["baro_pressure"]
                .as_ref()
                .unwrap()
                .first()
                .unwrap(),
            101.0
        ));
        // No baro -> no boost.
        ecu.pids.remove(&0x33);
        let ro = ObdSession::new().read(&mut ecu, &[0x0B]);
        assert!(!ro.values.contains_key("boost_kpa"));
    }

    #[test]
    fn single_errors_are_reported_per_key() {
        let mut diag = |req: &[u8]| -> Result<Vec<u8>, String> {
            match req {
                [0x01, 0x0C] => Ok(vec![0x41, 0x0C, 0x0F, 0xA0]),
                [0x01, 0x05] => Ok(vec![0x7F, 0x01, 0x31]),
                [0x01, 0x0D] => Ok(vec![0x41, 0x0D]),
                _ => Err("NO DATA".into()),
            }
        };
        let ro = ObdSession::new().read(&mut diag, &[0x0C, 0x05, 0x0D]);
        assert!(close(
            ro.values["rpm"].as_ref().unwrap().first().unwrap(),
            1000.0
        ));
        assert!(ro.errors["coolant_temp"].contains("NRC 31"));
        assert!(ro.errors["speed"].contains("short response"));
    }

    #[test]
    fn vin_mode09_and_uds_fallback() {
        let mut ecu = SimEcu::demo(0);
        assert_eq!(read_vin(&mut ecu).as_deref(), Some("WVWZZZ3CZ9E000000"));
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
        assert_eq!(read_vin(&mut uds).as_deref(), Some("1HGCM82633A004352"));
        let mut none = |_: &[u8]| -> Result<Vec<u8>, String> { Err("NO DATA".into()) };
        assert_eq!(read_vin(&mut none), None);
    }

    #[test]
    fn readout_serialises_to_contract_shape() {
        let mut ecu = SimEcu::demo(0);
        let ro = ObdSession::new().read(&mut ecu, &[0x78, 0x0C, 0x79]);
        let j = serde_json::to_value(&ro).unwrap();
        assert!(j["values"]["rpm"].is_number());
        assert!(j["values"]["egt_b1"].is_array());
        assert!(j["values"]["egt_b1"][2].is_null());
        assert!(j["values"]["egt_b2"].is_null());
        assert!(j["errors"]["egt_b2"].is_string());
    }
}
