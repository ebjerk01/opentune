//! # opentune-coding
//!
//! Module **coding / configuration** for OpenTune: read a control module's
//! configuration bytes, decode them to human-readable parameters, edit a
//! parameter, see a before/after diff, and write the change back — all driven by
//! a **data-defined coding-label set** ([`CodingDef`]) so new modules are new
//! *data*, not new code. This is the generic engine behind the suite's "Coding"
//! pillar (VAG long-coding, Ford As-Built/CCC, and any UDS-config module).
//!
//! ## Design
//!
//! * The **pure engine** (decode / get / set / diff, [`read_field`] /
//!   [`write_field`]) has no I/O and no transport dependency — it is total and
//!   unit-testable on plain byte buffers.
//! * The **I/O layer** ([`read_coding`] / [`write_coding`]) talks UDS over a tiny
//!   [`RawDiag`] request abstraction (one `request(&[u8]) -> Vec<u8>`), which the
//!   app adapts to `opentune-core`'s `DiagTransport` at integration. This keeps
//!   the crate standalone and lets tests drive it with a scripted responder.
//!
//! ## Safety
//!
//! Writing configuration can brick or immobilise a module. [`write_coding`]
//! therefore **reads a backup first**, applies only the changed blocks, and
//! **reads back to verify**, returning the backup and a diff for one-click
//! restore. It also supports [`WriteOptions::dry_run`] to produce the plan
//! without writing. Security access (UDS 0x27) is the caller's responsibility and
//! is signalled by [`CodingDef::requires_security`].
//!
//! ## Bit-field convention
//!
//! A parameter occupies `len` consecutive bits starting at (`byte`, `bit`) where
//! **bit 0 is the least-significant bit** of `byte`; the field extends toward
//! more-significant bits and into higher byte indices, LSB-first. A single byte
//! (`bit = 0, len = 8`), a bool (`len = 1`) and a nibble (`len = 4`) are the
//! common cases; multi-byte numeric fields follow the same LSB-first order.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors from the coding engine and its I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodingError {
    /// No parameter with this name exists in the definition.
    UnknownParam(String),
    /// A value does not fit the parameter's bit length, or is not one of its
    /// declared options.
    InvalidValue {
        /// Parameter name.
        param: String,
        /// The rejected value.
        value: u64,
        /// Why it was rejected.
        reason: String,
    },
    /// A field's byte range falls outside the block's data.
    OutOfRange {
        /// Parameter name (or `"<field>"`).
        param: String,
        /// Block index referenced.
        block: usize,
        /// Byte offset referenced.
        byte: usize,
        /// Block length available.
        len: usize,
    },
    /// The definition referenced a block index the coding data does not have.
    MissingBlock(usize),
    /// The definition itself is invalid (duplicate names/DIDs, out-of-range bit
    /// field, bad block reference, …). Deserialization alone does not validate.
    InvalidDef(String),
    /// The coding data to write does not match the definition's block layout
    /// (wrong DID, wrong order/count, or a block length that changed under us).
    ShapeMismatch(String),
    /// The live value drifted from the baseline the edit was made against, at a
    /// byte the edit would overwrite (optimistic-concurrency conflict).
    StaleBaseline {
        /// Block (DID) where the live value no longer matches the baseline.
        did: u16,
        /// Byte offset within the block.
        byte: usize,
    },
    /// A UDS request returned a negative response (0x7F) or an unexpected SID.
    Protocol(String),
    /// The transport itself failed.
    Transport(String),
    /// A read-back after writing did not match what was written.
    VerifyMismatch {
        /// Block (DID) that failed to verify.
        did: u16,
    },
}

impl std::fmt::Display for CodingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodingError::UnknownParam(p) => write!(f, "unknown parameter `{p}`"),
            CodingError::InvalidValue {
                param,
                value,
                reason,
            } => write!(f, "invalid value {value} for `{param}`: {reason}"),
            CodingError::OutOfRange {
                param,
                block,
                byte,
                len,
            } => write!(
                f,
                "`{param}` byte {byte} out of range in block {block} (len {len})"
            ),
            CodingError::MissingBlock(b) => write!(f, "coding data is missing block {b}"),
            CodingError::InvalidDef(s) => write!(f, "invalid coding definition: {s}"),
            CodingError::ShapeMismatch(s) => write!(f, "coding data shape mismatch: {s}"),
            CodingError::StaleBaseline { did, byte } => write!(
                f,
                "stale baseline: live value at block 0x{did:04X} byte {byte} changed since the edit"
            ),
            CodingError::Protocol(s) => write!(f, "UDS protocol error: {s}"),
            CodingError::Transport(s) => write!(f, "transport error: {s}"),
            CodingError::VerifyMismatch { did } => {
                write!(f, "read-back verify mismatch for block 0x{did:04X}")
            }
        }
    }
}

impl std::error::Error for CodingError {}

/// Crate result type.
pub type Result<T> = std::result::Result<T, CodingError>;

// ---------------------------------------------------------------------------
// Definition format (data)
// ---------------------------------------------------------------------------

/// How a module's configuration bytes are addressed over UDS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Addressing {
    /// One or more UDS Data Identifiers, each read with ReadDataByIdentifier
    /// (0x22) and written with WriteDataByIdentifier (0x2E). Block index in a
    /// [`CodingParam`] selects which DID. Covers VAG coding DIDs and Ford
    /// As-Built/CCC blocks exposed as DIDs.
    Did {
        /// The configuration DIDs, in block order.
        dids: Vec<u16>,
    },
}

impl Addressing {
    /// The DIDs backing each block, in order.
    pub fn dids(&self) -> &[u16] {
        match self {
            Addressing::Did { dids } => dids,
        }
    }
}

/// What a parameter's value represents, for presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamKind {
    /// An enumerated choice; its meaning comes from [`CodingParam::options`].
    #[default]
    Enum,
    /// A single on/off bit.
    Bool,
    /// A plain integer.
    Number,
}

