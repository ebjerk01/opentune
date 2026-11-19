//! Timed actuator / output tests over UDS **InputOutputControlByIdentifier
//! (0x2F)**.
//!
//! [`run_actuator_test`](crate::run_actuator_test) is the one-shot form (adjust,
//! then return control at once). This module is the *timed* form behind the
//! desktop **Actuator tests** panel and `opentune adaptation actuator`:
//!
//! * [`actuators`] lists a definition's actuator channels with the control
//!   options, value range and duration cap each one **declares**;
//! * [`plan_test`] validates a request against that declaration — a declared
//!   channel, a declared option, a value inside the declared range, a duration
//!   inside the cap — and returns the exact requests, sending nothing;
//! * [`run_test`] executes a plan: extended session, SecurityAccess when the
//!   definition requires it, the control request, a hold (tester present every
//!   [`TESTER_PRESENT_INTERVAL_MS`]) until the duration ends or a stop is
//!   requested, then **always** returnControlToECU (retried once) — also after a
//!   refused control request, a link failure during the hold or a stop — and a
//!   best-effort return to the default session, which on an ISO 14229 module
//!   ends any IO control the tester still holds.
//!
//! Nothing here is free-form by default: a channel the definition does not
//! declare cannot be driven, and a raw control-state record is accepted only
//! with [`PlanOptions::allow_raw_state`] (the alpha Debug free-form test, see
//! [`freeform_def`]).
//!
//! Every exchange is reported through [`TestClock::on_exchange`] as it happens
//! and collected in the [`TestReport`], so a front end can show a live log.

use crate::{check, AdaptationDef, AdaptationError, ChannelKind, NumericField, RawDiag, Result};
use opentune_core::protocol::seedkey::SeedKey;
use serde::{Deserialize, Serialize, Serializer};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Duration cap of a channel that declares no `max_duration_ms`.
pub const DEFAULT_MAX_DURATION_MS: u64 = 10_000;
/// No test holds an output longer than this, whatever a definition says.
pub const HARD_MAX_DURATION_MS: u64 = 60_000;
/// Duration cap of a free-form (undeclared DID) test.
pub const FREEFORM_MAX_DURATION_MS: u64 = 5_000;
/// Tester-present period during the hold (well inside the ISO S3 timeout).
pub const TESTER_PRESENT_INTERVAL_MS: u64 = 2_000;
/// SecurityAccess requestSeed level when the definition names none.
pub const DEFAULT_SECURITY_LEVEL: u8 = 0x01;
/// The diagnostic session a test runs in.
pub const EXTENDED_SESSION: u8 = 0x03;
/// How often returnControlToECU is sent before the test reports it failed.
pub const RETURN_ATTEMPTS: u8 = 2;

/// Serialize bytes as uppercase space-separated hex.
pub(crate) fn hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn ser_hex<S: Serializer>(b: &[u8], s: S) -> std::result::Result<S::Ok, S::Error> {
    s.serialize_str(&hex(b))
}

fn ser_opt_hex<S: Serializer>(b: &Option<Vec<u8>>, s: S) -> std::result::Result<S::Ok, S::Error> {
    match b {
        Some(b) => s.serialize_str(&hex(b)),
        None => s.serialize_none(),
    }
}

// ---------------------------------------------------------------------------
// Control options
// ---------------------------------------------------------------------------

/// An InputOutputControlByIdentifier control option (ISO 14229-1
/// `inputOutputControlParameter`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IoControl {
    /// `0x00` returnControlToECU — the ECU drives the output again. Always
    /// allowed; it is the safe end state of every test.
    ReturnControlToEcu,
    /// `0x01` resetToDefault — drive the output to its default state.
    ResetToDefault,
    /// `0x02` freezeCurrentState — hold the output where it is.
    FreezeCurrentState,
    /// `0x03` shortTermAdjustment — drive the output to a given state.
    ShortTermAdjustment,
}

impl IoControl {
    /// All four options, in byte order.
    pub const ALL: [IoControl; 4] = [
        IoControl::ReturnControlToEcu,
        IoControl::ResetToDefault,
        IoControl::FreezeCurrentState,
        IoControl::ShortTermAdjustment,
    ];

    /// The option byte.
    pub fn byte(self) -> u8 {
        match self {
            IoControl::ReturnControlToEcu => 0x00,
            IoControl::ResetToDefault => 0x01,
            IoControl::FreezeCurrentState => 0x02,
            IoControl::ShortTermAdjustment => 0x03,
        }
    }

