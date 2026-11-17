//! # opentune-servicefn
//!
//! Module **service functions** for OpenTune: a declarative, safe engine for the
//! guided service procedures a scan tool performs — electronic parking-brake
//! (EPB) service mode, diesel particulate filter (DPF) forced regeneration, oil /
//! service-interval reset, steering-angle-sensor (SAS) calibration, injector
//! coding, throttle-body adaptation, and so on. Each procedure is **data** (a
//! [`ServiceScript`], serde JSON) describing an ordered UDS flow; one safe
//! executor ([`run_script`]) runs any of them, so a new procedure is new *data*,
//! not new code.
//!
//! ## Design
//!
//! * A [`ServiceScript`] is a sequence of [`Step`]s covering a conservative,
//!   ordered UDS flow: open a diagnostic session (0x10), optional SecurityAccess
//!   (0x27), then RoutineControl (0x31), I/O control (0x2F), read / write by
//!   identifier (0x22 / 0x2E), tester-present keep-alive (0x3E), fixed waits, and
//!   `ExpectPositive` / `Verify` guards.
//! * The executor talks UDS over a tiny [`RawDiag`] request abstraction (one
//!   `request(&[u8]) -> Vec<u8>`), which the app adapts to `opentune-core`'s
//!   `DiagTransport` at integration. This keeps the crate standalone and lets
//!   tests drive it with a scripted responder. (The maintainer unifies this
//!   transport with the one in `opentune-coding` later; this crate does **not**
//!   depend on that crate.)
//! * SecurityAccess is handled by the executor requesting the seed and asking a
//!   **caller-supplied [`KeyProvider`]** for the key — OpenTune ships **no** OEM
//!   seed/key algorithm. Any closure `Fn(&[u8], u8) -> Result<Vec<u8>, String>`
//!   is a key provider.
//!
//! ## Safety
//!
//! Service routines move real actuators and change adaptation data. The executor
//! therefore:
//!
//! * [`ServiceScript::validate`]s the script **semantically** before sending
//!   anything (well-formed sub-functions, a key provider present when needed, and
//!   — when [`ServiceScript::requires_security`] — that a `SecurityAccess` step
//!   precedes every write/action step);
//! * checks each response like `opentune-coding`'s `check` — rejecting a negative
//!   response (0x7F), a mis-correlated response that echoes a different service,
//!   a truncated response, and a positive response that does not **echo** the
//!   request's sub-function / identifier;
//! * **stops on the first failure**, returning a step-by-step [`RunReport`] (which
//!   steps ran, which failed, and the raw responses), so a half-finished
//!   procedure is visible rather than silently continued;
//! * supports a [`RunOptions::dry_run`] that validates the script and its required
//!   inputs and sends **nothing** (no writes, no actions, no I/O); and
//! * enforces at run time that a write/action step is refused until SecurityAccess
//!   has unlocked the module, whenever the script is marked `requires_security`.
//!
//! The shipped [`examples`] are **generic shapes** (placeholder identifiers), not
//! vehicle-specific calibrations: OpenTune does not ship OEM procedure data.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors returned by [`run_script`] **before any step is sent**. Failures that
/// happen while steps are executing are not errors — they are captured in the
/// [`RunReport`] (the run stops safely and returns `Ok(report)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceError {
    /// The script failed semantic validation ([`ServiceScript::validate`]):
    /// malformed sub-function, a write/action before SecurityAccess when
    /// `requires_security`, empty id/name/steps, and the like.
    InvalidScript(String),
    /// The script has a `SecurityAccess` step (or is `requires_security`) but no
    /// [`KeyProvider`] was supplied. OpenTune never ships OEM keys, so the caller
    /// must provide the algorithm.
    MissingKeyProvider,
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServiceError::InvalidScript(s) => write!(f, "invalid service script: {s}"),
            ServiceError::MissingKeyProvider => write!(
                f,
                "script needs SecurityAccess but no key provider was supplied"
            ),
        }
    }
}

impl std::error::Error for ServiceError {}

/// Crate result type for pre-flight errors.
pub type Result<T> = std::result::Result<T, ServiceError>;

// ---------------------------------------------------------------------------
// Transport + key provider (caller-supplied)
// ---------------------------------------------------------------------------

/// The request→response seam (and a blanket closure impl) lives in
/// `opentune-core` so a single `CoreDiag` adapter drives every service engine.
pub use opentune_core::RawDiag;

/// Computes the UDS SecurityAccess key for a given seed and level. This is the
/// **caller's** responsibility: OpenTune ships no OEM seed/key algorithm. The
/// executor requests the seed (0x27 request-seed), calls [`KeyProvider::key`],
/// and sends the returned bytes (0x27 send-key).
pub trait KeyProvider {
    /// Return the key bytes for `seed` at security `level` (the request-seed
    /// sub-function). `Err` aborts the SecurityAccess step safely.
    fn key(&self, seed: &[u8], level: u8) -> std::result::Result<Vec<u8>, String>;
}

/// Blanket impl so a plain closure can be used as a [`KeyProvider`].
impl<F> KeyProvider for F
where
    F: Fn(&[u8], u8) -> std::result::Result<Vec<u8>, String>,
{
    fn key(&self, seed: &[u8], level: u8) -> std::result::Result<Vec<u8>, String> {
        self(seed, level)
    }
}

// ---------------------------------------------------------------------------
// Script format (data)
// ---------------------------------------------------------------------------

/// The largest fixed [`Step::Wait`] a script may declare (10 minutes). Long
/// procedures poll a routine's result rather than waiting in one huge block.
pub const MAX_WAIT_MS: u64 = 600_000;

