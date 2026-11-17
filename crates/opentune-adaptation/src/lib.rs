//! # opentune-adaptation
//!
//! Module **adaptation / basic-settings / actuator-test** engine for OpenTune:
//! read a control module's adaptation value, write/apply a new one with range
//! validation and read-back verify, run a UDS routine (basic settings), and
//! actuate an output for a component test — all driven by a **data-defined
//! adaptation-channel set** ([`AdaptationDef`]) so new modules are new *data*,
//! not new code. This is the generic engine behind the suite's "Adaptation"
//! pillar (VAG measuring blocks / basic settings, generic UDS routines and
//! actuator tests), the sibling of the `opentune-coding` "Coding" engine.
//!
//! ## Design
//!
//! * The **pure engine** (field extraction, scaling, range validation, def
//!   validation, [`read_be`] / [`write_be`]) has no I/O and no transport
//!   dependency — it is total and unit-testable on plain byte buffers.
//! * The **I/O layer** ([`read_value`] / [`write_value`] / [`run_routine`] /
//!   [`run_actuator_test`]) talks UDS over a tiny [`RawDiag`] request
//!   abstraction (one `request(&[u8]) -> Vec<u8>`), which the app adapts to
//!   `opentune-core`'s diagnostic transport at integration. This keeps the
//!   crate standalone and lets tests drive it with a scripted responder. (This
//!   crate deliberately does **not** depend on `opentune-coding`; the maintainer
//!   unifies the two transports later.)
//!
//! ## Safety
//!
//! Adaptation and actuation can move real hardware and immobilise a vehicle.
//!
//! * [`write_value`] **reads the DID fresh first** (the backup), splices only
//!   the target field, validates the new value against its width and declared
//!   range, supports [`WriteOptions::dry_run`] to produce a plan without
//!   writing, and **reads back to verify**.
//! * [`run_actuator_test`] always issues **returnControlToECU** after the test,
//!   even when the adjustment or a status read failed, and surfaces loudly via
//!   [`ActuatorOutcome::ControlReturnFailed`] if control could *not* be handed
//!   back (the dangerous case where an output is left forced).
//! * The definition is semantically validated ([`AdaptationDef::validate`])
//!   before any write or actuation: unique channel names and sane field
//!   widths / ranges.
//!
//! Security access (UDS 0x27) is the caller's responsibility and is advisory
//! only, signalled by [`AdaptationDef::requires_security`] and echoed into every
//! report's `security_required` flag.
//!
//! ## UDS mechanisms
//!
//! Each [`AdaptationChannel`] names one of three real UDS mechanisms:
//!
//! * **RoutineControl (0x31)** — start / stop / requestResults a routine by id,
//!   with an optional data record, and decode a numeric result.
//! * **ReadDataByIdentifier / WriteDataByIdentifier (0x22 / 0x2E)** — read an
//!   adaptation value from a DID and write a new one, as a big-endian numeric
//!   field with linear scaling and an optional raw min/max range.
//! * **InputOutputControlByIdentifier (0x2F)** — actuator / output test: drive
//!   an IO DID with shortTermAdjustment, then return control to the ECU.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors from the adaptation engine and its I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdaptationError {
    /// No channel with this name exists in the definition.
    UnknownChannel(String),
    /// A channel was used through the wrong mechanism (e.g. [`read_value`] on a
    /// routine channel).
    ChannelKindMismatch {
        /// Channel name.
        channel: String,
        /// The mechanism the operation required.
        expected: &'static str,
    },
    /// A value does not fit the field's byte width or falls outside its declared
    /// range.
    InvalidValue {
        /// Channel name (or `"<field>"`).
        channel: String,
        /// The rejected raw value.
        value: u64,
        /// Why it was rejected.
        reason: String,
    },
    /// A numeric field's byte range falls outside the data it was read from.
    OutOfRange {
        /// Field/channel name (or `"<field>"`).
        field: String,
        /// Byte offset referenced.
        offset: usize,
        /// Data length available.
        len: usize,
    },
    /// The definition itself is invalid (duplicate names, bad width, inverted
    /// range, …). Deserialization alone does not validate.
    InvalidDef(String),
    /// A UDS request returned a negative response (0x7F) or an unexpected SID.
    Protocol(String),
    /// The transport itself failed.
    Transport(String),
    /// A read-back after writing did not match what was written.
    VerifyMismatch {
        /// DID that failed to verify.
        did: u16,
    },
}

impl std::fmt::Display for AdaptationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdaptationError::UnknownChannel(c) => write!(f, "unknown channel `{c}`"),
            AdaptationError::ChannelKindMismatch { channel, expected } => {
                write!(f, "channel `{channel}` is not a {expected} channel")
            }
            AdaptationError::InvalidValue {
                channel,
                value,
                reason,
            } => write!(f, "invalid value {value} for `{channel}`: {reason}"),
            AdaptationError::OutOfRange { field, offset, len } => {
                write!(f, "`{field}` byte {offset} out of range (len {len})")
            }
            AdaptationError::InvalidDef(s) => write!(f, "invalid adaptation definition: {s}"),
            AdaptationError::Protocol(s) => write!(f, "UDS protocol error: {s}"),
            AdaptationError::Transport(s) => write!(f, "transport error: {s}"),
            AdaptationError::VerifyMismatch { did } => {
                write!(f, "read-back verify mismatch for DID 0x{did:04X}")
            }
        }
    }
}

impl std::error::Error for AdaptationError {}

/// Crate result type.
pub type Result<T> = std::result::Result<T, AdaptationError>;

// ---------------------------------------------------------------------------
// Definition format (data)
// ---------------------------------------------------------------------------

/// Linear scaling applied to a raw numeric field: `engineering = raw * factor +
/// offset`. The `unit` is presentation only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scaling {
    /// Multiplicative factor (defaults to 1.0).
    #[serde(default = "default_factor")]
    pub factor: f64,
    /// Additive offset in engineering units (defaults to 0.0).
    #[serde(default)]
    pub offset: f64,
    /// Engineering unit label (free text, e.g. `"°C"`, `"%"`, `"rpm"`).
    #[serde(default)]
    pub unit: String,
}

