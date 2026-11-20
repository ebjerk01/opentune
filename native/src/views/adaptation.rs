//! Adaptation: data-defined adaptation channels over [`opentune_adaptation`]
//! (port of the webview app's Adaptation panel, `feat_adaptation.rs`): UDS
//! value channels (read / write), UDS routine channels (basic settings), and
//! VAG KWP2000 measuring blocks and basic settings over TP2.0 / K-line.
//!
//! Which module: a UDS definition's `req_id` / `resp_id`, else the app
//! target. A KWP definition's `kwp_addr` (or the address typed here), on the
//! transport the desktop's `kwp_key` picks by cable: TP2.0 on a raw-CAN
//! cable, the K-line address on a K-line cable (confirmation shows e.g.
//! "TP2.0 → module 0x01"). Changes are offered only when that module IS the
//! app target (the `Ack` addresses the app target).
//!
//! ## Per-op table (reviewed before merge)
//!
//! | Operation | SIDs sent (in order) | Path | Backup source |
//! |---|---|---|---|
//! | Read value channel (UDS) | `22 <did>` | `read` | n/a (read only) |
//! | Write value channel (UDS) — 1. confirm | none | `Host::confirm_ecu` (typed `WRITE`) | — |
//! | Write value — 2. pre-read | `22 <did>` | `read` (the confirmed module) | — |
//! | Write value — 3. backup | none | `Host::backup` (refused when empty) | the channel's WHOLE DID record from step 2 |
//! | Write value — 4. write | `10 03`; `27 <lvl>`, `27 <lvl+1> <key>` (only with a seed/key id; never retried); `22 <did>` (must equal the backup, else NOTHING is written); `22 <did>` (engine's fresh read), `2E <did> <record>` (only when it differs), `22 <did>` (verify); `10 01` (best effort) | `mutate` (the `Ack`'s module; receipt must match the step-2 record: CRC + length) | as step 3 |
//! | Run routine channel (UDS basic settings) | run: `10 03`; `27 <lvl>`, `27 <lvl+1> <key>` (only with a seed/key id); `31 01 <id> [start data]`; `31 03 <id>` × polls. restore (always): `31 02 <id> [stop data]` (a negative reply = not running = fine), `10 01` | `actuate` (the `Ack`'s module) | n/a (non-persistent). **Flag for review:** a relearn routine may store newly learned values in the module; the definition names no data to read first, so nothing can be backed up |
//! | Actuator channel | — | not here: the Actuator tests panel runs them through the hub's actuator gates | — |
//! | Read measuring block (KWP `kwp_group`) | `10 89`, `21 <group>` (once more after a session-loss symptom: `10 89`, `21 <group>`) | `read` (TP2.0 / K-line module) | n/a (read only) |
//! | Basic settings (KWP `kwp_basic_settings`) | run: `10 89`; `31 <enter> 00 00` (enumerate, if configured); `31 <enter> <bs> <group>`; `31 <read> <bs> <group>` × polls; `32 <enter> <bs> <group>`. restore: `32 <enter> <bs> <group>` only when the run did not get the stop acknowledged and the session was not lost (else nothing) | `actuate` (the `Ack`'s TP2.0 / K-line module) | n/a (non-persistent). Same **flag** as the UDS routine. No login: a module that answers NRC 0x33 is reported as needing one |
//! | KWP adaptation channel read / test / save | — | **not ported**: even the read sends `31 B8`/`31 B9`/`31 BA` and `32 B8` (not on `READ_SERVICES`), so the backup read cannot run on the read path; needs approval | — |

use super::coding::plumbing as pl;
use crate::host::{
    Ack, Actuation, AdapterAccess, BackupReceipt, ConfirmRequest, ConfirmResult, DiagTarget, Host,
    JobHandle, NativeView, Tier, Unlocker, ViewGroup,
};
use crate::icons;
use crate::theme::{self, Tone};
use crate::widgets::packs::{PackKind, PackPicker};
use eframe::egui::{self, RichText};
use opentune_adaptation::kwp::{
    self, BasicSettingsOutcome, BasicSettingsReport, GroupReading, KwpLogin, KwpWriteOptions,
};
use opentune_adaptation::{
    read_value, run_routine, write_value, AdaptationDef, ChannelKind, RoutineOp, RoutineResult,
    ValueReading, ValueWriteReport, WriteOptions, WriteOutcome,
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Registration tier, read before the view is built ([`Tier::Bench`]: only
/// alpha / bench builds register it).
pub const TIER: Tier = Tier::Bench;

pub fn create() -> Box<dyn NativeView> {
    Box::new(AdaptationView::default())
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct Loaded {
    pub def: AdaptationDef,
    /// The UDS module the definition names (`req_id` / `resp_id`).
    pub module: Option<DiagTarget>,
    /// The KWP module address the definition names (`kwp_addr`).
    pub kwp_addr: Option<u8>,
}

fn err(e: impl ToString) -> String {
    e.to_string()
}

pub(crate) fn load(path: &Path) -> Result<Loaded, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("read adaptation definition {}: {e}", path.display()))?;
    let def = AdaptationDef::from_json(&text).map_err(|e| format!("parse adaptation def: {e}"))?;
    def.validate().map_err(err)?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(err)?;
    let kwp_addr = match v.get("kwp_addr") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Number(n)) => Some(
            n.as_u64()
                .filter(|n| *n <= 0xFF)
                .ok_or("kwp_addr is not 0..=255")? as u8,
        ),
        Some(serde_json::Value::String(s)) => Some(pl::parse_num("kwp_addr", s, 0xFF)? as u8),
        Some(_) => return Err("kwp_addr is not a number".into()),
    };
    Ok(Loaded {
        def,
        module: pl::def_module(&text)?,
        kwp_addr,
    })
}

