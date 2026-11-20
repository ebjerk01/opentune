//! Service functions: plan (dry run) and run data-defined service scripts
//! over [`opentune_servicefn`] (port of the webview app's Service functions
//! panel, `feat_service.rs`).
//!
//! A script is data, so the panel classifies it by the step KINDS it
//! contains, with this fixed, written-down table (the strictest kind present
//! decides the script's class):
//!
//! | Step kind | SID | Class |
//! |---|---|---|
//! | `diagnostic_session`, `security_access`, `read_data_by_identifier`, `verify`, `tester_present`, `wait`, `expect_positive`, `obd_request` 01/03/07/09/0A | `10` `27` `22` `3E` OBD reads | read |
//! | `io_control` | `2F` | output test (non-persistent) |
//! | `routine_control` 03 (results) | `31 03` | output test (non-persistent; `31` is not a read service) |
//! | `obd_request` 04 | OBD `04` | DTC clear |
//! | `write_data_by_identifier`, `routine_control` 01 / 02 | `2E`, `31 01/02` | data-changing (persistent) |
//!
//! Precedence: data-changing > DTC clear > output test > read.
//!
//! ## Per-op table (reviewed before merge)
//!
//! | Operation | SIDs sent (in order) | Path | Backup source |
//! |---|---|---|---|
//! | Plan (dry run) | none (no adapter touched) | offline | n/a |
//! | Run a READ script | the script's steps (all on `READ_SERVICES`) | `read` (the script's module, else the app target) | n/a (read only) |
//! | Run an OUTPUT-TEST script | 1. confirm (`Host::confirm_ecu`); 2. run: the script's steps; restore (always): `2F <did> 00` for every IO DID the run may have left driven (all of them if the run reported nothing), then `10 01` when the script opened a session | `actuate` (the `Ack`'s module) | n/a (non-persistent) |
//! | Run a DTC-CLEAR script | 1. read: OBD `01 00`, `03` (the stored codes) → a NEW DTC report file (panel record); 2. confirm (typed `CLEAR`, "clears stored fault codes — not reversible"); 3. run: `01 00`, `03` again (must equal the report, else NOTHING is sent further), then the script's steps (incl. `04`); restore as an output-test script | `read` → `actuate` | n/a (not reversible; the report is the record) |
//! | Run a DATA-CHANGING script — 1. confirm | none | `Host::confirm_ecu` (typed `WRITE`) | — |
//! | 2. pre-read | `22 <did>` for every DID the script writes, verifies or reads (in step order, each once) | `read` (the confirmed module) | — |
//! | 3. backup | none | `Host::backup` (refused when empty: a script that names NO DID is refused, nothing sent) | the step-2 records, concatenated in that order (label lists DID:len) |
//! | 4. run | `22 <did>` per backed-up DID (must equal the backup, else NOTHING is written); the script's steps (stops at the first failure); then always `2F <did> 00` for any IO DID it may have left driven, `10 01` when it opened a session. A script with `04` also gets the DTC report of the DTC-clear row first | `mutate` (the `Ack`'s module; receipt must match the step-2 bytes) | as step 3 |
//!
//! Not ported: scripts that need SecurityAccess (the executor computes the
//! key through a `KeyProvider`; `Host::unlocker` cannot hand one over). They
//! can be planned, and running them is refused before anything is sent.