    /// The serialized (snake_case) name.
    pub fn name(self) -> &'static str {
        match self {
            IoControl::ReturnControlToEcu => "return_control_to_ecu",
            IoControl::ResetToDefault => "reset_to_default",
            IoControl::FreezeCurrentState => "freeze_current_state",
            IoControl::ShortTermAdjustment => "short_term_adjustment",
        }
    }

    /// The ISO 14229 name, e.g. `returnControlToECU`.
    pub fn label(self) -> &'static str {
        match self {
            IoControl::ReturnControlToEcu => "returnControlToECU",
            IoControl::ResetToDefault => "resetToDefault",
            IoControl::FreezeCurrentState => "freezeCurrentState",
            IoControl::ShortTermAdjustment => "shortTermAdjustment",
        }
    }

    /// Whether the option takes control of the output (everything but
    /// returnControlToECU): such a test holds and then returns control.
    pub fn takes_control(self) -> bool {
        self != IoControl::ReturnControlToEcu
    }

    /// Parse an option: the snake_case name, the ISO name, a short form
    /// (`return`, `reset`, `freeze`, `adjust` / `sta`) or the byte (`0`..`3`,
    /// decimal or `0x` hex). Case-insensitive.
    pub fn parse(s: &str) -> Result<IoControl> {
        let t = s.trim().to_ascii_lowercase().replace(['-', ' '], "_");
        let by_byte = match t.strip_prefix("0x") {
            Some(h) => u8::from_str_radix(h, 16).ok(),
            None => t.parse::<u8>().ok(),
        };
        if let Some(b) = by_byte {
            if let Some(o) = IoControl::ALL.into_iter().find(|o| o.byte() == b) {
                return Ok(o);
            }
        }
        let o = match t.as_str() {
            "return" | "return_control" | "return_control_to_ecu" | "returncontroltoecu" => {
                IoControl::ReturnControlToEcu
            }
            "reset" | "reset_to_default" | "resettodefault" => IoControl::ResetToDefault,
            "freeze" | "freeze_current_state" | "freezecurrentstate" => {
                IoControl::FreezeCurrentState
            }
            "adjust" | "sta" | "short_term_adjustment" | "shorttermadjustment" => {
                IoControl::ShortTermAdjustment
            }
            _ => {
                return Err(AdaptationError::InvalidValue {
                    channel: "<option>".into(),
                    value: 0,
                    reason: format!(
                        "unknown IO control option '{s}' (return, reset, freeze or adjust)"
                    ),
                })
            }
        };
        Ok(o)
    }
}

// ---------------------------------------------------------------------------
// Channel description
// ---------------------------------------------------------------------------

/// The adjustable value of an actuator channel: its unit, scaling and range
/// (raw and engineering).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ValueRange {
    /// Engineering unit.
    pub unit: String,
    /// `engineering = raw * factor + offset`.
    pub factor: f64,
    /// See `factor`.
    pub offset: f64,
    /// Field width in bytes.
    pub width: u8,
    /// Smallest raw value accepted.
    pub min_raw: u64,
    /// Largest raw value accepted.
    pub max_raw: u64,
    /// Smallest engineering value accepted.
    pub min: f64,
    /// Largest engineering value accepted.
    pub max: f64,
}

/// A value for a shortTermAdjustment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TestValue {
    /// In the channel's engineering unit (scaled to raw and rounded).
    Engineering(f64),
    /// The raw field value.
    Raw(u64),
}

impl ValueRange {
    fn new(field: &NumericField, min: Option<u64>, max: Option<u64>) -> Self {
        let cap = crate::width_cap(field.width);
        let min_raw = min.unwrap_or(0);
        let max_raw = max.unwrap_or(cap);
        let a = field.scaling.apply(min_raw);
        let b = field.scaling.apply(max_raw);
        ValueRange {
            unit: field.scaling.unit.clone(),
            factor: field.scaling.factor,
            offset: field.scaling.offset,
            width: field.width,
            min_raw,
            max_raw,
            min: a.min(b),
            max: a.max(b),
        }
    }

    /// The raw value for `v`, range-checked against the declared range.
    pub fn to_raw(&self, channel: &str, v: TestValue) -> Result<u64> {
        let bad = |value: u64, reason: String| AdaptationError::InvalidValue {
            channel: channel.to_string(),
            value,
            reason,
        };
        let raw = match v {
            TestValue::Raw(r) => r,
            TestValue::Engineering(e) => {
                if !e.is_finite() {
                    return Err(bad(0, "the value is not a number".into()));
                }
                if self.factor == 0.0 {
                    return Err(bad(0, "the channel's scaling factor is 0".into()));
                }
                let r = ((e - self.offset) / self.factor).round();
                if r < 0.0 || r > u64::MAX as f64 {
                    return Err(bad(
                        0,
                        format!(
                            "{e} {} is outside {}..={} {}",
                            self.unit, self.min, self.max, self.unit
                        ),
                    ));
                }
                r as u64
            }
        };
        if raw < self.min_raw || raw > self.max_raw {
            return Err(bad(
                raw,
                format!(
                    "outside the declared range {}..={} {} (raw {}..={})",
                    self.min, self.max, self.unit, self.min_raw, self.max_raw
                ),
            ));
        }
        Ok(raw)
    }

    /// The engineering value of `raw`.
    pub fn engineering(&self, raw: u64) -> f64 {
        raw as f64 * self.factor + self.offset
    }
}

/// One actuator channel as a front end lists it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ActuatorInfo {
    /// Channel name.
    pub name: String,
    /// IO data identifier.
    pub did: u16,
    /// Explanatory note from the definition.
    pub note: String,
    /// Options a test may use: returnControlToECU first, then the declared
    /// ones (shortTermAdjustment when the channel declares none).
    pub options: Vec<IoControl>,
    /// The adjustable value, when the channel declares one.
    pub value: Option<ValueRange>,
    /// The default control-state record (sent with shortTermAdjustment when
    /// the channel has no adjustable value; the template when it has one).
    #[serde(serialize_with = "ser_hex")]
    pub default_state: Vec<u8>,
    /// Unit of the status decoded from the control response (empty if none).
    pub status_unit: String,
    /// The longest test this channel allows.
    pub max_duration_ms: u64,
    /// Whether the definition requires SecurityAccess.
    pub requires_security: bool,
    /// The SecurityAccess requestSeed level.
    pub security_level: u8,
}

/// The parsed actuator channel.
struct Spec<'a> {
    did: u16,
    state: &'a [u8],
    status: &'a Option<NumericField>,
    options: Vec<IoControl>,
    value: Option<(&'a NumericField, ValueRange)>,
    max_duration_ms: u64,
}