/// The `Ack` must name exactly the module the operation means.
fn check_ack(ack: &Ack, module: DiagTarget) -> Result<(), String> {
    if ack.target() == Some(module) {
        Ok(())
    } else {
        Err(format!(
            "refused: the confirmation does not name module {module}; nothing was sent"
        ))
    }
}

fn kind<'a>(def: &'a AdaptationDef, ch: &str) -> Result<&'a ChannelKind, String> {
    def.channel(ch)
        .map(|c| &c.kind)
        .ok_or_else(|| format!("unknown channel '{ch}'"))
}

/// The DID behind a value channel.
fn value_did(def: &AdaptationDef, ch: &str) -> Result<u16, String> {
    match kind(def, ch)? {
        ChannelKind::Value { did, .. } => Ok(*did),
        _ => Err(format!("'{ch}' is not a value channel")),
    }
}

/// Read a value channel (read path: `22`).
pub(crate) fn read_value_op(
    a: &Arc<dyn AdapterAccess>,
    target: DiagTarget,
    def: AdaptationDef,
    ch: String,
) -> Result<ValueReading, String> {
    a.read("adaptation read", target, move |d| {
        read_value(d, &def, &ch).map_err(err)
    })
}

/// The whole DID record behind a value channel (read path: `22`), what
/// the vault keeps before a write.
pub(crate) fn read_record_op(
    a: &Arc<dyn AdapterAccess>,
    target: DiagTarget,
    def: &AdaptationDef,
    ch: &str,
) -> Result<Vec<u8>, String> {
    let did = value_did(def, ch)?;
    a.read("adaptation pre-read", target, move |d| pl::read_did(d, did))
}

pub(crate) struct ValueArgs {
    pub def: AdaptationDef,
    pub channel: String,
    /// The record the vault backup holds.
    pub old: Vec<u8>,
    pub raw: u64,
    pub unlock: Option<(Unlocker, u8)>,
}

/// Write a value channel on the confirmed module (`mutate`).
pub(crate) fn write_value_op(
    a: &Arc<dyn AdapterAccess>,
    ack: Ack,
    module: DiagTarget,
    receipt: &BackupReceipt,
    args: ValueArgs,
) -> Result<ValueWriteReport, String> {
    check_ack(&ack, module)?;
    if receipt.len != args.old.len() || receipt.crc32 != crate::views::vault::crc32(&args.old) {
        return Err(
            "adaptation write refused: the backup receipt is not the backup of the record \
             about to be replaced; nothing was sent"
                .into(),
        );
    }
    let did = value_did(&args.def, &args.channel)?;
    a.mutate("adaptation write", ack, receipt, move |d| {
        opentune_hub::adapter::enter_extended_session(d)?;
        let r = (|| {
            if let Some((u, level)) = &args.unlock {
                u.unlock_uds(d, *level)?;
            }
            if pl::read_did(d, did)? != args.old {
                return Err(
                    "the module's value changed since it was backed up; nothing was written"
                        .to_string(),
                );
            }
            write_value(
                d,
                &args.def,
                &args.channel,
                args.raw,
                &WriteOptions::default(),
            )
            .map_err(err)
        })();
        opentune_hub::adapter::leave_session(d);
        r
    })
}

pub(crate) struct RoutineArgs {
    pub def: AdaptationDef,
    pub channel: String,
    pub extra: Vec<u8>,
    pub polls: u32,
    pub interval_ms: u64,
    pub unlock: Option<(Unlocker, u8)>,
}

/// Run a routine channel on the confirmed module (`actuate`): start, poll
/// the results, and always stop it and leave the session afterwards.
pub(crate) fn routine_op(
    a: &Arc<dyn AdapterAccess>,
    ack: Ack,
    module: DiagTarget,
    args: RoutineArgs,
) -> Result<Actuation<Vec<RoutineResult>>, String> {
    check_ack(&ack, module)?;
    let (id, stop_data) = match kind(&args.def, &args.channel)? {
        ChannelKind::Routine { id, stop_data, .. } => (*id, stop_data.clone()),
        _ => return Err(format!("'{}' is not a routine channel", args.channel)),
    };
    a.actuate(
        "adaptation routine",
        ack,
        move |d| {
            opentune_hub::adapter::enter_extended_session(d)?;
            if let Some((u, level)) = &args.unlock {
                u.unlock_uds(d, *level)?;
            }
            let mut out =
                vec![
                    run_routine(d, &args.def, &args.channel, RoutineOp::Start, &args.extra)
                        .map_err(err)?,
                ];
            for _ in 0..args.polls {
                std::thread::sleep(std::time::Duration::from_millis(args.interval_ms));
                out.push(
                    run_routine(d, &args.def, &args.channel, RoutineOp::RequestResults, &[])
                        .map_err(err)?,
                );
            }
            Ok(out)
        },
        move |d| {
            let mut req = vec![0x31, 0x02, (id >> 8) as u8, id as u8];
            req.extend_from_slice(&stop_data);
            let r = d.request(&req);
            opentune_hub::adapter::leave_session(d);
            match r {
                // Stopped, or refused because it is not running (any 7F 31).
                Ok(resp) if matches!(resp.first(), Some(0x71)) => Ok(()),
                Ok(resp) if resp.starts_with(&[0x7F, 0x31]) => Ok(()),
                Ok(resp) => Err(format!("stop routine: unexpected reply {resp:02X?}")),
                Err(e) => Err(format!("stop routine: {e}")),
            }
        },
    )
}

