//! # opentune-autoscan
//!
//! Full-vehicle **module auto-scan** for OpenTune, in the spirit of ForScan or
//! VCDS' "Auto-Scan": walk a list of a vehicle's diagnostic modules and, for
//! each one, find out whether it answers, read its identity, and read its
//! stored **Diagnostic Trouble Codes**.
//!
//! ## Design
//!
//! * The engine talks to the vehicle through one tiny abstraction, [`RawDiag`]
//!   — a single `request(bytes) -> bytes` method, with a blanket impl so a
//!   closure is a transport. This crate deliberately does **not** depend on
//!   `opentune-coding` or `opentune-adaptation`; the only OpenTune dependency is
//!   [`opentune_dtc`], whose DTC database and decoder are reused rather than
//!   reimplemented.
//! * The vehicle's modules are **data, not code**: a [`VehicleModules`] list
//!   (serde JSON) of [`ModuleAddr`] entries — the known diagnostic addresses to
//!   probe. Supporting a new car is a new data file.
//! * [`scan`] is **read-only**. It never writes to a module. The one mutating
//!   operation, clearing stored codes, lives in [`clear_all`] and is only run
//!   when the caller asks for it explicitly.
//!
//! ## Addressing
//!
//! Each [`ModuleAddr`] carries the module's diagnostic `addr` and CAN
//! `req_id` / `resp_id`. [`RawDiag`] is intentionally a bare byte channel, so
//! wiring those arbitration IDs to the physical layer is the transport's job: a
//! production `RawDiag` either routes by the addresses carried in the model (a
//! multiplexing bus) or is re-bound per module by the application. The engine
//! itself issues each module's requests on `diag` **in module order**, pinging a
//! module exactly once before reading from it.
//!
//! ## Honest response checking
//!
//! Every UDS exchange is validated the way `opentune-coding`'s engine validates
//! its own: a positive response must carry the matching response SID
//! (`request + 0x40`); a negative response (`0x7F`) is rejected with the reason;
//! a truncated `0x7F`, or one that echoes a *different* service, is rejected as
//! mis-correlated. A module that does not answer at all is recorded
//! `present = false` — not a hard error — and the scan moves on.
//!
//! ## Example
//!
//! ```
//! use opentune_autoscan::{scan, Protocol, ModuleAddr, VehicleModules};
//!
//! // One UDS engine module that answers TesterPresent and reports no DTCs.
//! let mut diag = |req: &[u8]| -> Result<Vec<u8>, String> {
//!     match req {
//!         [0x3E, 0x00] => Ok(vec![0x7E, 0x00]),            // TesterPresent ok
//!         [0x19, 0x02, _mask] => Ok(vec![0x59, 0x02, 0xFF]), // 0x19/02: no records
//!         [0x22, ..] => Err("no such DID".into()),          // identity: nothing
//!         _ => Err("no response".into()),
//!     }
//! };
//! let modules = VehicleModules {
//!     vehicle: "demo".into(),
//!     modules: vec![ModuleAddr {
//!         name: "Engine".into(),
//!         addr: 0x01,
//!         req_id: 0x7E0,
//!         resp_id: 0x7E8,
//!         protocol: Protocol::Uds,
//!     }],
//! };
//! let report = scan(&mut diag, &modules);
//! assert!(report.modules[0].present);
//! assert!(report.modules[0].dtcs.is_empty());
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use serde::{Deserialize, Serialize};

// Re-export so callers can classify a scanned DTC's system without a second
// dependency on the DTC crate.
pub use opentune_dtc::System;