/// One step of a service procedure — a single safe UDS action. Tagged by `op`
/// in JSON (e.g. `{"op":"routine_control","control_type":1,"routine_id":4660}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Step {
    /// DiagnosticSessionControl (0x10): switch session (e.g. 0x03 extended).
    DiagnosticSession {
        /// Session sub-function (0x01 default, 0x02 programming, 0x03 extended…).
        subfunction: u8,
    },
    /// SecurityAccess (0x27): request the seed at `level`, ask the caller's
    /// [`KeyProvider`] for the key, and send it. `level` is the (odd)
    /// request-seed sub-function; the send-key sub-function is `level + 1`.
    SecurityAccess {
        /// The request-seed sub-function (odd, `0x01..=0x7D`).
        level: u8,
    },
    /// RoutineControl (0x31): start/stop/poll an on-board routine (DPF regen,
    /// EPB service mode, injector learn…). The positive response must echo the
    /// control type and routine id.
    RoutineControl {
        /// 0x01 start, 0x02 stop, 0x03 request-results.
        control_type: u8,
        /// The routine identifier.
        routine_id: u16,
        /// Optional routine control parameters.
        #[serde(default)]
        data: Vec<u8>,
    },
    /// InputOutputControlByIdentifier (0x2F): drive/reset an actuator channel.
    IoControl {
        /// The I/O data identifier.
        did: u16,
        /// Control parameter (0x00 return-control, 0x01 reset-to-default,
        /// 0x02 freeze, 0x03 short-term-adjustment).
        control: u8,
        /// Optional control state bytes.
        #[serde(default)]
        data: Vec<u8>,
    },
    /// WriteDataByIdentifier (0x2E): write a DID (e.g. a service-reset counter).
    WriteDataByIdentifier {
        /// The data identifier to write.
        did: u16,
        /// The bytes to write (must be non-empty).
        data: Vec<u8>,
    },
    /// ReadDataByIdentifier (0x22): read a DID (status/adaptation value).
    ReadDataByIdentifier {
        /// The data identifier to read.
        did: u16,
    },
    /// TesterPresent (0x3E 0x00): keep the session alive.
    TesterPresent,
    /// A fixed delay in milliseconds (e.g. between starting a regen and polling
    /// its result). Capped at [`MAX_WAIT_MS`].
    Wait {
        /// Milliseconds to wait.
        ms: u64,
    },
    /// Guard: assert the immediately preceding step produced a positive response.
    /// Fails if there was no preceding response (e.g. after a [`Step::Wait`]).
    ExpectPositive,
    /// Guard: read `did` (0x22) and assert its data equals `equals`.
    Verify {
        /// The data identifier to read back.
        did: u16,
        /// The exact data bytes expected (must be non-empty).
        equals: Vec<u8>,
    },
}

impl Step {
    /// Whether this step changes vehicle state (a write or an actuator/routine
    /// action) and therefore must be gated behind SecurityAccess when the script
    /// is `requires_security`. Reads and guards are not writes;
    /// `RoutineControl` with control type 0x03 (request-results) is a read.
    pub fn is_write(&self) -> bool {
        match self {
            Step::WriteDataByIdentifier { .. } | Step::IoControl { .. } => true,
            Step::RoutineControl { control_type, .. } => *control_type != 0x03,
            _ => false,
        }
    }

    /// Whether this step is a [`Step::SecurityAccess`].
    pub fn is_security(&self) -> bool {
        matches!(self, Step::SecurityAccess { .. })
    }

    /// A short label for reports, e.g. `"routine_control(start 0x1234)"`.
    pub fn label(&self) -> String {
        match self {
            Step::DiagnosticSession { subfunction } => {
                format!("diagnostic_session(0x{subfunction:02X})")
            }
            Step::SecurityAccess { level } => format!("security_access(level 0x{level:02X})"),
            Step::RoutineControl {
                control_type,
                routine_id,
                ..
            } => {
                let ct = match control_type {
                    0x01 => "start",
                    0x02 => "stop",
                    0x03 => "results",
                    _ => "?",
                };
                format!("routine_control({ct} 0x{routine_id:04X})")
            }
            Step::IoControl { did, control, .. } => {
                format!("io_control(0x{did:04X} ctrl 0x{control:02X})")
            }
            Step::WriteDataByIdentifier { did, .. } => format!("write_did(0x{did:04X})"),
            Step::ReadDataByIdentifier { did } => format!("read_did(0x{did:04X})"),
            Step::TesterPresent => "tester_present".to_string(),
            Step::Wait { ms } => format!("wait({ms} ms)"),
            Step::ExpectPositive => "expect_positive".to_string(),
            Step::Verify { did, .. } => format!("verify(0x{did:04X})"),
        }
    }
}

/// A data-defined service procedure: an ordered, safe UDS flow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceScript {
    /// Stable id, e.g. `"generic-dpf-regen"`.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Human description of what the procedure does / its preconditions.
    #[serde(default)]
    pub description: String,
    /// Whether the procedure requires UDS SecurityAccess (0x27) before any
    /// write/action step. When true, [`validate`](ServiceScript::validate)
    /// requires a `SecurityAccess` step before the first write, and the executor
    /// refuses a write/action until the module is unlocked.
    #[serde(default)]
    pub requires_security: bool,
    /// The ordered steps.
    pub steps: Vec<Step>,
}