impl Default for Scaling {
    fn default() -> Self {
        Scaling {
            factor: 1.0,
            offset: 0.0,
            unit: String::new(),
        }
    }
}

impl Scaling {
    /// Apply the scaling to a raw value.
    pub fn apply(&self, raw: u64) -> f64 {
        raw as f64 * self.factor + self.offset
    }
}

fn default_factor() -> f64 {
    1.0
}

fn default_width() -> u8 {
    1
}

/// A big-endian numeric field within a byte record: `width` bytes at `offset`,
/// most-significant byte first, interpreted through `scaling`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NumericField {
    /// Byte offset of the field's most-significant byte within the record.
    #[serde(default)]
    pub offset: usize,
    /// Field width in bytes (1..=8). Defaults to 1.
    #[serde(default = "default_width")]
    pub width: u8,
    /// Scaling and unit for presentation.
    #[serde(default)]
    pub scaling: Scaling,
}

impl Default for NumericField {
    fn default() -> Self {
        NumericField {
            offset: 0,
            width: default_width(),
            scaling: Scaling::default(),
        }
    }
}

/// The UDS mechanism and parameters of an [`AdaptationChannel`]. Serialized with
/// a `"kind"` tag: `"routine"`, `"value"` or `"actuator"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChannelKind {
    /// **RoutineControl (0x31)**: start / stop / requestResults a routine by id.
    Routine {
        /// RoutineControl routine identifier.
        id: u16,
        /// Data record sent with startRoutine (0x01); empty if none.
        #[serde(default)]
        start_data: Vec<u8>,
        /// Data record sent with stopRoutine (0x02); empty if none.
        #[serde(default)]
        stop_data: Vec<u8>,
        /// Optional interpretation of a numeric result in the routine's response
        /// data record.
        #[serde(default)]
        result: Option<NumericField>,
    },
    /// **Read/WriteDataByIdentifier (0x22 / 0x2E)**: an adaptation value held in
    /// a DID as a big-endian numeric field with scaling and an optional raw
    /// range.
    Value {
        /// Data identifier holding the value.
        did: u16,
        /// The numeric field within the DID data.
        #[serde(default)]
        field: NumericField,
        /// Inclusive minimum raw value accepted on write (if set).
        #[serde(default)]
        min: Option<u64>,
        /// Inclusive maximum raw value accepted on write (if set).
        #[serde(default)]
        max: Option<u64>,
    },
    /// **InputOutputControlByIdentifier (0x2F)**: an IO DID actuated for a
    /// component test.
    Actuator {
        /// Data identifier of the IO control.
        did: u16,
        /// Default control-state record sent with shortTermAdjustment when the
        /// caller supplies none; empty if none.
        #[serde(default)]
        state: Vec<u8>,
        /// Optional interpretation of a numeric status in the 0x2F response.
        #[serde(default)]
        status: Option<NumericField>,
    },
}

/// One named adaptation channel: a mechanism plus its parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdaptationChannel {
    /// Display name (unique within the definition).
    pub name: String,
    /// The UDS mechanism and its parameters.
    #[serde(flatten)]
    pub kind: ChannelKind,
    /// Optional explanatory note.
    #[serde(default)]
    pub note: String,
}

/// A data-defined adaptation-channel set for one module.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdaptationDef {
    /// Stable id, e.g. `"vw-engine-med17"`.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Module/platform note (free text).
    #[serde(default)]
    pub module: String,
    /// Whether writing / actuating requires UDS SecurityAccess (0x27) to be
    /// unlocked first. The engine does not perform the unlock; it is advisory for
    /// the caller and surfaced in every report.
    #[serde(default)]
    pub requires_security: bool,
    /// The channels.
    pub channels: Vec<AdaptationChannel>,
}

impl AdaptationDef {
    /// Parse a definition from JSON.
    pub fn from_json(s: &str) -> std::result::Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    /// Find a channel by name.
    pub fn channel(&self, name: &str) -> Option<&AdaptationChannel> {
        self.channels.iter().find(|c| c.name == name)
    }

    /// Semantically validate the definition. `serde` deserialization only proves
    /// the JSON shape; this checks the invariants the engine relies on: at least
    /// one channel, **unique** channel names, every numeric field `width` in
    /// `1..=8`, and, for value channels, a non-inverted `min <= max` range whose
    /// bounds fit the field width. Called before any write or actuation.
    pub fn validate(&self) -> Result<()> {
        if self.channels.is_empty() {
            return Err(AdaptationError::InvalidDef("no channels declared".into()));
        }
        let mut seen = std::collections::HashSet::new();
        for ch in &self.channels {
            if !seen.insert(ch.name.as_str()) {
                return Err(AdaptationError::InvalidDef(format!(
                    "duplicate channel name `{}`",
                    ch.name
                )));
            }
            match &ch.kind {
                ChannelKind::Routine { result, .. } => {
                    if let Some(r) = result {
                        check_width(&ch.name, r.width)?;
                    }
                }
                ChannelKind::Value {
                    field, min, max, ..
                } => {
                    check_width(&ch.name, field.width)?;
                    let cap = width_cap(field.width);
                    for (which, bound) in [("min", min), ("max", max)] {
                        if let Some(v) = bound {
                            if *v > cap {
                                return Err(AdaptationError::InvalidDef(format!(
                                    "`{}` {which} {v} does not fit in {} byte(s)",
                                    ch.name, field.width
                                )));
                            }
                        }
                    }
                    if let (Some(lo), Some(hi)) = (min, max) {
                        if lo > hi {
                            return Err(AdaptationError::InvalidDef(format!(
                                "`{}` range is inverted (min {lo} > max {hi})",
                                ch.name
                            )));
                        }
                    }
                }
                ChannelKind::Actuator { status, .. } => {
                    if let Some(s) = status {
                        check_width(&ch.name, s.width)?;
                    }
                }
            }
        }
        Ok(())
    }
}

fn check_width(name: &str, width: u8) -> Result<()> {
    if width == 0 || width > 8 {
        return Err(AdaptationError::InvalidDef(format!(
            "`{name}` field width {width} is not 1..=8"
        )));
    }
    Ok(())
}