/// Serde adapter for [`System`] (which has no serde impls of its own): encode
/// it as its canonical one-letter code.
mod system_serde {
    use super::System;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(system: &System, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_char(system.letter())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<System, D::Error> {
        let c = char::deserialize(de)?;
        System::from_letter(c)
            .ok_or_else(|| serde::de::Error::custom(format!("invalid DTC system letter {c:?}")))
    }
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// One UDS/OBD request → response exchange: send `req`, get the ECU's raw
/// response bytes back.
///
/// The response is returned **as received**, including a `0x7F` negative
/// response — the engine classifies that itself. An `Err` means the request did
/// not complete at the transport level (no answer, timeout, I/O error); the
/// engine treats that as "the module did not answer".
///
/// A blanket impl makes any `FnMut(&[u8]) -> Result<Vec<u8>, String>` a
/// `RawDiag`, so a closure (or a scripted responder in tests) is a transport.
pub use opentune_core::RawDiag;

// ---------------------------------------------------------------------------
// Module list (data-defined)
// ---------------------------------------------------------------------------

/// The diagnostic protocol spoken to a module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// ISO 14229 Unified Diagnostic Services (TesterPresent, ReadDataByIdentifier,
    /// ReadDTCInformation, ClearDiagnosticInformation).
    Uds,
    /// Legacy ISO 15031 / SAE J1979 OBD-II service modes (01, 03, 04).
    Obd,
}

/// One known diagnostic module address to probe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleAddr {
    /// Human-readable module name, e.g. `"Engine"` or `"ABS"`.
    pub name: String,
    /// The module's diagnostic address (ISO-TP target / logical ECU address).
    pub addr: u8,
    /// CAN arbitration id for requests sent to the module.
    pub req_id: u32,
    /// CAN arbitration id the module answers on.
    pub resp_id: u32,
    /// The protocol used to talk to this module.
    pub protocol: Protocol,
}

/// A vehicle's full module list: the data that drives a scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VehicleModules {
    /// Vehicle identifier (model / platform label — free text).
    pub vehicle: String,
    /// The known diagnostic addresses to probe.
    pub modules: Vec<ModuleAddr>,
}

impl VehicleModules {
    /// Parse a module list from its JSON form.
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    /// Serialise the module list to pretty JSON.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

// ---------------------------------------------------------------------------
// Scan result model
// ---------------------------------------------------------------------------

/// A diagnostic trouble code found on a module: its decoded code and
/// description (via [`opentune_dtc`]) plus the UDS status byte when one was
/// supplied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dtc {
    /// Canonical 5-character code, e.g. `"P0101"`.
    pub code: String,
    /// Human-readable description from the bundled DTC table, or a generic
    /// fallback for a code that is not in it.
    pub description: String,
    /// The vehicle system the code belongs to (from its first letter).
    /// Serialised as its one-letter code (`"P"`/`"C"`/`"B"`/`"U"`).
    #[serde(with = "system_serde")]
    pub system: System,
    /// ISO 14229-1 `DTCStatusMask` byte for a UDS record; `None` for an OBD
    /// mode-03 code, which carries no status.
    pub status: Option<u8>,
}

/// One `DID -> value` reading taken while identifying a module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DidValue {
    /// The data identifier that was read, e.g. `0xF187`.
    pub did: u16,
    /// A short label for the DID (e.g. `"part_number"`).
    pub label: String,
    /// The raw value bytes.
    pub raw: Vec<u8>,
    /// The value decoded as text, when every byte is printable ASCII.
    pub text: Option<String>,
}

/// Identity information read from a (UDS) module.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    /// Vehicle Identification Number (DID `0xF190`), when the module returned
    /// a printable one.
    pub vin: Option<String>,
    /// Other identity DIDs that were read (part number, software version, …).
    pub dids: Vec<DidValue>,
}

/// The result of probing one module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleResult {
    /// The module name, copied from its [`ModuleAddr`].
    pub name: String,
    /// Whether the module answered the presence probe.
    pub present: bool,
    /// Identity read from the module (UDS only); `None` if absent, not read, or
    /// the read failed.
    pub identity: Option<Identity>,
    /// Stored diagnostic trouble codes read from the module.
    pub dtcs: Vec<Dtc>,
    /// A non-fatal error encountered while reading a *present* module's identity
    /// or DTCs. The module still appears in the report; the scan continues.
    pub error: Option<String>,
}

/// The outcome of a whole-vehicle scan: one [`ModuleResult`] per module probed,
/// in the order they appear in the [`VehicleModules`] list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VehicleScan {
    /// Per-module results.
    pub modules: Vec<ModuleResult>,
}

impl VehicleScan {
    /// The modules that answered the presence probe.
    pub fn present(&self) -> impl Iterator<Item = &ModuleResult> {
        self.modules.iter().filter(|m| m.present)
    }

    /// Total number of DTCs found across all modules.
    pub fn dtc_count(&self) -> usize {
        self.modules.iter().map(|m| m.dtcs.len()).sum()
    }
}

/// The result of clearing one module's stored codes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClearResult {
    /// The module name.
    pub name: String,
    /// Whether the module was present and acknowledged the clear.
    pub cleared: bool,
    /// Why the clear did not happen, when it did not.
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// UDS / OBD service constants
// ---------------------------------------------------------------------------