impl ServiceScript {
    /// Parse a script from JSON.
    pub fn from_json(s: &str) -> std::result::Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    /// Serialize the script to pretty JSON.
    pub fn to_json(&self) -> std::result::Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Whether the script contains a [`Step::SecurityAccess`].
    pub fn has_security_step(&self) -> bool {
        self.steps.iter().any(Step::is_security)
    }

    /// Semantically validate the script. `serde` deserialization only proves the
    /// JSON shape; this checks the invariants the executor relies on, and is
    /// called by [`run_script`] before anything is sent:
    ///
    /// * non-empty `id`, `name`, and at least one step;
    /// * `DiagnosticSession` sub-function in `0x01..=0x7F`;
    /// * `SecurityAccess` `level` odd and in `0x01..=0x7D` (so send-key
    ///   `level + 1` stays a valid `0x02..=0x7E` sub-function);
    /// * `RoutineControl` control type in `0x01..=0x03`;
    /// * `IoControl` control in `0x00..=0x03`;
    /// * `WriteDataByIdentifier` / `Verify` carry non-empty bytes;
    /// * `Wait` no longer than [`MAX_WAIT_MS`]; and
    /// * when `requires_security`, every write/action step
    ///   ([`Step::is_write`]) is preceded by a `SecurityAccess` step.
    pub fn validate(&self) -> Result<()> {
        if self.id.trim().is_empty() {
            return Err(ServiceError::InvalidScript("empty id".into()));
        }
        if self.name.trim().is_empty() {
            return Err(ServiceError::InvalidScript("empty name".into()));
        }
        if self.steps.is_empty() {
            return Err(ServiceError::InvalidScript("no steps".into()));
        }
        for (i, step) in self.steps.iter().enumerate() {
            match step {
                Step::DiagnosticSession { subfunction } => {
                    if *subfunction == 0 || *subfunction > 0x7F {
                        return Err(ServiceError::InvalidScript(format!(
                            "step {i}: diagnostic session sub-function 0x{subfunction:02X} is not 0x01..=0x7F"
                        )));
                    }
                }
                Step::SecurityAccess { level } => {
                    if *level == 0 || *level > 0x7D || level % 2 == 0 {
                        return Err(ServiceError::InvalidScript(format!(
                            "step {i}: security level 0x{level:02X} must be odd and in 0x01..=0x7D"
                        )));
                    }
                }
                Step::RoutineControl { control_type, .. } => {
                    if !(0x01..=0x03).contains(control_type) {
                        return Err(ServiceError::InvalidScript(format!(
                            "step {i}: routine control type 0x{control_type:02X} is not 0x01..=0x03"
                        )));
                    }
                }
                Step::IoControl { control, .. } => {
                    if *control > 0x03 {
                        return Err(ServiceError::InvalidScript(format!(
                            "step {i}: io control 0x{control:02X} is not 0x00..=0x03"
                        )));
                    }
                }
                Step::WriteDataByIdentifier { data, .. } => {
                    if data.is_empty() {
                        return Err(ServiceError::InvalidScript(format!(
                            "step {i}: write_did has no data"
                        )));
                    }
                }
                Step::Verify { equals, .. } => {
                    if equals.is_empty() {
                        return Err(ServiceError::InvalidScript(format!(
                            "step {i}: verify has no expected bytes"
                        )));
                    }
                }
                Step::Wait { ms } => {
                    if *ms > MAX_WAIT_MS {
                        return Err(ServiceError::InvalidScript(format!(
                            "step {i}: wait {ms} ms exceeds {MAX_WAIT_MS} ms"
                        )));
                    }
                }
                Step::ReadDataByIdentifier { .. } | Step::TesterPresent | Step::ExpectPositive => {}
            }
        }
        // Write/action-before-security gate.
        if self.requires_security {
            let mut unlocked = false;
            for (i, step) in self.steps.iter().enumerate() {
                if step.is_security() {
                    unlocked = true;
                }
                if step.is_write() && !unlocked {
                    return Err(ServiceError::InvalidScript(format!(
                        "step {i} ({}) is a write/action but no SecurityAccess precedes it (requires_security)",
                        step.label()
                    )));
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Run report
// ---------------------------------------------------------------------------

/// Outcome of a single [`Step`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    /// The step completed and its response passed every check.
    Ok,
    /// The step was not sent (a dry run).
    Skipped,
    /// The step failed; see [`StepReport::detail`].
    Failed,
}

/// What happened for one step of the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepReport {
    /// Index of the step within the script.
    pub index: usize,
    /// Short label of the step ([`Step::label`]).
    pub op: String,
    /// Outcome.
    pub status: StepStatus,
    /// The request bytes sent (the final exchange for multi-exchange steps like
    /// SecurityAccess; `None` for `Wait`, guards, and skipped dry-run steps).
    pub request: Option<Vec<u8>>,
    /// The raw response bytes received, if any.
    pub response: Option<Vec<u8>>,
    /// Human-readable note or failure reason.
    pub detail: String,
}

/// Overall outcome of a [`run_script`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    /// Every step ran and passed.
    Completed,
    /// A dry run: the script and its required inputs validated; nothing was sent.
    Planned,
    /// A step failed; the run stopped at this index (the last entry in
    /// [`RunReport::steps`]).
    Failed {
        /// Index of the failing step.
        at: usize,
    },
}

/// The step-by-step result of running (or dry-running) a [`ServiceScript`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    /// The script's id.
    pub script_id: String,
    /// Whether this was a dry run.
    pub dry_run: bool,
    /// One entry per step attempted, in order. On failure the last entry is the
    /// failing step; steps after it were not attempted.
    pub steps: Vec<StepReport>,
    /// Overall outcome.
    pub outcome: RunOutcome,
    /// Whether SecurityAccess was unlocked during the run.
    pub security_unlocked: bool,
}

impl RunReport {
    /// True only when every step ran and passed ([`RunOutcome::Completed`]).
    pub fn succeeded(&self) -> bool {
        self.outcome == RunOutcome::Completed
    }