/// Read a KWP measuring block (read path: `10 89`, `21 <group>`).
pub(crate) fn kwp_group_op(
    a: &Arc<dyn AdapterAccess>,
    target: DiagTarget,
    def: AdaptationDef,
    ch: String,
) -> Result<GroupReading, String> {
    a.read("KWP measuring block", target, move |d| {
        kwp::read_group(d, &def, &ch, &KwpLogin::vag()).map_err(err)
    })
}

pub(crate) struct BasicArgs {
    pub def: AdaptationDef,
    pub channel: String,
    pub polls: u32,
    pub interval_ms: u64,
}

/// What the basic-settings run left behind, for the hand-back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BasicEnd {
    /// Nothing was started (the engine's `Err`).
    NotStarted,
    /// The stop was acknowledged, or the session is gone (the module leaves
    /// basic settings with it).
    Ended,
    /// It may still be running: the hand-back sends the stop.
    MayRun,
}

/// Run KWP basic settings on the confirmed TP2.0 / K-line module
/// (`actuate`). The result is `Ok` only for a completed run; the report of
/// any other ending is in the error text.
pub(crate) fn kwp_basic_op(
    a: &Arc<dyn AdapterAccess>,
    ack: Ack,
    module: DiagTarget,
    args: BasicArgs,
) -> Result<Actuation<BasicSettingsReport>, String> {
    check_ack(&ack, module)?;
    if pl::is_isotp(module) {
        return Err("KWP basic settings needs a TP2.0 or K-line module; nothing was sent".into());
    }
    let (group, k) = match kind(&args.def, &args.channel)? {
        ChannelKind::KwpBasicSettings { group, kwp } => (*group, kwp.clone()),
        _ => {
            return Err(format!(
                "'{}' is not a kwp_basic_settings channel",
                args.channel
            ))
        }
    };
    // None = the run did not report (it panicked): stop to be safe.
    let end: Arc<Mutex<Option<BasicEnd>>> = Arc::new(Mutex::new(None));
    let (e1, e2) = (end.clone(), end);
    let set = move |e: BasicEnd| *e1.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
    a.actuate(
        "KWP basic settings",
        ack,
        move |d| {
            let opts = KwpWriteOptions {
                dry_run: false,
                confirmed: true,
                workshop_code: None,
            };
            let r = match kwp::run_basic_settings(
                d,
                &args.def,
                &args.channel,
                args.polls,
                args.interval_ms,
                &KwpLogin::vag(),
                &opts,
            ) {
                Ok(r) => r,
                Err(e) => {
                    set(BasicEnd::NotStarted);
                    return Err(e.to_string());
                }
            };
            let lost = matches!(
                r.outcome,
                BasicSettingsOutcome::Aborted {
                    session_lost: true,
                    ..
                }
            );
            set(if r.stopped || lost {
                BasicEnd::Ended
            } else {
                BasicEnd::MayRun
            });
            match &r.outcome {
                BasicSettingsOutcome::Completed => Ok(r),
                BasicSettingsOutcome::Planned => Err("planned only".into()),
                BasicSettingsOutcome::StopFailed { message } => Err(format!(
                    "basic settings ran, but the stop was refused: {message}"
                )),
                BasicSettingsOutcome::Aborted { step, message, .. } => Err(format!(
                    "basic settings aborted at {step}: {message} ({} sample(s))",
                    r.samples.len()
                )),
            }
        },
        move |d| {
            let st = *e2.lock().unwrap_or_else(|p| p.into_inner());
            match st {
                Some(BasicEnd::NotStarted) | Some(BasicEnd::Ended) => Ok(()),
                None | Some(BasicEnd::MayRun) => {
                    let req = [0x32, k.enter, k.basic_settings_selector, group];
                    match d.request(&req) {
                        Ok(r) if r.first() == Some(&0x72) => Ok(()),
                        Ok(r) => Err(format!(
                            "basic-settings stop refused ({r:02X?}): the module may stay in \
                             basic settings until its session times out"
                        )),
                        Err(e) => Err(format!("basic-settings stop: {e}")),
                    }
                }
            }
        },
    )
}

fn value_text(r: &ValueWriteReport) -> (Tone, String) {
    let o = match &r.outcome {
        WriteOutcome::Verified => {
            return (Tone::Ok, format!("written and verified: {}", r.new_raw))
        }
        WriteOutcome::NoChange => return (Tone::Muted, "no change: nothing written".into()),
        WriteOutcome::Planned => "planned only".to_string(),
        WriteOutcome::Unverified => "written, not verified".to_string(),
        WriteOutcome::VerifyFailed { did } => format!("VERIFY FAILED on DID 0x{did:04X}"),
        WriteOutcome::WriteError { did, message } => {
            format!(
                "write error on DID 0x{did:04X} (state UNKNOWN, do not retry blindly): {message}"
            )
        }
    };
    (Tone::Crit, o)
}

// ---------------------------------------------------------------------------
// Pipelines
// ---------------------------------------------------------------------------

pub(crate) enum Action {
    WriteValue {
        channel: String,
        raw: u64,
        unlock: Option<(Unlocker, u8)>,
    },
    Routine(RoutineArgs),
    Basic(BasicArgs),
}

pub(crate) enum Step {
    Idle,
    Confirm(ConfirmRequest, DiagTarget, Action),
    PreRead(Ack, JobHandle<Vec<u8>>, String, u64, Option<(Unlocker, u8)>),
    Running(JobHandle<(Tone, String)>),
}

pub struct AdaptationView {
    picker: PackPicker,
    pub(crate) loaded: Option<Loaded>,
    sel: Option<usize>,
    value_text: String,
    extra_text: String,
    polls: u32,
    interval_ms: u64,
    kwp_addr_text: String,
    read_job: Option<JobHandle<String>>,
    reading: Option<String>,
    pub(crate) step: Step,
    pub(crate) last_backup: Option<PathBuf>,
    pub(crate) security: pl::SecurityInput,
    pub(crate) records: pl::Records,
    pub(crate) msg: Option<(Tone, String)>,
}