/// One named value→label mapping for an [`ParamKind::Enum`] parameter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodingOption {
    /// The raw field value.
    pub value: u64,
    /// The human-readable meaning.
    pub label: String,
}

/// One configurable parameter: a named bit-field inside a block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodingParam {
    /// Display name.
    pub name: String,
    /// Which block (index into [`Addressing::dids`]). Defaults to 0.
    #[serde(default)]
    pub block: usize,
    /// Byte offset of the field's low bit within the block.
    pub byte: usize,
    /// Starting bit within `byte` (0 = least-significant). Defaults to 0.
    #[serde(default)]
    pub bit: u8,
    /// Field width in bits. Defaults to 8 (a whole byte).
    #[serde(default = "default_len")]
    pub len: u8,
    /// Presentation kind.
    #[serde(default)]
    pub kind: ParamKind,
    /// Enumerated options (for [`ParamKind::Enum`]); empty = free value.
    #[serde(default)]
    pub options: Vec<CodingOption>,
    /// Optional explanatory note.
    #[serde(default)]
    pub note: String,
}

fn default_len() -> u8 {
    8
}

/// A data-defined coding-label set for one module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodingDef {
    /// Stable id, e.g. `"ford-bcm-c346"`.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Module/platform note (free text).
    #[serde(default)]
    pub module: String,
    /// Whether writing requires UDS SecurityAccess (0x27) to be unlocked first.
    /// The engine does not perform the unlock; it is advisory for the caller and
    /// surfaced in the write plan.
    #[serde(default)]
    pub requires_security: bool,
    /// How the configuration bytes are addressed.
    pub addressing: Addressing,
    /// The parameters.
    pub params: Vec<CodingParam>,
}

impl CodingDef {
    /// Parse a definition from JSON.
    pub fn from_json(s: &str) -> std::result::Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    /// Find a parameter by name.
    pub fn param(&self, name: &str) -> Option<&CodingParam> {
        self.params.iter().find(|p| p.name == name)
    }

