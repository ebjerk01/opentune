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

/// Check a UDS response against the expected positive SID, mapping 0x7F to a
/// descriptive protocol error.
fn check(expect_sid: u8, resp: &[u8]) -> Result<()> {
    if resp.is_empty() {
        return Err(CodingError::Protocol("empty response".into()));
    }
    if resp[0] == 0x7F {
        let nrc = resp.get(2).copied().unwrap_or(0);
        return Err(CodingError::Protocol(format!(
            "negative response 0x{nrc:02X} to service 0x{:02X}",
            resp.get(1).copied().unwrap_or(0)
        )));
    }
    if resp[0] != expect_sid {
        return Err(CodingError::Protocol(format!(
            "unexpected response SID 0x{:02X} (wanted 0x{expect_sid:02X})",
            resp[0]
        )));
    }
    Ok(())
}

/// Read one DID with ReadDataByIdentifier (0x22) and return its data bytes.
fn read_did(diag: &mut dyn RawDiag, did: u16) -> Result<Vec<u8>> {
    let req = [0x22, (did >> 8) as u8, did as u8];
    let resp = diag.request(&req).map_err(CodingError::Transport)?;
    check(0x62, &resp)?;
    if resp.len() < 3 || resp[1] != (did >> 8) as u8 || resp[2] != did as u8 {
        return Err(CodingError::Protocol(format!(
            "0x22 response did not echo DID 0x{did:04X}"
        )));
    }
    Ok(resp[3..].to_vec())
}

/// Write one DID with WriteDataByIdentifier (0x2E).
fn write_did(diag: &mut dyn RawDiag, did: u16, data: &[u8]) -> Result<()> {
    let mut req = Vec::with_capacity(3 + data.len());
    req.extend_from_slice(&[0x2E, (did >> 8) as u8, did as u8]);
    req.extend_from_slice(data);
    let resp = diag.request(&req).map_err(CodingError::Transport)?;
    check(0x6E, &resp)?;
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
}

/// The outcome of a (planned or executed) coding write.
#[derive(Debug, Clone)]
pub struct WriteReport {
    /// The module's coding as read immediately before writing (for restore).
    pub backup: Coding,
    /// The raw bytes that changed.
    pub byte_changes: Vec<ByteChange>,
    /// The parameters that changed (decoded).
    pub param_changes: Vec<ParamChange>,
    /// DIDs actually written (empty on a dry run).
    pub written: Vec<u16>,
    /// Whether every written block read back identical (false if skipped/dry).
    pub verified: bool,
    /// True if nothing was written because the flag was set.
    pub dry_run: bool,
    /// True when [`CodingDef::requires_security`] — a reminder the caller must
    /// have unlocked SecurityAccess (0x27) first.
    pub security_required: bool,
}

/// Write `desired` coding to the module, safely: read a fresh backup, write only
/// the blocks that changed, then read back to verify. On `dry_run`, returns the
/// plan without writing. The backup in the report enables one-click restore.
///
/// This does **not** perform SecurityAccess; when `def.requires_security` the
/// caller must already have unlocked the module (the report flags it).
pub fn write_coding(
    diag: &mut dyn RawDiag,
    def: &CodingDef,
    desired: &Coding,
    opts: &WriteOptions,
) -> Result<WriteReport> {
    let backup = read_coding(diag, def)?;
    let byte_changes = diff_bytes(&backup, desired);
    let param_changes = diff_params(def, &backup, desired)?;

    let changed_blocks: Vec<usize> = {
        let mut v: Vec<usize> = byte_changes.iter().map(|c| c.block).collect();
        v.sort_unstable();
        v.dedup();
        v
    };

    let mut report = WriteReport {
        backup: backup.clone(),
        byte_changes,
        param_changes,
        written: Vec::new(),
        verified: false,
        dry_run: opts.dry_run,
        security_required: def.requires_security,
    };
    if opts.dry_run || report.byte_changes.is_empty() {
        report.verified = report.byte_changes.is_empty();
        return Ok(report);
    }

    for &bi in &changed_blocks {
        let block = &desired.blocks[bi];
        write_did(diag, block.did, &block.bytes)?;
        report.written.push(block.did);
    }

    if opts.skip_verify {
        report.verified = false;
        return Ok(report);
    }
    // Read back every written block and confirm it matches.
    let after = read_coding(diag, def)?;
    for &bi in &changed_blocks {
        let want = &desired.blocks[bi];
        let got = after
            .blocks
            .get(bi)
            .ok_or(CodingError::MissingBlock(bi))?;
        if got.bytes != want.bytes {
            return Err(CodingError::VerifyMismatch { did: want.did });
        }
    }
    report.verified = true;
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
        let rep = write_coding(&mut io, &def, &desired, &WriteOptions::default()).unwrap();
        assert!(rep.verified && rep.security_required);
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
        let rep = write_coding(&mut io, &def, &desired, &WriteOptions { dry_run: true, skip_verify: false }).unwrap();
        assert!(rep.dry_run && rep.written.is_empty() && !rep.param_changes.is_empty());
        // the ECU still reads the original value
        assert_eq!(get(&def, &read_coding(&mut io, &def).unwrap(), "Start-Stop").unwrap(), 1);
    }

    #[test]
    fn negative_response_surfaces() {
        let def = sample_def();
        let mut io = |req: &[u8]| -> std::result::Result<Vec<u8>, String> {
            // 0x22 → security denied
            Ok(vec![0x7F, req[0], 0x33])
        };
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
}