impl Default for AdaptationView {
    fn default() -> Self {
        Self {
            picker: PackPicker::new(PackKind::Adaptation),
            loaded: None,
            sel: None,
            value_text: String::new(),
            extra_text: String::new(),
            polls: 3,
            interval_ms: 500,
            kwp_addr_text: String::new(),
            read_job: None,
            reading: None,
            step: Step::Idle,
            last_backup: None,
            security: Default::default(),
            records: Default::default(),
            msg: None,
        }
    }
}

impl AdaptationView {
    pub(crate) fn set_def(&mut self, path: &Path) {
        match load(path) {
            Ok(l) => {
                self.kwp_addr_text = l.kwp_addr.map(|a| format!("0x{a:02X}")).unwrap_or_default();
                self.loaded = Some(l);
                self.sel = None;
                self.reading = None;
                self.msg = None;
            }
            Err(e) => self.msg = Some((Tone::Crit, e)),
        }
    }

    fn busy(&self) -> bool {
        self.read_job.is_some() || !matches!(self.step, Step::Idle)
    }

    /// The module the selected channel addresses.
    pub(crate) fn module_for(&self, host: &Host<'_>, ch: &str) -> Result<DiagTarget, String> {
        let l = self.loaded.as_ref().ok_or("no definition")?;
        if kind(&l.def, ch)?.is_kwp() {
            let addr = pl::parse_num("module address", &self.kwp_addr_text, 0xFF).map_err(|_| {
                "no KWP module address: type the TP2.0 / K-line address (e.g. 0x01) or add \
                 \"kwp_addr\" to the definition"
                    .to_string()
            })? as u8;
            pl::kwp_target(&host.link().driver, addr)
        } else {
            let t = pl::module_of(host, l.module)?;
            if !pl::is_isotp(t) {
                return Err(format!(
                    "a UDS channel needs an ISO-TP (CAN) target, not {t}"
                ));
            }
            Ok(t)
        }
    }

    fn start_read(&mut self, host: &Host<'_>, ch: String) {
        let (Some(l), Some(a)) = (self.loaded.clone(), host.adapter()) else {
            return;
        };
        let target = match self.module_for(host, &ch) {
            Ok(t) => t,
            Err(e) => return self.msg = Some((Tone::Crit, e)),
        };
        let is_kwp = kind(&l.def, &ch).map(|k| k.is_kwp()).unwrap_or(false);
        self.read_job = Some(host.spawn_job("adaptation read", move |_| {
            if is_kwp {
                kwp_group_op(&a, target, l.def, ch).map(|g| {
                    let cells: Vec<String> = g
                        .cells
                        .iter()
                        .map(|c| match c.value {
                            Some(v) => format!("{v:.2} {}", c.unit),
                            None => format!("raw {:02X} {:02X} {:02X}", c.formula, c.a, c.b),
                        })
                        .collect();
                    format!("group {}: {}", g.group, cells.join("  |  "))
                })
            } else {
                read_value_op(&a, target, l.def, ch)
                    .map(|r| format!("{} {} (raw {})", r.engineering, r.unit, r.raw))
            }
        }));
    }

    /// Ask for an ECU change on channel `ch` (opens the confirmation).
    /// Refused, sending nothing, when the module is not the app target, a
    /// backup folder is missing (writes) or the seed/key id does not resolve.
    pub(crate) fn request(&mut self, host: &mut Host<'_>, ch: &str) {
        let Some(l) = self.loaded.clone() else { return };
        let module = match self.module_for(host, ch) {
            Ok(t) => t,
            Err(e) => return self.msg = Some((Tone::Crit, e)),
        };
        if let Some(why) = pl::change_blocker(host, module) {
            return self.msg = Some((Tone::Crit, why));
        }
        let unlock = match self.security.resolve(host) {
            Ok(u) => u,
            Err(e) => return self.msg = Some((Tone::Crit, e)),
        };
        let kind = match kind(&l.def, ch) {
            Ok(k) => k.clone(),
            Err(e) => return self.msg = Some((Tone::Crit, e)),
        };
        let (title, body, typed, action) = match kind {
            ChannelKind::Value { .. } => {
                if self.records.near(host).is_none() {
                    return self.msg = Some((
                        Tone::Crit,
                        "choose a backup folder first: the old value must be kept".into(),
                    ));
                }
                let raw = match pl::parse_num("value", &self.value_text, u64::MAX) {
                    Ok(v) => v,
                    Err(e) => return self.msg = Some((Tone::Crit, e)),
                };
                (
                    format!("Write adaptation: {ch} = {raw}"),
                    "Writes a new adaptation value (persistent). The current record is read and \
                     saved to the vault first; the write is refused if that fails or the module \
                     changes in between."
                        .to_string(),
                    Some("WRITE".to_string()),
                    Action::WriteValue {
                        channel: ch.into(),
                        raw,
                        unlock,
                    },
                )
            }
            ChannelKind::Routine { .. } => {
                let extra = match pl::parse_hex(&self.extra_text) {
                    Ok(x) => x,
                    Err(e) => return self.msg = Some((Tone::Crit, e)),
                };
                (
                    format!("Run routine: {ch}"),
                    format!(
                        "Starts the routine on the module, polls its result {} time(s), then \
                         stops it and returns to the default session (always, also after an \
                         error). A basic-settings routine may store newly learned values in the \
                         module; they cannot be backed up.",
                        self.polls
                    ),
                    None,
                    Action::Routine(RoutineArgs {
                        def: l.def.clone(),
                        channel: ch.into(),
                        extra,
                        polls: self.polls,
                        interval_ms: self.interval_ms,
                        unlock,
                    }),
                )
            }
            ChannelKind::KwpBasicSettings { .. } => (
                format!("Basic settings: {ch}"),
                format!(
                    "Runs basic settings on the module (KWP2000), reads the block {} time(s), \
                     then stops it (always, also after an error). It may store newly learned \
                     values in the module; they cannot be backed up.",
                    self.polls
                ),
                None,
                Action::Basic(BasicArgs {
                    def: l.def.clone(),
                    channel: ch.into(),
                    polls: self.polls,
                    interval_ms: self.interval_ms,
                }),
            ),
            _ => {
                return self.msg = Some((
                    Tone::Crit,
                    format!("'{ch}' cannot be changed from this panel"),
                ))
            }
        };
        let req = ConfirmRequest {
            key: "adaptation-change".into(),
            title,
            body,
            typed,
            confirm_label: "Run on module".into(),
        };
        self.step = Step::Confirm(req, module, action);
    }