fn spec<'a>(def: &'a AdaptationDef, name: &str) -> Result<Spec<'a>> {
    let ch = def
        .channel(name)
        .ok_or_else(|| AdaptationError::UnknownChannel(name.to_string()))?;
    let ChannelKind::Actuator {
        did,
        state,
        status,
        options,
        value,
        min,
        max,
        max_duration_ms,
    } = &ch.kind
    else {
        return Err(AdaptationError::ChannelKindMismatch {
            channel: name.to_string(),
            expected: "actuator",
        });
    };
    let mut opts = vec![IoControl::ReturnControlToEcu];
    let declared: &[IoControl] = if options.is_empty() {
        &[IoControl::ShortTermAdjustment]
    } else {
        options
    };
    for o in declared {
        if !opts.contains(o) {
            opts.push(*o);
        }
    }
    Ok(Spec {
        did: *did,
        state,
        status,
        options: opts,
        value: value.as_ref().map(|f| (f, ValueRange::new(f, *min, *max))),
        max_duration_ms: max_duration_ms
            .unwrap_or(DEFAULT_MAX_DURATION_MS)
            .min(HARD_MAX_DURATION_MS),
    })
}

/// The SecurityAccess requestSeed level of a definition.
pub fn security_level(def: &AdaptationDef) -> u8 {
    def.security_level.unwrap_or(DEFAULT_SECURITY_LEVEL)
}

/// One actuator channel's description.
pub fn actuator_info(def: &AdaptationDef, name: &str) -> Result<ActuatorInfo> {
    let s = spec(def, name)?;
    let ch = def.channel(name).expect("spec found it");
    Ok(ActuatorInfo {
        name: ch.name.clone(),
        did: s.did,
        note: ch.note.clone(),
        options: s.options,
        value: s.value.map(|(_, r)| r),
        default_state: s.state.to_vec(),
        status_unit: s
            .status
            .as_ref()
            .map(|f| f.scaling.unit.clone())
            .unwrap_or_default(),
        max_duration_ms: s.max_duration_ms,
        requires_security: def.requires_security,
        security_level: security_level(def),
    })
}

/// Every actuator channel of a (validated) definition, in definition order.
pub fn actuators(def: &AdaptationDef) -> Result<Vec<ActuatorInfo>> {
    def.validate()?;
    def.channels
        .iter()
        .filter(|c| matches!(c.kind, ChannelKind::Actuator { .. }))
        .map(|c| actuator_info(def, &c.name))
        .collect()
}

/// A one-channel definition for a **free-form** test of an IO DID no
/// definition declares (the alpha Debug path): every option, no value range
/// (the caller supplies the raw state, see [`PlanOptions::allow_raw_state`]),
/// capped at [`FREEFORM_MAX_DURATION_MS`]. `requires_security` and
/// `security_level` are the caller's.
pub fn freeform_def(did: u16, requires_security: bool, level: Option<u8>) -> AdaptationDef {
    AdaptationDef {
        id: "freeform".into(),
        name: "Free-form IO control (alpha Debug)".into(),
        module: "undeclared — no definition backs this DID".into(),
        requires_security,
        security_level: level,
        channels: vec![crate::AdaptationChannel {
            name: freeform_name(did),
            kind: ChannelKind::Actuator {
                did,
                state: Vec::new(),
                status: None,
                options: vec![
                    IoControl::ResetToDefault,
                    IoControl::FreezeCurrentState,
                    IoControl::ShortTermAdjustment,
                ],
                value: None,
                min: None,
                max: None,
                max_duration_ms: Some(FREEFORM_MAX_DURATION_MS),
            },
            note: "free-form: not declared by any definition".into(),
        }],
    }
}

/// The channel name [`freeform_def`] uses for `did`.
pub fn freeform_name(did: u16) -> String {
    format!("Free-form IO 0x{did:04X}")
}

// ---------------------------------------------------------------------------
// Plan
// ---------------------------------------------------------------------------

/// What to test.
#[derive(Debug, Clone, PartialEq)]
pub struct TestRequest {
    /// Channel name (as in the definition).
    pub channel: String,
    /// The control option.
    pub option: IoControl,
    /// The value for shortTermAdjustment, when the channel declares one.
    pub value: Option<TestValue>,
    /// A raw control-state record (free-form tests only).
    pub state: Option<Vec<u8>>,
    /// How long to hold the output (ignored for returnControlToECU).
    pub duration_ms: u64,
}

/// Planning switches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlanOptions {
    /// Accept [`TestRequest::state`] (the alpha Debug free-form test).
    pub allow_raw_state: bool,
}

/// The phase an exchange belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// DiagnosticSessionControl into the extended session.
    Session,
    /// SecurityAccess.
    Security,
    /// The control request itself.
    Control,
    /// Tester present while the output is held.
    Hold,
    /// returnControlToECU.
    Return,
    /// Back to the default session.
    Leave,
}

/// One planned request.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanStep {
    /// Phase.
    pub phase: Phase,
    /// Request bytes (`<key>` is computed at run time and not shown).
    #[serde(serialize_with = "ser_hex")]
    pub request: Vec<u8>,
    /// What it does.
    pub note: String,
}

/// A validated test, ready to run. Sends nothing by itself.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TestPlan {
    /// Channel name.
    pub channel: String,
    /// IO data identifier.
    pub did: u16,
    /// The control option.
    pub option: IoControl,
    /// The control-state record sent with the option.
    #[serde(serialize_with = "ser_hex")]
    pub record: Vec<u8>,
    /// The raw adjustment value, when the channel declares one.
    pub raw: Option<u64>,
    /// Its engineering value.
    pub value: Option<f64>,
    /// Its unit.
    pub unit: String,
    /// How long the output is held (0 for returnControlToECU).
    pub duration_ms: u64,
    /// Whether SecurityAccess runs first.
    pub requires_security: bool,
    /// The requestSeed level.
    pub security_level: u8,
    /// The requests, in order.
    pub steps: Vec<PlanStep>,
    /// Status field decoded from the control response.
    #[serde(skip)]
    pub status: Option<NumericField>,
}