const SID_TESTER_PRESENT: u8 = 0x3E;
const SID_READ_DATA_BY_ID: u8 = 0x22;
const SID_READ_DTC_INFO: u8 = 0x19;
const SID_CLEAR_DTC: u8 = 0x14;
const DTC_SUBFN_BY_STATUS_MASK: u8 = 0x02;
const DTC_STATUS_MASK_ALL: u8 = 0xFF;

const OBD_MODE_CURRENT_DATA: u8 = 0x01;
const OBD_MODE_READ_DTC: u8 = 0x03;
const OBD_MODE_CLEAR_DTC: u8 = 0x04;
const OBD_PID_SUPPORTED: u8 = 0x00;

/// DID `0xF190` — VIN (ISO 14229-1 identification DID).
const DID_VIN: u16 = 0xF190;

/// Identity DIDs read from a UDS module, with their labels. `0xF190` (VIN) is
/// handled separately and lifted into [`Identity::vin`].
const IDENTITY_DIDS: &[(u16, &str)] = &[
    (0xF187, "part_number"),       // VehicleManufacturerSparePartNumber
    (0xF189, "sw_version"),        // VehicleManufacturerECUSoftwareVersionNumber
    (0xF191, "hw_number"),         // VehicleManufacturerECUHardwareNumber
    (0xF195, "sw_version_number"), // SystemSupplierECUSoftwareVersionNumber
];

// ---------------------------------------------------------------------------
// Response checking (mirrors opentune-coding's `check`)
// ---------------------------------------------------------------------------

/// Validate a UDS response against the request's service id (`req_sid`): accept
/// a positive response (`req_sid + 0x40`), map a negative response (`0x7F`) to a
/// descriptive error, and reject a truncated `0x7F` or one that echoes a
/// *different* requested service (a mis-correlated response).
fn check(req_sid: u8, resp: &[u8]) -> Result<(), String> {
    if resp.is_empty() {
        return Err("empty response".into());
    }
    if resp[0] == 0x7F {
        if resp.len() < 3 {
            return Err("truncated negative response".into());
        }
        if resp[1] != req_sid {
            return Err(format!(
                "negative response echoes service 0x{:02X}, expected 0x{req_sid:02X}",
                resp[1]
            ));
        }
        return Err(format!(
            "negative response 0x{:02X} to service 0x{req_sid:02X}",
            resp[2]
        ));
    }
    let want = req_sid.wrapping_add(0x40);
    if resp[0] != want {
        return Err(format!(
            "unexpected response SID 0x{:02X} (wanted 0x{want:02X})",
            resp[0]
        ));
    }
    Ok(())
}

/// Whether `resp` is a well-formed reply to our own service `req_sid` — a
/// positive response, or a negative response that correctly echoes our service.
/// Used for presence probing, where even a "service not supported" refusal
/// proves the module is on the bus and answering.
fn responded(req_sid: u8, resp: &[u8]) -> bool {
    match resp.first() {
        None => false,
        Some(&0x7F) => resp.len() >= 3 && resp[1] == req_sid,
        Some(&sid) => sid == req_sid.wrapping_add(0x40),
    }
}

// ---------------------------------------------------------------------------
// Per-module reads
// ---------------------------------------------------------------------------

/// Presence probe: TesterPresent (UDS) or a cheap "supported PIDs" read (OBD).
/// Returns `true` only if the module produced a well-formed reply to the probe.
fn ping(diag: &mut dyn RawDiag, module: &ModuleAddr) -> bool {
    let (req, sid): (Vec<u8>, u8) = match module.protocol {
        Protocol::Uds => (vec![SID_TESTER_PRESENT, 0x00], SID_TESTER_PRESENT),
        Protocol::Obd => (
            vec![OBD_MODE_CURRENT_DATA, OBD_PID_SUPPORTED],
            OBD_MODE_CURRENT_DATA,
        ),
    };
    match diag.request(&req) {
        Ok(resp) => responded(sid, &resp),
        Err(_) => false,
    }
}