use super::coding::plumbing as pl;
use crate::host::{
    Ack, Actuation, AdapterAccess, BackupReceipt, ConfirmRequest, ConfirmResult, DiagTarget, Host,
    JobHandle, NativeView, Tier, ViewGroup,
};
use crate::icons;
use crate::theme::{self, Tone};
use crate::widgets::packs::{PackKind, PackPicker};
use eframe::egui;
use opentune_autoscan::{scan_module, Dtc, ModuleAddr, Protocol};
use opentune_core::RawDiag;
use opentune_servicefn::{
    run_script, RunOptions, RunOutcome, RunReport, ServiceError, ServiceScript, Step as SStep,
    StepStatus,
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Registration tier, read before the view is built ([`Tier::Bench`]: only
/// alpha / bench builds register it).
pub const TIER: Tier = Tier::Bench;

pub fn create() -> Box<dyn NativeView> {
    Box::new(ServiceView::default())
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

/// A script's class (see the table in the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Class {
    Read,
    Output,
    Clear,
    Data,
}

fn step_class(s: &SStep) -> Class {
    match s {
        SStep::WriteDataByIdentifier { .. } => Class::Data,
        SStep::RoutineControl { control_type, .. } if *control_type != 0x03 => Class::Data,
        SStep::RoutineControl { .. } | SStep::IoControl { .. } => Class::Output,
        SStep::ObdRequest { mode: 0x04, .. } => Class::Clear,
        _ => Class::Read,
    }
}

pub(crate) fn classify(s: &ServiceScript) -> Class {
    s.steps.iter().map(step_class).max().unwrap_or(Class::Read)
}

fn has_clear(s: &ServiceScript) -> bool {
    s.steps
        .iter()
        .any(|x| matches!(x, SStep::ObdRequest { mode: 0x04, .. }))
}

fn needs_key(s: &ServiceScript) -> bool {
    s.requires_security || s.has_security_step()
}

/// The DIDs a data-changing script's backup covers: every DID it writes,
/// verifies or reads, in step order, each once.
pub(crate) fn backup_dids(s: &ServiceScript) -> Vec<u16> {
    let mut v = Vec::new();
    for st in &s.steps {
        if let SStep::WriteDataByIdentifier { did, .. }
        | SStep::Verify { did, .. }
        | SStep::ReadDataByIdentifier { did } = st
        {
            if !v.contains(did) {
                v.push(*did);
            }
        }
    }
    v
}

/// The IO DIDs the script drives (a control other than return-control).
fn driven_dids(s: &ServiceScript) -> Vec<u16> {
    let mut v = Vec::new();
    for st in &s.steps {
        if let SStep::IoControl { did, control, .. } = st {
            if *control != 0x00 && !v.contains(did) {
                v.push(*did);
            }
        }
    }
    v
}

/// The IO DIDs a finished run may have left driven: driven by a step that
/// was sent (whatever its answer) and not returned by a later acknowledged
/// return-control step.
fn left_driven(s: &ServiceScript, rep: &RunReport) -> Vec<u16> {
    let mut v: Vec<u16> = Vec::new();
    for sr in &rep.steps {
        let Some(SStep::IoControl { did, control, .. }) = s.steps.get(sr.index) else {
            continue;
        };
        if *control != 0x00 && sr.request.is_some() {
            if !v.contains(did) {
                v.push(*did);
            }
        } else if *control == 0x00 && sr.status == StepStatus::Ok {
            v.retain(|d| d != did);
        }
    }
    v
}

fn opens_session(s: &ServiceScript) -> bool {
    s.steps
        .iter()
        .any(|x| matches!(x, SStep::DiagnosticSession { subfunction } if *subfunction != 0x01))
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct Loaded {
    pub script: ServiceScript,
    pub module: Option<DiagTarget>,
}

pub(crate) fn load(path: &Path) -> Result<Loaded, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("read service script {}: {e}", path.display()))?;
    let script =
        ServiceScript::from_json(&text).map_err(|e| format!("invalid service script: {e}"))?;
    script.validate().map_err(|e| e.to_string())?;
    Ok(Loaded {
        script,
        module: pl::def_module(&text)?,
    })
}

/// The dry run: validates and lists the steps; sends nothing (a transport
/// that refuses every request).
pub(crate) fn plan(s: &ServiceScript) -> Result<Vec<String>, String> {
    let mut none =
        |_r: &[u8]| -> Result<Vec<u8>, String> { Err("dry run: nothing is sent".into()) };
    let opts = RunOptions {
        dry_run: true,
        skip_waits: true,
    };
    match run_script(&mut none, s, None, &opts) {
        Ok(r) => Ok(r.steps.iter().map(|x| x.op.clone()).collect()),
        Err(ServiceError::MissingKeyProvider) => Ok(s
            .steps
            .iter()
            .map(|x| format!("{} (plan only: needs SecurityAccess)", x.label()))
            .collect()),
        Err(e) => Err(e.to_string()),
    }
}

fn exec(d: &mut dyn RawDiag, s: &ServiceScript) -> Result<RunReport, String> {
    let opts = RunOptions {
        dry_run: false,
        skip_waits: false,
    };
    run_script(d, s, None, &opts).map_err(|e| e.to_string())
}

fn summary(r: &RunReport) -> Result<RunReport, String> {
    match r.outcome {
        RunOutcome::Completed => Ok(r.clone()),
        _ => Err(format!(
            "the script stopped at step {}: {}",
            r.failed_at().unwrap_or(0),
            r.steps.last().map(|x| x.detail.clone()).unwrap_or_default()
        )),
    }
}

/// Run a READ script (read path; anything else is refused unsent).
pub(crate) fn run_read_op(
    a: &Arc<dyn AdapterAccess>,
    target: DiagTarget,
    s: ServiceScript,
) -> Result<RunReport, String> {
    if classify(&s) != Class::Read {
        return Err("not a read-only script".into());
    }
    a.read("service script (read)", target, move |d| {
        exec(d, &s).and_then(|r| summary(&r))
    })
}

/// The module as an OBD module, for the DTC read before a clear.
fn obd_module(t: DiagTarget) -> ModuleAddr {
    ModuleAddr {
        name: format!("OBD module {t}"),
        addr: 0,
        req_id: t.tx,
        resp_id: t.rx,
        protocol: Protocol::Obd,
    }
}

/// Read the stored codes (read path: `01 00`, `03`).
pub(crate) fn read_codes_op(
    a: &Arc<dyn AdapterAccess>,
    target: DiagTarget,
) -> Result<Vec<Dtc>, String> {
    a.read("read DTCs", target, move |d| {
        let r = scan_module(d, &obd_module(target));
        match (r.present, r.error) {
            (false, _) => Err("the module does not answer".into()),
            (true, Some(e)) if r.dtcs.is_empty() => Err(e),
            _ => Ok(r.dtcs),
        }
    })
}

/// The codes must still be what the report saved.
fn recheck_codes(d: &mut dyn RawDiag, t: DiagTarget, reported: &[Dtc]) -> Result<(), String> {
    let now = scan_module(d, &obd_module(t));
    if !now.present || now.dtcs != reported {
        return Err(
            "the stored codes changed since the report was saved; nothing was cleared".into(),
        );
    }
    Ok(())
}

/// The hand-back after an output test / any run: return every IO DID the
/// run may have left driven, then leave the session it opened.
fn hand_back(d: &mut dyn RawDiag, dids: &[u16], session: bool) -> Result<(), String> {
    let mut failed = Vec::new();
    for did in dids {
        let r = d.request(&[0x2F, (*did >> 8) as u8, *did as u8, 0x00]);
        if !matches!(&r, Ok(b) if b.first() == Some(&0x6F)) {
            failed.push(format!("0x{did:04X}: {r:02X?}"));
        }
    }
    let left = session && matches!(d.request(&[0x10, 0x01]), Ok(b) if b.first() == Some(&0x50));
    // ISO 14229: the default session ends any IO control the tester holds.
    if failed.is_empty() || left {
        Ok(())
    } else {
        Err(format!(
            "returnControlToECU FAILED for {}; the output may still be driven",
            failed.join(", ")
        ))
    }
}

fn check_ack(ack: &Ack, module: DiagTarget) -> Result<(), String> {
    if ack.target() == Some(module) {
        Ok(())
    } else {
        Err(format!(
            "refused: the confirmation does not name module {module}; nothing was sent"
        ))
    }
}

/// Run an OUTPUT-TEST or DTC-CLEAR script on the confirmed module
/// (`actuate`). `reported`: the codes a DTC-clear script's report saved.
pub(crate) fn run_actuate_op(
    a: &Arc<dyn AdapterAccess>,
    ack: Ack,
    module: DiagTarget,
    s: ServiceScript,
    reported: Option<Vec<Dtc>>,
) -> Result<Actuation<RunReport>, String> {
    check_ack(&ack, module)?;
    match classify(&s) {
        Class::Output | Class::Clear => {}
        c => return Err(format!("a {c:?} script does not run as an actuation")),
    }
    if has_clear(&s) && reported.is_none() {
        return Err("a DTC clear needs the saved DTC report first; nothing was sent".into());
    }
    // Pessimistic until the run says otherwise.
    let left = Arc::new(Mutex::new(driven_dids(&s)));
    let (l1, l2) = (left.clone(), left);
    let session = opens_session(&s);
    a.actuate(
        "service script",
        ack,
        move |d| {
            if let Some(codes) = &reported {
                if let Err(e) = recheck_codes(d, module, codes) {
                    *l1.lock().unwrap_or_else(|p| p.into_inner()) = Vec::new();
                    return Err(e);
                }
            }
            let r = exec(d, &s);
            *l1.lock().unwrap_or_else(|p| p.into_inner()) = match &r {
                Ok(rep) => left_driven(&s, rep),
                Err(_) => Vec::new(), // refused before anything was sent
            };
            r.and_then(|rep| summary(&rep))
        },
        move |d| {
            let dids = l2.lock().unwrap_or_else(|p| p.into_inner()).clone();
            hand_back(d, &dids, session)
        },
    )
}

/// The bytes a DATA-CHANGING script's backup holds (read path: `22`).
pub(crate) fn pre_read_op(
    a: &Arc<dyn AdapterAccess>,
    target: DiagTarget,
    dids: Vec<u16>,
) -> Result<Vec<(u16, Vec<u8>)>, String> {
    a.read("service pre-read", target, move |d| {
        dids.iter()
            .map(|did| pl::read_did(d, *did).map(|v| (*did, v)))
            .collect()
    })
}

pub(crate) fn blob(old: &[(u16, Vec<u8>)]) -> Vec<u8> {
    old.iter().flat_map(|(_, v)| v.clone()).collect()
}

/// Run a DATA-CHANGING script on the confirmed module (`mutate`). The
/// receipt must be the backup of `old` exactly.
pub(crate) fn run_mutate_op(
    a: &Arc<dyn AdapterAccess>,
    ack: Ack,
    module: DiagTarget,
    receipt: &BackupReceipt,
    s: ServiceScript,
    old: Vec<(u16, Vec<u8>)>,
    reported: Option<Vec<Dtc>>,
) -> Result<RunReport, String> {
    check_ack(&ack, module)?;
    if classify(&s) != Class::Data {
        return Err("not a data-changing script".into());
    }
    let b = blob(&old);
    if old.iter().map(|(d, _)| *d).collect::<Vec<_>>() != backup_dids(&s)
        || receipt.len != b.len()
        || receipt.crc32 != crate::views::vault::crc32(&b)
    {
        return Err(
            "refused: the backup receipt is not the backup of this script's data; nothing \
             was sent"
                .into(),
        );
    }
    if has_clear(&s) && reported.is_none() {
        return Err("a DTC clear needs the saved DTC report first; nothing was sent".into());
    }
    a.mutate("service script", ack, receipt, move |d| {
        for (did, bytes) in &old {
            if &pl::read_did(d, *did)? != bytes {
                return Err(format!(
                    "DID 0x{did:04X} changed since it was backed up; nothing was written"
                ));
            }
        }
        if let Some(codes) = &reported {
            recheck_codes(d, module, codes)?;
        }
        let r = exec(d, &s);
        let left = match &r {
            Ok(rep) => left_driven(&s, rep),
            Err(_) => Vec::new(),
        };
        let sent = r.is_ok();
        let back = hand_back(d, &left, sent && opens_session(&s));
        let rep = r?;
        back.map_err(|e| format!("the script ran, but {e}"))?;
        summary(&rep)
    })
}

// ---------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------

pub(crate) enum Step {
    Idle,
    ReadCodes(JobHandle<Vec<Dtc>>, DiagTarget),
    Confirm(ConfirmRequest, DiagTarget, Option<Vec<Dtc>>),
    PreRead(
        Ack,
        JobHandle<Vec<(u16, Vec<u8>)>>,
        DiagTarget,
        Option<Vec<Dtc>>,
    ),
    Running(JobHandle<Result<RunReport, String>>),
}

pub struct ServiceView {
    picker: PackPicker,
    pub(crate) loaded: Option<Loaded>,
    planned: Option<Result<Vec<String>, String>>,
    pub(crate) step: Step,
    pub(crate) last: Option<Result<RunReport, String>>,
    pub(crate) last_backup: Option<PathBuf>,
    pub(crate) last_report: Option<PathBuf>,
    pub(crate) records: pl::Records,
    pub(crate) msg: Option<(Tone, String)>,
}

impl Default for ServiceView {
    fn default() -> Self {
        Self {
            picker: PackPicker::new(PackKind::Service),
            loaded: None,
            planned: None,
            step: Step::Idle,
            last: None,
            last_backup: None,
            last_report: None,
            records: Default::default(),
            msg: None,
        }
    }
}

impl ServiceView {
    pub(crate) fn set_script(&mut self, path: &Path) {
        match load(path) {
            Ok(l) => {
                self.planned = Some(plan(&l.script));
                self.loaded = Some(l);
                self.msg = None;
                self.last = None;
            }
            Err(e) => self.msg = Some((Tone::Crit, e)),
        }
    }

    fn busy(&self) -> bool {
        !matches!(self.step, Step::Idle)
    }

    /// Run the loaded script: a read script runs at once (read path); any
    /// other first saves its DTC report (when it clears codes) and asks for
    /// the confirmation. Refused, sending nothing, for secured scripts, a
    /// module that is not the app target, or a missing records folder.
    pub(crate) fn request_run(&mut self, host: &mut Host<'_>) {
        let Some(l) = self.loaded.clone() else { return };
        if needs_key(&l.script) {
            return self.msg = Some((
                Tone::Crit,
                "this script needs SecurityAccess, which this panel cannot provide yet; it can \
                 be planned, not run"
                    .into(),
            ));
        }
        let module = match pl::module_of(host, l.module) {
            Ok(m) => m,
            Err(e) => return self.msg = Some((Tone::Crit, e)),
        };
        let Some(a) = host.adapter() else { return };
        let class = classify(&l.script);
        if class == Class::Read {
            let s = l.script.clone();
            let job = host.spawn_job("service script", move |_| Ok(run_read_op(&a, module, s)));
            self.step = Step::Running(job);
            return;
        }
        if let Some(why) = pl::change_blocker(host, module) {
            return self.msg = Some((Tone::Crit, why));
        }
        if (class == Class::Data || has_clear(&l.script)) && self.records.near(host).is_none() {
            return self.msg = Some((
                Tone::Crit,
                "choose a backup folder first (backups and DTC reports go there)".into(),
            ));
        }
        if class == Class::Data && backup_dids(&l.script).is_empty() {
            return self.msg = Some((
                Tone::Crit,
                "this script changes the module but names no data identifier (no write, \
                 verify or read step) to back up first, so it is refused"
                    .into(),
            ));
        }
        if has_clear(&l.script) {
            let job = host.spawn_job("read DTCs", move |_| read_codes_op(&a, module));
            self.step = Step::ReadCodes(job, module);
        } else {
            self.step = Step::Confirm(self.confirm_req(None), module, None);
        }
    }

    fn confirm_req(&self, report: Option<(&Path, usize)>) -> ConfirmRequest {
        let l = self.loaded.as_ref().expect("loaded");
        let class = classify(&l.script);
        let mut body = format!("{}\n\n", l.script.description);
        body.push_str(match class {
            Class::Data => {
                "Changes data on the module (persistent). The data the script writes, verifies \
                 or reads is saved to the vault first; the run is refused if that fails or \
                 the module changes in between. "
            }
            _ => {
                "Drives outputs / routines on the module; every output it drove is handed back \
                 to the ECU afterwards, also after an error. "
            }
        });
        if let Some((p, n)) = report {
            body.push_str(&format!(
                "It clears stored fault codes — not reversible. {n} code(s); the list was saved \
                 to {}.",
                p.display()
            ));
        }
        ConfirmRequest {
            key: "service-run".into(),
            title: format!("Run service function: {}", l.script.name),
            body,
            typed: match (class, report.is_some()) {
                (_, true) => Some("CLEAR".into()),
                (Class::Data, _) => Some("WRITE".into()),
                _ => None,
            },
            confirm_label: "Run on module".into(),
        }
    }

    pub(crate) fn tick(&mut self, host: &mut Host<'_>) {
        self.step = match std::mem::replace(&mut self.step, Step::Idle) {
            Step::Idle => Step::Idle,
            Step::ReadCodes(mut job, module) => match job.take() {
                None => Step::ReadCodes(job, module),
                Some(Err(e)) => {
                    self.msg = Some((Tone::Crit, format!("nothing sent: DTC read failed: {e}")));
                    Step::Idle
                }
                Some(Ok(codes)) => self.save_report(host, module, codes),
            },
            Step::Confirm(req, module, codes) => match host.confirm_ecu(req.clone()) {
                ConfirmResult::Pending => Step::Confirm(req, module, codes),
                ConfirmResult::Declined => {
                    self.msg = Some((Tone::Muted, "cancelled: nothing was sent".into()));
                    Step::Idle
                }
                ConfirmResult::Accepted(ack) => self.accepted(host, ack, module, codes),
            },
            Step::PreRead(ack, mut job, module, codes) => match job.take() {
                None => Step::PreRead(ack, job, module, codes),
                Some(Err(e)) => {
                    self.msg = Some((Tone::Crit, format!("pre-read failed, nothing run: {e}")));
                    Step::Idle
                }
                Some(Ok(old)) => self.backup_then_run(host, ack, module, old, codes),
            },
            Step::Running(mut job) => match job.take() {
                None => Step::Running(job),
                Some(r) => {
                    let r = r.and_then(|x| x);
                    self.msg = Some(match &r {
                        Ok(_) => (Tone::Ok, "completed".into()),
                        Err(e) => (Tone::Crit, e.clone()),
                    });
                    self.last = Some(r);
                    Step::Idle
                }
            },
        };
    }

    fn save_report(&mut self, host: &Host<'_>, module: DiagTarget, codes: Vec<Dtc>) -> Step {
        let Some(near) = self.records.near(host) else {
            return Step::Idle;
        };
        let mut text = pl::report_header(
            "saved before a service function clears codes",
            "OBD module",
            module,
        );
        text.push_str(&format!("read: 03\ncodes: {}\n", codes.len()));
        for d in &codes {
            text.push_str(&format!("{}  {}\n", d.code, d.description));
        }
        match pl::save_dtc_report(&near, &format!("service-{:X}", module.tx), &text) {
            Ok(p) => {
                let req = self.confirm_req(Some((&p, codes.len())));
                self.last_report = Some(p);
                Step::Confirm(req, module, Some(codes))
            }
            Err(e) => {
                self.msg = Some((Tone::Crit, format!("report not saved, nothing sent: {e}")));
                Step::Idle
            }
        }
    }

    fn accepted(
        &mut self,
        host: &Host<'_>,
        ack: Ack,
        module: DiagTarget,
        codes: Option<Vec<Dtc>>,
    ) -> Step {
        let (Some(a), Some(l)) = (host.adapter(), self.loaded.clone()) else {
            return Step::Idle;
        };
        if classify(&l.script) == Class::Data {
            let dids = backup_dids(&l.script);
            let job = host.spawn_job("service pre-read", move |_| pre_read_op(&a, module, dids));
            Step::PreRead(ack, job, module, codes)
        } else {
            let s = l.script;
            Step::Running(host.spawn_job("service script", move |_| {
                Ok(run_actuate_op(&a, ack, module, s, codes).and_then(Actuation::into_result))
            }))
        }
    }

    fn backup_then_run(
        &mut self,
        host: &Host<'_>,
        ack: Ack,
        module: DiagTarget,
        old: Vec<(u16, Vec<u8>)>,
        codes: Option<Vec<Dtc>>,
    ) -> Step {
        let (Some(a), Some(l), Some(near)) =
            (host.adapter(), self.loaded.clone(), self.records.near(host))
        else {
            return Step::Idle;
        };
        let parts: Vec<String> = old
            .iter()
            .map(|(d, v)| format!("DID{d:04X}x{}", v.len()))
            .collect();
        let label = format!(
            "service {} tx{:X} {}",
            l.script.id,
            module.tx,
            parts.join(" ")
        );
        let receipt = match host.backup(&near, &label, &blob(&old)) {
            Ok(r) => r,
            Err(e) => {
                self.msg = Some((Tone::Crit, format!("backup failed, nothing run: {e}")));
                return Step::Idle;
            }
        };
        self.last_backup = Some(receipt.path.clone());
        let s = l.script;
        Step::Running(host.spawn_job("service script", move |_| {
            Ok(run_mutate_op(&a, ack, module, &receipt, s, old, codes))
        }))
    }
}

impl NativeView for ServiceView {
    fn id(&self) -> &str {
        "service"
    }
    fn label(&self) -> &str {
        "Service functions"
    }
    fn icon(&self) -> &str {
        icons::HAMMER
    }
    fn group(&self) -> ViewGroup {
        ViewGroup::Vehicle
    }
    fn tier(&self) -> Tier {
        TIER
    }
    fn ui(&mut self, ui: &mut egui::Ui, host: &mut Host<'_>) {
        self.tick(host);
        let t = *host.tokens();
        if self.busy() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(100));
        }
        theme::panel(
            ui,
            &t,
            "Service functions",
            |_| {},
            |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.add_space(6.0);
                    if let Some(d) = self.picker.ui(ui, host) {
                        self.set_script(&d.path);
                    }
                    if theme::button(ui, &t, Some(icons::FOLDER_OPEN), "Open script…").clicked() {
                        if let Some(p) = host.pick_file("Service script", &[("JSON", &["json"])]) {
                            self.set_script(&p);
                        }
                    }
                    if let Some((tone, m)) = &self.msg {
                        pl::note(ui, host, *tone, m);
                    }
                    let Some(l) = self.loaded.clone() else {
                        pl::note(ui, host, Tone::Muted, "Choose a service script.");
                        return;
                    };
                    pl::mono(ui, host, &l.script.name);
                    if !l.script.description.is_empty() {
                        pl::note(ui, host, Tone::Muted, &l.script.description);
                    }
                    let class = classify(&l.script);
                    let module = pl::module_of(host, l.module);
                    pl::mono(
                        ui,
                        host,
                        &format!(
                            "class: {:?}  ·  module {}",
                            class,
                            module
                                .as_ref()
                                .map(|m| m.to_string())
                                .unwrap_or_else(|e| e.clone())
                        ),
                    );
                    theme::section_label(ui, &t, "Plan (nothing sent)");
                    match &self.planned {
                        Some(Ok(steps)) => {
                            for (i, s) in steps.iter().enumerate() {
                                pl::mono(ui, host, &format!("{i:>2}  {s}"));
                            }
                        }
                        Some(Err(e)) => pl::note(ui, host, Tone::Crit, e),
                        None => {}
                    }
                    if needs_key(&l.script) {
                        pl::note(
                            ui,
                            host,
                            Tone::Warn,
                            "Needs SecurityAccess: plan only in this panel.",
                        );
                    }
                    let blocker = match (&module, class) {
                        (Err(e), _) => Some(e.clone()),
                        (Ok(_), Class::Read) => None,
                        (Ok(m), _) => pl::change_blocker(host, *m),
                    };
                    let can =
                        crate::widgets::caps::gate(ui, host, &[crate::widgets::caps::Need::Diag]);
                    let ok = can && blocker.is_none() && !self.busy() && !needs_key(&l.script);
                    if ui
                        .add_enabled_ui(ok, |ui| {
                            theme::primary_button(ui, &t, Some(icons::PLAY), "Run…")
                        })
                        .inner
                        .clicked()
                    {
                        self.request_run(host);
                    }
                    if let Some(b) = &blocker {
                        pl::note(ui, host, Tone::Warn, b);
                    }
                    self.records.ui(ui, host);
                    for p in [&self.last_backup, &self.last_report].into_iter().flatten() {
                        pl::mono(ui, host, &format!("saved: {}", p.display()));
                    }
                    if let Some(Ok(rep)) = &self.last {
                        theme::section_label(ui, &t, "Last run");
                        for s in &rep.steps {
                            pl::mono(
                                ui,
                                host,
                                &format!(
                                    "{:>2}  {:?}  {}  {}",
                                    s.index,
                                    s.status,
                                    s.op,
                                    s.response.as_deref().map(pl::hex).unwrap_or_default()
                                ),
                            );
                        }
                    }
                });
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::coding::simkit::{Bus, Fixture};
    use super::*;
    use opentune_udssim::server::UdsServer;

    const ECU: DiagTarget = DiagTarget::can(0x7E0, 0x7E8);
    const CLU: DiagTarget = DiagTarget::can(0x714, 0x77E);

    fn script(json: &str) -> ServiceScript {
        let s = ServiceScript::from_json(json).unwrap();
        s.validate().unwrap();
        s
    }

    fn reset() -> ServiceScript {
        script(
            r#"{"id":"reset","name":"Reset","steps":[
                {"op":"diagnostic_session","subfunction":3},
                {"op":"write_data_by_identifier","did":1553,"data":[0,0]},
                {"op":"verify","did":1553,"equals":[0,0]}]}"#,
        )
    }

    fn fan() -> ServiceScript {
        script(
            r#"{"id":"fan","name":"Fan","steps":[
                {"op":"diagnostic_session","subfunction":3},
                {"op":"io_control","did":1280,"control":3,"data":[1]},
                {"op":"expect_positive"},
                {"op":"routine_control","control_type":3,"routine_id":4660}]}"#,
        )
    }

    fn bus() -> Bus {
        let s = || {
            let mut s = UdsServer::new();
            s.set_did(0x0611, vec![0x27, 0x10]);
            s.set_io(0x0500, vec![0x00]);
            s
        };
        Bus::default().uds(0x7E0, 0x7E8, s()).uds(0x714, 0x77E, s())
    }

    fn run(v: &mut ServiceView, fx: &mut Fixture, t: DiagTarget) {
        let t0 = std::time::Instant::now();
        loop {
            fx.host(t, |h| v.tick(h));
            if matches!(v.step, Step::Idle | Step::Confirm(..)) {
                break;
            }
            assert!(t0.elapsed() < std::time::Duration::from_secs(10), "stuck");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn view_with(fx: &Fixture, json: &str) -> ServiceView {
        let p = fx.dir.join("script.json");
        std::fs::write(&p, json).unwrap();
        let mut v = ServiceView {
            records: pl::Records::in_dir(&fx.dir),
            ..Default::default()
        };
        v.set_script(&p);
        assert!(v.loaded.is_some(), "{:?}", v.msg);
        v
    }

    #[test]
    fn classes_follow_the_written_table() {
        assert_eq!(classify(&reset()), Class::Data);
        assert_eq!(classify(&fan()), Class::Output);
        let clear = script(r#"{"id":"c","name":"C","steps":[{"op":"obd_request","mode":4}]}"#);
        assert_eq!(classify(&clear), Class::Clear);
        let vin =
            script(r#"{"id":"v","name":"V","steps":[{"op":"obd_request","mode":9,"pid":2}]}"#);
        assert_eq!(classify(&vin), Class::Read);
        assert_eq!(backup_dids(&reset()), vec![0x0611]);
        // The plan sends nothing (the transport refuses every request).
        assert_eq!(plan(&reset()).unwrap().len(), 3);
    }

    #[test]
    fn a_data_script_backs_up_first_and_reaches_only_the_confirmed_module() {
        let bus = bus();
        let mut fx = Fixture::new("svc-data");
        fx.adapter = Some(bus.access("sim"));
        let json = r#"{"id":"reset","name":"Reset","req_id":"714","resp_id":"77E","steps":[
            {"op":"diagnostic_session","subfunction":3},
            {"op":"write_data_by_identifier","did":1553,"data":[0,0]},
            {"op":"verify","did":1553,"equals":[0,0]}]}"#;
        let mut v = view_with(&fx, json);
        fx.host(CLU, |h| v.request_run(h));
        fx.host(CLU, |h| v.tick(h));
        assert!(bus.wire().is_empty(), "nothing before the confirmation");
        fx.confirms.answer("service-run", true);
        run(&mut v, &mut fx, CLU);
        assert!(v.last.as_ref().unwrap().is_ok(), "{:?}", v.last);
        let backup = v.last_backup.clone().unwrap();
        assert_eq!(std::fs::read(backup).unwrap(), vec![0x27, 0x10]);
        assert_eq!(
            bus.with_uds(0x714, 0x77E, |s| s.did(0x0611).unwrap().to_vec()),
            vec![0, 0]
        );
        assert!(bus.sent_to(0x7E0).is_empty());
        assert_eq!(
            bus.with_uds(0x7E0, 0x7E8, |s| s.did(0x0611).unwrap().to_vec()),
            vec![0x27, 0x10]
        );
        assert_eq!(
            bus.sent_to(0x714)[0],
            vec![0x22, 0x06, 0x11],
            "pre-read first"
        );
    }

    #[test]
    fn declined_plain_foreign_or_unbackable_runs_send_nothing() {
        let bus = bus();
        let a = bus.access("sim");
        let mut fx = Fixture::new("svc-refuse");
        fx.adapter = Some(a.clone());
        let json = r#"{"id":"reset","name":"Reset","req_id":"714","resp_id":"77E","steps":[
            {"op":"write_data_by_identifier","did":1553,"data":[0,0]}]}"#;
        let mut v = view_with(&fx, json);
        // Declined.
        fx.host(CLU, |h| v.request_run(h));
        fx.host(CLU, |h| v.tick(h));
        fx.confirms.answer("service-run", false);
        fx.host(CLU, |h| v.tick(h));
        assert!(matches!(v.step, Step::Idle));
        // Not the app target.
        fx.host(ECU, |h| v.request_run(h));
        assert!(matches!(v.step, Step::Idle));
        // A routine-only script names nothing to back up.
        let mut v2 = view_with(
            &fx,
            r#"{"id":"r","name":"R","steps":[
               {"op":"routine_control","control_type":1,"routine_id":576}]}"#,
        );
        fx.host(ECU, |h| v2.request_run(h));
        assert!(matches!(v2.step, Step::Idle));
        assert!(v2.msg.as_ref().unwrap().1.contains("back up"));
        // A plain Ack, or one for another module.
        let s = reset();
        let old = pre_read_op(&a, CLU, backup_dids(&s)).unwrap();
        let receipt = fx.backup(&blob(&old));
        bus.clear_wire();
        let req = ConfirmRequest {
            key: "plain".into(),
            title: "t".into(),
            body: "b".into(),
            typed: None,
            confirm_label: "ok".into(),
        };
        fx.host(CLU, |h| h.confirm(req.clone()));
        fx.confirms.answer("plain", true);
        let ConfirmResult::Accepted(plain) = fx.host(CLU, |h| h.confirm(req)) else {
            panic!()
        };
        assert!(run_mutate_op(&a, plain, CLU, &receipt, s.clone(), old.clone(), None).is_err());
        let eng = fx.ecu_ack(ECU);
        assert!(run_mutate_op(&a, eng, CLU, &receipt, s, old, None).is_err());
        assert!(bus.wire().is_empty(), "{:02X?}", bus.wire());
    }

    #[test]
    fn an_output_script_is_handed_back_even_when_it_fails() {
        let bus = bus();
        let a = bus.access("sim");
        let mut fx = Fixture::new("svc-output");
        // The routine 0x1234 is unknown: the script fails after driving IO
        // 0x0500; the hand-back returns it and leaves the session.
        let ack = fx.ecu_ack(ECU);
        let act = run_actuate_op(&a, ack, ECU, fan(), None).unwrap();
        assert!(act.result.is_err());
        assert!(act.restored.is_ok(), "{:?}", act.restored);
        let sent = bus.sent_to(0x7E0);
        assert!(sent.contains(&vec![0x2F, 0x05, 0x00, 0x00]), "{sent:02X?}");
        assert_eq!(sent.last().unwrap(), &vec![0x10, 0x01]);
        assert!(!bus.with_uds(0x7E0, 0x7E8, |s| s.io_overridden(0x0500)));
        assert!(bus.sent_to(0x714).is_empty());
    }

    #[test]
    fn a_dtc_clear_script_saves_the_report_first() {
        // A scripted OBD module answering mode 01 00 / 03 / 04.
        let codes = Arc::new(Mutex::new(vec![0x01u8, 0x01]));
        let c2 = codes.clone();
        let bus = Bus::default().script(
            0x7E0,
            0x7E8,
            Box::new(move |r: &[u8]| {
                let mut c = c2.lock().unwrap();
                Ok(match r {
                    [0x01, 0x00] => vec![0x41, 0x00, 0xBE, 0x1F, 0xA8, 0x13],
                    [0x03] => {
                        let mut v = vec![0x43, (c.len() / 2) as u8];
                        v.extend_from_slice(&c);
                        v
                    }
                    [0x04] => {
                        c.clear();
                        vec![0x44]
                    }
                    _ => vec![0x7F, r[0], 0x11],
                })
            }),
        );
        let mut fx = Fixture::new("svc-clear");
        fx.adapter = Some(bus.access("sim"));
        let json = r#"{"id":"clr","name":"Clear","req_id":"7E0","resp_id":"7E8",
                       "steps":[{"op":"obd_request","mode":4}]}"#;
        let mut v = view_with(&fx, json);
        fx.host(ECU, |h| v.request_run(h));
        run(&mut v, &mut fx, ECU);
        let report = v.last_report.clone().expect("report saved");
        assert!(std::fs::read_to_string(report).unwrap().contains("P0101"));
        assert!(
            bus.with_sid(0x04).is_empty(),
            "no clear before the confirmation"
        );
        let Step::Confirm(req, ..) = &v.step else {
            panic!("expected the confirmation")
        };
        assert!(req.body.contains("not reversible"));
        fx.host(ECU, |h| v.tick(h));
        fx.confirms.answer("service-run", true);
        run(&mut v, &mut fx, ECU);
        assert!(v.last.as_ref().unwrap().is_ok(), "{:?}", v.last);
        assert_eq!(bus.with_sid(0x04).len(), 1);
        assert!(codes.lock().unwrap().is_empty());
    }
}
