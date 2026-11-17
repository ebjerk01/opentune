//! A minimal **live datalogging loop** over a parsed [`LogDef`].
//!
//! The rest of this crate only *describes* how a parameter would be read. This
//! module actually reads it: for each selected parameter it issues the read its
//! addressing implies over a [`RawDiag`] transport, decodes the raw integer, and
//! applies the parameter's linear conversion. It is deliberately conservative —
//! three addressing kinds are driven today:
//!
//! * [`Addressing::UdsDid`] — UDS `0x22` (ReadDataByIdentifier), expecting a
//!   `62 <did-hi> <did-lo> <data…>` positive response.
//! * [`Addressing::ObdPid`] — OBD-II service `0x01` (show current data),
//!   expecting a `41 <pid> <data…>` positive response.
//! * [`Addressing::Memory`] — UDS `0x23` (ReadMemoryByAddress) with the fixed
//!   `addressAndLengthFormatIdentifier` [`MEM_ALFID`] (4-byte address, 1-byte
//!   length), expecting a `63 <data…>` positive response; a param with a `bit`
//!   index surfaces that single bit.
//!
//! [`Addressing::Unknown`] is reported as undetermined rather than guessed. A
//! negative (`0x7F …`) or short response is likewise recorded in the sample's
//! note, never guessed, and a non-linear conversion yields the raw value with a
//! note instead of an approximation.

use crate::model::{Addressing, ConversionFit, LogDef, LogParam};
use opentune_core::ecudef::Endian;
use opentune_core::RawDiag;
use std::io::Write;

/// `addressAndLengthFormatIdentifier` used for every [`Addressing::Memory`]
/// read: the high nibble is the byte count of the `memorySize` field (1) and the
/// low nibble is the byte count of the `memoryAddress` field (4), i.e. a 4-byte
/// big-endian address followed by a 1-byte length. This matches the `u32`
/// address / `u8` length the model carries, so no address or length that fits
/// the model can overflow the request.
const MEM_ALFID: u8 = 0x14;

/// One sampled parameter at one point in time.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    /// The parameter's id in the definition.
    pub param_id: String,
    /// Human-readable parameter name.
    pub name: String,
    /// Raw integer read from the ECU, if the read succeeded.
    pub raw: Option<u64>,
    /// Physical value after the (linear) conversion, if one could be applied.
    pub value: Option<f64>,
    /// Physical units.
    pub units: String,
    /// Why `raw`/`value` is missing, or any caveat (empty when fully decoded).
    pub note: String,
}

/// Decode `bytes` as an unsigned integer (up to 8 bytes) with the given endian.
fn decode_uint(bytes: &[u8], endian: Endian) -> u64 {
    let take = bytes.len().min(8);
    let mut v = 0u64;
    match endian {
        Endian::Big => {
            for &b in &bytes[..take] {
                v = (v << 8) | u64::from(b);
            }
        }
        Endian::Little => {
            for (i, &b) in bytes[..take].iter().enumerate() {
                v |= u64::from(b) << (8 * i);
            }
        }
    }
    v
}

/// Apply a parameter's conversion to a raw value, filling `value`/`units`/`note`.
fn apply_conversion(s: &mut Sample, p: &LogParam, raw: u64) {
    match &p.conversion {
        Some(c) => {
            if !c.units.is_empty() {
                s.units = c.units.clone();
            }
            match &c.fit {
                ConversionFit::Linear { factor, offset } => {
                    s.value = Some(raw as f64 * factor + offset);
                }
                ConversionFit::NonLinear { reason } => {
                    s.note = format!("non-linear conversion not evaluated: {reason}");
                }
            }
        }
        // No conversion: the raw value is the physical value.
        None => s.value = Some(raw as f64),
    }
}