    /// Semantically validate the definition. `serde` deserialization only proves
    /// the JSON shape; this checks the invariants the engine relies on: at least
    /// one block, **unique** DIDs, **unique** parameter names, every parameter's
    /// `block` in range, `bit < 8`, `1 <= len <= 64`, and every enum option value
    /// fitting in `len` bits. Called before any write.
    pub fn validate(&self) -> Result<()> {
        let dids = self.addressing.dids();
        if dids.is_empty() {
            return Err(CodingError::InvalidDef("no blocks (DIDs) declared".into()));
        }
        let mut seen_did = std::collections::HashSet::new();
        for &d in dids {
            if !seen_did.insert(d) {
                return Err(CodingError::InvalidDef(format!("duplicate DID 0x{d:04X}")));
            }
        }
        let mut seen_name = std::collections::HashSet::new();
        for p in &self.params {
            if !seen_name.insert(p.name.as_str()) {
                return Err(CodingError::InvalidDef(format!(
                    "duplicate parameter name `{}`",
                    p.name
                )));
            }
            if p.block >= dids.len() {
                return Err(CodingError::InvalidDef(format!(
                    "`{}` references block {} but only {} declared",
                    p.name,
                    p.block,
                    dids.len()
                )));
            }
            if p.bit >= 8 {
                return Err(CodingError::InvalidDef(format!(
                    "`{}` bit {} is not 0..=7",
                    p.name, p.bit
                )));
            }
            if p.len == 0 || p.len > 64 {
                return Err(CodingError::InvalidDef(format!(
                    "`{}` len {} is not 1..=64",
                    p.name, p.len
                )));
            }
            for o in &p.options {
                if p.len < 64 && o.value >= (1u64 << p.len) {
                    return Err(CodingError::InvalidDef(format!(
                        "`{}` option value {} does not fit in {} bit(s)",
                        p.name, o.value, p.len
                    )));
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Coding bytes (state)
// ---------------------------------------------------------------------------

/// One block of configuration bytes read from (or to be written to) a module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodingBlock {
    /// The DID this block came from / goes to.
    pub did: u16,
    /// The raw bytes.
    pub bytes: Vec<u8>,
}

/// A module's current configuration: the bytes of each block, in definition order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Coding {
    /// Blocks in the same order as [`Addressing::dids`].
    pub blocks: Vec<CodingBlock>,
}

impl Coding {
    /// Borrow block `i`'s bytes.
    fn block(&self, i: usize) -> Result<&[u8]> {
        self.blocks
            .get(i)
            .map(|b| b.bytes.as_slice())
            .ok_or(CodingError::MissingBlock(i))
    }
    /// Mutably borrow block `i`'s bytes.
    fn block_mut(&mut self, i: usize) -> Result<&mut Vec<u8>> {
        self.blocks
            .get_mut(i)
            .map(|b| &mut b.bytes)
            .ok_or(CodingError::MissingBlock(i))
    }
}

// ---------------------------------------------------------------------------
// Pure bit-field engine
// ---------------------------------------------------------------------------

/// Read `len` bits starting at (`byte`, `bit`) from `data`, LSB-first (see the
/// crate-level bit-field convention). Bits past the end of `data` read as 0.
pub fn read_field(data: &[u8], byte: usize, bit: u8, len: u8) -> u64 {
    let mut val: u64 = 0;
    for i in 0..len as usize {
        let abs = bit as usize + i;
        let b = byte + abs / 8;
        let off = abs % 8;
        if b < data.len() && (data[b] >> off) & 1 == 1 {
            val |= 1u64 << i;
        }
    }
    val
}

/// Write `len` bits of `val` starting at (`byte`, `bit`) into `data`, LSB-first.
/// Errors if the field would extend past `data` or `val` does not fit in `len`.
pub fn write_field(data: &mut [u8], byte: usize, bit: u8, len: u8, val: u64) -> Result<()> {
    if len < 64 && val >= (1u64 << len) {
        return Err(CodingError::InvalidValue {
            param: "<field>".into(),
            value: val,
            reason: format!("does not fit in {len} bit(s)"),
        });
    }
    // Bounds: the highest bit touched must be inside data.
    let top_abs = bit as usize + len.saturating_sub(1) as usize;
    let top_byte = byte + top_abs / 8;
    if len > 0 && top_byte >= data.len() {
        return Err(CodingError::OutOfRange {
            param: "<field>".into(),
            block: 0,
            byte: top_byte,
            len: data.len(),
        });
    }
    for i in 0..len as usize {
        let abs = bit as usize + i;
        let b = byte + abs / 8;
        let off = abs % 8;
        let set = (val >> i) & 1 == 1;
        if set {
            data[b] |= 1 << off;
        } else {
            data[b] &= !(1 << off);
        }
    }
    Ok(())
}

/// A decoded parameter: its raw value and, when it maps to an option, its label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedParam {
    /// Parameter name.
    pub name: String,
    /// Raw field value.
    pub value: u64,
    /// Human-readable value (option label, on/off, or the number as text).
    pub label: String,
}

fn label_for(param: &CodingParam, value: u64) -> String {
    match param.kind {
        ParamKind::Bool => if value != 0 { "on" } else { "off" }.to_string(),
        ParamKind::Number => value.to_string(),
        ParamKind::Enum => param
            .options
            .iter()
            .find(|o| o.value == value)
            .map(|o| o.label.clone())
            .unwrap_or_else(|| format!("unknown (0x{value:X})")),
    }
}

/// Decode every parameter against the current coding bytes.
pub fn decode(def: &CodingDef, coding: &Coding) -> Result<Vec<DecodedParam>> {
    let mut out = Vec::with_capacity(def.params.len());
    for p in &def.params {
        let data = coding.block(p.block)?;
        let value = read_field(data, p.byte, p.bit, p.len);
        out.push(DecodedParam {
            name: p.name.clone(),
            value,
            label: label_for(p, value),
        });
    }
    Ok(out)
}

/// Read one parameter's current raw value.
pub fn get(def: &CodingDef, coding: &Coding, name: &str) -> Result<u64> {
    let p = def
        .param(name)
        .ok_or_else(|| CodingError::UnknownParam(name.to_string()))?;
    let data = coding.block(p.block)?;
    Ok(read_field(data, p.byte, p.bit, p.len))
}

/// Return a copy of `coding` with parameter `name` set to `value`. Validates the
/// value against the parameter's bit length and (for enums) its declared options.
pub fn set(def: &CodingDef, coding: &Coding, name: &str, value: u64) -> Result<Coding> {
    let p = def
        .param(name)
        .ok_or_else(|| CodingError::UnknownParam(name.to_string()))?;
    if p.len < 64 && value >= (1u64 << p.len) {
        return Err(CodingError::InvalidValue {
            param: name.to_string(),
            value,
            reason: format!("does not fit in {} bit(s)", p.len),
        });
    }
    if matches!(p.kind, ParamKind::Enum)
        && !p.options.is_empty()
        && !p.options.iter().any(|o| o.value == value)
    {
        return Err(CodingError::InvalidValue {
            param: name.to_string(),
            value,
            reason: "not one of the declared options".into(),
        });
    }
    let mut next = coding.clone();
    let data = next.block_mut(p.block)?;
    write_field(data, p.byte, p.bit, p.len, value).map_err(|e| match e {
        // Re-label the generic field error with the parameter name/block.
        CodingError::OutOfRange { byte, len, .. } => CodingError::OutOfRange {
            param: name.to_string(),
            block: p.block,
            byte,
            len,
        },
        other => other,
    })?;
    Ok(next)
}

/// A single raw byte that differs between two codings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ByteChange {
    /// Block index.
    pub block: usize,
    /// DID of the block.
    pub did: u16,
    /// Byte offset within the block.
    pub byte: usize,
    /// Old byte.
    pub old: u8,
    /// New byte.
    pub new: u8,
}

/// A parameter whose decoded value differs between two codings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamChange {
    /// Parameter name.
    pub name: String,
    /// Old raw value and label.
    pub old: (u64, String),
    /// New raw value and label.
    pub new: (u64, String),
}

/// Raw byte-level diff between two codings (same block layout assumed).
pub fn diff_bytes(old: &Coding, new: &Coding) -> Vec<ByteChange> {
    let mut out = Vec::new();
    for (bi, (ob, nb)) in old.blocks.iter().zip(new.blocks.iter()).enumerate() {
        let n = ob.bytes.len().max(nb.bytes.len());
        for i in 0..n {
            let o = ob.bytes.get(i).copied().unwrap_or(0);
            let nv = nb.bytes.get(i).copied().unwrap_or(0);
            if o != nv {
                out.push(ByteChange {
                    block: bi,
                    did: ob.did,
                    byte: i,
                    old: o,
                    new: nv,
                });
            }
        }
    }
    out
}