/// Read one DID with ReadDataByIdentifier (0x22); returns its data bytes. The
/// positive response must echo the requested DID, so a mis-correlated reply is
/// rejected rather than taken as this DID's value.
fn read_did(diag: &mut dyn RawDiag, did: u16) -> Result<Vec<u8>, String> {
    let req = [SID_READ_DATA_BY_ID, (did >> 8) as u8, did as u8];
    let resp = diag.request(&req)?;
    check(SID_READ_DATA_BY_ID, &resp)?;
    if resp.len() < 3 || resp[1] != (did >> 8) as u8 || resp[2] != did as u8 {
        return Err(format!("0x22 response did not echo DID 0x{did:04X}"));
    }
    Ok(resp[3..].to_vec())
}

/// Decode a byte string as trimmed printable ASCII, or `None` if any byte is
/// non-printable (ignoring trailing NULs and spaces used as padding).
fn decode_ascii(raw: &[u8]) -> Option<String> {
    let trimmed: &[u8] = {
        let end = raw
            .iter()
            .rposition(|&b| b != 0x00 && b != b' ')
            .map(|i| i + 1)
            .unwrap_or(0);
        &raw[..end]
    };
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.iter().all(|&b| (0x20..=0x7E).contains(&b)) {
        Some(trimmed.iter().map(|&b| b as char).collect())
    } else {
        None
    }
}

/// Read a UDS module's identity: VIN (`0xF190`) plus the part/SW DIDs. A DID the
/// module does not provide is skipped, not treated as a failure; only an
/// unexpected protocol error (mis-correlated / truncated) stops identity read.
fn read_identity(diag: &mut dyn RawDiag) -> Result<Identity, String> {
    let mut identity = Identity::default();

    match read_did(diag, DID_VIN) {
        Ok(raw) => identity.vin = decode_ascii(&raw),
        Err(e) if is_unsupported(&e) => {}
        Err(e) => return Err(e),
    }

    for &(did, label) in IDENTITY_DIDS {
        match read_did(diag, did) {
            Ok(raw) => {
                let text = decode_ascii(&raw);
                identity.dids.push(DidValue {
                    did,
                    label: label.to_string(),
                    raw,
                    text,
                });
            }
            Err(e) if is_unsupported(&e) => {}
            Err(e) => return Err(e),
        }
    }

    Ok(identity)
}

/// Whether an error is a benign "this module does not offer that DID" — a UDS
/// negative response (the module answered, just not with data). A transport
/// failure or a mis-correlated reply is not benign.
fn is_unsupported(err: &str) -> bool {
    err.starts_with("negative response")
}

/// Read stored DTCs from a module, choosing the service by its protocol:
/// UDS ReadDTCInformation (0x19 sub 0x02 reportDTCByStatusMask), or OBD mode 03.
fn read_dtcs(diag: &mut dyn RawDiag, module: &ModuleAddr) -> Result<Vec<Dtc>, String> {
    match module.protocol {
        Protocol::Uds => read_dtcs_uds(diag),
        Protocol::Obd => read_dtcs_obd(diag),
    }
}

/// UDS path: `19 02 FF`, response `59 02 <availabilityMask> <records…>`, each
/// record `[dtc_hi, dtc_mid, dtc_lo, status]`. The records are handed to
/// [`opentune_dtc::decode_uds`] for code + status decoding.
fn read_dtcs_uds(diag: &mut dyn RawDiag) -> Result<Vec<Dtc>, String> {
    let req = [
        SID_READ_DTC_INFO,
        DTC_SUBFN_BY_STATUS_MASK,
        DTC_STATUS_MASK_ALL,
    ];
    let resp = diag.request(&req)?;
    check(SID_READ_DTC_INFO, &resp)?;
    // resp[0] = 0x59, resp[1] must echo the sub-function, resp[2] = availability
    // mask. Everything after is the 4-byte record list.
    if resp.len() < 2 || resp[1] != DTC_SUBFN_BY_STATUS_MASK {
        return Err(format!(
            "0x19 response did not echo sub-function 0x{DTC_SUBFN_BY_STATUS_MASK:02X}"
        ));
    }
    if resp.len() < 3 {
        return Err("0x19 response missing status-availability mask".into());
    }
    let records = &resp[3..];
    Ok(opentune_dtc::decode_uds(records)
        .into_iter()
        .map(|rec| Dtc {
            code: rec.dtc.code,
            description: rec.dtc.description,
            system: rec.dtc.system,
            status: Some(rec.status),
        })
        .collect())
}

