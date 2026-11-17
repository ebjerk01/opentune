//! A minimal **live datalogging loop** over a parsed [`LogDef`].
//!
//! The rest of this crate only *describes* how a parameter would be read. This
//! module actually reads it: for each selected parameter it issues the read its
//! addressing implies over a [`RawDiag`] transport, decodes the raw integer, and
//! applies the parameter's linear conversion. It is deliberately conservative —
//! only UDS `0x22` (ReadDataByIdentifier) reads are driven today; OBD-II PID and
//! raw-memory/SSM reads are reported as not-yet-supported rather than guessed,
//! and a non-linear conversion yields the raw value with a note instead of an
//! approximation.

use crate::model::{Addressing, ConversionFit, LogDef, LogParam};
use opentune_core::ecudef::Endian;
use opentune_core::RawDiag;
use std::io::Write;

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
            Addressing::ObdPid { .. } => {
                s.note = "OBD-II PID polling not implemented yet".into();
            }
            Addressing::Memory { .. } => {
                s.note = "raw memory / SSM read not implemented yet".into();
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