/// Parameter-level diff (only parameters whose value changed).
pub fn diff_params(def: &CodingDef, old: &Coding, new: &Coding) -> Result<Vec<ParamChange>> {
    let mut out = Vec::new();
    for p in &def.params {
        let ov = read_field(old.block(p.block)?, p.byte, p.bit, p.len);
        let nv = read_field(new.block(p.block)?, p.byte, p.bit, p.len);
        if ov != nv {
            out.push(ParamChange {
                name: p.name.clone(),
                old: (ov, label_for(p, ov)),
                new: (nv, label_for(p, nv)),
            });
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// I/O layer (UDS over a tiny transport abstraction)
// ---------------------------------------------------------------------------

/// One UDS request → response exchange. The app adapts `opentune-core`'s
/// `DiagTransport` to this; tests use a scripted responder. Implementations
/// return the raw response bytes (including a 0x7F negative response, which the
/// engine then classifies).
pub trait RawDiag {
    /// Send `req` and return the ECU's response bytes.
    fn request(&mut self, req: &[u8]) -> std::result::Result<Vec<u8>, String>;
}

/// Blanket impl so a plain closure can be used as a [`RawDiag`].
impl<F> RawDiag for F
where
    F: FnMut(&[u8]) -> std::result::Result<Vec<u8>, String>,
{
    fn request(&mut self, req: &[u8]) -> std::result::Result<Vec<u8>, String> {
        self(req)
    }
}

/// Adapter that runs the coding engine's [`RawDiag`] requests over an
/// `opentune-core` [`DiagTransport`](opentune_core::protocol::DiagTransport).
///
/// This is the integration seam the crate-level docs describe: the engine stays
/// standalone over the tiny [`RawDiag`] abstraction, and the application wraps a
/// real driver's transport in `CoreDiag` to drive it. The core transport already
/// handles ISO-TP framing and the UDS `responsePending` (0x78) loop, so each
/// `request` returns the final raw response bytes — **including** a `0x7F`
/// negative response, which the coding engine then classifies itself. Hard
/// transport failures (timeout, I/O) are mapped from the core [`Error`] to the
/// `String` that [`RawDiag`] uses.
///
/// [`Error`]: opentune_core::Error
pub struct CoreDiag<'a> {
    inner: &'a mut dyn opentune_core::protocol::DiagTransport,
    timeout_ms: u64,
}

impl<'a> CoreDiag<'a> {
    /// Wrap a core [`DiagTransport`](opentune_core::protocol::DiagTransport),
    /// issuing every request with `timeout_ms`.
    pub fn new(inner: &'a mut dyn opentune_core::protocol::DiagTransport, timeout_ms: u64) -> Self {
        CoreDiag { inner, timeout_ms }
    }
}

impl RawDiag for CoreDiag<'_> {
    fn request(&mut self, req: &[u8]) -> std::result::Result<Vec<u8>, String> {
        self.inner
            .request(req, self.timeout_ms)
            .map_err(|e| e.to_string())
    }
}

/// Check a UDS response against the request's service id (`req_sid`): accept a
/// positive response (`req_sid + 0x40`), and map a negative response (0x7F) to a
/// descriptive error — rejecting a truncated 0x7F and one that echoes a
/// *different* requested service (a mis-correlated response).
fn check(req_sid: u8, resp: &[u8]) -> Result<()> {
    if resp.is_empty() {
        return Err(CodingError::Protocol("empty response".into()));
    }
    if resp[0] == 0x7F {
        if resp.len() < 3 {
            return Err(CodingError::Protocol("truncated negative response".into()));
        }
        if resp[1] != req_sid {
            return Err(CodingError::Protocol(format!(
                "negative response echoes service 0x{:02X}, expected 0x{req_sid:02X}",
                resp[1]
            )));
        }
        return Err(CodingError::Protocol(format!(
            "negative response 0x{:02X} to service 0x{req_sid:02X}",
            resp[2]
        )));
    }
    let want = req_sid.wrapping_add(0x40);
    if resp[0] != want {
        return Err(CodingError::Protocol(format!(
            "unexpected response SID 0x{:02X} (wanted 0x{want:02X})",
            resp[0]
        )));
    }
    Ok(())
}

/// Read one DID with ReadDataByIdentifier (0x22) and return its data bytes.
fn read_did(diag: &mut dyn RawDiag, did: u16) -> Result<Vec<u8>> {
    let req = [0x22, (did >> 8) as u8, did as u8];
    let resp = diag.request(&req).map_err(CodingError::Transport)?;
    check(0x22, &resp)?;
    if resp.len() < 3 || resp[1] != (did >> 8) as u8 || resp[2] != did as u8 {
        return Err(CodingError::Protocol(format!(
            "0x22 response did not echo DID 0x{did:04X}"
        )));
    }
    Ok(resp[3..].to_vec())
}

/// Write one DID with WriteDataByIdentifier (0x2E). Requires the positive
/// response to **echo the full DID** (`6E hi lo`) — a bare `6E` or a `6E` for a
/// different DID is rejected, so a mis-correlated ack can't be taken as success.
fn write_did(diag: &mut dyn RawDiag, did: u16, data: &[u8]) -> Result<()> {
    let mut req = Vec::with_capacity(3 + data.len());
    req.extend_from_slice(&[0x2E, (did >> 8) as u8, did as u8]);
    req.extend_from_slice(data);
    let resp = diag.request(&req).map_err(CodingError::Transport)?;
    check(0x2E, &resp)?;
    if resp.len() < 3 || resp[1] != (did >> 8) as u8 || resp[2] != did as u8 {
        return Err(CodingError::Protocol(format!(
            "0x2E response did not echo DID 0x{did:04X}"
        )));
    }
    Ok(())
}

/// Read a module's full coding (every block DID in the definition).
pub fn read_coding(diag: &mut dyn RawDiag, def: &CodingDef) -> Result<Coding> {
    let mut blocks = Vec::new();
    for &did in def.addressing.dids() {
        blocks.push(CodingBlock {
            did,
            bytes: read_did(diag, did)?,
        });
    }
    Ok(Coding { blocks })
}

/// Options for [`write_coding`].
#[derive(Debug, Clone, Default)]
pub struct WriteOptions {
    /// Produce the plan (backup + diff) without writing anything.
    pub dry_run: bool,
    /// Skip the post-write read-back verify (not recommended).
    pub skip_verify: bool,
    /// Proceed even if the live value drifted from the baseline at a byte the
    /// edit overwrites (otherwise such drift is a [`CodingError::StaleBaseline`]).
    pub force_overwrite: bool,
}

/// What happened in a [`write_coding`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    /// `dry_run` — the plan was produced, nothing was written.
    Planned,
    /// The edit produced no change over the live value; nothing written.
    NoChange,
    /// Every changed block was written and read back identical.
    Verified,
    /// Blocks were written but verification was skipped by request.
    Unverified,
    /// A written block did not read back as written.
    VerifyFailed {
        /// The block (DID) that failed to verify.
        did: u16,
    },
    /// A write failed partway. [`WriteReport::written`] holds the acknowledged
    /// DIDs; this DID's outcome is **unknown** and must not be blindly retried.
    WriteError {
        /// The DID whose write did not complete.
        uncertain_did: u16,
        /// The underlying error text.
        message: String,
    },
}