    pub(crate) fn tick(&mut self, host: &mut Host<'_>) {
        if let Some(r) = pl::take(&mut self.read_job) {
            match r {
                Ok(s) => self.reading = Some(s),
                Err(e) => self.msg = Some((Tone::Crit, format!("read failed: {e}"))),
            }
        }
        self.step = match std::mem::replace(&mut self.step, Step::Idle) {
            Step::Idle => Step::Idle,
            Step::Confirm(req, module, action) => match host.confirm_ecu(req.clone()) {
                ConfirmResult::Pending => Step::Confirm(req, module, action),
                ConfirmResult::Declined => {
                    self.msg = Some((Tone::Muted, "cancelled: nothing was sent".into()));
                    Step::Idle
                }
                ConfirmResult::Accepted(ack) => self.accepted(host, ack, module, action),
            },
            Step::PreRead(ack, mut job, ch, raw, unlock) => match job.take() {
                None => Step::PreRead(ack, job, ch, raw, unlock),
                Some(Err(e)) => {
                    self.msg = Some((Tone::Crit, format!("pre-read failed, nothing written: {e}")));
                    Step::Idle
                }
                Some(Ok(old)) => self.backup_then_write(host, ack, ch, raw, unlock, old),
            },
            Step::Running(mut job) => match job.take() {
                None => Step::Running(job),
                Some(r) => {
                    self.msg = Some(r.unwrap_or_else(|e| (Tone::Crit, e)));
                    Step::Idle
                }
            },
        };
    }

    fn accepted(&mut self, host: &Host<'_>, ack: Ack, module: DiagTarget, action: Action) -> Step {
        let (Some(a), Some(l)) = (host.adapter(), self.loaded.clone()) else {
            return Step::Idle;
        };
        if ack.target() != Some(module) {
            self.msg = Some((
                Tone::Crit,
                "refused: the confirmed module is not the channel's; nothing was sent".into(),
            ));
            return Step::Idle;
        }
        match action {
            Action::WriteValue {
                channel,
                raw,
                unlock,
            } => {
                let ch = channel.clone();
                let job = host.spawn_job("adaptation pre-read", move |_| {
                    read_record_op(&a, module, &l.def, &ch)
                });
                Step::PreRead(ack, job, channel, raw, unlock)
            }
            Action::Routine(args) => {
                Step::Running(host.spawn_job("adaptation routine", move |_| {
                    Ok(match routine_op(&a, ack, module, args)?.into_result() {
                        Ok(v) => {
                            let last = v.last().map(|r| match r.value {
                                Some(x) => format!("{x} {}", r.unit),
                                None => pl::hex(&r.data),
                            });
                            (
                                Tone::Ok,
                                format!(
                                    "routine ran, stopped and handed back; last result: {}",
                                    last.unwrap_or_default()
                                ),
                            )
                        }
                        Err(e) => (Tone::Crit, e),
                    })
                }))
            }
            Action::Basic(args) => Step::Running(host.spawn_job("KWP basic settings", move |_| {
                Ok(match kwp_basic_op(&a, ack, module, args)?.into_result() {
                    Ok(r) => (
                        Tone::Ok,
                        format!("basic settings completed: {} sample(s)", r.samples.len()),
                    ),
                    Err(e) => (Tone::Crit, e),
                })
            })),
        }
    }

    fn backup_then_write(
        &mut self,
        host: &Host<'_>,
        ack: Ack,
        ch: String,
        raw: u64,
        unlock: Option<(Unlocker, u8)>,
        old: Vec<u8>,
    ) -> Step {
        let (Some(l), Some(a), Some(near)) =
            (self.loaded.clone(), host.adapter(), self.records.near(host))
        else {
            return Step::Idle;
        };
        let did = value_did(&l.def, &ch).unwrap_or(0);
        let label = format!("adaptation {} {} DID{did:04X}", l.def.id, ch);
        let receipt = match host.backup(&near, &label, &old) {
            Ok(r) => r,
            Err(e) => {
                self.msg = Some((Tone::Crit, format!("backup failed, nothing written: {e}")));
                return Step::Idle;
            }
        };
        self.last_backup = Some(receipt.path.clone());
        let module = ack.target().unwrap_or_default();
        let args = ValueArgs {
            def: l.def,
            channel: ch,
            old,
            raw,
            unlock,
        };
        Step::Running(host.spawn_job("adaptation write", move |_| {
            write_value_op(&a, ack, module, &receipt, args).map(|r| value_text(&r))
        }))
    }
}