/// Poll each selected parameter **once**. An empty `ids` selects every enabled
/// parameter. Each parameter always yields a [`Sample`]; a failed or
/// unsupported read is recorded in `note`, never silently dropped.
pub fn poll_once(diag: &mut dyn RawDiag, def: &LogDef, ids: &[&str]) -> Vec<Sample> {
    let mut out = Vec::new();
    for (_proto, _ecu, p) in def.params() {
        if p.enabled == Some(false) {
            continue;
        }
        if !ids.is_empty() && !ids.contains(&p.id.as_str()) {
            continue;
        }

        let mut s = Sample {
            param_id: p.id.clone(),
            name: p.name.clone(),
            raw: None,
            value: None,
            units: p.units.clone(),
            note: String::new(),
        };

        match &p.addressing {
            Addressing::UdsDid { did } => {
                let req = [0x22u8, (did >> 8) as u8, (did & 0xFF) as u8];
                match diag.request(&req) {
                    Ok(resp) => {
                        if resp.len() >= 3 && resp[0] == 0x62 && resp[1] == req[1] && resp[2] == req[2]
                        {
                            let raw = decode_uint(&resp[3..], p.endian);
                            s.raw = Some(raw);
                            apply_conversion(&mut s, p, raw);
                        } else if resp.first() == Some(&0x7F) {
                            s.note = format!("negative response {resp:02X?}");
                        } else {
                            s.note = format!("unexpected response {resp:02X?}");
                        }
                    }
                    Err(e) => s.note = e,
                }
            }
            Addressing::ObdPid { pid } => {
                // OBD-II service 0x01 "show current data": request is the service
                // byte followed by the single PID byte; a positive response
                // echoes `41 <pid>` and then the data bytes.
                let req = [0x01u8, *pid];
                match diag.request(&req) {
                    Ok(resp) => {
                        if resp.len() >= 3 && resp[0] == 0x41 && resp[1] == *pid {
                            let raw = decode_uint(&resp[2..], p.endian);
                            s.raw = Some(raw);
                            apply_conversion(&mut s, p, raw);
                        } else if resp.first() == Some(&0x7F) {
                            s.note = format!("negative response {resp:02X?}");
                        } else if resp.len() < 3
                            && resp.first() == Some(&0x41)
                            && resp.get(1) == Some(pid)
                        {
                            s.note = format!("short response, no PID data: {resp:02X?}");
                        } else {
                            s.note = format!("unexpected response {resp:02X?}");
                        }
                    }
                    Err(e) => s.note = e,
                }
            }
            Addressing::Memory {
                address,
                length,
                bit,
            } => {
                // UDS 0x23 ReadMemoryByAddress with the fixed MEM_ALFID: ALFID,
                // 4-byte big-endian address, 1-byte length. A positive response
                // is `63 <data…>` with no echo of the address.
                let req = [
                    0x23u8,
                    MEM_ALFID,
                    (address >> 24) as u8,
                    (address >> 16) as u8,
                    (address >> 8) as u8,
                    *address as u8,
                    *length,
                ];
                match diag.request(&req) {
                    Ok(resp) => {
                        if resp.first() == Some(&0x63) {
                            let data = &resp[1..];
                            if data.len() < usize::from(*length) {
                                s.note = format!(
                                    "short response, expected {length} data byte(s), got {}: {resp:02X?}",
                                    data.len()
                                );
                            } else {
                                let word = decode_uint(data, p.endian);
                                match bit {
                                    Some(b) => {
                                        // Surface the single requested bit (0/1).
                                        let value = (word >> u64::from(*b)) & 1;
                                        s.raw = Some(value);
                                        apply_conversion(&mut s, p, value);
                                        s.note = format!("bit {b}");
                                    }
                                    None => {
                                        s.raw = Some(word);
                                        apply_conversion(&mut s, p, word);
                                    }
                                }
                            }
                        } else if resp.first() == Some(&0x7F) {
                            s.note = format!("negative response {resp:02X?}");
                        } else {
                            s.note = format!("unexpected response {resp:02X?}");
                        }
                    }
                    Err(e) => s.note = e,
                }
            }
            Addressing::Unknown => {
                s.note = "addressing could not be determined from the definition".into();
            }
        }
        out.push(s);
    }
    out
}

/// Poll `rounds` times, writing a CSV (header + one row per sample per round) to
/// `out`. Between rounds it sleeps `interval_ms` (0 = no sleep, e.g. in tests).
pub fn poll_to_csv(
    diag: &mut dyn RawDiag,
    def: &LogDef,
    ids: &[&str],
    rounds: usize,
    interval_ms: u64,
    out: &mut impl Write,
) -> std::io::Result<()> {
    writeln!(out, "round,param_id,name,raw,value,units,note")?;
    for r in 0..rounds {
        for s in poll_once(diag, def, ids) {
            let raw = s.raw.map(|v| v.to_string()).unwrap_or_default();
            let value = s.value.map(|v| v.to_string()).unwrap_or_default();
            // Keep the CSV single-line and comma-safe.
            let name = s.name.replace(',', ";");
            let note = s.note.replace(['\n', ','], " ");
            writeln!(
                out,
                "{r},{},{name},{raw},{value},{},{note}",
                s.param_id, s.units
            )?;
        }
        if interval_ms > 0 && r + 1 < rounds {
            std::thread::sleep(std::time::Duration::from_millis(interval_ms));
        }
    }
    Ok(())
}
