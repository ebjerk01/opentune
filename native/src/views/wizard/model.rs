//! "Check my car" logic (pure: no egui, no adapter), the port of the webview
//! app's `wizard-core.js`. Everything that decides what a step SAYS lives
//! here so the tests can check it: the step state machine (run / done /
//! failed / skipped / retry), plain-language errors with the raw detail kept,
//! the battery-voltage rating, the scan summary, the explanations (only what
//! the data says — never a cause), the live-snapshot rating and the HTML /
//! Markdown report.

use opentune_autoscan::ModuleResult as ModuleScanResult;
use opentune_core::obd::inspect::{obd_ecu_label, Inspection, MonitorState};
use opentune_core::obd::{self, Readout};
use opentune_core::Capabilities;
use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------------------
// Steps
// ---------------------------------------------------------------------------

/// The six steps, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StepId {
    Connect,
    Identify,
    Scan,
    Explain,
    Live,
    Report,
}

impl StepId {
    pub const ALL: [StepId; 6] = [
        StepId::Connect,
        StepId::Identify,
        StepId::Scan,
        StepId::Explain,
        StepId::Live,
        StepId::Report,
    ];

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn title(self) -> &'static str {
        match self {
            StepId::Connect => "Connect",
            StepId::Identify => "Identify",
            StepId::Scan => "Scan",
            StepId::Explain => "Explain",
            StepId::Live => "Live check",
            StepId::Report => "Report",
        }
    }

    /// The step talks to the car (it waits for the user to press Run).
    pub fn io(self) -> bool {
        !matches!(self, StepId::Explain | StepId::Report)
    }

    /// Steps whose result is made stale when this one is re-run or skipped.
    fn dependents(self) -> &'static [StepId] {
        match self {
            StepId::Scan => &[StepId::Explain],
            _ => &[],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Pending,
    Running,
    Done,
    Failed,
    Skipped,
}

impl Status {
    pub fn resolved(self) -> bool {
        matches!(self, Status::Done | Status::Skipped)
    }
    /// The report's word for it.
    pub fn text(self) -> &'static str {
        match self {
            Status::Done => "done",
            Status::Skipped => "skipped",
            Status::Failed => "failed",
            Status::Pending => "not run",
            Status::Running => "running",
        }
    }
    /// CSS class / stable key.
    pub fn key(self) -> &'static str {
        match self {
            Status::Done => "done",
            Status::Skipped => "skipped",
            Status::Failed => "failed",
            Status::Pending => "pending",
            Status::Running => "running",
        }
    }
}

/// What a step produced.
#[derive(Clone, Debug, PartialEq)]
pub enum StepData {
    Connect(ConnectInfo),
    Identify(Identity),
    Scan(ScanData),
    Explain(Vec<ExplainItem>),
    Live(LiveData),
    Report,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub status: Status,
    pub data: Option<StepData>,
    pub error: Option<ErrorInfo>,
    pub attempts: u32,
}

impl Default for Step {
    fn default() -> Self {
        Self {
            status: Status::Pending,
            data: None,
            error: None,
            attempts: 0,
        }
    }
}

/// One check, start to report.
#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub started_ms: u64,
    /// Index into [`StepId::ALL`] of the step on screen.
    pub current: usize,
    pub steps: [Step; 6],
}

impl Run {
    pub fn new(now_ms: u64) -> Self {
        Self {
            started_ms: now_ms,
            current: 0,
            steps: Default::default(),
        }
    }

    pub fn step(&self, id: StepId) -> &Step {
        &self.steps[id.index()]
    }

    fn step_mut(&mut self, id: StepId) -> &mut Step {
        &mut self.steps[id.index()]
    }

    pub fn status(&self, id: StepId) -> Status {
        self.step(id).status
    }

    pub fn cur(&self) -> StepId {
        StepId::ALL[self.current]
    }

    fn stale(&mut self, id: StepId) {
        for &d in id.dependents() {
            let s = self.step_mut(d);
            if s.status != Status::Running {
                s.status = Status::Pending;
                s.data = None;
                s.error = None;
            }
        }
    }

    pub fn begin(&mut self, id: StepId) {
        let s = self.step_mut(id);
        s.status = Status::Running;
        s.error = None;
        s.attempts += 1;
    }

    pub fn succeed(&mut self, id: StepId, data: StepData) {
        let s = self.step_mut(id);
        s.status = Status::Done;
        s.data = Some(data);
        s.error = None;
        self.stale(id);
    }

    pub fn fail(&mut self, id: StepId, raw: &str) {
        let s = self.step_mut(id);
        s.status = Status::Failed;
        s.error = Some(error_info(raw));
        self.stale(id);
    }

    /// Skipping keeps nothing from an earlier attempt: a skipped step reports
    /// nothing. A running step cannot be skipped.
    pub fn skip(&mut self, id: StepId) {
        let s = self.step_mut(id);
        if s.status == Status::Running {
            return;
        }
        s.status = Status::Skipped;
        s.data = None;
        s.error = None;
        self.stale(id);
    }

    pub fn busy(&self) -> bool {
        self.steps.iter().any(|s| s.status == Status::Running)
    }

    pub fn can_next(&self) -> bool {
        !self.busy() && self.status(self.cur()).resolved() && self.current < StepId::ALL.len() - 1
    }

    pub fn next(&mut self) {
        if self.can_next() {
            self.current += 1;
        }
    }

    pub fn back(&mut self) {
        if !self.busy() && self.current > 0 {
            self.current -= 1;
        }
    }

    /// Any step up to the first unresolved one can be opened from the step bar.
    pub fn reachable(&self, i: usize) -> bool {
        i < StepId::ALL.len() && StepId::ALL[..i].iter().all(|&s| self.status(s).resolved())
    }

    pub fn go_to(&mut self, i: usize) {
        if !self.busy() && self.reachable(i) {
            self.current = i;
        }
    }

    // Typed views of the step data.