/// The outcome of a (planned or executed) coding write. `backup` is always the
/// live value read immediately before writing, so a restore is possible even
/// after a partial failure.
#[derive(Debug, Clone)]
pub struct WriteReport {
    /// The module's coding as read immediately before writing (for restore).
    pub backup: Coding,
    /// The merged target (live + the intended edits) that would be / was written.
    pub target: Coding,
    /// The raw bytes that change between `backup` and `target`.
    pub byte_changes: Vec<ByteChange>,
    /// The parameters that change (decoded).
    pub param_changes: Vec<ParamChange>,
    /// DIDs actually written and acknowledged, in order.
    pub written: Vec<u16>,
    /// What happened.
    pub outcome: WriteOutcome,
    /// True when [`CodingDef::requires_security`] — a reminder the caller must
    /// have unlocked SecurityAccess (0x27) first.
    pub security_required: bool,
}

impl WriteReport {
    /// True only when every changed block was written and verified.
    pub fn is_verified(&self) -> bool {
        self.outcome == WriteOutcome::Verified
    }
}

/// Confirm a coding's blocks match the definition's DIDs (count + order), and
/// optionally that each block is the same length as a reference coding's.
fn check_layout(def: &CodingDef, c: &Coding, what: &str, reference: Option<&Coding>) -> Result<()> {
    let dids = def.addressing.dids();
    if c.blocks.len() != dids.len() {
        return Err(CodingError::ShapeMismatch(format!(
            "{what} has {} block(s), definition declares {}",
            c.blocks.len(),
            dids.len()
        )));
    }
    for (i, (&did, b)) in dids.iter().zip(c.blocks.iter()).enumerate() {
        if b.did != did {
            return Err(CodingError::ShapeMismatch(format!(
                "{what} block {i} is DID 0x{:04X}, definition says 0x{did:04X}",
                b.did
            )));
        }
        if let Some(r) = reference {
            if r.blocks[i].bytes.len() != b.bytes.len() {
                return Err(CodingError::ShapeMismatch(format!(
                    "{what} block 0x{did:04X} is {} byte(s), expected {}",
                    b.bytes.len(),
                    r.blocks[i].bytes.len()
                )));
            }
        }
    }
    Ok(())
}