/// The largest raw value representable in `width` bytes.
fn width_cap(width: u8) -> u64 {
    if width >= 8 {
        u64::MAX
    } else {
        (1u64 << (width as u32 * 8)) - 1
    }
}

// ---------------------------------------------------------------------------
// Pure numeric-field engine
// ---------------------------------------------------------------------------

/// Read a big-endian unsigned integer of `width` bytes at `offset` from `data`.
/// Errors if the field extends past `data`, or `width` is 0 or > 8.
pub fn read_be(data: &[u8], offset: usize, width: u8) -> Result<u64> {
    if width == 0 || width > 8 {
        return Err(AdaptationError::InvalidValue {
            channel: "<field>".into(),
            value: width as u64,
            reason: "width must be 1..=8 bytes".into(),
        });
    }
    let w = width as usize;
    if offset + w > data.len() {
        return Err(AdaptationError::OutOfRange {
            field: "<field>".into(),
            offset,
            len: data.len(),
        });
    }
    let mut val: u64 = 0;
    for &b in &data[offset..offset + w] {
        val = (val << 8) | b as u64;
    }
    Ok(val)
}

/// Write `val` as a big-endian unsigned integer of `width` bytes at `offset`
/// into `data`. Errors if `width` is out of range, `val` does not fit `width`
/// bytes, or the field extends past `data`.
pub fn write_be(data: &mut [u8], offset: usize, width: u8, val: u64) -> Result<()> {
    if width == 0 || width > 8 {
        return Err(AdaptationError::InvalidValue {
            channel: "<field>".into(),
            value: width as u64,
            reason: "width must be 1..=8 bytes".into(),
        });
    }
    if val > width_cap(width) {
        return Err(AdaptationError::InvalidValue {
            channel: "<field>".into(),
            value: val,
            reason: format!("does not fit in {width} byte(s)"),
        });
    }
    let w = width as usize;
    if offset + w > data.len() {
        return Err(AdaptationError::OutOfRange {
            field: "<field>".into(),
            offset,
            len: data.len(),
        });
    }
    for (i, slot) in data[offset..offset + w].iter_mut().rev().enumerate() {
        *slot = (val >> (8 * i as u32)) as u8;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// I/O layer (UDS over a tiny transport abstraction)
// ---------------------------------------------------------------------------

/// The request→response seam (and a blanket closure impl) lives in
/// `opentune-core` so a single `CoreDiag` adapter drives every service engine.
pub use opentune_core::RawDiag;

/// Check a UDS response against the request's service id (`req_sid`): accept a
/// positive response (`req_sid + 0x40`), and map a negative response (0x7F) to a
/// descriptive error — rejecting a truncated 0x7F and one that echoes a
/// *different* requested service (a mis-correlated response).
fn check(req_sid: u8, resp: &[u8]) -> Result<()> {
    if resp.is_empty() {
        return Err(AdaptationError::Protocol("empty response".into()));
    }
    if resp[0] == 0x7F {
        if resp.len() < 3 {
            return Err(AdaptationError::Protocol(
                "truncated negative response".into(),
            ));
        }
        if resp[1] != req_sid {
            return Err(AdaptationError::Protocol(format!(
                "negative response echoes service 0x{:02X}, expected 0x{req_sid:02X}",
                resp[1]
            )));
        }
        return Err(AdaptationError::Protocol(format!(
            "negative response 0x{:02X} to service 0x{req_sid:02X}",
            resp[2]
        )));
    }
    let want = req_sid.wrapping_add(0x40);
    if resp[0] != want {
        return Err(AdaptationError::Protocol(format!(
            "unexpected response SID 0x{:02X} (wanted 0x{want:02X})",
            resp[0]
        )));
    }
    Ok(())
}

/// Read one DID with ReadDataByIdentifier (0x22) and return its data bytes.
fn read_did(diag: &mut dyn RawDiag, did: u16) -> Result<Vec<u8>> {
    let req = [0x22, (did >> 8) as u8, did as u8];
    let resp = diag.request(&req).map_err(AdaptationError::Transport)?;
    check(0x22, &resp)?;
    if resp.len() < 3 || resp[1] != (did >> 8) as u8 || resp[2] != did as u8 {
        return Err(AdaptationError::Protocol(format!(
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
    let resp = diag.request(&req).map_err(AdaptationError::Transport)?;
    check(0x2E, &resp)?;
    if resp.len() < 3 || resp[1] != (did >> 8) as u8 || resp[2] != did as u8 {
        return Err(AdaptationError::Protocol(format!(
            "0x2E response did not echo DID 0x{did:04X}"
        )));
    }
    Ok(())
}

/// Issue a RoutineControl (0x31) request with sub-function `sub` and return the
/// response's data record (bytes after `71 sub idhi idlo`). Rejects a response
/// that does not echo the sub-function and routine id.
fn routine_control(diag: &mut dyn RawDiag, sub: u8, id: u16, data: &[u8]) -> Result<Vec<u8>> {
    let mut req = Vec::with_capacity(4 + data.len());
    req.extend_from_slice(&[0x31, sub, (id >> 8) as u8, id as u8]);
    req.extend_from_slice(data);
    let resp = diag.request(&req).map_err(AdaptationError::Transport)?;
    check(0x31, &resp)?;
    if resp.len() < 4 || resp[1] != sub || resp[2] != (id >> 8) as u8 || resp[3] != id as u8 {
        return Err(AdaptationError::Protocol(format!(
            "0x31 response did not echo sub-function 0x{sub:02X} / routine 0x{id:04X}"
        )));
    }
    Ok(resp[4..].to_vec())
}

/// Issue an InputOutputControlByIdentifier (0x2F) request with control option
/// `opt` and return the response's control-state record (bytes after `6F idhi
/// idlo opt`). Rejects a response that does not echo the DID and control option.
fn io_control(diag: &mut dyn RawDiag, did: u16, opt: u8, state: &[u8]) -> Result<Vec<u8>> {
    let mut req = Vec::with_capacity(4 + state.len());
    req.extend_from_slice(&[0x2F, (did >> 8) as u8, did as u8, opt]);
    req.extend_from_slice(state);
    let resp = diag.request(&req).map_err(AdaptationError::Transport)?;
    check(0x2F, &resp)?;
    if resp.len() < 4 || resp[1] != (did >> 8) as u8 || resp[2] != did as u8 || resp[3] != opt {
        return Err(AdaptationError::Protocol(format!(
            "0x2F response did not echo DID 0x{did:04X} / option 0x{opt:02X}"
        )));
    }
    Ok(resp[4..].to_vec())
}

// InputOutputControl option bytes (ISO 14229).
const IO_RETURN_CONTROL_TO_ECU: u8 = 0x00;
const IO_SHORT_TERM_ADJUSTMENT: u8 = 0x03;

// ---------------------------------------------------------------------------
// Options shared by writing / actuating
// ---------------------------------------------------------------------------

/// Options for [`write_value`] and [`run_actuator_test`].
#[derive(Debug, Clone, Default)]
pub struct WriteOptions {
    /// Produce the plan without performing any I/O (no write / no actuation).
    pub dry_run: bool,
    /// Skip the post-write read-back verify (not recommended; [`write_value`]
    /// only).
    pub skip_verify: bool,
}

// ---------------------------------------------------------------------------
// Value channels (0x22 / 0x2E)
// ---------------------------------------------------------------------------

/// A decoded adaptation value: its raw integer and scaled engineering value.
#[derive(Debug, Clone, PartialEq)]
pub struct ValueReading {
    /// Channel name.
    pub channel: String,
    /// Raw big-endian field value.
    pub raw: u64,
    /// Scaled engineering value (`raw * factor + offset`).
    pub engineering: f64,
    /// Engineering unit label.
    pub unit: String,
}

/// Borrow a value channel's `(did, field, min, max)`, or error if the named
/// channel is missing or of another kind.
fn value_channel<'a>(
    def: &'a AdaptationDef,
    name: &str,
) -> Result<(u16, &'a NumericField, Option<u64>, Option<u64>)> {
    let ch = def
        .channel(name)
        .ok_or_else(|| AdaptationError::UnknownChannel(name.to_string()))?;
    match &ch.kind {
        ChannelKind::Value {
            did,
            field,
            min,
            max,
        } => Ok((*did, field, *min, *max)),
        _ => Err(AdaptationError::ChannelKindMismatch {
            channel: name.to_string(),
            expected: "value",
        }),
    }
}

/// Read a value channel's current reading (0x22), decoding its numeric field.
pub fn read_value(diag: &mut dyn RawDiag, def: &AdaptationDef, channel: &str) -> Result<ValueReading> {
    def.validate()?;
    let (did, field, _min, _max) = value_channel(def, channel)?;
    let data = read_did(diag, did)?;
    let raw = read_be(&data, field.offset, field.width).map_err(|e| relabel(e, channel))?;
    Ok(ValueReading {
        channel: channel.to_string(),
        raw,
        engineering: field.scaling.apply(raw),
        unit: field.scaling.unit.clone(),
    })
}

/// Re-label a generic `<field>` error with the channel name.
fn relabel(e: AdaptationError, channel: &str) -> AdaptationError {
    match e {
        AdaptationError::OutOfRange { offset, len, .. } => AdaptationError::OutOfRange {
            field: channel.to_string(),
            offset,
            len,
        },
        AdaptationError::InvalidValue { value, reason, .. } => AdaptationError::InvalidValue {
            channel: channel.to_string(),
            value,
            reason,
        },
        other => other,
    }
}

/// What happened in a [`write_value`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    /// `dry_run` — the plan was produced, nothing was written.
    Planned,
    /// The new value equals the live value; nothing written.
    NoChange,
    /// The DID was written and read back identical.
    Verified,
    /// The DID was written but verification was skipped by request.
    Unverified,
    /// The written DID did not read back as written.
    VerifyFailed {
        /// The DID that failed to verify.
        did: u16,
    },
    /// The write failed; the DID's state is **unknown** and must not be blindly
    /// retried. The `backup` is retained for restore.
    WriteError {
        /// The DID whose write did not complete.
        did: u16,
        /// The underlying error text.
        message: String,
    },
}

/// The outcome of a (planned or executed) value write. `backup` is always the
/// live DID data read immediately before writing, so a restore is possible even
/// after a failure.
#[derive(Debug, Clone, PartialEq)]
pub struct ValueWriteReport {
    /// Channel written.
    pub channel: String,
    /// DID behind the channel.
    pub did: u16,
    /// Full DID data read immediately before writing (for restore).
    pub backup: Vec<u8>,
    /// Full DID data that would be / was written (backup + spliced field).
    pub target: Vec<u8>,
    /// Raw field value before.
    pub old_raw: u64,
    /// Raw field value requested.
    pub new_raw: u64,
    /// What happened.
    pub outcome: WriteOutcome,
    /// True when [`AdaptationDef::requires_security`] — a reminder the caller
    /// must have unlocked SecurityAccess (0x27) first.
    pub security_required: bool,
}

impl ValueWriteReport {
    /// True only when the DID was written and verified.
    pub fn is_verified(&self) -> bool {
        self.outcome == WriteOutcome::Verified
    }
}

/// Validate `new_raw` against a value channel's width and declared range.
fn validate_raw(channel: &str, field: &NumericField, min: Option<u64>, max: Option<u64>, new_raw: u64) -> Result<()> {
    if new_raw > width_cap(field.width) {
        return Err(AdaptationError::InvalidValue {
            channel: channel.to_string(),
            value: new_raw,
            reason: format!("does not fit in {} byte(s)", field.width),
        });
    }
    if let Some(lo) = min {
        if new_raw < lo {
            return Err(AdaptationError::InvalidValue {
                channel: channel.to_string(),
                value: new_raw,
                reason: format!("below minimum {lo}"),
            });
        }
    }
    if let Some(hi) = max {
        if new_raw > hi {
            return Err(AdaptationError::InvalidValue {
                channel: channel.to_string(),
                value: new_raw,
                reason: format!("above maximum {hi}"),
            });
        }
    }
    Ok(())
}

/// Write a new raw value to a value channel (0x2E), safely.
///
/// The DID is read **fresh** first (the `backup`), the target field is spliced
/// into that fresh read, the new value is range-validated, and — unless
/// [`WriteOptions::dry_run`] — the merged record is written and (unless
/// [`WriteOptions::skip_verify`]) read back to verify. Unrelated bytes in the DID
/// are preserved from the fresh read.
///
/// Returns `Err` only when **nothing was written** (invalid definition, wrong
/// channel kind, out-of-range value, or the initial read failed). Once writing
/// starts, the result is `Ok(report)` whose [`ValueWriteReport::outcome`] records
/// success, a verify failure or a write error — always with the `backup`.
///
/// This does **not** perform SecurityAccess; when `def.requires_security` the
/// caller must already have unlocked the module (the report flags it).
pub fn write_value(
    diag: &mut dyn RawDiag,
    def: &AdaptationDef,
    channel: &str,
    new_raw: u64,
    opts: &WriteOptions,
) -> Result<ValueWriteReport> {
    def.validate()?;
    let (did, field, min, max) = value_channel(def, channel)?;
    validate_raw(channel, field, min, max, new_raw)?;

    // Read fresh: this is the backup and the splice base.
    let backup = read_did(diag, did)?;
    let old_raw = read_be(&backup, field.offset, field.width).map_err(|e| relabel(e, channel))?;
    let mut target = backup.clone();
    write_be(&mut target, field.offset, field.width, new_raw).map_err(|e| relabel(e, channel))?;

    let mut report = ValueWriteReport {
        channel: channel.to_string(),
        did,
        backup: backup.clone(),
        target: target.clone(),
        old_raw,
        new_raw,
        outcome: WriteOutcome::NoChange,
        security_required: def.requires_security,
    };

    if opts.dry_run {
        report.outcome = WriteOutcome::Planned;
        return Ok(report);
    }
    if target == backup {
        return Ok(report); // NoChange
    }

    if let Err(e) = write_did(diag, did, &target) {
        report.outcome = WriteOutcome::WriteError {
            did,
            message: e.to_string(),
        };
        return Ok(report);
    }

    if opts.skip_verify {
        report.outcome = WriteOutcome::Unverified;
        return Ok(report);
    }
    let after = read_did(diag, did)?;
    report.outcome = if after == target {
        WriteOutcome::Verified
    } else {
        WriteOutcome::VerifyFailed { did }
    };
    Ok(report)
}

// ---------------------------------------------------------------------------
// Routine channels (0x31)
// ---------------------------------------------------------------------------

/// Which RoutineControl sub-function to issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutineOp {
    /// startRoutine (0x01).
    Start,
    /// stopRoutine (0x02).
    Stop,
    /// requestRoutineResults (0x03).
    RequestResults,
}

impl RoutineOp {
    /// The UDS sub-function byte for this operation.
    pub fn subfunction(self) -> u8 {
        match self {
            RoutineOp::Start => 0x01,
            RoutineOp::Stop => 0x02,
            RoutineOp::RequestResults => 0x03,
        }
    }
}

/// The result of one RoutineControl exchange.
#[derive(Debug, Clone, PartialEq)]
pub struct RoutineResult {
    /// Channel name.
    pub channel: String,
    /// The operation performed.
    pub op: RoutineOp,
    /// Raw response data record (bytes after the routine-id echo).
    pub data: Vec<u8>,
    /// Decoded numeric result, when the channel declares a `result` field and the
    /// data record is long enough.
    pub value: Option<f64>,
    /// Engineering unit for `value` (empty when none).
    pub unit: String,
    /// True when [`AdaptationDef::requires_security`].
    pub security_required: bool,
}

/// Run a routine channel's RoutineControl operation (0x31).
///
/// For [`RoutineOp::Start`] / [`RoutineOp::Stop`] the channel's `start_data` /
/// `stop_data` is sent unless `extra` is non-empty, in which case `extra`
/// overrides it. [`RoutineOp::RequestResults`] sends `extra` (usually empty). A
/// declared `result` field decodes a numeric value from the response.
pub fn run_routine(
    diag: &mut dyn RawDiag,
    def: &AdaptationDef,
    channel: &str,
    op: RoutineOp,
    extra: &[u8],
) -> Result<RoutineResult> {
    def.validate()?;
    let ch = def
        .channel(channel)
        .ok_or_else(|| AdaptationError::UnknownChannel(channel.to_string()))?;
    let (id, start_data, stop_data, result) = match &ch.kind {
        ChannelKind::Routine {
            id,
            start_data,
            stop_data,
            result,
        } => (*id, start_data, stop_data, result),
        _ => {
            return Err(AdaptationError::ChannelKindMismatch {
                channel: channel.to_string(),
                expected: "routine",
            })
        }
    };

    let data: Vec<u8> = if !extra.is_empty() {
        extra.to_vec()
    } else {
        match op {
            RoutineOp::Start => start_data.clone(),
            RoutineOp::Stop => stop_data.clone(),
            RoutineOp::RequestResults => Vec::new(),
        }
    };

    let record = routine_control(diag, op.subfunction(), id, &data)?;
    let (value, unit) = match result {
        Some(r) => match read_be(&record, r.offset, r.width) {
            Ok(raw) => (Some(r.scaling.apply(raw)), r.scaling.unit.clone()),
            Err(_) => (None, String::new()),
        },
        None => (None, String::new()),
    };

    Ok(RoutineResult {
        channel: channel.to_string(),
        op,
        data: record,
        value,
        unit,
        security_required: def.requires_security,
    })
}

// ---------------------------------------------------------------------------
// Actuator channels (0x2F)
// ---------------------------------------------------------------------------

/// What happened in a [`run_actuator_test`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActuatorOutcome {
    /// `dry_run` — nothing was actuated.
    Planned,
    /// The adjustment was applied and control was handed back to the ECU.
    Completed,
    /// The adjustment was applied but control could **not** be returned — the
    /// output may be left forced; the caller must recover. (The dangerous case.)
    ControlReturnFailed {
        /// Why returning control failed.
        message: String,
    },
    /// The adjustment itself failed; a returnControlToECU was still attempted and
    /// its success is reflected in [`ActuatorReport::control_returned`].
    ApplyFailed {
        /// Why the adjustment failed.
        message: String,
    },
}

/// The outcome of a (planned or executed) actuator test.
#[derive(Debug, Clone, PartialEq)]
pub struct ActuatorReport {
    /// Channel actuated.
    pub channel: String,
    /// DID behind the channel.
    pub did: u16,
    /// Whether the shortTermAdjustment was acknowledged.
    pub applied: bool,
    /// Whether returnControlToECU was acknowledged (true is the safe end state).
    pub control_returned: bool,
    /// Control-state record returned by the shortTermAdjustment response.
    pub response: Vec<u8>,
    /// Decoded numeric status, when the channel declares a `status` field.
    pub status: Option<f64>,
    /// Engineering unit for `status` (empty when none).
    pub status_unit: String,
    /// What happened.
    pub outcome: ActuatorOutcome,
    /// True when [`AdaptationDef::requires_security`].
    pub security_required: bool,
}

impl ActuatorReport {
    /// True only when the output was actuated and control handed back cleanly.
    pub fn is_completed(&self) -> bool {
        self.outcome == ActuatorOutcome::Completed
    }
}

/// Run an actuator / output test on an actuator channel (0x2F).
///
/// Issues **shortTermAdjustment** (0x2F option 0x03) with `state` (or the
/// channel's default `state` when `state` is empty), then **always** issues
/// **returnControlToECU** (option 0x00) — even if the adjustment or a status read
/// failed — so the output is never knowingly left forced. If control could not
/// be returned the report's outcome is [`ActuatorOutcome::ControlReturnFailed`].
///
/// With [`WriteOptions::dry_run`] no I/O is performed. Returns `Err` only for
/// pre-flight failures (invalid definition, unknown channel, wrong kind).
pub fn run_actuator_test(
    diag: &mut dyn RawDiag,
    def: &AdaptationDef,
    channel: &str,
    state: &[u8],
    opts: &WriteOptions,
) -> Result<ActuatorReport> {
    def.validate()?;
    let ch = def
        .channel(channel)
        .ok_or_else(|| AdaptationError::UnknownChannel(channel.to_string()))?;
    let (did, default_state, status_field) = match &ch.kind {
        ChannelKind::Actuator { did, state, status } => (*did, state, status),
        _ => {
            return Err(AdaptationError::ChannelKindMismatch {
                channel: channel.to_string(),
                expected: "actuator",
            })
        }
    };

    let mut report = ActuatorReport {
        channel: channel.to_string(),
        did,
        applied: false,
        control_returned: false,
        response: Vec::new(),
        status: None,
        status_unit: String::new(),
        outcome: ActuatorOutcome::Planned,
        security_required: def.requires_security,
    };

    if opts.dry_run {
        return Ok(report);
    }

    let drive: Vec<u8> = if state.is_empty() {
        default_state.clone()
    } else {
        state.to_vec()
    };

    // Apply the shortTermAdjustment, capturing — but not short-circuiting on — an
    // error, so control is always returned afterwards.
    let apply = io_control(diag, did, IO_SHORT_TERM_ADJUSTMENT, &drive);
    let apply_err = match apply {
        Ok(record) => {
            report.applied = true;
            if let Some(f) = status_field {
                if let Ok(raw) = read_be(&record, f.offset, f.width) {
                    report.status = Some(f.scaling.apply(raw));
                    report.status_unit = f.scaling.unit.clone();
                }
            }
            report.response = record;
            None
        }
        Err(e) => Some(e.to_string()),
    };

    // Always hand control back to the ECU.
    let ret = io_control(diag, did, IO_RETURN_CONTROL_TO_ECU, &[]);
    report.control_returned = ret.is_ok();

    report.outcome = match (apply_err, &ret) {
        (Some(message), _) => ActuatorOutcome::ApplyFailed { message },
        (None, Ok(_)) => ActuatorOutcome::Completed,
        (None, Err(e)) => ActuatorOutcome::ControlReturnFailed {
            message: e.to_string(),
        },
    };
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn sample_def() -> AdaptationDef {
        AdaptationDef {
            id: "test-mod".into(),
            name: "Test module".into(),
            module: "synthetic".into(),
            requires_security: true,
            channels: vec![
                AdaptationChannel {
                    name: "Idle RPM".into(),
                    kind: ChannelKind::Value {
                        did: 0x2001,
                        field: NumericField {
                            offset: 0,
                            width: 2,
                            scaling: Scaling {
                                factor: 0.25,
                                offset: 0.0,
                                unit: "rpm".into(),
                            },
                        },
                        min: Some(2000),
                        max: Some(4000),
                    },
                    note: String::new(),
                },
                AdaptationChannel {
                    name: "Throttle learn".into(),
                    kind: ChannelKind::Routine {
                        id: 0x0203,
                        start_data: vec![0x01],
                        stop_data: vec![],
                        result: Some(NumericField {
                            offset: 1,
                            width: 1,
                            scaling: Scaling {
                                factor: 1.0,
                                offset: 0.0,
                                unit: "%".into(),
                            },
                        }),
                    },
                    note: String::new(),
                },
                AdaptationChannel {
                    name: "Cooling fan".into(),
                    kind: ChannelKind::Actuator {
                        did: 0x3010,
                        state: vec![0x01],
                        status: Some(NumericField {
                            offset: 0,
                            width: 1,
                            scaling: Scaling::default(),
                        }),
                    },
                    note: String::new(),
                },
            ],
        }
    }

    // --- pure engine -------------------------------------------------------

    #[test]
    fn be_read_write_roundtrip_and_bounds() {
        let mut d = vec![0u8; 4];
        write_be(&mut d, 1, 2, 0xABCD).unwrap();
        assert_eq!(d, vec![0x00, 0xAB, 0xCD, 0x00]); // big-endian, MSB first
        assert_eq!(read_be(&d, 1, 2).unwrap(), 0xABCD);
        // value does not fit width
        assert!(matches!(
            write_be(&mut d, 0, 1, 0x100),
            Err(AdaptationError::InvalidValue { .. })
        ));
        // out of range
        assert!(matches!(
            write_be(&mut d, 3, 2, 1),
            Err(AdaptationError::OutOfRange { .. })
        ));
        assert!(matches!(
            read_be(&d, 3, 2),
            Err(AdaptationError::OutOfRange { .. })
        ));
    }

    #[test]
    fn def_validate_catches_bad_defs() {
        assert!(sample_def().validate().is_ok());
        // duplicate channel name
        let mut d = sample_def();
        d.channels[1].name = "Idle RPM".into();
        assert!(matches!(d.validate(), Err(AdaptationError::InvalidDef(_))));
        // zero width
        let mut d2 = sample_def();
        if let ChannelKind::Value { field, .. } = &mut d2.channels[0].kind {
            field.width = 0;
        }
        assert!(matches!(d2.validate(), Err(AdaptationError::InvalidDef(_))));
        // inverted range
        let mut d3 = sample_def();
        if let ChannelKind::Value { min, max, .. } = &mut d3.channels[0].kind {
            *min = Some(5000);
            *max = Some(100);
        }
        assert!(matches!(d3.validate(), Err(AdaptationError::InvalidDef(_))));
        // bound does not fit width
        let mut d4 = sample_def();
        if let ChannelKind::Value { field, max, .. } = &mut d4.channels[0].kind {
            field.width = 1;
            *max = Some(300); // > 255
        }
        assert!(matches!(d4.validate(), Err(AdaptationError::InvalidDef(_))));
        // empty def
        let mut d5 = sample_def();
        d5.channels.clear();
        assert!(matches!(d5.validate(), Err(AdaptationError::InvalidDef(_))));
    }

    #[test]
    fn def_parses_from_json() {
        let js = r#"{
          "id":"vw-eng","name":"Engine","module":"med17","requires_security":true,
          "channels":[
            {"name":"Idle","kind":"value","did":8193,
             "field":{"offset":0,"width":2,"scaling":{"factor":0.25,"unit":"rpm"}},
             "min":2000,"max":4000},
            {"name":"Learn","kind":"routine","id":515,"start_data":[1]},
            {"name":"Fan","kind":"actuator","did":12304,"state":[1]}
          ]
        }"#;
        let def = AdaptationDef::from_json(js).unwrap();
        assert!(def.validate().is_ok());
        assert_eq!(def.channels.len(), 3);
        assert!(matches!(
            def.channel("Idle").unwrap().kind,
            ChannelKind::Value { did: 0x2001, .. }
        ));
    }

    // --- scripted responder ------------------------------------------------

    /// Answers 0x22/0x2E from a per-DID store, 0x31 by echoing and appending a
    /// canned routine result, 0x2F by echoing, and logs every request.
    struct Script {
        store: HashMap<u16, Vec<u8>>,
        routine_result: Vec<u8>,
        log: Vec<Vec<u8>>,
    }
    impl Script {
        fn new(blocks: &[(u16, Vec<u8>)]) -> Self {
            Script {
                store: blocks.iter().cloned().collect(),
                routine_result: Vec::new(),
                log: Vec::new(),
            }
        }
        fn with_routine_result(mut self, r: Vec<u8>) -> Self {
            self.routine_result = r;
            self
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
                0x31 => {
                    // 71 sub idhi idlo <result>
                    let mut r = vec![0x71, req[1], req[2], req[3]];
                    r.extend_from_slice(&self.routine_result);
                    Ok(r)
                }
                0x2F => {
                    // 6F idhi idlo opt <echo of control state>
                    let mut r = vec![0x6F, req[1], req[2], req[3]];
                    r.extend_from_slice(&req[4..]);
                    Ok(r)
                }
                _ => Err("unexpected service".into()),
            }
        }
    }

    // --- value channels ----------------------------------------------------

    #[test]
    fn read_value_decodes_scaling() {
        let def = sample_def();
        // 0x0BB8 = 3000 raw -> 3000 * 0.25 = 750.0 rpm
        let mut io = Script::new(&[(0x2001, vec![0x0B, 0xB8])]);
        let r = read_value(&mut io, &def, "Idle RPM").unwrap();
        assert_eq!(r.raw, 3000);
        assert_eq!(r.engineering, 750.0);
        assert_eq!(r.unit, "rpm");
    }

    #[test]
    fn write_value_verifies_and_preserves_unrelated_bytes() {
        let def = sample_def();
        // DID has a trailing unrelated byte 0xEE that must survive the write.
        let mut io = Script::new(&[(0x2001, vec![0x0B, 0xB8, 0xEE])]);
        let rep = write_value(&mut io, &def, "Idle RPM", 3200, &WriteOptions::default()).unwrap();
        assert!(rep.is_verified() && rep.security_required);
        assert_eq!(rep.old_raw, 3000);
        assert_eq!(rep.new_raw, 3200);
        // read back: new value applied, unrelated byte preserved
        let after = read_value(&mut io, &def, "Idle RPM").unwrap();
        assert_eq!(after.raw, 3200);
        assert_eq!(io.store.get(&0x2001).unwrap()[2], 0xEE);
    }

    #[test]
    fn write_value_dry_run_writes_nothing() {
        let def = sample_def();
        let mut io = Script::new(&[(0x2001, vec![0x0B, 0xB8])]);
        let rep = write_value(
            &mut io,
            &def,
            "Idle RPM",
            3200,
            &WriteOptions {
                dry_run: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(rep.outcome, WriteOutcome::Planned);
        // no 0x2E was ever sent
        assert!(io.log.iter().all(|r| r[0] != 0x2E));
        assert_eq!(read_value(&mut io, &def, "Idle RPM").unwrap().raw, 3000);
    }

    #[test]
    fn write_value_rejects_out_of_range() {
        let def = sample_def();
        let mut io = Script::new(&[(0x2001, vec![0x0B, 0xB8])]);
        // max is 4000; 5000 is rejected before any I/O
        assert!(matches!(
            write_value(&mut io, &def, "Idle RPM", 5000, &WriteOptions::default()),
            Err(AdaptationError::InvalidValue { .. })
        ));
        assert!(io.log.is_empty());
    }

    #[test]
    fn wrong_channel_kind_is_rejected() {
        let def = sample_def();
        let mut io = Script::new(&[]);
        assert!(matches!(
            read_value(&mut io, &def, "Cooling fan"),
            Err(AdaptationError::ChannelKindMismatch { .. })
        ));
        assert!(matches!(
            read_value(&mut io, &def, "Nope"),
            Err(AdaptationError::UnknownChannel(_))
        ));
    }

    // --- routine channels --------------------------------------------------

    #[test]
    fn routine_roundtrip_decodes_result() {
        let def = sample_def();
        // result field is offset 1 width 1; canned record [0x00, 0x2A] -> 42 %
        let mut io = Script::new(&[]).with_routine_result(vec![0x00, 0x2A]);
        let start = run_routine(&mut io, &def, "Throttle learn", RoutineOp::Start, &[]).unwrap();
        assert_eq!(start.op, RoutineOp::Start);
        // start sent the channel's start_data (0x01)
        assert_eq!(io.log[0], vec![0x31, 0x01, 0x02, 0x03, 0x01]);
        let res =
            run_routine(&mut io, &def, "Throttle learn", RoutineOp::RequestResults, &[]).unwrap();
        assert_eq!(res.value, Some(42.0));
        assert_eq!(res.unit, "%");
        assert_eq!(io.log[1], vec![0x31, 0x03, 0x02, 0x03]); // requestResults, no data
    }

    // --- actuator channels -------------------------------------------------

    #[test]
    fn actuator_test_returns_control() {
        let def = sample_def();
        let mut io = Script::new(&[]);
        let rep = run_actuator_test(&mut io, &def, "Cooling fan", &[], &WriteOptions::default())
            .unwrap();
        assert!(rep.is_completed());
        assert!(rep.applied && rep.control_returned);
        // shortTermAdjustment (0x03) with default state, then returnControlToECU (0x00)
        assert_eq!(io.log[0], vec![0x2F, 0x30, 0x10, 0x03, 0x01]);
        assert_eq!(io.log[1], vec![0x2F, 0x30, 0x10, 0x00]);
        // status field decoded from echoed state byte
        assert_eq!(rep.status, Some(1.0));
    }

    #[test]
    fn actuator_test_dry_run_does_nothing() {
        let def = sample_def();
        let mut io = Script::new(&[]);
        let rep = run_actuator_test(
            &mut io,
            &def,
            "Cooling fan",
            &[],
            &WriteOptions {
                dry_run: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(rep.outcome, ActuatorOutcome::Planned);
        assert!(io.log.is_empty());
    }

    #[test]
    fn actuator_apply_failure_still_returns_control() {
        let def = sample_def();
        // shortTermAdjustment is denied; returnControlToECU still succeeds.
        let mut io = |req: &[u8]| -> std::result::Result<Vec<u8>, String> {
            match (req[0], req[3]) {
                (0x2F, IO_SHORT_TERM_ADJUSTMENT) => Ok(vec![0x7F, 0x2F, 0x22]), // conditionsNotCorrect
                (0x2F, IO_RETURN_CONTROL_TO_ECU) => Ok(vec![0x6F, req[1], req[2], req[3]]),
                _ => Err("?".into()),
            }
        };
        let rep = run_actuator_test(&mut io, &def, "Cooling fan", &[], &WriteOptions::default())
            .unwrap();
        assert!(matches!(rep.outcome, ActuatorOutcome::ApplyFailed { .. }));
        assert!(!rep.applied && rep.control_returned); // control handed back despite failure
    }

    #[test]
    fn actuator_control_return_failure_is_loud() {
        let def = sample_def();
        // adjustment succeeds but control return is denied -> dangerous outcome.
        let mut io = |req: &[u8]| -> std::result::Result<Vec<u8>, String> {
            match (req[0], req[3]) {
                (0x2F, IO_SHORT_TERM_ADJUSTMENT) => {
                    Ok(vec![0x6F, req[1], req[2], req[3], 0x01])
                }
                (0x2F, IO_RETURN_CONTROL_TO_ECU) => Ok(vec![0x7F, 0x2F, 0x22]),
                _ => Err("?".into()),
            }
        };
        let rep = run_actuator_test(&mut io, &def, "Cooling fan", &[], &WriteOptions::default())
            .unwrap();
        assert!(matches!(
            rep.outcome,
            ActuatorOutcome::ControlReturnFailed { .. }
        ));
        assert!(rep.applied && !rep.control_returned);
    }

    // --- honest UDS checking ----------------------------------------------

    #[test]
    fn negative_response_surfaces() {
        let def = sample_def();
        let mut io = |req: &[u8]| -> std::result::Result<Vec<u8>, String> {
            Ok(vec![0x7F, req[0], 0x33]) // securityAccessDenied, echoes requested SID
        };
        assert!(matches!(
            read_value(&mut io, &def, "Idle RPM"),
            Err(AdaptationError::Protocol(_))
        ));
    }

    #[test]
    fn mis_correlated_and_truncated_rejected() {
        let def = sample_def();
        // 0x22 request but the 0x7F echoes service 0x2E -> mis-correlated
        let mut io = |_req: &[u8]| -> std::result::Result<Vec<u8>, String> {
            Ok(vec![0x7F, 0x2E, 0x33])
        };
        assert!(matches!(
            read_value(&mut io, &def, "Idle RPM"),
            Err(AdaptationError::Protocol(_))
        ));
        // truncated negative response
        let mut io2 = |_req: &[u8]| -> std::result::Result<Vec<u8>, String> { Ok(vec![0x7F]) };
        assert!(matches!(
            read_value(&mut io2, &def, "Idle RPM"),
            Err(AdaptationError::Protocol(_))
        ));
    }

    #[test]
    fn routine_response_must_echo_subfunction_and_id() {
        let def = sample_def();
        // response echoes the wrong routine id -> rejected
        let mut io = |req: &[u8]| -> std::result::Result<Vec<u8>, String> {
            assert_eq!(req[0], 0x31);
            Ok(vec![0x71, req[1], 0x99, 0x99]) // wrong id echo
        };
        assert!(matches!(
            run_routine(&mut io, &def, "Throttle learn", RoutineOp::Start, &[]),
            Err(AdaptationError::Protocol(_))
        ));
    }
}