/// Validate `req` against `def` and return the plan. Refuses (before any I/O)
/// an undeclared channel or option, a value outside the declared range or for
/// a channel without one, a raw state outside a free-form test, and a duration
/// of zero or above the channel's cap.
pub fn plan_test(def: &AdaptationDef, req: &TestRequest, opts: PlanOptions) -> Result<TestPlan> {
    def.validate()?;
    let s = spec(def, &req.channel)?;
    let bad = |reason: String| AdaptationError::InvalidValue {
        channel: req.channel.clone(),
        value: 0,
        reason,
    };
    if !s.options.contains(&req.option) {
        return Err(bad(format!(
            "{} is not declared for this channel (declared: {})",
            req.option.label(),
            s.options
                .iter()
                .map(|o| o.label())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    if req.state.is_some() && !opts.allow_raw_state {
        return Err(bad(
            "a raw control state is accepted only for a free-form test (alpha Debug)".into(),
        ));
    }
    if (req.value.is_some() || req.state.is_some()) && req.option != IoControl::ShortTermAdjustment
    {
        return Err(bad(format!(
            "a value applies only to shortTermAdjustment, not {}",
            req.option.label()
        )));
    }

    let (mut raw, mut value, mut unit) = (None, None, String::new());
    let record: Vec<u8> = match req.option {
        IoControl::ShortTermAdjustment => {
            if let Some(st) = &req.state {
                if st.is_empty() {
                    return Err(bad("the raw control state is empty".into()));
                }
                st.clone()
            } else if let Some((field, range)) = &s.value {
                let v = req.value.ok_or_else(|| {
                    bad(format!(
                        "enter a value ({}..={} {})",
                        range.min, range.max, range.unit
                    ))
                })?;
                let r = range.to_raw(&req.channel, v)?;
                let mut rec = s.state.to_vec();
                crate::write_be(&mut rec, field.offset, field.width, r)?;
                raw = Some(r);
                value = Some(range.engineering(r));
                unit = range.unit.clone();
                rec
            } else {
                if req.value.is_some() {
                    return Err(bad("this channel declares no adjustable value".into()));
                }
                if s.state.is_empty() {
                    return Err(bad(
                        "this channel declares no control state for shortTermAdjustment".into(),
                    ));
                }
                s.state.to_vec()
            }
        }
        _ => Vec::new(),
    };

    let duration_ms = if req.option.takes_control() {
        if req.duration_ms == 0 {
            return Err(bad("set a test duration".into()));
        }
        if req.duration_ms > s.max_duration_ms {
            return Err(bad(format!(
                "duration {} ms is above this channel's limit of {} ms",
                req.duration_ms, s.max_duration_ms
            )));
        }
        req.duration_ms
    } else {
        0
    };

    let level = security_level(def);
    let did = s.did;
    let mut steps = vec![PlanStep {
        phase: Phase::Session,
        request: vec![0x10, EXTENDED_SESSION],
        note: "extended diagnostic session".into(),
    }];
    if def.requires_security {
        steps.push(PlanStep {
            phase: Phase::Security,
            request: vec![0x27, level],
            note: "SecurityAccess requestSeed".into(),
        });
        steps.push(PlanStep {
            phase: Phase::Security,
            request: vec![0x27, level + 1],
            note: "SecurityAccess sendKey <key from the seed/key handler>".into(),
        });
    }
    let mut ctl = vec![0x2F, (did >> 8) as u8, did as u8, req.option.byte()];
    ctl.extend_from_slice(&record);
    steps.push(PlanStep {
        phase: Phase::Control,
        request: ctl,
        note: req.option.label().into(),
    });
    if req.option.takes_control() {
        steps.push(PlanStep {
            phase: Phase::Hold,
            request: vec![0x3E, 0x00],
            note: format!(
                "tester present every {} ms while the output is held for {duration_ms} ms (STOP ends it early)",
                TESTER_PRESENT_INTERVAL_MS
            ),
        });
        steps.push(PlanStep {
            phase: Phase::Return,
            request: vec![0x2F, (did >> 8) as u8, did as u8, 0x00],
            note: "returnControlToECU — always, also after an error or STOP (retried once)".into(),
        });
    }
    steps.push(PlanStep {
        phase: Phase::Leave,
        request: vec![0x10, 0x01],
        note: "default session (ends any IO control still held)".into(),
    });
    Ok(TestPlan {
        channel: req.channel.clone(),
        did,
        option: req.option,
        record,
        raw,
        value,
        unit,
        duration_ms,
        requires_security: def.requires_security,
        security_level: level,
        steps,
        status: s.status.clone(),
    })
}

// ---------------------------------------------------------------------------
// Run
// ---------------------------------------------------------------------------

/// One request/response exchange of a test.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Exchange {
    /// Milliseconds since the test started.
    pub t_ms: u64,
    /// Phase.
    pub phase: Phase,
    /// Request bytes.
    #[serde(serialize_with = "ser_hex")]
    pub request: Vec<u8>,
    /// Response bytes (none on a transport failure).
    #[serde(serialize_with = "ser_opt_hex")]
    pub response: Option<Vec<u8>>,
    /// Why the exchange failed, if it did.
    pub error: Option<String>,
}

/// Time, stop requests and the live exchange feed of a running test.
pub trait TestClock {
    /// Milliseconds since an arbitrary fixed point.
    fn now_ms(&mut self) -> u64;
    /// Sleep up to `ms`, returning early once a stop is requested.
    fn wait(&mut self, ms: u64);
    /// Whether the operator asked to stop (STOP, panel closed, disconnect).
    fn stop_requested(&mut self) -> bool;
    /// Called with every exchange as it completes.
    fn on_exchange(&mut self, _ex: &Exchange) {}
}

/// The wall-clock [`TestClock`]: sleeps in short slices and stops when the
/// shared flag is set.
pub struct RealClock {
    t0: Instant,
    stop: Arc<AtomicBool>,
}

impl RealClock {
    /// A clock that stops when `stop` is set.
    pub fn new(stop: Arc<AtomicBool>) -> Self {
        RealClock {
            t0: Instant::now(),
            stop,
        }
    }
}

impl TestClock for RealClock {
    fn now_ms(&mut self) -> u64 {
        self.t0.elapsed().as_millis() as u64
    }
    fn wait(&mut self, ms: u64) {
        let end = Instant::now() + Duration::from_millis(ms);
        while !self.stop.load(Ordering::SeqCst) {
            let now = Instant::now();
            if now >= end {
                break;
            }
            std::thread::sleep((end - now).min(Duration::from_millis(25)));
        }
    }
    fn stop_requested(&mut self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
}

/// How a test session logs in.
#[derive(Default)]
pub struct TestLogin<'a> {
    /// The operator confirmed this run (per-run confirmation). Required.
    pub confirmed: bool,
    /// Seed/key handler for SecurityAccess, when the definition requires it.
    /// OpenTune ships none; it comes from a plugin or the user's SA2 script.
    pub seed_key: Option<&'a dyn SeedKey>,
}

/// How a test ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum TestOutcome {
    /// Held for the full duration (or the one-shot returnControlToECU was
    /// acknowledged) and control was returned.
    Completed,
    /// Stopped before the end (STOP, panel closed, disconnect); control was
    /// returned, or never taken when the stop came before the control request.
    Stopped,
    /// Failed before any control request — nothing was driven.
    Aborted {
        /// Where it failed.
        phase: Phase,
        /// Why.
        message: String,
    },
    /// The control request was refused or failed; returnControlToECU was
    /// still sent (see `control_returned`).
    ApplyFailed {
        /// Why.
        message: String,
    },
    /// The link failed while the output was held; returnControlToECU was
    /// still sent (see `control_returned`).
    HoldFailed {
        /// Why.
        message: String,
    },
    /// The output was driven and returnControlToECU was **not** acknowledged:
    /// it may still be forced. The caller must recover the module.
    ControlReturnFailed {
        /// Why.
        message: String,
    },
}

/// The result of [`run_test`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TestReport {
    /// Channel name.
    pub channel: String,
    /// IO data identifier.
    pub did: u16,
    /// The control option.
    pub option: IoControl,
    /// The control-state record sent.
    #[serde(serialize_with = "ser_hex")]
    pub record: Vec<u8>,
    /// Planned hold.
    pub duration_ms: u64,
    /// How long the output was actually held.
    pub held_ms: u64,
    /// SecurityAccess was unlocked.
    pub security_unlocked: bool,
    /// The control request was acknowledged.
    pub applied: bool,
    /// returnControlToECU was acknowledged (or the test itself was a
    /// successful returnControlToECU).
    pub control_returned: bool,
    /// How many returnControlToECU requests were sent after the control request.
    pub return_attempts: u8,
    /// The default session was restored.
    pub default_session: bool,
    /// The operator stopped the test early.
    pub stopped: bool,
    /// Decoded status from the control response.
    pub status: Option<f64>,
    /// Its unit.
    pub status_unit: String,
    /// The control response's state record.
    #[serde(serialize_with = "ser_hex")]
    pub response: Vec<u8>,
    /// How it ended.
    #[serde(flatten)]
    pub outcome: TestOutcome,
    /// True when no output can be left forced: control was returned, or the
    /// control request never reached a state that could have applied it.
    pub safe: bool,
    /// Every exchange, in order.
    pub exchanges: Vec<Exchange>,
}

struct Runner<'c> {
    ctl: &'c mut dyn TestClock,
    t0: u64,
    log: Vec<Exchange>,
}