/// Write the edits in `desired` (relative to `baseline`) to the module, safely.
///
/// `baseline` is the coding the edit was made against; `desired` is the edited
/// copy. The module is read **fresh** first (the `backup`), and only the bytes
/// the user actually changed (`desired` vs `baseline`) are merged onto that fresh
/// read — so a value that drifted elsewhere since the edit is never clobbered. If
/// the live value drifted at a byte the edit *overwrites*, that is a
/// [`CodingError::StaleBaseline`] conflict unless [`WriteOptions::force_overwrite`].
/// Changed blocks are written (0x2E) then read back to verify.
///
/// Returns `Err` only when **nothing was written** (invalid definition, shape
/// mismatch, stale-baseline conflict, or the initial read failed). Once writing
/// starts, the result is `Ok(report)` whose [`WriteReport::outcome`] records
/// success, a verify failure or a partial write — always with the `backup`.
///
/// This does **not** perform SecurityAccess; when `def.requires_security` the
/// caller must already have unlocked the module (the report flags it).
pub fn write_coding(
    diag: &mut dyn RawDiag,
    def: &CodingDef,
    baseline: &Coding,
    desired: &Coding,
    opts: &WriteOptions,
) -> Result<WriteReport> {
    def.validate()?;
    check_layout(def, baseline, "baseline", None)?;
    check_layout(def, desired, "desired", Some(baseline))?;

    // Read the module fresh; this is the backup and the merge base.
    let live = read_coding(diag, def)?;
    check_layout(def, &live, "live read", Some(baseline))?;

    // Merge the intended edits (desired vs baseline) onto the fresh live read,
    // rejecting drift at any byte the edit would overwrite.
    let dids = def.addressing.dids().to_vec();
    let mut target = live.clone();
    for bi in 0..dids.len() {
        let base = &baseline.blocks[bi].bytes;
        let des = &desired.blocks[bi].bytes;
        let liveb = &live.blocks[bi].bytes;
        for byte in 0..base.len() {
            if des[byte] != base[byte] {
                if !opts.force_overwrite && liveb[byte] != base[byte] {
                    return Err(CodingError::StaleBaseline { did: dids[bi], byte });
                }
                target.blocks[bi].bytes[byte] = des[byte];
            }
        }
    }

    let byte_changes = diff_bytes(&live, &target);
    let param_changes = diff_params(def, &live, &target)?;
    let changed_blocks: Vec<usize> = {
        let mut v: Vec<usize> = byte_changes.iter().map(|c| c.block).collect();
        v.sort_unstable();
        v.dedup();
        v
    };

    let mut report = WriteReport {
        backup: live.clone(),
        target: target.clone(),
        byte_changes,
        param_changes,
        written: Vec::new(),
        outcome: WriteOutcome::NoChange,
        security_required: def.requires_security,
    };

    if opts.dry_run {
        report.outcome = WriteOutcome::Planned;
        return Ok(report);
    }
    if changed_blocks.is_empty() {
        return Ok(report); // NoChange
    }

    // Write each changed block. A failure retains the backup and the DIDs acked
    // so far, and marks the failing DID uncertain (do not blindly retry).
    for &bi in &changed_blocks {
        let block = &target.blocks[bi];
        match write_did(diag, block.did, &block.bytes) {
            Ok(()) => report.written.push(block.did),
            Err(e) => {
                report.outcome = WriteOutcome::WriteError {
                    uncertain_did: block.did,
                    message: e.to_string(),
                };
                return Ok(report);
            }
        }
    }

    if opts.skip_verify {
        report.outcome = WriteOutcome::Unverified;
        return Ok(report);
    }
    let after = read_coding(diag, def)?;
    for &bi in &changed_blocks {
        let want = &target.blocks[bi];
        let got = after.blocks.get(bi).ok_or(CodingError::MissingBlock(bi))?;
        if got.bytes != want.bytes {
            report.outcome = WriteOutcome::VerifyFailed { did: want.did };
            return Ok(report);
        }
    }
    report.outcome = WriteOutcome::Verified;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_def() -> CodingDef {
        // Two-block module: block0 DID 0x0401, block1 DID 0x0402.
        CodingDef {
            id: "test-mod".into(),
            name: "Test module".into(),
            module: "synthetic".into(),
            requires_security: true,
            addressing: Addressing::Did {
                dids: vec![0x0401, 0x0402],
            },
            params: vec![
                CodingParam {
                    name: "Start-Stop".into(),
                    block: 0,
                    byte: 0,
                    bit: 0,
                    len: 1,
                    kind: ParamKind::Bool,
                    options: vec![],
                    note: String::new(),
                },
                CodingParam {
                    name: "DRL mode".into(),
                    block: 0,
                    byte: 0,
                    bit: 1,
                    len: 2,
                    kind: ParamKind::Enum,
                    options: vec![
                        CodingOption { value: 0, label: "off".into() },
                        CodingOption { value: 1, label: "on".into() },
                        CodingOption { value: 2, label: "auto".into() },
                    ],
                    note: String::new(),
                },
                CodingParam {
                    name: "Country".into(),
                    block: 1,
                    byte: 2,
                    bit: 0,
                    len: 8,
                    kind: ParamKind::Number,
                    options: vec![],
                    note: String::new(),
                },
            ],
        }
    }

    fn sample_coding() -> Coding {
        Coding {
            blocks: vec![
                CodingBlock { did: 0x0401, bytes: vec![0b0000_0101, 0x00] }, // SS=1, DRL=2(auto)
                CodingBlock { did: 0x0402, bytes: vec![0x00, 0x00, 0x2A] },  // Country=42
            ],
        }
    }

    #[test]
    fn field_read_write_lsb_first_and_multibyte() {
        let mut d = vec![0u8; 3];
        write_field(&mut d, 0, 4, 8, 0xAB).unwrap(); // spans byte0 hi nibble + byte1 lo nibble
        assert_eq!(read_field(&d, 0, 4, 8), 0xAB);
        assert_eq!(d[0] & 0x0F, 0x00);
        // out of range + overflow are rejected
        assert!(matches!(write_field(&mut d, 2, 4, 8, 0x01), Err(CodingError::OutOfRange { .. })));
        assert!(matches!(write_field(&mut d, 0, 0, 2, 4), Err(CodingError::InvalidValue { .. })));
    }

    #[test]
    fn decode_labels() {
        let def = sample_def();
        let dec = decode(&def, &sample_coding()).unwrap();
        assert_eq!(dec[0], DecodedParam { name: "Start-Stop".into(), value: 1, label: "on".into() });
        assert_eq!(dec[1], DecodedParam { name: "DRL mode".into(), value: 2, label: "auto".into() });
        assert_eq!(dec[2].label, "42");
    }

    #[test]
    fn set_validates_and_diffs() {
        let def = sample_def();
        let c0 = sample_coding();
        // enum rejects an undeclared option
        assert!(matches!(set(&def, &c0, "DRL mode", 3), Err(CodingError::InvalidValue { .. })));
        // unknown param
        assert!(matches!(set(&def, &c0, "Nope", 0), Err(CodingError::UnknownParam(_))));
        // valid change
        let c1 = set(&def, &c0, "DRL mode", 1).unwrap();
        let pc = diff_params(&def, &c0, &c1).unwrap();
        assert_eq!(pc.len(), 1);
        assert_eq!(pc[0].name, "DRL mode");
        assert_eq!(pc[0].old, (2, "auto".into()));
        assert_eq!(pc[0].new, (1, "on".into()));
        // a bit-field edit touches only block 0
        assert!(diff_bytes(&c0, &c1).iter().all(|b| b.block == 0));
    }

    /// Scripted responder: answers 0x22 from a per-DID store, applies 0x2E, and
    /// records every request so the test can assert the exchange.
    struct Script {
        store: std::collections::HashMap<u16, Vec<u8>>,
        log: Vec<Vec<u8>>,
    }
    impl Script {
        fn new(blocks: &[(u16, Vec<u8>)]) -> Self {
            Script {
                store: blocks.iter().cloned().collect(),
                log: Vec::new(),
            }
        }
    }
    impl RawDiag for Script {
        fn request(&mut self, req: &[u8]) -> std::result::Result<Vec<u8>, String> {
            self.log.push(req.to_vec());
            match req[0] {
                0x22 => {
                    let did = ((req[1] as u16) << 8) | req[2] as u16;
                    let data = self.store.get(&did).ok_or("no such DID")?;
                    let mut r = vec![0x62, req[1], req[2]];
                    r.extend_from_slice(data);
                    Ok(r)
                }
                0x2E => {
                    let did = ((req[1] as u16) << 8) | req[2] as u16;
                    self.store.insert(did, req[3..].to_vec());
                    Ok(vec![0x6E, req[1], req[2]])
                }
                _ => Err("unexpected service".into()),
            }
        }
    }

    #[test]
    fn read_then_write_roundtrip_verifies() {
        let def = sample_def();
        let mut io = Script::new(&[
            (0x0401, vec![0b0000_0101, 0x00]),
            (0x0402, vec![0x00, 0x00, 0x2A]),
        ]);
        let current = read_coding(&mut io, &def).unwrap();
        let desired = set(&def, &current, "Country", 7).unwrap();
        let rep = write_coding(&mut io, &def, &current, &desired, &WriteOptions::default()).unwrap();
        assert!(rep.is_verified() && rep.security_required);
        assert_eq!(rep.written, vec![0x0402]); // only the changed block
        assert_eq!(rep.param_changes.len(), 1);
        assert_eq!(get(&def, &read_coding(&mut io, &def).unwrap(), "Country").unwrap(), 7);
    }

    #[test]
    fn dry_run_writes_nothing() {
        let def = sample_def();
        let mut io = Script::new(&[
            (0x0401, vec![0b0000_0101, 0x00]),
            (0x0402, vec![0x00, 0x00, 0x2A]),
        ]);
        let current = read_coding(&mut io, &def).unwrap();
        let desired = set(&def, &current, "Start-Stop", 0).unwrap();
        let rep = write_coding(&mut io, &def, &current, &desired, &WriteOptions { dry_run: true, ..Default::default() }).unwrap();
        assert_eq!(rep.outcome, WriteOutcome::Planned);
        assert!(rep.written.is_empty() && !rep.param_changes.is_empty());
        assert_eq!(get(&def, &read_coding(&mut io, &def).unwrap(), "Start-Stop").unwrap(), 1);
    }

    #[test]
    fn stale_baseline_conflict_detected() {
        // Edit vs baseline Country=42; the live byte drifted to 99 -> conflict,
        // nothing written. force_overwrite proceeds.
        let def = sample_def();
        let baseline = sample_coding();
        let desired = set(&def, &baseline, "Country", 7).unwrap();
        let mut io = Script::new(&[
            (0x0401, vec![0b0000_0101, 0x00]),
            (0x0402, vec![0x00, 0x00, 99]), // live drifted on the Country byte
        ]);
        assert!(matches!(
            write_coding(&mut io, &def, &baseline, &desired, &WriteOptions::default()),
            Err(CodingError::StaleBaseline { did: 0x0402, byte: 2 })
        ));
        let rep = write_coding(&mut io, &def, &baseline, &desired, &WriteOptions { force_overwrite: true, ..Default::default() }).unwrap();
        assert!(rep.is_verified());
    }

    #[test]
    fn merge_preserves_unrelated_live_drift() {
        // User changed only Country; Start-Stop drifted live. The merge must keep
        // the live Start-Stop and only write the Country edit (never clobber to 1).
        let def = sample_def();
        let baseline = sample_coding(); // SS=1
        let desired = set(&def, &baseline, "Country", 7).unwrap();
        let mut io = Script::new(&[
            (0x0401, vec![0b0000_0100, 0x00]), // live SS=0 (drifted)
            (0x0402, vec![0x00, 0x00, 0x2A]),
        ]);
        let rep = write_coding(&mut io, &def, &baseline, &desired, &WriteOptions::default()).unwrap();
        assert!(rep.is_verified());
        let after = read_coding(&mut io, &def).unwrap();
        assert_eq!(get(&def, &after, "Country").unwrap(), 7); // edit applied
        assert_eq!(get(&def, &after, "Start-Stop").unwrap(), 0); // live drift preserved
    }

    #[test]
    fn write_rejects_shape_and_bad_did() {
        let def = sample_def();
        let baseline = sample_coding();
        let mut io = Script::new(&[
            (0x0401, vec![0b0000_0101, 0x00]),
            (0x0402, vec![0x00, 0x00, 0x2A]),
        ]);
        let mut bad = baseline.clone();
        bad.blocks[1].did = 0x9999; // DID not in the definition
        assert!(matches!(
            write_coding(&mut io, &def, &baseline, &bad, &WriteOptions::default()),
            Err(CodingError::ShapeMismatch(_))
        ));
        let mut bad2 = baseline.clone();
        bad2.blocks[1].bytes.push(0); // wrong block length
        assert!(matches!(
            write_coding(&mut io, &def, &baseline, &bad2, &WriteOptions::default()),
            Err(CodingError::ShapeMismatch(_))
        ));
    }

    #[test]
    fn partial_write_failure_retains_backup() {
        let def = sample_def();
        let baseline = sample_coding();
        let desired = set(&def, &baseline, "Country", 7).unwrap();
        let mut io = |req: &[u8]| -> std::result::Result<Vec<u8>, String> {
            match req[0] {
                0x22 => {
                    let did = ((req[1] as u16) << 8) | req[2] as u16;
                    let data: &[u8] = if did == 0x0401 { &[0b0000_0101, 0x00] } else { &[0x00, 0x00, 0x2A] };
                    let mut r = vec![0x62, req[1], req[2]];
                    r.extend_from_slice(data);
                    Ok(r)
                }
                0x2E => Ok(vec![0x7F, 0x2E, 0x22]), // conditionsNotCorrect
                _ => Err("?".into()),
            }
        };
        let rep = write_coding(&mut io, &def, &baseline, &desired, &WriteOptions::default()).unwrap();
        assert!(matches!(rep.outcome, WriteOutcome::WriteError { uncertain_did: 0x0402, .. }));
        assert!(rep.written.is_empty());
        assert_eq!(rep.backup.blocks[1].bytes, vec![0x00, 0x00, 0x2A]); // backup kept
    }

    #[test]
    fn write_did_requires_full_echo() {
        let def = sample_def();
        let baseline = sample_coding();
        let desired = set(&def, &baseline, "Country", 7).unwrap();
        let mut io = |req: &[u8]| -> std::result::Result<Vec<u8>, String> {
            match req[0] {
                0x22 => {
                    let did = ((req[1] as u16) << 8) | req[2] as u16;
                    let data: &[u8] = if did == 0x0401 { &[0b0000_0101, 0x00] } else { &[0x00, 0x00, 0x2A] };
                    let mut r = vec![0x62, req[1], req[2]];
                    r.extend_from_slice(data);
                    Ok(r)
                }
                0x2E => Ok(vec![0x6E]), // bare ack, no DID echo -> rejected
                _ => Err("?".into()),
            }
        };
        let rep = write_coding(&mut io, &def, &baseline, &desired, &WriteOptions::default()).unwrap();
        assert!(matches!(rep.outcome, WriteOutcome::WriteError { .. }));
    }

    #[test]
    fn def_validate_catches_bad_defs() {
        assert!(sample_def().validate().is_ok());
        let mut d = sample_def();
        d.addressing = Addressing::Did { dids: vec![0x0401, 0x0401] };
        assert!(matches!(d.validate(), Err(CodingError::InvalidDef(_)))); // dup DID
        let mut d2 = sample_def();
        d2.params[0].bit = 8;
        assert!(matches!(d2.validate(), Err(CodingError::InvalidDef(_)))); // bit >= 8
        let mut d3 = sample_def();
        d3.params[0].len = 0;
        assert!(matches!(d3.validate(), Err(CodingError::InvalidDef(_)))); // len 0
        let mut d4 = sample_def();
        d4.params[1].len = 1; // "DRL mode" has option value 2, won't fit in 1 bit
        assert!(matches!(d4.validate(), Err(CodingError::InvalidDef(_))));
        let mut d5 = sample_def();
        d5.params[2].block = 5;
        assert!(matches!(d5.validate(), Err(CodingError::InvalidDef(_)))); // block out of range
    }

    #[test]
    fn negative_response_surfaces() {
        let def = sample_def();
        let mut io = |req: &[u8]| -> std::result::Result<Vec<u8>, String> {
            Ok(vec![0x7F, req[0], 0x33]) // security denied, echoes requested SID
        };
        assert!(matches!(read_coding(&mut io, &def), Err(CodingError::Protocol(_))));
    }

    #[test]
    fn mis_correlated_negative_response_rejected() {
        let def = sample_def();
        // 0x22 request but the 0x7F echoes service 0x2E -> rejected as mis-correlated
        let mut io = |_req: &[u8]| -> std::result::Result<Vec<u8>, String> { Ok(vec![0x7F, 0x2E, 0x33]) };
        assert!(matches!(read_coding(&mut io, &def), Err(CodingError::Protocol(_))));
    }

    #[test]
    fn def_parses_from_json() {
        let js = r#"{
          "id":"ford-bcm","name":"BCM","module":"c346","requires_security":true,
          "addressing":{"kind":"did","dids":[1025,1026]},
          "params":[{"name":"X","byte":0,"bit":0,"len":1,"kind":"bool"}]
        }"#;
        let def = CodingDef::from_json(js).unwrap();
        assert_eq!(def.addressing.dids(), &[1025, 1026]);
        assert_eq!(def.params[0].len, 1);
    }

    /// A scripted `opentune-core` [`DiagTransport`] mirroring the `Script` in
    /// `opentune-core/src/protocol/uds.rs`: answers 0x22 from a per-DID store,
    /// applies 0x2E, and records the per-request timeout it was called with so
    /// the [`CoreDiag`] adapter's wiring can be asserted.
    struct CoreScript {
        store: std::collections::HashMap<u16, Vec<u8>>,
        timeouts: Vec<u64>,
    }
    impl CoreScript {
        fn new(blocks: &[(u16, Vec<u8>)]) -> Self {
            CoreScript {
                store: blocks.iter().cloned().collect(),
                timeouts: Vec::new(),
            }
        }
    }
    impl opentune_core::protocol::DiagTransport for CoreScript {
        fn request(&mut self, req: &[u8], timeout_ms: u64) -> opentune_core::Result<Vec<u8>> {
            self.timeouts.push(timeout_ms);
            match req[0] {
                0x22 => {
                    let did = ((req[1] as u16) << 8) | req[2] as u16;
                    let data = self
                        .store
                        .get(&did)
                        .ok_or_else(|| opentune_core::Error::Protocol("no such DID".into()))?;
                    let mut r = vec![0x62, req[1], req[2]];
                    r.extend_from_slice(data);
                    Ok(r)
                }
                0x2E => {
                    let did = ((req[1] as u16) << 8) | req[2] as u16;
                    self.store.insert(did, req[3..].to_vec());
                    Ok(vec![0x6E, req[1], req[2]])
                }
                other => Err(opentune_core::Error::Protocol(format!(
                    "unexpected service 0x{other:02X}"
                ))),
            }
        }
    }

    #[test]
    fn core_diag_adapter_round_trips_over_diagtransport() {
        let def = sample_def();
        let mut transport = CoreScript::new(&[
            (0x0401, vec![0b0000_0101, 0x00]),
            (0x0402, vec![0x00, 0x00, 0x2A]),
        ]);
        {
            let mut diag = CoreDiag::new(&mut transport, 2_000);
            // Read -> edit -> write -> verify, all through the core transport.
            let current = read_coding(&mut diag, &def).unwrap();
            assert_eq!(get(&def, &current, "Country").unwrap(), 42);
            let desired = set(&def, &current, "Country", 7).unwrap();
            let rep =
                write_coding(&mut diag, &def, &current, &desired, &WriteOptions::default()).unwrap();
            assert!(rep.is_verified());
            assert_eq!(rep.written, vec![0x0402]); // only the changed block
            assert_eq!(get(&def, &read_coding(&mut diag, &def).unwrap(), "Country").unwrap(), 7);
        }
        // The adapter passed the configured timeout to every exchange.
        assert!(transport.timeouts.iter().all(|&t| t == 2_000));
    }

    #[test]
    fn core_diag_adapter_maps_transport_error_to_string() {
        let def = sample_def();
        // Empty store -> every 0x22 is a core Error, which the adapter must turn
        // into the `String` the coding engine surfaces as a transport error.
        let mut transport = CoreScript::new(&[]);
        let mut diag = CoreDiag::new(&mut transport, 1_000);
        assert!(matches!(
            read_coding(&mut diag, &def),
            Err(CodingError::Transport(_))
        ));
    }
}