/// OBD path: mode `03`, response `43 [count] <2-byte DTCs…>`. ISO 15031-5 codes
/// carry no status byte. Each 2-byte code uses the same bit layout as the first
/// two UDS DTC bytes, so [`opentune_dtc::format_code`] + [`opentune_dtc::lookup`]
/// decode it. `0x0000` pairs (no code / padding) are skipped.
fn read_dtcs_obd(diag: &mut dyn RawDiag) -> Result<Vec<Dtc>, String> {
    let resp = diag.request(&[OBD_MODE_READ_DTC])?;
    if resp.first() != Some(&(OBD_MODE_READ_DTC + 0x40)) {
        return Err(format!(
            "unexpected OBD mode-03 response 0x{:02X} (wanted 0x43)",
            resp.first().copied().unwrap_or(0)
        ));
    }
    let mut body = &resp[1..];
    // Some stacks prefix the DTC pairs with a count byte. If what remains is an
    // odd number of bytes, the leading byte can only be that count; drop it so
    // the rest parses as whole 2-byte codes.
    if body.len() % 2 == 1 {
        body = &body[1..];
    }
    let mut out = Vec::new();
    for pair in body.chunks_exact(2) {
        if pair == [0x00, 0x00] {
            continue; // no code / padding
        }
        let code = opentune_dtc::format_code(pair[0], pair[1], 0x00);
        let dtc = opentune_dtc::lookup(&code).unwrap_or(opentune_dtc::Dtc {
            code: code.clone(),
            description: "Manufacturer-specific / unknown code".to_string(),
            system: System::Powertrain,
        });
        out.push(Dtc {
            code: dtc.code,
            description: dtc.description,
            system: dtc.system,
            status: None,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

/// Scan a whole vehicle: probe each module in the list, and for every one that
/// answers, read its identity (UDS only) and its stored DTCs.
///
/// This is **read-only** — no module is ever written to. A module that does not
/// answer the presence probe is recorded with `present = false` and no error;
/// the scan always continues to the next module. A *present* module whose
/// identity or DTC read fails is still reported, with the reason in
/// [`ModuleResult::error`].
pub fn scan(diag: &mut dyn RawDiag, modules: &VehicleModules) -> VehicleScan {
    let mut results = Vec::with_capacity(modules.modules.len());

    for module in &modules.modules {
        if !ping(diag, module) {
            results.push(ModuleResult {
                name: module.name.clone(),
                present: false,
                identity: None,
                dtcs: Vec::new(),
                error: None,
            });
            continue;
        }

        let mut errors: Vec<String> = Vec::new();

        let identity = match module.protocol {
            Protocol::Uds => match read_identity(diag) {
                Ok(id) => Some(id),
                Err(e) => {
                    errors.push(format!("identity: {e}"));
                    None
                }
            },
            // Generic OBD modules are not identified by DID; mode 09 is out of
            // scope for this engine.
            Protocol::Obd => None,
        };

        let dtcs = match read_dtcs(diag, module) {
            Ok(d) => d,
            Err(e) => {
                errors.push(format!("dtc: {e}"));
                Vec::new()
            }
        };

        results.push(ModuleResult {
            name: module.name.clone(),
            present: true,
            identity,
            dtcs,
            error: if errors.is_empty() {
                None
            } else {
                Some(errors.join("; "))
            },
        });
    }

    VehicleScan { modules: results }
}

/// Clear stored DTCs on **every present module** with ClearDiagnosticInformation
/// (UDS `0x14` all-groups `FF FF FF`, or OBD mode `04`).
///
/// This is the engine's only mutating operation, and it is deliberately **not**
/// part of [`scan`]: the caller invokes it explicitly. Each module is pinged
/// first; absent modules are skipped (recorded `cleared = false`, no error).
pub fn clear_all(diag: &mut dyn RawDiag, modules: &VehicleModules) -> Vec<ClearResult> {
    let mut out = Vec::with_capacity(modules.modules.len());

    for module in &modules.modules {
        if !ping(diag, module) {
            out.push(ClearResult {
                name: module.name.clone(),
                cleared: false,
                error: None,
            });
            continue;
        }

        let result = match module.protocol {
            Protocol::Uds => {
                let req = [SID_CLEAR_DTC, 0xFF, 0xFF, 0xFF];
                diag.request(&req).and_then(|resp| check(SID_CLEAR_DTC, &resp))
            }
            Protocol::Obd => diag.request(&[OBD_MODE_CLEAR_DTC]).and_then(|resp| {
                if resp.first() == Some(&(OBD_MODE_CLEAR_DTC + 0x40)) {
                    Ok(())
                } else {
                    Err(format!(
                        "unexpected OBD mode-04 response 0x{:02X} (wanted 0x44)",
                        resp.first().copied().unwrap_or(0)
                    ))
                }
            }),
        };

        match result {
            Ok(()) => out.push(ClearResult {
                name: module.name.clone(),
                cleared: true,
                error: None,
            }),
            Err(e) => out.push(ClearResult {
                name: module.name.clone(),
                cleared: false,
                error: Some(e),
            }),
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A scripted transport for a multi-module vehicle on a single simulated
    /// bus. The engine pings each module exactly once before reading from it, so
    /// each ping advances an internal cursor to the next module; every request
    /// after a ping (and before the next) is routed to that module's script.
    struct ScriptDiag {
        /// Per-module request→response tables, in module order.
        scripts: Vec<ModuleScript>,
        /// Index of the module currently selected (`None` before the first ping).
        cursor: Option<usize>,
    }

    /// One module's scripted behaviour.
    struct ModuleScript {
        /// `true` if the module answers the presence probe.
        present: bool,
        /// Exact-request → response map for everything after the ping.
        responses: HashMap<Vec<u8>, Vec<u8>>,
    }

    impl ScriptDiag {
        fn new(scripts: Vec<ModuleScript>) -> Self {
            ScriptDiag {
                scripts,
                cursor: None,
            }
        }

        fn is_ping(req: &[u8]) -> bool {
            matches!(
                req,
                [SID_TESTER_PRESENT, 0x00] | [OBD_MODE_CURRENT_DATA, OBD_PID_SUPPORTED]
            )
        }
    }

    impl RawDiag for ScriptDiag {
        fn request(&mut self, req: &[u8]) -> Result<Vec<u8>, String> {
            if Self::is_ping(req) {
                // Advance to the next module and answer the probe.
                let next = self.cursor.map(|c| c + 1).unwrap_or(0);
                self.cursor = Some(next);
                let script = self
                    .scripts
                    .get(next)
                    .ok_or_else(|| "ping past end of vehicle".to_string())?;
                return if script.present {
                    // A positive TesterPresent / supported-PID reply.
                    match req {
                        [SID_TESTER_PRESENT, 0x00] => Ok(vec![0x7E, 0x00]),
                        _ => Ok(vec![0x41, 0x00, 0x00, 0x00, 0x00]),
                    }
                } else {
                    Err("no response (module absent)".to_string())
                };
            }

            let idx = self.cursor.ok_or_else(|| "request before any ping".to_string())?;
            let script = &self.scripts[idx];
            script
                .responses
                .get(req)
                .cloned()
                .ok_or_else(|| format!("no scripted response for {req:02X?}"))
        }
    }

    fn uds(name: &str, addr: u8) -> ModuleAddr {
        ModuleAddr {
            name: name.into(),
            addr,
            req_id: 0x700 + addr as u32,
            resp_id: 0x708 + addr as u32,
            protocol: Protocol::Uds,
        }
    }

    fn obd(name: &str, addr: u8) -> ModuleAddr {
        ModuleAddr {
            name: name.into(),
            addr,
            req_id: 0x7DF,
            resp_id: 0x7E8,
            protocol: Protocol::Obd,
        }
    }

    /// Build the ASCII payload of a 0x22 positive response for `did`.
    fn did_resp(did: u16, text: &str) -> Vec<u8> {
        let mut v = vec![0x62, (did >> 8) as u8, did as u8];
        v.extend_from_slice(text.as_bytes());
        v
    }

    #[test]
    fn three_module_vehicle_two_present_one_absent() {
        // Module 0: UDS Engine, present, VIN + part/SW, two DTCs (P0101, U0100).
        let mut engine = HashMap::new();
        engine.insert(
            vec![0x22, 0xF1, 0x90],
            did_resp(0xF190, "WVWZZZ1KZAW000001"),
        );
        engine.insert(vec![0x22, 0xF1, 0x87], did_resp(0xF187, "04E906016AD"));
        engine.insert(vec![0x22, 0xF1, 0x89], did_resp(0xF189, "0004"));
        // DIDs the module does not offer answer with a UDS negative response.
        engine.insert(vec![0x22, 0xF1, 0x91], vec![0x7F, 0x22, 0x31]);
        engine.insert(vec![0x22, 0xF1, 0x95], vec![0x7F, 0x22, 0x31]);
        // 0x19/02: availability 0xFF, then P0101 (status 0x24) and U0100 (0x2F).
        engine.insert(
            vec![0x19, 0x02, 0xFF],
            vec![0x59, 0x02, 0xFF, 0x01, 0x01, 0x00, 0x24, 0xC1, 0x00, 0x00, 0x2F],
        );
        let engine_script = ModuleScript {
            present: true,
            responses: engine,
        };

        // Module 1: OBD Transmission, present, no identity (OBD), one DTC P0700.
        let mut trans = HashMap::new();
        // mode 03: count byte 0x01, then P0700 (0x07 0x00).
        trans.insert(vec![0x03], vec![0x43, 0x01, 0x07, 0x00]);
        let trans_script = ModuleScript {
            present: true,
            responses: trans,
        };

        // Module 2: UDS ABS, absent.
        let abs_script = ModuleScript {
            present: false,
            responses: HashMap::new(),
        };

        let mut diag = ScriptDiag::new(vec![engine_script, trans_script, abs_script]);

        let modules = VehicleModules {
            vehicle: "test-vehicle".into(),
            modules: vec![uds("Engine", 0x01), obd("Transmission", 0x02), uds("ABS", 0x03)],
        };

        let report = scan(&mut diag, &modules);
        assert_eq!(report.modules.len(), 3);

        // --- Engine (UDS) ---
        let eng = &report.modules[0];
        assert_eq!(eng.name, "Engine");
        assert!(eng.present);
        assert!(eng.error.is_none(), "unexpected error: {:?}", eng.error);
        let id = eng.identity.as_ref().expect("engine identity");
        assert_eq!(id.vin.as_deref(), Some("WVWZZZ1KZAW000001"));
        // Only the two offered DIDs were recorded; unsupported ones were skipped.
        assert_eq!(id.dids.len(), 2);
        assert_eq!(id.dids[0].did, 0xF187);
        assert_eq!(id.dids[0].text.as_deref(), Some("04E906016AD"));
        // DTCs decoded + status via opentune-dtc.
        assert_eq!(eng.dtcs.len(), 2);
        assert_eq!(eng.dtcs[0].code, "P0101");
        assert_eq!(eng.dtcs[0].status, Some(0x24));
        assert_eq!(eng.dtcs[0].system, System::Powertrain);
        assert_eq!(eng.dtcs[1].code, "U0100");
        assert_eq!(eng.dtcs[1].status, Some(0x2F));
        assert_eq!(eng.dtcs[1].system, System::Network);
        // The DTC description came from the bundled table (reused, not invented).
        assert_eq!(eng.dtcs[0].description, opentune_dtc::lookup("P0101").unwrap().description);

        // --- Transmission (OBD) ---
        let trans = &report.modules[1];
        assert!(trans.present);
        assert!(trans.identity.is_none(), "OBD module has no DID identity");
        assert_eq!(trans.dtcs.len(), 1);
        assert_eq!(trans.dtcs[0].code, "P0700");
        assert_eq!(trans.dtcs[0].status, None, "OBD codes carry no status");

        // --- ABS (absent) ---
        let abs = &report.modules[2];
        assert!(!abs.present);
        assert!(abs.identity.is_none());
        assert!(abs.dtcs.is_empty());
        assert!(abs.error.is_none(), "absent module is not a hard error");

        // Aggregate helpers.
        assert_eq!(report.present().count(), 2);
        assert_eq!(report.dtc_count(), 3);
    }

    #[test]
    fn negative_response_to_dtc_read_is_handled_gracefully() {
        // A present UDS module that refuses ReadDTCInformation with a negative
        // response. The module is still reported; the error is captured; the
        // scan does not panic or abort.
        let mut m = HashMap::new();
        m.insert(vec![0x22, 0xF1, 0x90], did_resp(0xF190, "VIN0000000000001"));
        // Unsupported identity DIDs.
        for did in [0xF187u16, 0xF189, 0xF191, 0xF195] {
            m.insert(vec![0x22, (did >> 8) as u8, did as u8], vec![0x7F, 0x22, 0x31]);
        }
        // 0x19 → negative response 0x31 (requestOutOfRange).
        m.insert(vec![0x19, 0x02, 0xFF], vec![0x7F, 0x19, 0x31]);
        let script = ModuleScript {
            present: true,
            responses: m,
        };
        let mut diag = ScriptDiag::new(vec![script]);
        let modules = VehicleModules {
            vehicle: "neg".into(),
            modules: vec![uds("Engine", 0x01)],
        };
        let report = scan(&mut diag, &modules);
        let eng = &report.modules[0];
        assert!(eng.present);
        assert!(eng.dtcs.is_empty());
        let err = eng.error.as_ref().expect("error should be recorded");
        assert!(err.contains("dtc:"), "error should name the dtc read: {err}");
        assert!(err.contains("0x31"), "error should carry the NRC: {err}");
        // Identity still succeeded despite the DTC failure.
        assert_eq!(
            eng.identity.as_ref().and_then(|i| i.vin.as_deref()),
            Some("VIN0000000000001")
        );
    }

    #[test]
    fn mis_correlated_response_is_rejected() {
        // The module answers 0x22 for a *different* DID than requested; the read
        // must reject it rather than accept the wrong value.
        let mut m = HashMap::new();
        // Asked for 0xF190, module echoes 0xF191.
        m.insert(vec![0x22, 0xF1, 0x90], vec![0x62, 0xF1, 0x91, b'X']);
        let script = ModuleScript {
            present: true,
            responses: m,
        };
        let mut diag = ScriptDiag::new(vec![script]);
        let modules = VehicleModules {
            vehicle: "mis".into(),
            modules: vec![uds("Engine", 0x01)],
        };
        let report = scan(&mut diag, &modules);
        let eng = &report.modules[0];
        assert!(eng.present);
        assert!(eng.identity.is_none(), "mis-correlated DID is not accepted");
        let err = eng.error.as_ref().expect("error recorded");
        assert!(err.contains("identity:"), "{err}");
        assert!(err.contains("did not echo DID"), "{err}");
    }

    #[test]
    fn clear_all_targets_only_present_modules() {
        let present_uds = ModuleScript {
            present: true,
            responses: {
                let mut m = HashMap::new();
                m.insert(vec![0x14, 0xFF, 0xFF, 0xFF], vec![0x54]);
                m
            },
        };
        let present_obd = ModuleScript {
            present: true,
            responses: {
                let mut m = HashMap::new();
                m.insert(vec![0x04], vec![0x44]);
                m
            },
        };
        let absent = ModuleScript {
            present: false,
            responses: HashMap::new(),
        };
        let mut diag = ScriptDiag::new(vec![present_uds, present_obd, absent]);
        let modules = VehicleModules {
            vehicle: "clear".into(),
            modules: vec![uds("Engine", 0x01), obd("Transmission", 0x02), uds("ABS", 0x03)],
        };
        let results = clear_all(&mut diag, &modules);
        assert_eq!(results.len(), 3);
        assert!(results[0].cleared);
        assert!(results[1].cleared);
        assert!(!results[2].cleared, "absent module is not cleared");
        assert!(results[2].error.is_none());
    }

    #[test]
    fn vehicle_modules_json_round_trip() {
        let json = r#"{
            "vehicle": "VW Passat B7",
            "modules": [
                {"name": "Engine", "addr": 1, "req_id": 2016, "resp_id": 2024, "protocol": "uds"},
                {"name": "OBD", "addr": 0, "req_id": 2015, "resp_id": 2024, "protocol": "obd"}
            ]
        }"#;
        let vm = VehicleModules::from_json(json).expect("parse");
        assert_eq!(vm.vehicle, "VW Passat B7");
        assert_eq!(vm.modules.len(), 2);
        assert_eq!(vm.modules[0].protocol, Protocol::Uds);
        assert_eq!(vm.modules[1].protocol, Protocol::Obd);
        // Round-trips through serialisation.
        let round = VehicleModules::from_json(&vm.to_json().unwrap()).unwrap();
        assert_eq!(round, vm);
    }

    #[test]
    fn ping_rejects_mis_correlated_and_truncated_replies() {
        // A positive reply to the wrong service, and a truncated negative, must
        // not count as "present".
        assert!(!responded(0x3E, &[0x50, 0x00])); // wrong positive SID
        assert!(!responded(0x3E, &[0x7F, 0x3E])); // truncated negative
        assert!(!responded(0x3E, &[0x7F, 0x22, 0x11])); // negative for other svc
        assert!(responded(0x3E, &[0x7E, 0x00])); // correct positive
        assert!(responded(0x3E, &[0x7F, 0x3E, 0x11])); // well-formed refusal = present
    }
}