impl Runner<'_> {
    /// Send `req`, log it and check the response's service id.
    fn send(&mut self, diag: &mut dyn RawDiag, phase: Phase, req: Vec<u8>) -> Result<Vec<u8>> {
        let r = diag.request(&req);
        let t_ms = self.ctl.now_ms().saturating_sub(self.t0);
        let (response, err) = match &r {
            Ok(b) => (Some(b.clone()), check(req[0], b).err()),
            Err(e) => (None, Some(AdaptationError::Transport(e.clone()))),
        };
        let ex = Exchange {
            t_ms,
            phase,
            request: req,
            response: response.clone(),
            error: err.as_ref().map(ToString::to_string),
        };
        self.ctl.on_exchange(&ex);
        self.log.push(ex);
        match err {
            Some(e) => Err(e),
            None => Ok(response.unwrap_or_default()),
        }
    }

    fn io(
        &mut self,
        diag: &mut dyn RawDiag,
        phase: Phase,
        did: u16,
        opt: u8,
        rec: &[u8],
    ) -> Result<Vec<u8>> {
        let mut req = vec![0x2F, (did >> 8) as u8, did as u8, opt];
        req.extend_from_slice(rec);
        let resp = self.send(diag, phase, req)?;
        if resp.len() < 4 || resp[1] != (did >> 8) as u8 || resp[2] != did as u8 || resp[3] != opt {
            return Err(AdaptationError::Protocol(format!(
                "0x2F response did not echo DID 0x{did:04X} / option 0x{opt:02X}"
            )));
        }
        Ok(resp[4..].to_vec())
    }
}