    pub fn connect(&self) -> Option<&ConnectInfo> {
        match &self.step(StepId::Connect).data {
            Some(StepData::Connect(c)) => Some(c),
            _ => None,
        }
    }
    pub fn identify(&self) -> Option<&Identity> {
        match &self.step(StepId::Identify).data {
            Some(StepData::Identify(c)) => Some(c),
            _ => None,
        }
    }
    pub fn scan(&self) -> Option<&ScanData> {
        match &self.step(StepId::Scan).data {
            Some(StepData::Scan(c)) => Some(c),
            _ => None,
        }
    }
    pub fn explain(&self) -> Option<&[ExplainItem]> {
        match &self.step(StepId::Explain).data {
            Some(StepData::Explain(c)) => Some(c),
            _ => None,
        }
    }
    pub fn live(&self) -> Option<&LiveData> {
        match &self.step(StepId::Live).data {
            Some(StepData::Live(c)) => Some(c),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Plain language first; the backend's own text stays available as `raw`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorInfo {
    pub text: String,
    pub raw: String,
}

pub const BUSY_TEXT: &str = "Another panel is using the adapter right now (for example the \
     Dashboard or a live log). Stop it there, then press Retry.";
const OPEN_TEXT: &str = "The adapter could not be opened. Check that it is plugged in, that \
     the cable and port chosen in the top bar are right and that no other program is using it.";
const LINK_TEXT: &str = "The adapter is there but could not talk to the car. Check that the \
     ignition is on; on a dongle with an HS/MS switch, set it to HS.";
const SILENT_TEXT: &str = "The car did not answer. Check that the ignition is on and the \
     adapter is firmly in the OBD socket; on a dongle with an HS/MS switch, set it to HS.";
const LIST_TEXT: &str = "The vehicle module list could not be read. Choose another module \
     list, or skip the full module scan.";
const NODIAG_TEXT: &str = "This adapter cannot carry diagnostic requests (it needs CAN or \
     DoIP). Choose another cable in the top bar.";
const OTHER_TEXT: &str = "Something went wrong while talking to the car. The technical detail \
     below says what the app received.";

/// `word` appears in `s` as a whole word (`\bword\b`).
fn has_word(s: &str, word: &str) -> bool {
    let wc = |c: char| c.is_ascii_alphanumeric() || c == '_';
    s.match_indices(word).any(|(i, _)| {
        let before = s[..i].chars().next_back();
        let after = s[i + word.len()..].chars().next();
        !before.is_some_and(wc) && !after.is_some_and(wc)
    })
}

pub fn error_info(raw: &str) -> ErrorInfo {
    let s = raw.to_lowercase();
    let any = |ws: &[&str]| ws.iter().any(|w| s.contains(w));
    let text = if s.contains("adapter busy") {
        BUSY_TEXT
    } else if any(&[
        "no adapter",
        "no interface",
        "not found",
        "could not open",
        "cannot open",
        "permission denied",
        "no such file",
        "os error 2",
        "access is denied",
    ]) || has_word(&s, "port")
    {
        OPEN_TEXT
    } else if any(&[
        "unable to connect",
        "bus init",
        "can error",
        "bus off",
        "busoff",
        "stopped",
    ]) {
        LINK_TEXT
    } else if any(&[
        "time out",
        "timeout",
        "timed out",
        "no response",
        "no data",
        "did not answer",
        "silent",
    ]) {
        SILENT_TEXT
    } else if any(&["module list", "parse"])
        || s.find("read ").is_some_and(|i| s[i..].contains("json"))
    {
        LIST_TEXT
    } else if any(&["cannot carry diagnostics", "no diagnostic link"]) {
        NODIAG_TEXT
    } else {
        OTHER_TEXT
    };
    ErrorInfo {
        text: text.into(),
        raw: raw.into(),
    }
}

// ---------------------------------------------------------------------------
// Levels
// ---------------------------------------------------------------------------

/// How a reading or an explanation rates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Crit,
    /// Needs attention (a code, the lamp on, no car power).
    Bad,
    Info,
    Unknown,
    /// No threshold: shown without a rating.
    Unrated,
}

impl Level {
    /// The badge word.
    pub fn word(self) -> &'static str {
        match self {
            Level::Ok => "OK",
            Level::Warn => "warning",
            Level::Crit => "critical",
            Level::Bad => "attention",
            Level::Info => "info",
            Level::Unknown => "unknown",
            Level::Unrated => "\u{2014}",
        }
    }
    /// The report's rating cell (`ok`, `warn`, `crit`; `—` when unrated).
    pub fn key(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Warn => "warn",
            Level::Crit => "crit",
            Level::Bad => "bad",
            Level::Info => "info",
            Level::Unknown => "unknown",
            Level::Unrated => "\u{2014}",
        }
    }
    fn worse(self) -> u8 {
        match self {
            Level::Unrated => 0,
            Level::Ok => 1,
            Level::Warn => 2,
            Level::Crit => 3,
            _ => 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Battery voltage
// ---------------------------------------------------------------------------
//
// OpenTune has no shared voltage rating for reads, so the wizard uses these
// simple bands (millivolts, measured at the OBD socket by the adapter):
//   < 11.0 V        bad   — the Auto-debug check's "low / no car power" limit
//   11.0 – 12.0 V   warn  — below 12.0 V, the minimum OpenTune's flash policy
//                           requires before any write
//   12.0 – 13.0 V   ok    — the usual range with the engine off
//   13.0 – 15.0 V   ok    — the usual charging range with the engine running
//   > 15.0 V        warn  — above the usual charging range
// A rating describes the reading only; it never names a cause.

pub const VOLT_NO_POWER: u32 = 11_000;
pub const VOLT_MIN_WRITE: u32 = 12_000;
pub const VOLT_RUNNING: u32 = 13_000;
pub const VOLT_HIGH: u32 = 15_000;

fn volts(mv: u32) -> String {
    format!("{:.1} V", mv as f64 / 1000.0)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Verdict {
    pub level: Level,
    pub mv: Option<u32>,
    pub text: String,
}

pub fn voltage_verdict(mv: Option<u32>) -> Verdict {
    let Some(mv) = mv else {
        return Verdict {
            level: Level::Unknown,
            mv: None,
            text: "This adapter does not report the battery voltage.".into(),
        };
    };
    let v = volts(mv);
    let (level, text) = if mv < VOLT_NO_POWER {
        (
            Level::Bad,
            format!(
                "{v} — below {}, which OpenTune treats as low or no car power. Check that the \
                 ignition is on.",
                volts(VOLT_NO_POWER)
            ),
        )
    } else if mv < VOLT_MIN_WRITE {
        (
            Level::Warn,
            format!(
                "{v} — below {}, the minimum OpenTune requires before writing to a control \
                 unit. Reading is still possible.",
                volts(VOLT_MIN_WRITE)
            ),
        )
    } else if mv < VOLT_RUNNING {
        (
            Level::Ok,
            format!(
                "{v} — in the usual range with the engine off ({}–{}).",
                volts(VOLT_MIN_WRITE),
                volts(VOLT_RUNNING)
            ),
        )
    } else if mv <= VOLT_HIGH {
        (
            Level::Ok,
            format!(
                "{v} — in the usual charging range with the engine running ({}–{}).",
                volts(VOLT_RUNNING),
                volts(VOLT_HIGH)
            ),
        )
    } else {
        (
            Level::Warn,
            format!(
                "{v} — above {}, higher than the usual charging range.",
                volts(VOLT_HIGH)
            ),
        )
    };
    Verdict {
        level,
        mv: Some(mv),
        text,
    }
}

// ---------------------------------------------------------------------------
// Connect
// ---------------------------------------------------------------------------

/// What the Connect step learned about the app's cable.
#[derive(Clone, Debug, PartialEq)]
pub struct ConnectProbe {
    pub driver: String,
    /// The driver's display name.
    pub name: String,
    pub port: String,
    /// Opening the cable on the target (adapter set-up only, nothing sent to
    /// a control unit).
    pub opened: Result<(), String>,
    pub battery_mv: Option<u32>,
    /// The adapter's capabilities, when known.
    pub caps: Option<Capabilities>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ConnectInfo {
    pub driver: String,
    pub name: String,
    pub port: String,
    pub battery_mv: Option<u32>,
    pub caps: Option<Capabilities>,
    pub voltage: Verdict,
}

/// Interpret the probe. `Err` (the raw text) when the step fails.
pub fn connect_result(p: ConnectProbe) -> Result<ConnectInfo, String> {
    p.opened?;
    if let Some(c) = &p.caps {
        if !(c.can || c.doip) {
            return Err(format!(
                "{} cannot carry diagnostics: no diagnostic link (CAN or DoIP)",
                if p.name.is_empty() {
                    &p.driver
                } else {
                    &p.name
                }
            ));
        }
    }
    Ok(ConnectInfo {
        voltage: voltage_verdict(p.battery_mv),
        driver: p.driver,
        name: p.name,
        port: p.port,
        battery_mv: p.battery_mv,
        caps: p.caps,
    })
}

// ---------------------------------------------------------------------------
// Identify
// ---------------------------------------------------------------------------

pub const VIN_MODE09: &str = "OBD mode 09";
pub const VIN_FALLBACK: &str = "OBD mode 09 / UDS 22 F190";

/// What the VIN itself says (`opentune-vin`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VinDecoded {
    pub manufacturer: Option<String>,
    pub country: Option<String>,
    pub model_year: Option<String>,
    pub flags: Vec<String>,
}

pub fn decode_vin(vin: &str) -> Option<VinDecoded> {
    let v = opentune_vin::decode_vin(vin).ok()?;
    let my = &v.model_year;
    let model_year = my.resolved.map(|y| y.to_string()).or_else(|| {
        (!my.candidates.is_empty()).then(|| {
            my.candidates
                .iter()
                .map(u16::to_string)
                .collect::<Vec<_>>()
                .join(" or ")
        })
    });
    Some(VinDecoded {
        manufacturer: v.wmi_info.manufacturer.map(str::to_string),
        country: v.wmi_info.country.map(str::to_string),
        model_year,
        flags: v.flags,
    })
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Identity {
    pub vin: Option<String>,
    pub vin_source: Option<&'static str>,
    pub ecu_name: Option<String>,
    pub calids: Vec<String>,
    pub cvns: Vec<String>,
    /// The module asked, `7E0/7E8`.
    pub target: Option<String>,
    pub responded: bool,
    pub decoded: Option<VinDecoded>,
    /// Why an item is missing.
    pub errors: BTreeMap<String, String>,
}

/// The identity from the engine computer's mode-09 answer (`ins`), with the
/// VIN fall-back read (`fallback_vin`) when mode 09 had none.
pub fn identity(
    ins: Option<&Inspection>,
    target: Option<String>,
    fallback_vin: Option<String>,
    decoded: Option<VinDecoded>,
) -> Identity {
    let info = ins.and_then(|i| i.info.as_ref());
    let mut vin = info.and_then(|i| i.vin.clone());
    let mut vin_source = vin.as_ref().map(|_| VIN_MODE09);
    if vin.is_none() {
        if let Some(v) = fallback_vin {
            vin = Some(v);
            vin_source = Some(VIN_FALLBACK);
        }
    }
    let mut errors = info.map(|i| i.errors.clone()).unwrap_or_default();
    if let Some(i) = ins {
        errors.extend(i.errors.clone());
    }
    Identity {
        vin,
        vin_source,
        ecu_name: info.and_then(|i| i.ecu_name.clone()),
        calids: info.map(|i| i.calids.clone()).unwrap_or_default(),
        cvns: info.map(|i| i.cvns.clone()).unwrap_or_default(),
        target: ins.and(target),
        responded: ins.is_some_and(|i| i.responded),
        decoded,
        errors,
    }
}

// ---------------------------------------------------------------------------
// Scan summary
// ---------------------------------------------------------------------------

/// One OBD module that answered.
#[derive(Clone, Debug, PartialEq)]
pub struct ObdEcu {
    pub tx: u32,
    pub rx: u32,
    pub ins: Inspection,
}

/// The OBD part of the scan: modules that answered, addresses that did not.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ObdScan {
    pub ecus: Vec<ObdEcu>,
    /// `7E2`, ...
    pub silent: Vec<String>,
}

/// The full module scan (Auto-scan over a module list).
#[derive(Clone, Debug, PartialEq)]
pub struct ModuleScan {
    pub vehicle: String,
    pub modules: Vec<ModuleScanResult>,
}

/// Where a code was seen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SeenKind {
    Stored,
    Pending,
    Permanent,
    /// In a module's own fault memory (full module scan).
    Module,
}

impl SeenKind {
    pub fn key(self) -> &'static str {
        match self {
            SeenKind::Stored => "stored",
            SeenKind::Pending => "pending",
            SeenKind::Permanent => "permanent",
            SeenKind::Module => "module",
        }
    }
    fn text(self) -> &'static str {
        match self {
            SeenKind::Stored => {
                "stored (confirmed) over OBD: the fault was detected often enough for the \
                 control unit to store it"
            }
            SeenKind::Pending => {
                "pending over OBD: the control unit saw it in the current or last drive cycle \
                 but has not confirmed it"
            }
            SeenKind::Permanent => {
                "permanent over OBD: it stays until the control unit itself sees the system \
                 pass again — clearing codes does not remove it"
            }
            SeenKind::Module => "stored in the module's fault memory",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    pub source: String,
    pub kind: SeenKind,
    /// The UDS status byte (module codes only).
    pub status: Option<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtcSummary {
    pub code: String,
    pub description: String,
    pub seen_as: Vec<Seen>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadinessSummary {
    pub source: String,
    pub complete: Vec<&'static str>,
    pub incomplete: Vec<&'static str>,
    pub unsupported: Vec<&'static str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModulesSummary {
    pub vehicle: String,
    pub total: usize,
    pub present: Vec<String>,
    pub absent: Vec<String>,
    /// `(module, error)` of present modules that could not be read fully.
    pub errors: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanSummary {
    /// `None`: no module reported its readiness status.
    pub mil: Option<bool>,
    pub mil_by: Vec<String>,
    pub readiness: Option<ReadinessSummary>,
    pub dtcs: Vec<DtcSummary>,
    pub modules: Option<ModulesSummary>,
    pub obd_modules: Vec<String>,
    pub silent: Vec<String>,
    pub notes: Vec<String>,
}

/// A part of the scan that could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    pub what: String,
    pub info: ErrorInfo,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScanData {
    pub summary: ScanSummary,
    pub problems: Vec<Problem>,
    pub modules_note: Option<String>,
}

/// `Engine (OBD 7E0)`.
pub fn obd_source(tx: u32) -> String {
    let label = match obd_ecu_label(tx) {
        "engine" => "Engine",
        "transmission" => "Transmission",
        _ => "Module",
    };
    format!("{label} (OBD {tx:03X})")
}

const UNKNOWN_DESC: [&str; 3] = ["manufacturer-specific", "unknown code", "not in the"];

fn unknown_desc(d: &str) -> bool {
    let d = d.to_lowercase();
    UNKNOWN_DESC.iter().any(|w| d.contains(w))
}

/// Merge the OBD and module scans: MIL, readiness, and every code once with
/// every place it was seen.
pub fn scan_summary(obd: Option<&ObdScan>, mods: Option<&ModuleScan>) -> ScanSummary {
    let mut by_code: BTreeMap<String, DtcSummary> = BTreeMap::new();
    let mut add = |code: &str, desc: &str, seen: Seen| {
        let code = code.trim().to_uppercase();
        if code.is_empty() {
            return;
        }
        let x = by_code.entry(code.clone()).or_insert_with(|| DtcSummary {
            code,
            description: desc.to_string(),
            seen_as: Vec::new(),
        });
        if x.description.is_empty() && !desc.is_empty() {
            x.description = desc.to_string();
        }
        if !x
            .seen_as
            .iter()
            .any(|s| s.source == seen.source && s.kind == seen.kind)
        {
            x.seen_as.push(seen);
        }
    };
    let ecus: &[ObdEcu] = obd.map(|o| o.ecus.as_slice()).unwrap_or_default();
    let mut mil_by = Vec::new();
    let mut mil_known = false;
    let mut notes = Vec::new();
    for e in ecus.iter().filter(|e| e.ins.responded) {
        let src = obd_source(e.tx);
        if let Some(r) = &e.ins.readiness {
            mil_known = true;
            if r.status.mil {
                mil_by.push(src.clone());
            }
        }
        if let Some(d) = &e.ins.dtcs {
            for (kind, list) in [
                (SeenKind::Stored, &d.stored),
                (SeenKind::Pending, &d.pending),
                (SeenKind::Permanent, &d.permanent),
            ] {
                for x in list {
                    add(
                        &x.code,
                        &x.description,
                        Seen {
                            source: src.clone(),
                            kind,
                            status: None,
                        },
                    );
                }
            }
            for (k, err) in &d.errors {
                notes.push(format!("{src}: {k} codes not read ({err})"));
            }
        }
    }
    // Readiness: the engine's view (it owns the emissions monitors).
    let eng = ecus
        .iter()
        .find(|e| e.ins.responded && obd_ecu_label(e.tx) == "engine" && e.ins.readiness.is_some())
        .or_else(|| {
            ecus.iter()
                .find(|e| e.ins.responded && e.ins.readiness.is_some())
        });
    let readiness = eng.and_then(|e| {
        let st = &e.ins.readiness.as_ref()?.status;
        let pick = |s: MonitorState| -> Vec<&'static str> {
            st.monitors
                .iter()
                .filter(|m| m.state == s)
                .map(|m| m.name)
                .collect()
        };
        Some(ReadinessSummary {
            source: obd_source(e.tx),
            complete: pick(MonitorState::Complete),
            incomplete: pick(MonitorState::Incomplete),
            unsupported: pick(MonitorState::NotSupported),
        })
    });
    let modules = mods.map(|m| {
        for r in &m.modules {
            for x in &r.dtcs {
                add(
                    &x.code,
                    &x.description,
                    Seen {
                        source: r.name.clone(),
                        kind: SeenKind::Module,
                        status: x.status,
                    },
                );
            }
        }
        ModulesSummary {
            vehicle: m.vehicle.clone(),
            total: m.modules.len(),
            present: m
                .modules
                .iter()
                .filter(|r| r.present)
                .map(|r| r.name.clone())
                .collect(),
            absent: m
                .modules
                .iter()
                .filter(|r| !r.present)
                .map(|r| r.name.clone())
                .collect(),
            errors: m
                .modules
                .iter()
                .filter(|r| r.present)
                .filter_map(|r| r.error.clone().map(|e| (r.name.clone(), e)))
                .collect(),
        }
    });
    let dtcs = by_code
        .into_values()
        .map(|mut x| {
            x.seen_as.sort_by_key(|s| s.kind);
            x
        })
        .collect();
    ScanSummary {
        mil: mil_known.then_some(!mil_by.is_empty()),
        mil_by,
        readiness,
        dtcs,
        modules,
        obd_modules: ecus
            .iter()
            .filter(|e| e.ins.responded)
            .map(|e| obd_source(e.tx))
            .collect(),
        silent: obd.map(|o| o.silent.clone()).unwrap_or_default(),
        notes,
    }
}

/// How many modules answered (OBD + module list).
pub fn answered(s: &ScanSummary) -> usize {
    s.obd_modules.len() + s.modules.as_ref().map_or(0, |m| m.present.len())
}

// ---------------------------------------------------------------------------
// Explain
// ---------------------------------------------------------------------------

/// ISO 14229-1 DTC status bits a reader can act on, in plain words.
const UDS_STATUS: [(u8, &str); 5] = [
    (0x01, "failing at the moment of the scan"),
    (
        0x04,
        "pending (seen in the current or last drive cycle, not confirmed yet)",
    ),
    (0x08, "confirmed and stored"),
    (0x20, "has failed since the codes were last cleared"),
    (0x80, "the module asks for a warning lamp"),
];

pub fn uds_status_text(status: Option<u8>) -> Vec<&'static str> {
    let Some(n) = status else {
        return Vec::new();
    };
    UDS_STATUS
        .iter()
        .filter(|(bit, _)| n & bit != 0)
        .map(|(_, t)| *t)
        .collect()
}

fn system_text(letter: char) -> Option<&'static str> {
    match letter.to_ascii_uppercase() {
        'P' => Some("Powertrain (engine / transmission / emissions)"),
        'C' => Some("Chassis (ABS / steering / suspension)"),
        'B' => Some("Body (airbags / lighting / HVAC / comfort)"),
        'U' => Some("Network / communication (CAN bus / modules)"),
        _ => None,
    }
}

pub const INCOMPLETE_TEXT: &str = "An incomplete monitor means the engine computer has not \
     finished that self-test since its fault memory was last cleared or the battery was \
     disconnected. It is not a fault by itself. Monitors finish on their own during normal \
     driving; some need particular driving conditions. Some emissions inspections require them \
     to be complete.";
pub const CODE_TEXT: &str = "A trouble code names the test that failed. It does not by itself \
     say which part is at fault.";

/// The DTC database's answer for a code (`opentune-dtc`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtcLookup {
    pub description: String,
    pub system: String,
}

pub fn lookup(code: &str) -> Option<DtcLookup> {
    let d = opentune_dtc::lookup(code)?;
    Some(DtcLookup {
        description: d.description,
        system: system_text(d.system.letter()).unwrap_or_default().into(),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKind {
    NoData,
    Mil,
    Dtc,
    Readiness,
    Modules,
    Note,
}

/// One plain-language explanation.
#[derive(Clone, Debug, PartialEq)]
pub struct ExplainItem {
    pub kind: ItemKind,
    pub level: Level,
    pub title: String,
    pub text: String,
    pub code: Option<String>,
    pub system: Option<String>,
    pub seen: Vec<String>,
    /// Monitors not finished.
    pub list: Vec<String>,
    pub extra: Option<String>,
}

impl ExplainItem {
    fn new(
        kind: ItemKind,
        level: Level,
        title: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            level,
            title: title.into(),
            text: text.into(),
            code: None,
            system: None,
            seen: Vec::new(),
            list: Vec::new(),
            extra: None,
        }
    }
}

/// The Explain step's items. `lookups` maps a code to the DTC database's
/// answer; its description wins over the scan's.
pub fn explain(
    summary: Option<&ScanSummary>,
    lookups: &BTreeMap<String, DtcLookup>,
    modules_note: Option<&str>,
) -> Vec<ExplainItem> {
    use ItemKind as K;
    let mut items = Vec::new();
    let Some(s) = summary else {
        items.push(ExplainItem::new(
            K::NoData,
            Level::Info,
            "Nothing to explain",
            "The scan was skipped, so there are no results to explain. Go back to the Scan \
             step to run it.",
        ));
        return items;
    };
    items.push(match s.mil {
        Some(true) => ExplainItem::new(
            K::Mil,
            Level::Bad,
            "Check-engine lamp (MIL): ON",
            format!(
                "{} reports that it is commanding the check-engine lamp on.",
                s.mil_by.join(", ")
            ),
        ),
        Some(false) => ExplainItem::new(
            K::Mil,
            Level::Ok,
            "Check-engine lamp (MIL): off",
            "No module that answered over OBD reports the check-engine lamp on.",
        ),
        None => ExplainItem::new(
            K::Mil,
            Level::Info,
            "Check-engine lamp (MIL): not read",
            "No module reported its readiness status, so the lamp state is unknown.",
        ),
    });
    if s.dtcs.is_empty() {
        let where_ = s.obd_modules.len() + s.modules.as_ref().map_or(0, |m| m.present.len());
        items.push(if where_ > 0 {
            ExplainItem::new(
                K::Dtc,
                Level::Ok,
                "No trouble codes",
                "None of the modules that answered reported a trouble code.",
            )
        } else {
            ExplainItem::new(
                K::Dtc,
                Level::Info,
                "No trouble codes",
                "No module answered, so no trouble codes were read.",
            )
        });
    }
    for d in &s.dtcs {
        let db = lookups.get(&d.code);
        let desc = db
            .map(|x| x.description.as_str())
            .filter(|x| !x.is_empty())
            .unwrap_or(&d.description);
        let unknown = desc.is_empty() || unknown_desc(desc);
        let sys = db
            .map(|x| x.system.clone())
            .filter(|x| !x.is_empty())
            .or_else(|| {
                d.code
                    .chars()
                    .next()
                    .and_then(system_text)
                    .map(str::to_string)
            });
        let seen = d
            .seen_as
            .iter()
            .map(|x| {
                let flags = if x.kind == SeenKind::Module {
                    uds_status_text(x.status)
                } else {
                    Vec::new()
                };
                let tail = match x.status {
                    Some(st) if !flags.is_empty() => {
                        format!(" — status 0x{st:02X}: {}", flags.join(", "))
                    }
                    Some(st) => format!(" — status 0x{st:02X}"),
                    None => String::new(),
                };
                format!("{}: {}{tail}", x.source, x.kind.text())
            })
            .collect();
        let pending_only = d.seen_as.iter().all(|x| x.kind == SeenKind::Pending);
        let mut it = ExplainItem::new(
            K::Dtc,
            if pending_only {
                Level::Warn
            } else {
                Level::Bad
            },
            format!(
                "{} — {}",
                d.code,
                if unknown {
                    "no description in OpenTune's code table"
                } else {
                    desc
                }
            ),
            format!(
                "{}{CODE_TEXT}",
                if unknown {
                    "OpenTune's code table has no description for this code (it is \
                     manufacturer-specific or unknown). "
                } else {
                    ""
                }
            ),
        );
        it.code = Some(d.code.clone());
        it.system = sys;
        it.seen = seen;
        items.push(it);
    }
    match &s.readiness {
        Some(r) => {
            let finished = |r: &ReadinessSummary| format!("Finished: {}.", r.complete.join(", "));
            if !r.incomplete.is_empty() {
                let mut it = ExplainItem::new(
                    K::Readiness,
                    Level::Warn,
                    format!("Readiness: {} monitor(s) not finished", r.incomplete.len()),
                    INCOMPLETE_TEXT,
                );
                it.list = r.incomplete.iter().map(|m| m.to_string()).collect();
                it.extra = (!r.complete.is_empty()).then(|| finished(r));
                items.push(it);
            } else {
                items.push(ExplainItem::new(
                    K::Readiness,
                    Level::Ok,
                    "Readiness: every supported monitor has finished",
                    if r.complete.is_empty() {
                        "The engine computer reports no supported monitors.".to_string()
                    } else {
                        finished(r)
                    },
                ));
            }
            if !r.unsupported.is_empty() {
                items.push(ExplainItem::new(
                    K::Readiness,
                    Level::Info,
                    "Monitors this car does not run",
                    format!(
                        "Not supported by this engine computer, so they are not checked: {}.",
                        r.unsupported.join(", ")
                    ),
                ));
            }
        }
        None => items.push(ExplainItem::new(
            K::Readiness,
            Level::Info,
            "Readiness: not read",
            "No module reported readiness monitors.",
        )),
    }
    if let Some(m) = &s.modules {
        items.push(ExplainItem::new(
            K::Modules,
            if m.absent.is_empty() {
                Level::Ok
            } else {
                Level::Info
            },
            format!("Modules: {} of {} answered", m.present.len(), m.total),
            if m.absent.is_empty() {
                "Every module in the list answered.".to_string()
            } else {
                format!(
                    "Did not answer: {}. A module that is not fitted, or is on the other CAN \
                     bus, does not answer.",
                    m.absent.join(", ")
                )
            },
        ));
    } else if let Some(n) = modules_note {
        items.push(ExplainItem::new(
            K::Modules,
            Level::Info,
            "Full module scan: not run",
            n,
        ));
    }
    for n in &s.notes {
        items.push(ExplainItem::new(
            K::Note,
            Level::Info,
            "Not read",
            n.clone(),
        ));
    }
    items
}

/// The Explain step from the scan step's result (lookups from the bundled
/// DTC table).
pub fn explain_run(run: &Run) -> Vec<ExplainItem> {
    let sc = run.scan();
    let mut lookups = BTreeMap::new();
    if let Some(sc) = sc {
        for d in &sc.summary.dtcs {
            if let Some(l) = lookup(&d.code) {
                lookups.insert(d.code.clone(), l);
            }
        }
    }
    explain(
        sc.map(|s| &s.summary),
        &lookups,
        sc.and_then(|s| s.modules_note.as_deref()),
    )
}

// ---------------------------------------------------------------------------
// Live snapshot
// ---------------------------------------------------------------------------

/// Key mode-01 values; ratings use only the channel table's own warn / crit.
pub const LIVE_KEYS: [&str; 6] = [
    "coolant_temp",
    "rpm",
    "speed",
    "stft_b1",
    "ltft_b1",
    "module_voltage",
];
pub const LIVE_SAMPLES: usize = 6;
pub const LIVE_GAP_MS: u64 = 500;
pub const NO_LIVE_VALUES: &str = "the engine computer reports none of the values this check \
     reads (coolant, engine speed, fuel trims, voltage)";

/// The PIDs to read: the live keys' PIDs the ECU supports (all of them while
/// support is unknown).
pub fn live_pids(supported: Option<&BTreeSet<u8>>) -> Vec<u8> {
    let mut pids = BTreeSet::new();
    for k in LIVE_KEYS {
        if let Some(c) = obd::channel(k) {
            if !c.is_derived() && supported.is_none_or(|s| s.contains(&c.pid)) {
                pids.insert(c.pid);
            }
        }
    }
    pids.into_iter().collect()
}

/// One reading of the live keys.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sample {
    /// Requested keys (`None`: no value this time).
    pub values: BTreeMap<String, Option<f64>>,
    pub errors: BTreeMap<String, String>,
    pub battery_mv: Option<u32>,
}

impl Sample {
    pub fn from_readout(ro: &Readout, battery_mv: Option<u32>) -> Self {
        Self {
            values: ro
                .values
                .iter()
                .filter(|(k, _)| LIVE_KEYS.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.as_ref().and_then(|v| v.first())))
                .collect(),
            errors: ro
                .errors
                .iter()
                .filter(|(k, _)| LIVE_KEYS.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            battery_mv,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LiveRow {
    pub key: &'static str,
    pub name: &'static str,
    pub unit: &'static str,
    pub warn: Option<f64>,
    pub crit: Option<f64>,
    /// Readings with a value.
    pub n: usize,
    pub last: Option<f64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub level: Level,
    pub note: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdleState {
    Unknown,
    Off,
    Moving,
    Idle,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Idle {
    pub state: IdleState,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LiveSummary {
    pub samples: usize,
    pub rows: Vec<LiveRow>,
    pub battery: Verdict,
    pub idle: Idle,
}

/// Where the live readings came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiveSource {
    /// The wizard's own short reads.
    Reads,
    /// Another panel's polling, through the live feed (the adapter was busy).
    Feed(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct LiveData {
    pub pids: Vec<u8>,
    pub summary: LiveSummary,
    /// Ended early (stopped by the user).
    pub stopped: bool,
    pub source: LiveSource,
}

fn rate(warn: Option<f64>, crit: Option<f64>, v: f64) -> Level {
    if warn.is_none() && crit.is_none() {
        return Level::Unrated;
    }
    if crit.is_some_and(|c| v >= c) {
        return Level::Crit;
    }
    if warn.is_some_and(|w| v >= w) {
        return Level::Warn;
    }
    Level::Ok
}

/// A value with the unit's decimals (V: 2, %: 1, else 0); `—` for none.
pub fn fmt_val(v: Option<f64>, unit: &str) -> String {
    let Some(v) = v else {
        return "\u{2014}".into();
    };
    let dp = match unit {
        "V" => 2,
        "%" => 1,
        _ => 0,
    };
    let s = format!("{v:.dp$}");
    // No "-0" for a value that rounds to zero.
    if s.starts_with('-') && s[1..].chars().all(|c| c == '0' || c == '.') {
        s[1..].to_string()
    } else {
        s
    }
}

/// A threshold as written in the channel table (`105`, `0.5`).
fn num(v: f64) -> String {
    format!("{v}")
}

pub fn live_summary(samples: &[Sample]) -> LiveSummary {
    let mut rows = Vec::new();
    for k in LIVE_KEYS {
        let Some(t) = obd::channel(k) else {
            continue;
        };
        if !samples.iter().any(|s| s.values.contains_key(k)) {
            continue;
        }
        let vals: Vec<f64> = samples
            .iter()
            .filter_map(|s| s.values.get(k).copied().flatten())
            .collect();
        let err = samples.iter().find_map(|s| s.errors.get(k));
        let mut row = LiveRow {
            key: t.key,
            name: t.name,
            unit: t.unit,
            warn: t.warn,
            crit: t.crit,
            n: vals.len(),
            last: None,
            min: None,
            max: None,
            level: Level::Unrated,
            note: String::new(),
        };
        if vals.is_empty() {
            row.note = match err {
                Some(e) => format!("Not reported: {e}."),
                None => "Not reported by this ECU.".into(),
            };
        } else {
            let level =
                vals.iter()
                    .map(|&v| rate(t.warn, t.crit, v))
                    .fold(
                        Level::Unrated,
                        |a, b| if b.worse() > a.worse() { b } else { a },
                    );
            let max = vals.iter().copied().fold(f64::MIN, f64::max);
            row.last = vals.last().copied();
            row.min = Some(vals.iter().copied().fold(f64::MAX, f64::min));
            row.max = Some(max);
            row.level = level;
            let u = t.unit;
            row.note = if t.warn.is_none() && t.crit.is_none() {
                "OpenTune has no threshold for this value, so it is shown without a rating.".into()
            } else if level == Level::Crit {
                format!(
                    "Reached {} {u}, at or above the critical mark of {} {u}.",
                    fmt_val(Some(max), u),
                    num(t.crit.unwrap_or_default())
                )
            } else if level == Level::Warn {
                format!(
                    "Reached {} {u}, at or above the warning mark of {} {u}.",
                    fmt_val(Some(max), u),
                    num(t.warn.unwrap_or_default())
                )
            } else {
                format!(
                    "Below the warning mark of {} {u}.",
                    num(t.warn.or(t.crit).unwrap_or_default())
                )
            };
        }
        rows.push(row);
    }
    let battery = voltage_verdict(samples.iter().rev().find_map(|s| s.battery_mv));
    let row = |k: &str| rows.iter().find(|r| r.key == k);
    let idle = match (row("rpm"), row("speed")) {
        (Some(rpm), speed) if rpm.last.is_some() => {
            if rpm.max == Some(0.0) {
                Idle {
                    state: IdleState::Off,
                    text: "The engine was not running (0 rpm). Start the engine and run the live \
                           check again for an idle reading."
                        .into(),
                }
            } else if let Some(sp) = speed.and_then(|s| s.max).filter(|&m| m > 0.0) {
                Idle {
                    state: IdleState::Moving,
                    text: format!(
                        "The car was moving (up to {} km/h), so this is not an idle reading.",
                        fmt_val(Some(sp), "km/h")
                    ),
                }
            } else {
                Idle {
                    state: IdleState::Idle,
                    text: format!(
                        "Idle speed {}–{} rpm over {} reading(s). OpenTune has no idle \
                         threshold, so it is shown without a rating.",
                        fmt_val(rpm.min, "rpm"),
                        fmt_val(rpm.max, "rpm"),
                        rpm.n
                    ),
                }
            }
        }
        _ => Idle {
            state: IdleState::Unknown,
            text: "Engine speed was not reported, so there is no idle reading.".into(),
        },
    };
    LiveSummary {
        samples: samples.len(),
        rows,
        battery,
        idle,
    }
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

pub const DISCLAIMER: &str = "Read-only check: nothing was written to the car and no codes \
     were cleared. OpenTune reports what the car's control units returned; it does not \
     diagnose causes.";

/// How a report is made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportMeta {
    pub now_ms: u64,
    pub app: String,
    /// Put the VIN in the report (off by default: a shared report then does
    /// not identify the car).
    pub include_vin: bool,
}

/// One neutral block both renderers walk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    P(String),
    Kv(Vec<(String, String)>),
    List(Vec<String>),
    Table {
        head: Vec<String>,
        rows: Vec<Vec<String>>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    pub id: StepId,
    pub title: &'static str,
    pub status: Status,
    pub error: Option<ErrorInfo>,
    pub blocks: Vec<Block>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportModel {
    pub title: &'static str,
    pub when: String,
    pub app: String,
    /// The VIN, only when the report includes it.
    pub vehicle: Option<String>,
    pub disclaimer: &'static str,
    pub sections: Vec<Section>,
}

/// `(year, month, day, hour, minute)` in UTC of a Unix time in ms.
pub fn utc(ms: u64) -> (i64, u32, u32, u32, u32) {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    // Civil date from days since 1970-01-01 (H. Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d, (sod / 3600) as u32, ((sod % 3600) / 60) as u32)
}

fn stamp(ms: u64) -> String {
    let (y, mo, d, h, mi) = utc(ms);
    format!("{y}-{mo:02}-{d:02} {h:02}:{mi:02} UTC")
}

fn kv(k: &str, v: impl Into<String>) -> (String, String) {
    (k.to_string(), v.into())
}

fn names(v: &[&str]) -> String {
    if v.is_empty() {
        "none".into()
    } else {
        v.join(", ")
    }
}

fn identify_blocks(i: &Identity, include_vin: bool) -> Vec<Block> {
    let vin = match (&i.vin, include_vin) {
        (Some(v), true) => format!("{v} ({})", i.vin_source.unwrap_or(VIN_MODE09)),
        (Some(_), false) => "read from the car (left out of this report)".into(),
        (None, _) => "not reported".into(),
    };
    let mut rows = vec![kv("VIN", vin)];
    if let Some(d) = &i.decoded {
        if let Some(m) = &d.manufacturer {
            rows.push(kv("Manufacturer (from the VIN)", m.clone()));
        }
        if let Some(y) = &d.model_year {
            rows.push(kv("Model year (from the VIN)", y.clone()));
        }
    }
    if let Some(n) = &i.ecu_name {
        rows.push(kv("Engine computer name", n.clone()));
    }
    if !i.calids.is_empty() {
        rows.push(kv("Calibration IDs", i.calids.join(", ")));
    }
    if !i.cvns.is_empty() {
        rows.push(kv("Calibration verification numbers", i.cvns.join(", ")));
    }
    if let Some(t) = &i.target {
        rows.push(kv("Asked", t.clone()));
    }
    vec![Block::Kv(rows)]
}

fn scan_blocks(sc: &ScanData) -> Vec<Block> {
    let sm = &sc.summary;
    let mut rows = vec![
        kv(
            "Check-engine lamp (MIL)",
            match sm.mil {
                None => "not read".into(),
                Some(true) => format!("ON ({})", sm.mil_by.join(", ")),
                Some(false) => "off".into(),
            },
        ),
        kv("Trouble codes", sm.dtcs.len().to_string()),
        kv(
            "OBD modules that answered",
            if sm.obd_modules.is_empty() {
                "none".into()
            } else {
                sm.obd_modules.join(", ")
            },
        ),
    ];
    if let Some(m) = &sm.modules {
        rows.push(kv(
            "Module list",
            format!(
                "{} — {} of {} answered",
                m.vehicle,
                m.present.len(),
                m.total
            ),
        ));
    } else if let Some(n) = &sc.modules_note {
        rows.push(kv("Module list", n.clone()));
    }
    let mut b = vec![Block::Kv(rows)];
    if !sm.dtcs.is_empty() {
        b.push(Block::Table {
            head: vec!["Code".into(), "Description".into(), "Seen as".into()],
            rows: sm
                .dtcs
                .iter()
                .map(|d| {
                    vec![
                        d.code.clone(),
                        d.description.clone(),
                        d.seen_as
                            .iter()
                            .map(|s| {
                                let st =
                                    s.status.map(|x| format!(" 0x{x:02X}")).unwrap_or_default();
                                format!("{}: {}{st}", s.source, s.kind.key())
                            })
                            .collect::<Vec<_>>()
                            .join("; "),
                    ]
                })
                .collect(),
        });
    }
    if let Some(r) = &sm.readiness {
        b.push(Block::Kv(vec![
            kv("Monitors finished", names(&r.complete)),
            kv("Monitors not finished", names(&r.incomplete)),
        ]));
    }
    for p in &sc.problems {
        b.push(Block::P(format!(
            "Not read: {} ({})",
            p.info.text, p.info.raw
        )));
    }
    b
}

fn explain_blocks(items: &[ExplainItem]) -> Vec<Block> {
    items
        .iter()
        .map(|it| {
            let mut l = vec![format!("{}. {}", it.title, it.text)];
            if let Some(s) = &it.system {
                l.push(format!("System: {s}"));
            }
            l.extend(it.seen.iter().cloned());
            if !it.list.is_empty() {
                l.push(format!("Not finished: {}", it.list.join(", ")));
            }
            if let Some(e) = &it.extra {
                l.push(e.clone());
            }
            Block::List(l)
        })
        .collect()
}

fn live_blocks(lv: &LiveData) -> Vec<Block> {
    let s = &lv.summary;
    let mut kvs = vec![
        kv("Battery", s.battery.text.clone()),
        kv("Idle", s.idle.text.clone()),
        kv("Readings", s.samples.to_string()),
    ];
    if let LiveSource::Feed(src) = &lv.source {
        kvs.push(kv(
            "Source",
            format!("the {src} panel's live values (the adapter was in use)"),
        ));
    }
    vec![
        Block::Table {
            head: ["Value", "Last", "Min – max", "Rating", "Note"]
                .map(String::from)
                .to_vec(),
            rows: s
                .rows
                .iter()
                .map(|r| {
                    let has = r.last.is_some();
                    vec![
                        r.name.to_string(),
                        if has {
                            format!("{} {}", fmt_val(r.last, r.unit), r.unit)
                        } else {
                            "\u{2014}".into()
                        },
                        if has {
                            format!(
                                "{} – {} {}",
                                fmt_val(r.min, r.unit),
                                fmt_val(r.max, r.unit),
                                r.unit
                            )
                        } else {
                            "\u{2014}".into()
                        },
                        r.level.key().into(),
                        r.note.clone(),
                    ]
                })
                .collect(),
        },
        Block::Kv(kvs),
    ]
}

pub fn report_model(run: &Run, meta: &ReportMeta) -> ReportModel {
    let sec = |id: StepId, blocks: &dyn Fn() -> Vec<Block>| {
        let st = run.step(id);
        Section {
            id,
            title: id.title(),
            status: st.status,
            error: st.error.clone(),
            blocks: if st.status == Status::Done {
                blocks()
            } else {
                Vec::new()
            },
        }
    };
    let none = Vec::<Block>::new;
    ReportModel {
        title: "OpenTune car check",
        when: stamp(meta.now_ms),
        app: meta.app.clone(),
        vehicle: run
            .identify()
            .and_then(|i| i.vin.clone())
            .filter(|_| meta.include_vin),
        disclaimer: DISCLAIMER,
        sections: vec![
            sec(StepId::Connect, &|| match run.connect() {
                Some(c) => vec![Block::Kv(vec![
                    kv("Adapter", c.name.clone()),
                    kv("Battery", c.voltage.text.clone()),
                ])],
                None => none(),
            }),
            sec(StepId::Identify, &|| match run.identify() {
                Some(i) => identify_blocks(i, meta.include_vin),
                None => none(),
            }),
            sec(StepId::Scan, &|| {
                run.scan().map(scan_blocks).unwrap_or_default()
            }),
            sec(StepId::Explain, &|| {
                run.explain().map(explain_blocks).unwrap_or_default()
            }),
            sec(StepId::Live, &|| {
                run.live().map(live_blocks).unwrap_or_default()
            }),
        ],
    }
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            c => o.push(c),
        }
    }
    o
}

fn md_esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '`' | '*' | '_' | '[' | ']' | '|' | '<' | '>') {
            o.push('\\');
        }
        o.push(c);
    }
    o
}

pub fn report_markdown(run: &Run, meta: &ReportMeta) -> String {
    let m = report_model(run, meta);
    let mut out: Vec<String> = vec![
        format!("# {}", m.title),
        String::new(),
        format!(
            "{} · {}{}",
            m.when,
            m.app,
            m.vehicle
                .as_ref()
                .map(|v| format!(" · VIN {v}"))
                .unwrap_or_default()
        ),
        String::new(),
        format!("> {}", m.disclaimer),
        String::new(),
    ];
    for s in &m.sections {
        out.push(format!("## {} — {}", s.title, s.status.text()));
        out.push(String::new());
        if let (Status::Failed, Some(e)) = (s.status, &s.error) {
            out.push(md_esc(&e.text));
            out.push(String::new());
            out.push(format!("Detail: `{}`", e.raw.replace('`', "'")));
            out.push(String::new());
        }
        if s.status == Status::Skipped {
            out.push("This step was skipped.".into());
            out.push(String::new());
        }
        for b in &s.blocks {
            match b {
                Block::P(p) => {
                    out.push(md_esc(p));
                    out.push(String::new());
                }
                Block::Kv(rows) => {
                    for (k, v) in rows {
                        out.push(format!("- **{}:** {}", md_esc(k), md_esc(v)));
                    }
                    out.push(String::new());
                }
                Block::List(l) => {
                    for x in l {
                        out.push(format!("- {}", md_esc(x)));
                    }
                    out.push(String::new());
                }
                Block::Table { head, rows } => {
                    let line = |r: &[String]| {
                        format!(
                            "| {} |",
                            r.iter().map(|x| md_esc(x)).collect::<Vec<_>>().join(" | ")
                        )
                    };
                    out.push(line(head));
                    out.push(format!("|{}", " --- |".repeat(head.len())));
                    for r in rows {
                        out.push(line(r));
                    }
                    out.push(String::new());
                }
            }
        }
    }
    out.join("\n")
}

const REPORT_CSS: &str = "body{font:14px/1.45 system-ui,sans-serif;color:#1d232b;background:#fff;\
     max-width:880px;margin:24px auto;padding:0 16px}\
     h1{font-size:22px;margin:0 0 4px}h2{font-size:16px;margin:22px 0 8px;border-bottom:1px solid \
     #d9dde3;padding-bottom:4px}\
     .meta{color:#5b6573}.note{background:#f3f5f8;border-left:3px solid #8a96a8;padding:8px \
     12px;margin:12px 0}\
     .st{font-size:12px;font-weight:600;padding:1px 8px;border-radius:10px;background:#e9edf2;\
     margin-left:6px}\
     .st.done{background:#e3f4e8;color:#1c6b37}.st.failed{background:#fbe5e5;color:#9b1c1c}\
     table{border-collapse:collapse;width:100%;margin:6px 0 10px}th,td{border:1px solid \
     #d9dde3;padding:4px 8px;text-align:left;vertical-align:top}\
     dl{display:grid;grid-template-columns:max-content 1fr;gap:4px 14px;margin:6px 0 10px}\
     dt{color:#5b6573}dd{margin:0}code{font-size:12px}";

/// The report as HTML; `full` wraps it in a standalone document (no script).
pub fn report_html(run: &Run, meta: &ReportMeta, full: bool) -> String {
    let m = report_model(run, meta);
    let vin_meta = m
        .vehicle
        .as_ref()
        .map(|v| format!(" · VIN {}", esc(v)))
        .unwrap_or_default();
    let mut h = format!(
        "<h1>{}</h1><div class=\"meta\">{} · {}{vin_meta}</div><div class=\"note\">{}</div>",
        esc(m.title),
        esc(&m.when),
        esc(&m.app),
        esc(m.disclaimer)
    );
    for s in &m.sections {
        h += &format!(
            "<h2>{}<span class=\"st {}\">{}</span></h2>",
            esc(s.title),
            s.status.key(),
            esc(s.status.text())
        );
        if let (Status::Failed, Some(e)) = (s.status, &s.error) {
            h += &format!(
                "<p>{}</p><p class=\"meta\">Detail: <code>{}</code></p>",
                esc(&e.text),
                esc(&e.raw)
            );
        }
        match s.status {
            Status::Skipped => h += "<p class=\"meta\">This step was skipped.</p>",
            Status::Pending => h += "<p class=\"meta\">This step was not run.</p>",
            _ => {}
        }
        for b in &s.blocks {
            match b {
                Block::P(p) => h += &format!("<p>{}</p>", esc(p)),
                Block::Kv(rows) => {
                    h += "<dl>";
                    for (k, v) in rows {
                        h += &format!("<dt>{}</dt><dd>{}</dd>", esc(k), esc(v));
                    }
                    h += "</dl>";
                }
                Block::List(l) => {
                    h += "<ul>";
                    for x in l {
                        h += &format!("<li>{}</li>", esc(x));
                    }
                    h += "</ul>";
                }
                Block::Table { head, rows } => {
                    h += "<table><thead><tr>";
                    for x in head {
                        h += &format!("<th>{}</th>", esc(x));
                    }
                    h += "</tr></thead><tbody>";
                    for r in rows {
                        h += "<tr>";
                        for x in r {
                            h += &format!("<td>{}</td>", esc(x));
                        }
                        h += "</tr>";
                    }
                    h += "</tbody></table>";
                }
            }
        }
    }
    if !full {
        return h;
    }
    let title_vin = m
        .vehicle
        .as_ref()
        .map(|v| format!(" — {}", esc(v)))
        .unwrap_or_default();
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" \
         content=\"width=device-width, initial-scale=1\"><title>{}{title_vin}</title><style>{REPORT_CSS}\
         </style></head><body>{h}</body></html>\n",
        esc(m.title)
    )
}

/// `opentune-car-check-2026-01-02.html`; with the VIN included, its last six
/// letters / digits are added.
pub fn report_file_name(run: &Run, ext: &str, meta: &ReportMeta) -> String {
    let (y, m, d, ..) = utc(meta.now_ms);
    let tail = match run.identify().and_then(|i| i.vin.as_ref()) {
        Some(v) if meta.include_vin => {
            let clean: Vec<char> = v.chars().filter(char::is_ascii_alphanumeric).collect();
            let from = clean.len().saturating_sub(6);
            format!("-{}", clean[from..].iter().collect::<String>())
        }
        _ => String::new(),
    };
    format!("opentune-car-check-{y}-{m:02}-{d:02}{tail}.{ext}")
}