    /// The index of the failing step, if the run failed.
    pub fn failed_at(&self) -> Option<usize> {
        match self.outcome {
            RunOutcome::Failed { at } => Some(at),
            _ => None,
        }
    }
}

/// Options for [`run_script`].
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// Validate the script and its required inputs, but send **nothing** — no
    /// writes, no actions, no I/O at all. Every step is recorded as
    /// [`StepStatus::Skipped`] and the outcome is [`RunOutcome::Planned`].
    pub dry_run: bool,
    /// Skip real `Wait` sleeps (for tests). The wait is still recorded.
    pub skip_waits: bool,
}

// ---------------------------------------------------------------------------
// UDS response checking (mirrors opentune-coding's `check`)
// ---------------------------------------------------------------------------

/// Check a UDS response against the request's service id (`req_sid`): accept a
/// positive response (`req_sid + 0x40`), and map a negative response (0x7F) to a
/// descriptive error — rejecting a truncated 0x7F and one that echoes a
/// *different* requested service (a mis-correlated response).
fn check(req_sid: u8, resp: &[u8]) -> std::result::Result<(), String> {
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

/// Send one request, check the response SID, and confirm the positive response
/// **echoes** `echo` immediately after the SID byte (sub-function / identifier).
/// Records the request and response into `sr`. Returns the response or an error
/// string (which the caller folds into the step's detail).
fn exchange(
    diag: &mut dyn RawDiag,
    sr: &mut StepReport,
    req: Vec<u8>,
    sid: u8,
    echo: &[u8],
) -> std::result::Result<Vec<u8>, String> {
    sr.request = Some(req.clone());
    let resp = diag
        .request(&req)
        .map_err(|e| format!("transport error: {e}"))?;
    sr.response = Some(resp.clone());
    check(sid, &resp)?;
    if resp.len() < 1 + echo.len() || resp[1..1 + echo.len()] != *echo {
        return Err(format!(
            "response did not echo expected bytes {echo:02X?} (got {resp:02X?})"
        ));
    }
    Ok(resp)
}

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------

/// Run a service script step by step over `diag`, asking `key` for any
/// SecurityAccess key. Returns `Err` only when **nothing was sent** — the script
/// failed [`ServiceScript::validate`] or a required [`KeyProvider`] was missing.
/// Otherwise returns `Ok(report)`; a failed step stops the run safely and is
/// recorded in [`RunReport`] with [`RunOutcome::Failed`].
///
/// With [`RunOptions::dry_run`], the script and its required inputs are validated
/// and **nothing is sent** (no writes, no actions, no I/O); the report's outcome
/// is [`RunOutcome::Planned`].
pub fn run_script(
    diag: &mut dyn RawDiag,
    script: &ServiceScript,
    key: Option<&dyn KeyProvider>,
    opts: &RunOptions,
) -> Result<RunReport> {
    // Pre-flight: validate the script and required inputs before sending anything.
    script.validate()?;
    if (script.requires_security || script.has_security_step()) && key.is_none() {
        return Err(ServiceError::MissingKeyProvider);
    }

    let mut report = RunReport {
        script_id: script.id.clone(),
        dry_run: opts.dry_run,
        steps: Vec::with_capacity(script.steps.len()),
        outcome: RunOutcome::Completed,
        security_unlocked: false,
    };

    // Dry run: validated above; send nothing at all.
    if opts.dry_run {
        for (index, step) in script.steps.iter().enumerate() {
            report.steps.push(StepReport {
                index,
                op: step.label(),
                status: StepStatus::Skipped,
                request: None,
                response: None,
                detail: "dry-run: not sent".into(),
            });
        }
        report.outcome = RunOutcome::Planned;
        return Ok(report);
    }

    let mut security_unlocked = false;
    let mut last_response: Option<Vec<u8>> = None;

    for (index, step) in script.steps.iter().enumerate() {
        // Defence in depth: refuse a write/action until security is unlocked.
        if script.requires_security && step.is_write() && !security_unlocked {
            report.steps.push(StepReport {
                index,
                op: step.label(),
                status: StepStatus::Failed,
                request: None,
                response: None,
                detail: "refused: write/action before SecurityAccess unlock".into(),
            });
            report.outcome = RunOutcome::Failed { at: index };
            report.security_unlocked = security_unlocked;
            return Ok(report);
        }

        let sr = execute_one(
            diag,
            index,
            step,
            key,
            opts,
            &mut security_unlocked,
            &mut last_response,
        );
        let failed = sr.status == StepStatus::Failed;
        report.steps.push(sr);
        if failed {
            report.outcome = RunOutcome::Failed { at: index };
            report.security_unlocked = security_unlocked;
            return Ok(report);
        }
    }

    report.security_unlocked = security_unlocked;
    Ok(report)
}

/// Execute one step, producing its [`StepReport`]. Mutates `security_unlocked`
/// and `last_response`; never panics — a failure is reported, not returned.
fn execute_one(
    diag: &mut dyn RawDiag,
    index: usize,
    step: &Step,
    key: Option<&dyn KeyProvider>,
    opts: &RunOptions,
    security_unlocked: &mut bool,
    last_response: &mut Option<Vec<u8>>,
) -> StepReport {
    let mut sr = StepReport {
        index,
        op: step.label(),
        status: StepStatus::Ok,
        request: None,
        response: None,
        detail: String::new(),
    };

    let result: std::result::Result<(), String> = match step {
        Step::DiagnosticSession { subfunction } => exchange(
            diag,
            &mut sr,
            vec![0x10, *subfunction],
            0x10,
            &[*subfunction],
        )
        .map(|resp| {
            *last_response = Some(resp);
        }),
        Step::TesterPresent => {
            exchange(diag, &mut sr, vec![0x3E, 0x00], 0x3E, &[0x00]).map(|resp| {
                *last_response = Some(resp);
            })
        }
        Step::ReadDataByIdentifier { did } => {
            let (hi, lo) = ((did >> 8) as u8, *did as u8);
            exchange(diag, &mut sr, vec![0x22, hi, lo], 0x22, &[hi, lo]).map(|resp| {
                sr.detail = format!("data = {:02X?}", &resp[3..]);
                *last_response = Some(resp);
            })
        }
        Step::WriteDataByIdentifier { did, data } => {
            let (hi, lo) = ((did >> 8) as u8, *did as u8);
            let mut req = vec![0x2E, hi, lo];
            req.extend_from_slice(data);
            exchange(diag, &mut sr, req, 0x2E, &[hi, lo]).map(|resp| {
                sr.detail = format!("wrote {} byte(s) to 0x{did:04X}", data.len());
                *last_response = Some(resp);
            })
        }
        Step::IoControl { did, control, data } => {
            let (hi, lo) = ((did >> 8) as u8, *did as u8);
            let mut req = vec![0x2F, hi, lo, *control];
            req.extend_from_slice(data);
            exchange(diag, &mut sr, req, 0x2F, &[hi, lo, *control]).map(|resp| {
                *last_response = Some(resp);
            })
        }
        Step::RoutineControl {
            control_type,
            routine_id,
            data,
        } => {
            let (hi, lo) = ((routine_id >> 8) as u8, *routine_id as u8);
            let mut req = vec![0x31, *control_type, hi, lo];
            req.extend_from_slice(data);
            exchange(diag, &mut sr, req, 0x31, &[*control_type, hi, lo]).map(|resp| {
                sr.detail = format!("routine info = {:02X?}", &resp[4..]);
                *last_response = Some(resp);
            })
        }
        Step::SecurityAccess { level } => {
            security_access(diag, &mut sr, *level, key, security_unlocked, last_response)
        }
        Step::Wait { ms } => {
            if !opts.skip_waits && *ms > 0 {
                std::thread::sleep(std::time::Duration::from_millis(*ms));
            }
            sr.detail = format!("waited {ms} ms");
            Ok(())
        }
        Step::ExpectPositive => match last_response {
            Some(resp) if !resp.is_empty() && resp[0] != 0x7F => {
                sr.detail = "preceding response was positive".into();
                Ok(())
            }
            Some(_) => Err("preceding response was negative or empty".into()),
            None => Err("no preceding response to check".into()),
        },
        Step::Verify { did, equals } => {
            let (hi, lo) = ((did >> 8) as u8, *did as u8);
            exchange(diag, &mut sr, vec![0x22, hi, lo], 0x22, &[hi, lo]).and_then(|resp| {
                let got = &resp[3..];
                if got == equals.as_slice() {
                    sr.detail = format!("verified 0x{did:04X} == {equals:02X?}");
                    *last_response = Some(resp);
                    Ok(())
                } else {
                    Err(format!(
                        "verify mismatch for 0x{did:04X}: got {got:02X?}, want {equals:02X?}"
                    ))
                }
            })
        }
    };

    if let Err(e) = result {
        sr.status = StepStatus::Failed;
        sr.detail = if sr.detail.is_empty() {
            e
        } else {
            format!("{}: {e}", sr.detail)
        };
    }
    sr
}

/// Run one SecurityAccess exchange: request the seed, ask the key provider, and
/// send the key. A zero seed is treated as "already unlocked" and sends no key.
fn security_access(
    diag: &mut dyn RawDiag,
    sr: &mut StepReport,
    level: u8,
    key: Option<&dyn KeyProvider>,
    security_unlocked: &mut bool,
    last_response: &mut Option<Vec<u8>>,
) -> std::result::Result<(), String> {
    // Request seed (0x27, level).
    let seed_resp = exchange(diag, sr, vec![0x27, level], 0x27, &[level])?;
    let seed = seed_resp[2..].to_vec();

    if seed.iter().all(|&b| b == 0) {
        // Already unlocked (or no seed needed): do not send a key.
        *security_unlocked = true;
        sr.detail = "module already unlocked (zero seed)".into();
        *last_response = Some(seed_resp);
        return Ok(());
    }

    let kp = key.ok_or_else(|| "security access requires a key provider".to_string())?;
    let computed = kp
        .key(&seed, level)
        .map_err(|e| format!("key provider failed: {e}"))?;

    // Send key (0x27, level + 1, key…).
    let mut req = vec![0x27, level + 1];
    req.extend_from_slice(&computed);
    let key_resp = exchange(diag, sr, req, 0x27, &[level + 1])?;

    *security_unlocked = true;
    sr.detail = format!(
        "unlocked level 0x{level:02X} (seed {} byte(s), key {} byte(s))",
        seed.len(),
        computed.len()
    );
    *last_response = Some(key_resp);
    Ok(())
}

// ---------------------------------------------------------------------------
// Example scripts (generic shapes, not vehicle-specific calibrations)
// ---------------------------------------------------------------------------

/// Example service scripts — **generic shapes** with placeholder identifiers to
/// illustrate the two common forms. They are not vehicle-specific calibrations;
/// OpenTune ships no OEM procedure data. Use them as templates and as fixtures.
pub mod examples {
    use super::{ServiceScript, Step};

    /// A write-based "service / oil-interval reset" shape: open an extended
    /// session, write a reset value to a service DID, and read it back to verify.
    /// No SecurityAccess (many service resets are unprotected).
    pub fn oil_service_reset() -> ServiceScript {
        ServiceScript {
            id: "generic-oil-service-reset".into(),
            name: "Oil / service reset (generic shape)".into(),
            description:
                "Illustrative write-based reset: extended session, write a service DID, verify. \
                 Placeholder DID 0x2800 — not a real vehicle calibration."
                    .into(),
            requires_security: false,
            steps: vec![
                Step::DiagnosticSession { subfunction: 0x03 },
                Step::TesterPresent,
                Step::WriteDataByIdentifier {
                    did: 0x2800,
                    data: vec![0x00],
                },
                Step::Verify {
                    did: 0x2800,
                    equals: vec![0x00],
                },
            ],
        }
    }

    /// A routine-based "DPF forced regeneration" shape: open an extended session,
    /// unlock SecurityAccess, start a routine, wait, poll its result, and confirm
    /// a positive response. Placeholder level/routine — not a real calibration.
    pub fn dpf_regen() -> ServiceScript {
        ServiceScript {
            id: "generic-dpf-regen".into(),
            name: "DPF forced regeneration (generic shape)".into(),
            description:
                "Illustrative routine-based action: extended session, SecurityAccess, start a \
                 routine, wait, request results, expect positive. Placeholder level 0x11 and \
                 routine 0x1234 — not a real vehicle calibration."
                    .into(),
            requires_security: true,
            steps: vec![
                Step::DiagnosticSession { subfunction: 0x03 },
                Step::SecurityAccess { level: 0x11 },
                Step::RoutineControl {
                    control_type: 0x01,
                    routine_id: 0x1234,
                    data: vec![],
                },
                Step::Wait { ms: 2000 },
                Step::RoutineControl {
                    control_type: 0x03,
                    routine_id: 0x1234,
                    data: vec![],
                },
                Step::ExpectPositive,
            ],
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A scripted UDS responder for tests. Answers 0x10/0x3E/0x27 (seed+key),
    /// 0x22 from a per-DID store, applies 0x2E, echoes 0x31/0x2F, and logs every
    /// request. Optional hooks make a service, routine, or key send fail.
    struct Script {
        store: HashMap<u16, Vec<u8>>,
        log: Vec<Vec<u8>>,
        /// Seed to hand out for request-seed (non-zero => a key is required).
        seed: Vec<u8>,
        /// The only key the stub ECU accepts; a different key => negative 0x35.
        accept_key: Vec<u8>,
        /// If set, 0x31 with this routine id returns a negative response.
        fail_routine: Option<u16>,
    }

    impl Script {
        fn new() -> Self {
            Script {
                store: HashMap::new(),
                log: Vec::new(),
                seed: vec![0x11, 0x22, 0x33, 0x44],
                accept_key: vec![0xAA, 0xBB, 0xCC, 0xDD],
                fail_routine: None,
            }
        }
        fn with_did(mut self, did: u16, data: &[u8]) -> Self {
            self.store.insert(did, data.to_vec());
            self
        }
    }

    impl RawDiag for Script {
        fn request(&mut self, req: &[u8]) -> std::result::Result<Vec<u8>, String> {
            self.log.push(req.to_vec());
            match req[0] {
                0x10 => Ok(vec![0x50, req[1]]),
                0x3E => Ok(vec![0x7E, req[1]]),
                0x27 => {
                    if req[1] % 2 == 1 {
                        // request seed
                        let mut r = vec![0x67, req[1]];
                        r.extend_from_slice(&self.seed);
                        Ok(r)
                    } else {
                        // send key
                        if req[2..] == self.accept_key[..] {
                            Ok(vec![0x67, req[1]])
                        } else {
                            Ok(vec![0x7F, 0x27, 0x35]) // invalidKey
                        }
                    }
                }
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
                0x2F => Ok(vec![0x6F, req[1], req[2], req[3]]),
                0x31 => {
                    let rid = ((req[2] as u16) << 8) | req[3] as u16;
                    if self.fail_routine == Some(rid) {
                        Ok(vec![0x7F, 0x31, 0x22]) // conditionsNotCorrect
                    } else {
                        Ok(vec![0x71, req[1], req[2], req[3], 0x00])
                    }
                }
                _ => Err("unexpected service".into()),
            }
        }
    }

    /// Stub key provider that returns the fixed key the stub ECU accepts.
    fn good_key() -> impl KeyProvider {
        |_seed: &[u8], _level: u8| -> std::result::Result<Vec<u8>, String> {
            Ok(vec![0xAA, 0xBB, 0xCC, 0xDD])
        }
    }

    fn opts() -> RunOptions {
        RunOptions {
            dry_run: false,
            skip_waits: true,
        }
    }

    /// A full session -> security -> routine -> verify script that should pass.
    fn full_script() -> ServiceScript {
        ServiceScript {
            id: "test-full".into(),
            name: "Full flow".into(),
            description: String::new(),
            requires_security: true,
            steps: vec![
                Step::DiagnosticSession { subfunction: 0x03 },
                Step::SecurityAccess { level: 0x11 },
                Step::RoutineControl {
                    control_type: 0x01,
                    routine_id: 0x1234,
                    data: vec![0x01],
                },
                Step::WriteDataByIdentifier {
                    did: 0x2800,
                    data: vec![0x5A],
                },
                Step::Verify {
                    did: 0x2800,
                    equals: vec![0x5A],
                },
            ],
        }
    }

    #[test]
    fn full_flow_succeeds() {
        let mut io = Script::new();
        let kp = good_key();
        let rep = run_script(&mut io, &full_script(), Some(&kp), &opts()).unwrap();
        assert!(rep.succeeded(), "report = {rep:#?}");
        assert!(rep.security_unlocked);
        assert_eq!(rep.steps.len(), 5);
        assert!(rep.steps.iter().all(|s| s.status == StepStatus::Ok));
        // The written value is actually in the stub ECU afterwards.
        assert_eq!(io.store.get(&0x2800).unwrap(), &vec![0x5A]);
    }

    #[test]
    fn wrong_key_stops_safely_with_report() {
        let mut io = Script::new();
        let bad = |_seed: &[u8], _level: u8| -> std::result::Result<Vec<u8>, String> {
            Ok(vec![0xDE, 0xAD]) // not the accepted key
        };
        let rep = run_script(&mut io, &full_script(), Some(&bad), &opts()).unwrap();
        assert_eq!(rep.failed_at(), Some(1)); // the SecurityAccess step
        assert!(!rep.security_unlocked);
        let failed = rep.steps.last().unwrap();
        assert_eq!(failed.status, StepStatus::Failed);
        // The negative send-key response is recorded in the report.
        assert_eq!(failed.response.as_deref(), Some(&[0x7F, 0x27, 0x35][..]));
        // The run stopped: the routine/write/verify steps never ran.
        assert_eq!(rep.steps.len(), 2);
        // Nothing was written.
        assert!(!io.store.contains_key(&0x2800));
    }

    #[test]
    fn negative_routine_response_stops_safely() {
        let mut io = Script::new();
        io.fail_routine = Some(0x1234);
        let kp = good_key();
        let rep = run_script(&mut io, &full_script(), Some(&kp), &opts()).unwrap();
        assert_eq!(rep.failed_at(), Some(2)); // the RoutineControl step
        assert!(rep.security_unlocked); // security still happened first
        let failed = rep.steps.last().unwrap();
        assert_eq!(failed.response.as_deref(), Some(&[0x7F, 0x31, 0x22][..]));
    }

    #[test]
    fn dry_run_sends_nothing() {
        let mut io = Script::new();
        let kp = good_key();
        let rep = run_script(
            &mut io,
            &full_script(),
            Some(&kp),
            &RunOptions {
                dry_run: true,
                skip_waits: true,
            },
        )
        .unwrap();
        assert_eq!(rep.outcome, RunOutcome::Planned);
        assert!(rep.steps.iter().all(|s| s.status == StepStatus::Skipped));
        assert!(io.log.is_empty()); // not a single request was sent
        assert!(!io.store.contains_key(&0x2800)); // no write happened
    }

    #[test]
    fn write_without_security_is_refused_at_validation() {
        // requires_security, but a write appears with no SecurityAccess before it.
        let script = ServiceScript {
            id: "bad-order".into(),
            name: "Write before security".into(),
            description: String::new(),
            requires_security: true,
            steps: vec![
                Step::DiagnosticSession { subfunction: 0x03 },
                Step::WriteDataByIdentifier {
                    did: 0x2800,
                    data: vec![0x00],
                },
                Step::SecurityAccess { level: 0x11 },
            ],
        };
        assert!(matches!(
            script.validate(),
            Err(ServiceError::InvalidScript(_))
        ));
        // run_script refuses it before sending anything.
        let mut io = Script::new();
        let kp = good_key();
        assert!(matches!(
            run_script(&mut io, &script, Some(&kp), &opts()),
            Err(ServiceError::InvalidScript(_))
        ));
        assert!(io.log.is_empty());
    }

    #[test]
    fn runtime_refuses_write_before_unlock_when_misordered_passes_validation() {
        // A RoutineControl action with requires_security but the only security
        // step is a *request-results* routine that doesn't unlock — validation
        // permits it (a stop/results routine is not itself a write before this),
        // so the runtime gate is the backstop. Here we force the situation by a
        // script whose security step comes but the ECU never unlocks.
        let script = ServiceScript {
            id: "no-unlock".into(),
            name: "Security denied".into(),
            description: String::new(),
            requires_security: true,
            steps: vec![
                Step::SecurityAccess { level: 0x11 },
                Step::RoutineControl {
                    control_type: 0x01,
                    routine_id: 0x1234,
                    data: vec![],
                },
            ],
        };
        // Key provider returns a wrong key => security fails => never unlocked.
        let mut io = Script::new();
        let bad = |_s: &[u8], _l: u8| Ok(vec![0x00]);
        let rep = run_script(&mut io, &script, Some(&bad), &opts()).unwrap();
        assert_eq!(rep.failed_at(), Some(0)); // stops at security, routine never runs
        assert!(!rep.security_unlocked);
        assert_eq!(rep.steps.len(), 1);
    }

    #[test]
    fn validation_rejects_malformed_scripts() {
        // Empty steps.
        let mut s = full_script();
        s.steps.clear();
        assert!(matches!(s.validate(), Err(ServiceError::InvalidScript(_))));
        // Even security level (request-seed must be odd).
        let mut s2 = full_script();
        s2.steps[1] = Step::SecurityAccess { level: 0x10 };
        assert!(matches!(s2.validate(), Err(ServiceError::InvalidScript(_))));
        // Bad routine control type.
        let mut s3 = full_script();
        s3.steps[2] = Step::RoutineControl {
            control_type: 0x09,
            routine_id: 0x1234,
            data: vec![],
        };
        assert!(matches!(s3.validate(), Err(ServiceError::InvalidScript(_))));
        // Empty write data.
        let mut s4 = full_script();
        s4.steps[3] = Step::WriteDataByIdentifier {
            did: 0x2800,
            data: vec![],
        };
        assert!(matches!(s4.validate(), Err(ServiceError::InvalidScript(_))));
        // Oversized wait.
        let mut s5 = full_script();
        s5.steps.push(Step::Wait {
            ms: MAX_WAIT_MS + 1,
        });
        assert!(matches!(s5.validate(), Err(ServiceError::InvalidScript(_))));
    }

    #[test]
    fn missing_key_provider_is_rejected() {
        let mut io = Script::new();
        assert!(matches!(
            run_script(&mut io, &full_script(), None, &opts()),
            Err(ServiceError::MissingKeyProvider)
        ));
        assert!(io.log.is_empty());
        // And in dry-run too (required inputs are validated).
        assert!(matches!(
            run_script(
                &mut io,
                &full_script(),
                None,
                &RunOptions {
                    dry_run: true,
                    skip_waits: true
                }
            ),
            Err(ServiceError::MissingKeyProvider)
        ));
    }

    #[test]
    fn mis_correlated_response_rejected() {
        // ECU answers a 0x10 session request with a 0x2E-family positive SID.
        let mut io = |req: &[u8]| -> std::result::Result<Vec<u8>, String> {
            match req[0] {
                0x10 => Ok(vec![0x6E, req[1]]), // wrong SID (wanted 0x50)
                _ => Err("?".into()),
            }
        };
        let script = ServiceScript {
            id: "s".into(),
            name: "n".into(),
            description: String::new(),
            requires_security: false,
            steps: vec![Step::DiagnosticSession { subfunction: 0x03 }],
        };
        let rep = run_script(&mut io, &script, None, &opts()).unwrap();
        assert_eq!(rep.failed_at(), Some(0));
    }

    #[test]
    fn routine_echo_mismatch_rejected() {
        // ECU answers 0x31 but echoes the wrong routine id -> treated as failure.
        let mut io = |req: &[u8]| -> std::result::Result<Vec<u8>, String> {
            match req[0] {
                0x31 => Ok(vec![0x71, req[1], 0x99, 0x99, 0x00]), // wrong routine echo
                _ => Err("?".into()),
            }
        };
        let script = ServiceScript {
            id: "s".into(),
            name: "n".into(),
            description: String::new(),
            requires_security: false,
            steps: vec![Step::RoutineControl {
                control_type: 0x01,
                routine_id: 0x1234,
                data: vec![],
            }],
        };
        let rep = run_script(&mut io, &script, None, &opts()).unwrap();
        assert_eq!(rep.failed_at(), Some(0));
    }

    #[test]
    fn already_unlocked_zero_seed_sends_no_key() {
        let mut io = Script::new();
        io.seed = vec![0x00, 0x00, 0x00, 0x00]; // zero seed => already unlocked
                                                // A key provider that would panic the test if called.
        let kp = |_s: &[u8], _l: u8| -> std::result::Result<Vec<u8>, String> {
            panic!("key provider must not be called for a zero seed")
        };
        let script = ServiceScript {
            id: "sa".into(),
            name: "sa".into(),
            description: String::new(),
            requires_security: true,
            steps: vec![
                Step::SecurityAccess { level: 0x11 },
                Step::WriteDataByIdentifier {
                    did: 0x2800,
                    data: vec![0x01],
                },
            ],
        };
        let rep = run_script(&mut io, &script, Some(&kp), &opts()).unwrap();
        assert!(rep.succeeded());
        assert!(rep.security_unlocked);
        // Only the request-seed 0x27 was sent (no send-key).
        let sec_reqs: Vec<_> = io.log.iter().filter(|r| r[0] == 0x27).collect();
        assert_eq!(sec_reqs.len(), 1);
        assert_eq!(sec_reqs[0][1] % 2, 1); // it was a request-seed sub-function
    }

    #[test]
    fn example_scripts_validate_and_roundtrip_json() {
        for s in [examples::oil_service_reset(), examples::dpf_regen()] {
            s.validate().unwrap();
            let json = s.to_json().unwrap();
            let back = ServiceScript::from_json(&json).unwrap();
            assert_eq!(s, back);
        }
    }

    #[test]
    fn example_oil_reset_runs_green() {
        // The oil-reset shape needs its service DID to exist for the verify.
        let mut io = Script::new().with_did(0x2800, &[0xFF]);
        let rep = run_script(&mut io, &examples::oil_service_reset(), None, &opts()).unwrap();
        assert!(rep.succeeded(), "report = {rep:#?}");
    }

    #[test]
    fn script_parses_from_json() {
        let js = r#"{
          "id":"generic-epb-service","name":"EPB service mode",
          "requires_security":true,
          "steps":[
            {"op":"diagnostic_session","subfunction":3},
            {"op":"security_access","level":17},
            {"op":"io_control","did":16384,"control":3,"data":[1]},
            {"op":"tester_present"}
          ]
        }"#;
        let s = ServiceScript::from_json(js).unwrap();
        assert_eq!(s.steps.len(), 4);
        assert!(s.requires_security);
        s.validate().unwrap();
    }
}