/// Whether an error is a clean negative response (the module answered and
/// refused, so it did not apply the request).
fn refused(e: &AdaptationError) -> bool {
    matches!(e, AdaptationError::Protocol(m) if m.starts_with("negative response"))
}

/// Run a planned test. Refuses before any I/O (`Err(NotConfirmed)`) when the
/// run is not confirmed or the definition requires SecurityAccess and no
/// seed/key handler is given; every later failure is reported in the
/// [`TestReport`], and once a control request has been sent,
/// returnControlToECU is always attempted.
pub fn run_test(
    diag: &mut dyn RawDiag,
    plan: &TestPlan,
    login: &TestLogin<'_>,
    clock: &mut dyn TestClock,
) -> Result<TestReport> {
    if !login.confirmed {
        return Err(AdaptationError::NotConfirmed(
            "an actuator test drives a real output: confirm this run first".into(),
        ));
    }
    if plan.requires_security && login.seed_key.is_none() {
        return Err(AdaptationError::NotConfirmed(format!(
            "this module needs SecurityAccess (level 0x{:02X}) before IO control, and no seed/key \
             handler was given — OpenTune ships none; a plugin or your own SA2 script supplies it",
            plan.security_level
        )));
    }
    let t0 = clock.now_ms();
    let mut r = Runner {
        ctl: clock,
        t0,
        log: Vec::new(),
    };
    let mut rep = TestReport {
        channel: plan.channel.clone(),
        did: plan.did,
        option: plan.option,
        record: plan.record.clone(),
        duration_ms: plan.duration_ms,
        held_ms: 0,
        security_unlocked: false,
        applied: false,
        control_returned: false,
        return_attempts: 0,
        default_session: false,
        stopped: false,
        status: None,
        status_unit: String::new(),
        response: Vec::new(),
        outcome: TestOutcome::Completed,
        safe: true,
        exchanges: Vec::new(),
    };

    // 1. Session and SecurityAccess: a failure here drives nothing.
    if let Err((phase, e)) = login_steps(diag, &mut r, plan, login) {
        rep.outcome = TestOutcome::Aborted {
            phase,
            message: e.to_string(),
        };
        leave(diag, &mut r, &mut rep);
        rep.exchanges = r.log;
        return Ok(rep);
    }
    rep.security_unlocked = plan.requires_security;

    // A stop that arrived before the control request: drive nothing.
    if r.ctl.stop_requested() {
        rep.stopped = true;
        rep.outcome = TestOutcome::Stopped;
        leave(diag, &mut r, &mut rep);
        rep.exchanges = r.log;
        return Ok(rep);
    }

    // 2. The control request.
    let did = plan.did;
    let opt = plan.option.byte();
    let applied_at = r.ctl.now_ms();
    let apply = r.io(diag, Phase::Control, did, opt, &plan.record);
    let mut failure: Option<TestOutcome> = None;
    let mut maybe_forced = plan.option.takes_control();
    match apply {
        Ok(record) => {
            rep.applied = true;
            if let Some(f) = &plan.status {
                if let Ok(raw) = crate::read_be(&record, f.offset, f.width) {
                    rep.status = Some(f.scaling.apply(raw));
                    rep.status_unit = f.scaling.unit.clone();
                }
            }
            rep.response = record;
        }
        Err(e) => {
            // A clean refusal applied nothing; a lost or garbled answer may
            // have, so control is returned either way.
            if refused(&e) {
                maybe_forced = false;
            }
            failure = Some(TestOutcome::ApplyFailed {
                message: e.to_string(),
            });
        }
    }

    if !plan.option.takes_control() {
        // The test *was* returnControlToECU.
        rep.control_returned = rep.applied;
        rep.outcome = failure.unwrap_or(TestOutcome::Completed);
        rep.safe = rep.applied || !maybe_forced;
        leave(diag, &mut r, &mut rep);
        rep.exchanges = r.log;
        return Ok(rep);
    }

    // 3. Hold: tester present until the duration ends or a stop is requested.
    if failure.is_none() {
        loop {
            if r.ctl.stop_requested() {
                rep.stopped = true;
                break;
            }
            let el = r.ctl.now_ms().saturating_sub(applied_at);
            if el >= plan.duration_ms {
                break;
            }
            r.ctl
                .wait((plan.duration_ms - el).min(TESTER_PRESENT_INTERVAL_MS));
            if r.ctl.stop_requested() {
                rep.stopped = true;
                break;
            }
            if r.ctl.now_ms().saturating_sub(applied_at) >= plan.duration_ms {
                break;
            }
            match r.send(diag, Phase::Hold, vec![0x3E, 0x00]) {
                Ok(resp) if resp.first() == Some(&0x7E) => {}
                Ok(resp) => {
                    failure = Some(TestOutcome::HoldFailed {
                        message: format!("unexpected tester-present reply {}", hex(&resp)),
                    });
                    break;
                }
                Err(e) => {
                    failure = Some(TestOutcome::HoldFailed {
                        message: e.to_string(),
                    });
                    break;
                }
            }
        }
    }
    rep.held_ms = if rep.applied {
        r.ctl.now_ms().saturating_sub(applied_at)
    } else {
        0
    };

    // 4. Always hand control back (retried once).
    let mut last_err = None;
    for attempt in 1..=RETURN_ATTEMPTS {
        rep.return_attempts = attempt;
        match r.io(diag, Phase::Return, did, 0x00, &[]) {
            Ok(_) => {
                rep.control_returned = true;
                last_err = None;
                break;
            }
            Err(e) => {
                last_err = Some(e.to_string());
                if attempt < RETURN_ATTEMPTS {
                    r.ctl.wait(100);
                }
            }
        }
    }
    rep.safe = rep.control_returned || !maybe_forced;
    rep.outcome = match (last_err, failure) {
        (Some(message), _) if maybe_forced => TestOutcome::ControlReturnFailed { message },
        (_, Some(f)) => f,
        (_, None) if rep.stopped => TestOutcome::Stopped,
        (_, None) => TestOutcome::Completed,
    };
    leave(diag, &mut r, &mut rep);
    rep.exchanges = r.log;
    Ok(rep)
}