impl NativeView for AdaptationView {
    fn id(&self) -> &str {
        "adaptation"
    }
    fn label(&self) -> &str {
        "Adaptation"
    }
    fn icon(&self) -> &str {
        icons::ARROWS_CLOCKWISE
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
            "Adaptation",
            |_| {},
            |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.add_space(6.0);
                    if let Some(d) = self.picker.ui(ui, host) {
                        self.set_def(&d.path);
                    }
                    if theme::button(ui, &t, Some(icons::FOLDER_OPEN), "Open definition…").clicked()
                    {
                        if let Some(p) =
                            host.pick_file("Adaptation definition", &[("JSON", &["json"])])
                        {
                            self.set_def(&p);
                        }
                    }
                    let Some(l) = self.loaded.clone() else {
                        pl::note(ui, host, Tone::Muted, "Choose an adaptation definition.");
                        return;
                    };
                    pl::mono(ui, host, &format!("{}  ·  {}", l.def.name, l.def.module));
                    if let Some((tone, m)) = &self.msg {
                        pl::note(ui, host, *tone, m);
                    }
                    if let Some(b) = &self.last_backup {
                        pl::mono(ui, host, &format!("backup: {}", b.display()));
                    }
                    ui.add_space(4.0);
                    egui::Grid::new("adaptation-channels")
                        .striped(true)
                        .show(ui, |ui| {
                            for (i, c) in l.def.channels.iter().enumerate() {
                                if ui.selectable_label(self.sel == Some(i), &c.name).clicked() {
                                    self.sel = Some(i);
                                    self.reading = None;
                                }
                                ui.label(RichText::new(c.kind.tag()).color(t.fg_dim));
                                ui.label(RichText::new(&c.note).color(t.fg_faint));
                                ui.end_row();
                            }
                        });
                    let Some(c) = self.sel.and_then(|i| l.def.channels.get(i)).cloned() else {
                        return;
                    };
                    ui.add_space(8.0);
                    theme::section_label(ui, &t, &c.name);
                    let module = self.module_for(host, &c.name);
                    match &module {
                        Ok(m) => pl::mono(ui, host, &format!("module {m}")),
                        Err(e) => pl::note(ui, host, Tone::Warn, e),
                    }
                    if c.kind.is_kwp() {
                        ui.horizontal(|ui| {
                            ui.label("KWP module address");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.kwp_addr_text)
                                    .desired_width(60.0),
                            );
                        });
                    }
                    let blocker = module
                        .as_ref()
                        .ok()
                        .and_then(|m| pl::change_blocker(host, *m));
                    let idle = !self.busy() && module.is_ok();
                    let mut change = false;
                    match &c.kind {
                        ChannelKind::Value { .. } | ChannelKind::KwpGroup { .. } => {
                            if ui
                                .add_enabled_ui(idle, |ui| {
                                    theme::button(ui, &t, Some(icons::DOWNLOAD), "Read")
                                })
                                .inner
                                .clicked()
                            {
                                self.start_read(host, c.name.clone());
                            }
                            if let Some(r) = &self.reading {
                                pl::mono(ui, host, r);
                            }
                            if matches!(c.kind, ChannelKind::Value { .. }) {
                                ui.horizontal(|ui| {
                                    ui.label("new raw value");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.value_text)
                                            .desired_width(90.0),
                                    );
                                    change = ui
                                        .add_enabled_ui(idle && blocker.is_none(), |ui| {
                                            theme::primary_button(
                                                ui,
                                                &t,
                                                Some(icons::WARNING),
                                                "Write…",
                                            )
                                        })
                                        .inner
                                        .clicked();
                                });
                            }
                        }
                        ChannelKind::Routine { .. } | ChannelKind::KwpBasicSettings { .. } => {
                            ui.horizontal(|ui| {
                                ui.label("polls");
                                ui.add(egui::DragValue::new(&mut self.polls).range(0..=100));
                                ui.label("interval ms");
                                ui.add(
                                    egui::DragValue::new(&mut self.interval_ms).range(0..=10_000),
                                );
                                if matches!(c.kind, ChannelKind::Routine { .. }) {
                                    ui.label("start data (hex)");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.extra_text)
                                            .desired_width(90.0),
                                    );
                                }
                            });
                            change = ui
                                .add_enabled_ui(idle && blocker.is_none(), |ui| {
                                    theme::primary_button(ui, &t, Some(icons::PLAY), "Run…")
                                })
                                .inner
                                .clicked();
                        }
                        ChannelKind::Actuator { .. } => pl::note(
                            ui,
                            host,
                            Tone::Muted,
                            "Actuator channels run in the Actuator tests panel (timed, with \
                             the hand-back gates).",
                        ),
                        ChannelKind::KwpAdaptation { .. } => pl::note(
                            ui,
                            host,
                            Tone::Warn,
                            "KWP adaptation channels are not available in the native app yet \
                             (their read uses routine services that the read-only path \
                             refuses).",
                        ),
                    }
                    if let Some(b) = &blocker {
                        pl::note(ui, host, Tone::Warn, b);
                    }
                    if !c.kind.is_kwp() {
                        self.security.ui(ui, host);
                    }
                    self.records.ui(ui, host);
                    if change {
                        self.request(host, &c.name);
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
    use opentune_adaptation::sim::SimKwpModule;
    use opentune_udssim::server::UdsServer;

    const ECU: DiagTarget = DiagTarget::can(0x7E0, 0x7E8);
    const TCU: DiagTarget = DiagTarget::can(0x7E1, 0x7E9);

    fn def_file(dir: &Path) -> PathBuf {
        let p = dir.join("adapt.json");
        std::fs::write(
            &p,
            r#"{"id":"syn","name":"Synthetic","req_id":"7E0","resp_id":"7E8","kwp_addr":"0x01",
               "channels":[
                {"name":"Idle","kind":"value","did":8193,"field":{"offset":0,"width":2},"min":0,"max":4000},
                {"name":"Learn","kind":"routine","id":515,"start_data":[1]},
                {"name":"Block1","kind":"kwp_group","group":1},
                {"name":"BS60","kind":"kwp_basic_settings","group":60}]}"#,
        )
        .unwrap();
        p
    }

    fn uds_bus() -> Bus {
        let s = || {
            let mut s = UdsServer::new();
            s.set_did(0x2001, vec![0x03, 0x20, 0xAA]);
            s.set_routine(0x0203, vec![0x00, 0x42]);
            s
        };
        Bus::default().uds(0x7E0, 0x7E8, s()).uds(0x7E1, 0x7E9, s())
    }

    fn did(bus: &Bus, t: DiagTarget) -> Vec<u8> {
        bus.with_uds(t.tx, t.rx, |s| s.did(0x2001).unwrap().to_vec())
    }

    fn run(v: &mut AdaptationView, fx: &mut Fixture, t: DiagTarget) {
        let t0 = std::time::Instant::now();
        while !matches!(v.step, Step::Idle) {
            assert!(t0.elapsed() < std::time::Duration::from_secs(10), "stuck");
            fx.host(t, |h| v.tick(h));
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn value_write_backs_up_the_old_record_and_writes_only_the_confirmed_module() {
        let bus = uds_bus();
        let mut fx = Fixture::new("adapt-write");
        fx.adapter = Some(bus.access("sim"));
        let mut v = AdaptationView {
            records: pl::Records::in_dir(&fx.dir),
            ..Default::default()
        };
        v.set_def(&def_file(&fx.dir));
        v.value_text = "900".into();
        fx.host(ECU, |h| v.request(h, "Idle"));
        fx.host(ECU, |h| v.tick(h));
        assert!(bus.wire().is_empty(), "nothing before the confirmation");
        fx.confirms.answer("adaptation-change", true);
        run(&mut v, &mut fx, ECU);
        assert_eq!(v.msg.as_ref().unwrap().0, Tone::Ok, "{:?}", v.msg);
        let backup = v.last_backup.clone().unwrap();
        assert_eq!(std::fs::read(backup).unwrap(), vec![0x03, 0x20, 0xAA]);
        assert_eq!(did(&bus, ECU), vec![0x03, 0x84, 0xAA]);
        assert!(bus.sent_to(0x7E1).is_empty());
        assert_eq!(did(&bus, TCU), vec![0x03, 0x20, 0xAA]);
        // The first request was the pre-read (read path).
        assert_eq!(bus.sent_to(0x7E0)[0], vec![0x22, 0x20, 0x01]);
    }

    #[test]
    fn a_declined_plain_or_foreign_confirmation_writes_nothing() {
        let bus = uds_bus();
        let a = bus.access("sim");
        let mut fx = Fixture::new("adapt-decline");
        fx.adapter = Some(a.clone());
        let mut v = AdaptationView {
            records: pl::Records::in_dir(&fx.dir),
            ..Default::default()
        };
        v.set_def(&def_file(&fx.dir));
        v.value_text = "900".into();
        // The definition's module (7E0) is not the app target (7E1).
        fx.host(TCU, |h| v.request(h, "Idle"));
        assert!(matches!(v.step, Step::Idle));
        fx.host(ECU, |h| v.request(h, "Idle"));
        fx.host(ECU, |h| v.tick(h));
        fx.confirms.answer("adaptation-change", false);
        fx.host(ECU, |h| v.tick(h));
        assert!(matches!(v.step, Step::Idle));
        assert!(bus.wire().is_empty());
        // Plain (file) Ack and an Ack for another module: refused.
        let l = load(&def_file(&fx.dir)).unwrap();
        let old = read_record_op(&a, ECU, &l.def, "Idle").unwrap();
        let receipt = fx.backup(&old);
        let req = ConfirmRequest {
            key: "plain".into(),
            title: "t".into(),
            body: "b".into(),
            typed: None,
            confirm_label: "ok".into(),
        };
        fx.host(ECU, |h| h.confirm(req.clone()));
        fx.confirms.answer("plain", true);
        let ConfirmResult::Accepted(plain) = fx.host(ECU, |h| h.confirm(req)) else {
            panic!()
        };
        let args = || ValueArgs {
            def: l.def.clone(),
            channel: "Idle".into(),
            old: old.clone(),
            raw: 900,
            unlock: None,
        };
        assert!(write_value_op(&a, plain, ECU, &receipt, args()).is_err());
        let tcu = fx.ecu_ack(TCU);
        assert!(write_value_op(&a, tcu, ECU, &receipt, args()).is_err());
        assert!(bus.with_sid(0x2E).is_empty());
        assert_eq!(did(&bus, ECU), vec![0x03, 0x20, 0xAA]);
    }

    #[test]
    fn the_routine_is_stopped_and_handed_back_even_when_it_fails() {
        let bus = uds_bus();
        let a = bus.access("sim");
        let mut fx = Fixture::new("adapt-routine");
        let l = load(&def_file(&fx.dir)).unwrap();
        let args = |polls| RoutineArgs {
            def: l.def.clone(),
            channel: "Learn".into(),
            extra: Vec::new(),
            polls,
            interval_ms: 0,
            unlock: None,
        };
        let ack = fx.ecu_ack(ECU);
        let act = routine_op(&a, ack, ECU, args(2)).unwrap();
        assert!(act.restored.is_ok(), "{:?}", act.restored);
        assert_eq!(act.result.unwrap().len(), 3);
        let sent = bus.sent_to(0x7E0);
        assert_eq!(&sent[sent.len() - 2][..2], &[0x31, 0x02], "{sent:02X?}");
        assert_eq!(sent.last().unwrap(), &vec![0x10, 0x01]);
        assert!(bus.sent_to(0x7E1).is_empty());
        // An unknown routine: the start is refused, the hand-back still runs.
        bus.clear_wire();
        bus.with_uds(0x7E0, 0x7E8, |s| *s = UdsServer::new());
        let ack = fx.ecu_ack(ECU);
        let act = routine_op(&a, ack, ECU, args(0)).unwrap();
        assert!(act.result.is_err());
        let sent = bus.sent_to(0x7E0);
        assert!(sent.iter().any(|r| r[..2] == [0x31, 0x02]), "{sent:02X?}");
        assert_eq!(sent.last().unwrap(), &vec![0x10, 0x01]);
    }

    fn kwp_bus(needs_login: bool) -> Bus {
        let m = SimKwpModule {
            basic_settings_needs_security: needs_login,
            ..Default::default()
        };
        Bus::default().kwp(0x01, 0x300, m)
    }

    #[test]
    fn kwp_reads_go_over_tp20_to_the_module_and_only_read() {
        let bus = kwp_bus(false);
        let mut fx = Fixture::new("adapt-kwp-read");
        fx.adapter = Some(bus.access("socketcan"));
        let mut v = AdaptationView::default();
        v.set_def(&def_file(&fx.dir));
        let t = fx.host(ECU, |h| v.module_for(h, "Block1")).unwrap();
        assert_eq!(t, DiagTarget::tp20(0x01));
        assert_eq!(t.to_string(), "TP2.0 → module 0x01");
        let l = v.loaded.clone().unwrap();
        let g = kwp_group_op(fx.adapter.as_ref().unwrap(), t, l.def, "Block1".into()).unwrap();
        assert_eq!(g.group, 1);
        let seen = bus.with_kwp(0x01, 0x300, |m| m.services());
        assert_eq!(seen, vec![0x10, 0x21]);
        // A K-line cable picks the K-line transport; a plain OBD cable none.
        assert_eq!(pl::kwp_target("kkl", 1).unwrap(), DiagTarget::kline(1));
        assert!(pl::kwp_target("elm327obd", 1).is_err());
    }

    #[test]
    fn kwp_basic_settings_runs_on_the_confirmed_module_and_always_stops() {
        let bus = kwp_bus(false);
        let a = bus.access("socketcan");
        let mut fx = Fixture::new("adapt-kwp-bs");
        let l = load(&def_file(&fx.dir)).unwrap();
        let args = || BasicArgs {
            def: l.def.clone(),
            channel: "BS60".into(),
            polls: 2,
            interval_ms: 0,
        };
        // A plain Ack, or one for another module, sends nothing.
        let other = fx.ecu_ack(ECU);
        assert!(kwp_basic_op(&a, other, DiagTarget::tp20(0x01), args()).is_err());
        assert!(bus.wire().is_empty());
        let ack = fx.ecu_ack(DiagTarget::tp20(0x01));
        let act = kwp_basic_op(&a, ack, DiagTarget::tp20(0x01), args()).unwrap();
        let rep = act.result.unwrap();
        assert_eq!(rep.samples.len(), 2);
        assert!(rep.stopped);
        assert!(bus.with_kwp(0x01, 0x300, |m| m.basic_active.is_none()));
        // Only one stop: the run's (the hand-back saw it acknowledged).
        assert_eq!(bus.with_sid(0x32).len(), 1);
    }

    #[test]
    fn kwp_basic_settings_hand_back_stops_after_a_failed_run() {
        // A module whose poll fails and whose first stop is refused: the run
        // fails, and the hand-back sends the stop again (accepted).
        let mut m = SimKwpModule {
            basic_settings_needs_security: false,
            ..Default::default()
        };
        let mut stops = 0;
        let bus = Bus::default().script(
            0x01,
            0x300,
            Box::new(move |req: &[u8]| {
                if req == [0x31, 0xBA, 0x00, 60] {
                    return Ok(vec![0x7F, 0x31, 0x22]);
                }
                if req.first() == Some(&0x32) {
                    stops += 1;
                    if stops == 1 {
                        return Ok(vec![0x7F, 0x32, 0x22]);
                    }
                }
                Ok(m.answer(req))
            }),
        );
        let a = bus.access("socketcan");
        let mut fx = Fixture::new("adapt-kwp-fail");
        let l = load(&def_file(&fx.dir)).unwrap();
        let ack = fx.ecu_ack(DiagTarget::tp20(0x01));
        let act = kwp_basic_op(
            &a,
            ack,
            DiagTarget::tp20(0x01),
            BasicArgs {
                def: l.def,
                channel: "BS60".into(),
                polls: 2,
                interval_ms: 0,
            },
        )
        .unwrap();
        assert!(act.result.is_err());
        assert!(act.restored.is_ok(), "{:?}", act.restored);
        assert_eq!(bus.with_sid(0x32).len(), 2, "{:02X?}", bus.wire());
    }

    #[test]
    fn kwp_basic_settings_needing_a_login_is_reported_not_forced() {
        let bus = kwp_bus(true);
        let a = bus.access("socketcan");
        let mut fx = Fixture::new("adapt-kwp-login");
        let l = load(&def_file(&fx.dir)).unwrap();
        let ack = fx.ecu_ack(DiagTarget::tp20(0x01));
        let act = kwp_basic_op(
            &a,
            ack,
            DiagTarget::tp20(0x01),
            BasicArgs {
                def: l.def,
                channel: "BS60".into(),
                polls: 1,
                interval_ms: 0,
            },
        )
        .unwrap();
        let e = act.result.unwrap_err();
        assert!(e.to_lowercase().contains("login"), "{e}");
        assert!(bus.with_sid(0x27).is_empty(), "no key is ever sent");
    }
}