/// Extended session, then SecurityAccess when required.
fn login_steps(
    diag: &mut dyn RawDiag,
    r: &mut Runner<'_>,
    plan: &TestPlan,
    login: &TestLogin<'_>,
) -> std::result::Result<(), (Phase, AdaptationError)> {
    let resp = r
        .send(diag, Phase::Session, vec![0x10, EXTENDED_SESSION])
        .map_err(|e| (Phase::Session, e))?;
    if resp.get(1) != Some(&EXTENDED_SESSION) {
        return Err((
            Phase::Session,
            AdaptationError::Protocol(format!(
                "session reply {} did not echo 0x{EXTENDED_SESSION:02X}",
                hex(&resp)
            )),
        ));
    }
    if !plan.requires_security {
        return Ok(());
    }
    let sec = |e: AdaptationError| (Phase::Security, e);
    let level = plan.security_level;
    let resp = r
        .send(diag, Phase::Security, vec![0x27, level])
        .map_err(sec)?;
    if resp.get(1) != Some(&level) {
        return Err(sec(AdaptationError::Protocol(format!(
            "requestSeed reply {} did not echo level 0x{level:02X}",
            hex(&resp)
        ))));
    }
    let seed = &resp[2..];
    if seed.iter().all(|b| *b == 0) {
        return Ok(()); // already unlocked (ISO: all-zero seed)
    }
    let sk = login.seed_key.expect("checked before any I/O");
    let key = sk.compute(seed, level).map_err(|e| {
        sec(AdaptationError::SecurityDenied(format!(
            "seed/key '{}': {e}",
            sk.id()
        )))
    })?;
    let mut req = vec![0x27, level + 1];
    req.extend_from_slice(&key);
    r.send(diag, Phase::Security, req).map_err(|e| {
        sec(match e {
            AdaptationError::Protocol(m) => AdaptationError::SecurityDenied(m),
            other => other,
        })
    })?;
    Ok(())
}

/// Best-effort return to the default session.
fn leave(diag: &mut dyn RawDiag, r: &mut Runner<'_>, rep: &mut TestReport) {
    rep.default_session = r.send(diag, Phase::Leave, vec![0x10, 0x01]).is_ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def() -> AdaptationDef {
        AdaptationDef::from_json(
            r#"{"id":"t","name":"T","requires_security":false,"channels":[
              {"name":"Fan","kind":"actuator","did":12304,"state":[0,0],
               "options":["short_term_adjustment","reset_to_default","freeze_current_state"],
               "value":{"offset":1,"width":1,"scaling":{"factor":0.5,"unit":"%"}},
               "min":0,"max":200,"max_duration_ms":8000},
              {"name":"Lamp","kind":"actuator","did":12305,"state":[1]},
              {"name":"Bare","kind":"actuator","did":12306},
              {"name":"Idle","kind":"value","did":8193}
            ]}"#,
        )
        .unwrap()
    }

    fn req(ch: &str, option: IoControl, value: Option<TestValue>, ms: u64) -> TestRequest {
        TestRequest {
            channel: ch.into(),
            option,
            value,
            state: None,
            duration_ms: ms,
        }
    }

    #[test]
    fn options_parse_in_every_spelling() {
        for (s, o) in [
            ("return", IoControl::ReturnControlToEcu),
            ("returnControlToECU", IoControl::ReturnControlToEcu),
            ("reset-to-default", IoControl::ResetToDefault),
            ("freeze", IoControl::FreezeCurrentState),
            ("sta", IoControl::ShortTermAdjustment),
            ("0x03", IoControl::ShortTermAdjustment),
            ("1", IoControl::ResetToDefault),
        ] {
            assert_eq!(IoControl::parse(s).unwrap(), o, "{s}");
        }
        assert!(IoControl::parse("blink").is_err());
        assert!(IoControl::parse("4").is_err());
    }

    #[test]
    fn list_shows_declared_options_range_and_cap() {
        let a = actuators(&def()).unwrap();
        assert_eq!(a.len(), 3, "value channels are not listed");
        assert_eq!(
            a[0].options,
            vec![
                IoControl::ReturnControlToEcu,
                IoControl::ShortTermAdjustment,
                IoControl::ResetToDefault,
                IoControl::FreezeCurrentState
            ]
        );
        let v = a[0].value.as_ref().unwrap();
        assert_eq!((v.min, v.max, v.unit.as_str()), (0.0, 100.0, "%"));
        assert_eq!(a[0].max_duration_ms, 8000);
        // Legacy channel: shortTermAdjustment with the default state only.
        assert_eq!(
            a[1].options,
            vec![
                IoControl::ReturnControlToEcu,
                IoControl::ShortTermAdjustment
            ]
        );
        assert_eq!(a[1].max_duration_ms, DEFAULT_MAX_DURATION_MS);
    }

    #[test]
    fn plan_splices_the_value_and_lists_every_request() {
        let p = plan_test(
            &def(),
            &req(
                "Fan",
                IoControl::ShortTermAdjustment,
                Some(TestValue::Engineering(37.5)),
                3000,
            ),
            PlanOptions::default(),
        )
        .unwrap();
        assert_eq!(p.record, vec![0, 75]);
        assert_eq!((p.raw, p.value), (Some(75), Some(37.5)));
        let reqs: Vec<String> = p.steps.iter().map(|s| hex(&s.request)).collect();
        assert_eq!(
            reqs,
            [
                "10 03",
                "2F 30 10 03 00 4B",
                "3E 00",
                "2F 30 10 00",
                "10 01"
            ]
        );
    }

    #[test]
    fn plan_refuses_what_the_definition_does_not_declare() {
        let d = def();
        let e = |r: TestRequest| plan_test(&d, &r, PlanOptions::default()).unwrap_err();
        // out of range (engineering and raw)
        let m = e(req(
            "Fan",
            IoControl::ShortTermAdjustment,
            Some(TestValue::Engineering(101.0)),
            1000,
        ));
        assert!(m.to_string().contains("declared range"), "{m}");
        e(req(
            "Fan",
            IoControl::ShortTermAdjustment,
            Some(TestValue::Raw(201)),
            1000,
        ));
        e(req(
            "Fan",
            IoControl::ShortTermAdjustment,
            Some(TestValue::Engineering(-3.0)),
            1000,
        ));
        e(req(
            "Fan",
            IoControl::ShortTermAdjustment,
            Some(TestValue::Engineering(f64::NAN)),
            1000,
        ));
        // value missing / value on a channel without one / value with reset
        e(req("Fan", IoControl::ShortTermAdjustment, None, 1000));
        e(req(
            "Lamp",
            IoControl::ShortTermAdjustment,
            Some(TestValue::Raw(1)),
            1000,
        ));
        e(req(
            "Fan",
            IoControl::ResetToDefault,
            Some(TestValue::Raw(1)),
            1000,
        ));
        // undeclared option
        let m = e(req("Lamp", IoControl::FreezeCurrentState, None, 1000));
        assert!(m.to_string().contains("not declared"), "{m}");
        // duration 0 / above the cap
        e(req("Lamp", IoControl::ShortTermAdjustment, None, 0));
        e(req("Fan", IoControl::ResetToDefault, None, 8001));
        // no state to send
        e(req("Bare", IoControl::ShortTermAdjustment, None, 1000));
        // unknown channel / wrong kind
        assert!(matches!(
            e(req("Nope", IoControl::ResetToDefault, None, 1)),
            AdaptationError::UnknownChannel(_)
        ));
        assert!(matches!(
            e(req("Idle", IoControl::ResetToDefault, None, 1)),
            AdaptationError::ChannelKindMismatch { .. }
        ));
        // a raw state needs the free-form switch
        let mut r = req("Lamp", IoControl::ShortTermAdjustment, None, 1000);
        r.state = Some(vec![9]);
        e(r.clone());
        let p = plan_test(
            &d,
            &r,
            PlanOptions {
                allow_raw_state: true,
            },
        )
        .unwrap();
        assert_eq!(p.record, vec![9]);
    }

    #[test]
    fn return_control_is_always_allowed_and_one_shot() {
        let p = plan_test(
            &def(),
            &req("Bare", IoControl::ReturnControlToEcu, None, 0),
            PlanOptions::default(),
        )
        .unwrap();
        assert_eq!(p.duration_ms, 0);
        let reqs: Vec<String> = p.steps.iter().map(|s| hex(&s.request)).collect();
        assert_eq!(reqs, ["10 03", "2F 30 12 00", "10 01"]);
    }

    #[test]
    fn freeform_def_is_capped_and_needs_a_raw_state() {
        let d = freeform_def(0x0999, false, None);
        let mut r = req(
            &freeform_name(0x0999),
            IoControl::ShortTermAdjustment,
            None,
            5000,
        );
        assert!(plan_test(
            &d,
            &r,
            PlanOptions {
                allow_raw_state: true
            }
        )
        .is_err());
        r.state = Some(vec![1, 2]);
        assert!(plan_test(&d, &r, PlanOptions::default()).is_err());
        assert!(plan_test(
            &d,
            &r,
            PlanOptions {
                allow_raw_state: true
            }
        )
        .is_ok());
        r.duration_ms = FREEFORM_MAX_DURATION_MS + 1;
        assert!(plan_test(
            &d,
            &r,
            PlanOptions {
                allow_raw_state: true
            }
        )
        .is_err());
    }

    #[test]
    fn definitions_with_bad_actuator_fields_are_rejected() {
        let bad = [
            // value field outside the state template
            r#"{"name":"A","kind":"actuator","did":1,"state":[0],"value":{"offset":1,"width":1}}"#,
            // range without a value field
            r#"{"name":"A","kind":"actuator","did":1,"state":[0],"min":1}"#,
            // inverted range
            r#"{"name":"A","kind":"actuator","did":1,"state":[0],"value":{"width":1},"min":5,"max":1}"#,
            // bound too wide for the field
            r#"{"name":"A","kind":"actuator","did":1,"state":[0],"value":{"width":1},"max":256}"#,
            // duration cap 0 / above the hard cap
            r#"{"name":"A","kind":"actuator","did":1,"state":[0],"max_duration_ms":0}"#,
            r#"{"name":"A","kind":"actuator","did":1,"state":[0],"max_duration_ms":60001}"#,
        ];
        for ch in bad {
            let js = format!(r#"{{"id":"x","name":"x","channels":[{ch}]}}"#);
            let d = AdaptationDef::from_json(&js).unwrap();
            assert!(d.validate().is_err(), "{ch}");
        }
        let js = r#"{"id":"x","name":"x","security_level":2,"channels":[{"name":"A","kind":"actuator","did":1}]}"#;
        assert!(
            AdaptationDef::from_json(js).unwrap().validate().is_err(),
            "even level"
        );
    }
}
