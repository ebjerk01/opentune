//! Haldex AWD: the OpenHaldex controller (status, mode hold / mode set), the
//! Haldex Gen4 read over VW TP2.0, and the offline drivetrain CAN-log decode
//! with chart series and Live-log CSV export (port of the webview app's Haldex
//! panel, `feat_haldex.rs`, over the `opentune-haldex` crate).
//!
//! ## Per-op table (reviewed before merge)
//!
//! The OpenHaldex's mode (`2E FD A2`) is the only thing this panel changes.
//! Whether the unit keeps it after a power cycle is not documented by the
//! public sources, so the panel offers two shapes and classifies each:
//!
//! | Operation | Kind | Requests (in order) | Confirm | Backup | Hand-back / restore |
//! |---|---|---|---|---|---|
//! | Status | read | `22 FD A0`, `22 FD A1` to 0x70F/0x779 | — | — | — |
//! | Hold mode — 1. pre-read | read | `22 FD A0` | — | — | — |
//! | Hold mode — 2. confirm | — | none | `Host::confirm_ecu_for(req, OH, "OpenHaldex (CAN 70F/779)")` (`ops::OH`; key per module) | n/a | — |
//! | Hold mode — 3. run | actuate | `22 FD A0` (must still be the pre-read mode, else nothing is written); `2E FD A2 <mode>`; `22 FD A0` read-back; then `22 FD A0` + `22 FD A1` every 250 ms | (the Ack, re-checked: `ack.target() == OH`) | n/a: not persistent by design (the hand-back undoes it) | restore (ALWAYS, `run_then_restore`): when the write went out, `2E FD A2 <previous mode>` + read-back, up to 3 attempts |
//! | Set & keep — 1. pre-read | read | `22 FD A0` | — | — | — |
//! | Set & keep — 2. confirm | — | none | `Host::confirm_ecu_for(req, OH, "OpenHaldex (CAN 70F/779)")`, typed `SET` | — | — |
//! | Set & keep — 3. backup | — | none | — | `Host::backup(folder, "openhaldex FDA0 mode <prev>", <3-byte record from 1>)`: refused when empty or without a folder | — |
//! | Set & keep — 4. write | mutate | `22 FD A0` (must still be the backed-up mode, else nothing is written); `2E FD A2 <mode>`; `22 FD A0` read-back | (Ack re-checked; receipt must be the backup of that record: CRC + length) | as step 3 | undo = Set & keep the previous mode (the same confirmed, backed-up path) |
//! | Gen4 read | read | TP2.0 to 0x0A: `10 89`, `1A 9B`, `18 02 FF 00`, `21 <group>`… (READ_SERVICES only) | — | — | — |
//! | Gen4 simulated read | offline | nothing on a bus (synthetic unit) | — | — | — |
//! | CAN-log decode / CSV export | offline | nothing | file replace: `Host::confirm` + `Host::write_file` | vault copy of a replaced file | — |
//!
//! Every way a hold ends runs the hand-back (`hold_restore`) on the same link
//! inside the one `actuate` operation:
//! * STOP: `HoldShared::stop` → `hold_run` returns `Stopped` → restore.
//! * Time up: `hold_run` returns `TimeUp` → restore.
//! * An error (link loss = a failed status poll, the unit leaving the mode, a
//!   panic): `hold_run` returns `Err` / panics → restore runs anyway
//!   (`ops::run_then_restore`); a restore that cannot reach the unit is
//!   reported and the panel offers "Set <previous> again".
//! * Leaving the panel (another view, a minimised window): no frame for the
//!   grace time → `hold_run` returns `PanelLeft` → restore.
//! * Closing the app: `Drop` → `HoldShared::stop` and waits (bounded) for the
//!   restore to finish.

mod ops;
#[cfg(test)]
mod tests;

use super::coding::plumbing as pl;
use super::ts::save::Saver;
use crate::host::{
    Actuation, ConfirmRequest, ConfirmResult, Host, JobHandle, NativeView, Tier, ViewGroup,
};
use crate::icons;
use crate::theme::{self, Tone};
use eframe::egui::{self, RichText};
use opentune_haldex::gen4::Gen4Report;
use opentune_haldex::openhaldex::{LockStatus, Mode};
use opentune_haldex::Bus;
use ops::{Decoded, HoldEnd, HoldReport, HoldShared, OhStatus, OH, OH_NAME};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Registration tier, read before the view is built ([`Tier::Bench`]: only
/// alpha / bench builds register it).
pub const TIER: Tier = Tier::Bench;

pub fn create() -> Box<dyn NativeView> {
    Box::new(HaldexView::default())
}

/// The panel counts as left after this long without a frame.
const GRACE: Duration = Duration::from_secs(3);
/// Status auto-refresh interval.
const STATUS_EVERY: Duration = Duration::from_secs(1);
/// How long closing the app waits for a hold's hand-back.
const DROP_WAIT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tab {
    OpenHaldex,
    Gen4,
    Log,
}

/// The two mode-change shapes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Intent {
    /// Hold while the panel is open, then hand back (actuate).
    Hold,
    /// Set and leave it (mutate, backup first).
    Keep,
}

pub(crate) enum Step {
    Idle,
    PreRead(Intent, Mode, JobHandle<Vec<u8>>),
    Confirm {
        intent: Intent,
        target: Mode,
        prev: Mode,
        record: Vec<u8>,
        req: ConfirmRequest,
    },
    Holding {
        job: JobHandle<Actuation<HoldReport>>,
        prev: Mode,
        target: Mode,
        since: Instant,
    },
    Setting(JobHandle<LockStatus>, Mode),
}

pub struct HaldexView {
    pub(crate) tab: Tab,
    // OpenHaldex
    pub(crate) mode: Mode,
    pub(crate) hold_s: u32,
    pub(crate) auto: bool,
    pub(crate) status: Option<Result<OhStatus, String>>,
    status_job: Option<JobHandle<OhStatus>>,
    status_at: Option<Instant>,
    pub(crate) step: Step,
    pub(crate) hold: Arc<HoldShared>,
    pub(crate) grace: Duration,
    pub(crate) records: pl::Records,
    pub(crate) last_backup: Option<PathBuf>,
    /// A hold whose hand-back failed: the mode the unit should return to.
    pub(crate) unrestored: Option<Mode>,
    pub(crate) msg: Option<(Tone, String)>,
    // Gen4
    pub(crate) gen4_sim: bool,
    pub(crate) gen4_groups: String,
    pub(crate) gen4_dtcs: bool,
    gen4_job: Option<JobHandle<Gen4Report>>,
    pub(crate) gen4: Option<Result<Gen4Report, String>>,
    // CAN log
    pub(crate) bus: Bus,
    pub(crate) log_path: Option<PathBuf>,
    log_job: Option<JobHandle<Decoded>>,
    pub(crate) log: Option<Result<Decoded, String>>,
    saver: Saver,
    pub(crate) log_msg: Option<(Tone, String)>,
}

impl Default for HaldexView {
    fn default() -> Self {
        Self {
            tab: Tab::OpenHaldex,
            mode: Mode::Stock,
            hold_s: 60,
            auto: false,
            status: None,
            status_job: None,
            status_at: None,
            step: Step::Idle,
            hold: HoldShared::new(GRACE),
            grace: GRACE,
            records: pl::Records::default(),
            last_backup: None,
            unrestored: None,
            msg: None,
            gen4_sim: false,
            gen4_groups: String::new(),
            gen4_dtcs: true,
            gen4_job: None,
            gen4: None,
            bus: Bus::Auto,
            log_path: None,
            log_job: None,
            log: None,
            saver: Saver::default(),
            log_msg: None,
        }
    }
}

impl HaldexView {
    pub(crate) fn busy(&self) -> bool {
        !matches!(self.step, Step::Idle)
    }

    // -- OpenHaldex --------------------------------------------------------

    pub(crate) fn request_status(&mut self, host: &Host<'_>) {
        if self.status_job.is_some() || self.busy() {
            return;
        }
        let Some(a) = host.adapter() else { return };
        self.status_at = Some(Instant::now());
        self.status_job = Some(host.spawn_job("OpenHaldex status", move |_| ops::status_op(&a)));
    }

    /// Start a mode change: the pre-read first (it names the mode to return
    /// to, and is the backup of a Set & keep). Refused, sending nothing,
    /// without a backup folder (Set & keep) or when busy.
    pub(crate) fn request(&mut self, host: &mut Host<'_>, intent: Intent, target: Mode) {
        if self.busy() {
            return;
        }
        if self.status_job.is_some() {
            return self.msg = Some((Tone::Muted, "a status read is running; try again".into()));
        }
        let Some(a) = host.adapter() else {
            return self.msg = Some((Tone::Crit, "no adapter layer in this build".into()));
        };
        if intent == Intent::Keep && self.records.near(host).is_none() {
            return self.msg = Some((
                Tone::Crit,
                "choose a backup folder first: the current mode must be kept".into(),
            ));
        }
        self.msg = None;
        let job = host.spawn_job("OpenHaldex pre-read", move |_| ops::pre_read_op(&a));
        self.step = Step::PreRead(intent, target, job);
    }

    pub(crate) fn request_stop(&self) {
        self.hold.stop();
    }

    fn confirm_request(intent: Intent, prev: Mode, target: Mode, hold_s: u32) -> ConfirmRequest {
        let safety = "Changing the AWD lock changes how the car drives: only with the car \
                      stationary or in a safe place.";
        match intent {
            Intent::Hold => ConfirmRequest {
                key: pl::confirm_key("openhaldex-hold", OH),
                title: format!("Hold OpenHaldex mode: {}", ops::mode_label(target)),
                body: format!(
                    "Sends {} to the OpenHaldex (0x70F/0x779) and holds mode {target} while \
                     this panel stays open, for at most {hold_s} s. Then it sets the unit back \
                     to {prev} ({}) — always: on Stop, when the time is up, on an error or a \
                     lost link (attempted), when you leave this panel and when the app closes. \
                     {safety}",
                    ops::mode_request_hex(target),
                    ops::mode_request_hex(prev),
                ),
                typed: None,
                confirm_label: "Hold mode".into(),
            },
            Intent::Keep => ConfirmRequest {
                key: pl::confirm_key("openhaldex-set", OH),
                title: format!("Set OpenHaldex mode: {}", ops::mode_label(target)),
                body: format!(
                    "Writes {} to the OpenHaldex (0x70F/0x779) and LEAVES it in mode {target} \
                     (the unit may keep it after a power cycle). Its current lock-status record \
                     (22 FD A0: mode {prev}) is saved to the vault first; the write is refused \
                     if that fails or the unit's mode changes in between. To undo, set {prev} \
                     again from this panel. {safety}",
                    ops::mode_request_hex(target),
                ),
                typed: Some("SET".into()),
                confirm_label: "Set mode".into(),
            },
        }
    }

    pub(crate) fn tick(&mut self, host: &mut Host<'_>) {
        self.hold.beat();
        if let Some(r) = pl::take(&mut self.status_job) {
            self.status = Some(r);
        }
        if let Some(r) = pl::take(&mut self.gen4_job) {
            self.gen4 = Some(r);
        }
        if let Some(r) = pl::take(&mut self.log_job) {
            self.log = Some(r);
        }
        if let Some(r) = self.saver.tick(host) {
            self.log_msg = Some(saved_msg(r));
        }
        self.step = match std::mem::replace(&mut self.step, Step::Idle) {
            Step::Idle => Step::Idle,
            Step::PreRead(intent, target, mut job) => match job.take() {
                None => Step::PreRead(intent, target, job),
                Some(Err(e)) => {
                    self.msg = Some((Tone::Crit, format!("pre-read failed, nothing sent: {e}")));
                    Step::Idle
                }
                Some(Ok(record)) => match ops::record_mode(&record) {
                    Err(e) => {
                        self.msg = Some((Tone::Crit, format!("{e}; nothing sent")));
                        Step::Idle
                    }
                    Ok(prev) if prev == target => {
                        self.msg = Some((
                            Tone::Muted,
                            format!("the unit is already in mode {target}; nothing sent"),
                        ));
                        Step::Idle
                    }
                    Ok(prev) => Step::Confirm {
                        intent,
                        target,
                        prev,
                        req: Self::confirm_request(intent, prev, target, self.hold_s),
                        record,
                    },
                },
            },
            Step::Confirm {
                intent,
                target,
                prev,
                record,
                req,
            } => {
                let label = pl::module_label(OH_NAME, OH);
                match host.confirm_ecu_for(req.clone(), OH, &label) {
                    ConfirmResult::Pending => Step::Confirm {
                        intent,
                        target,
                        prev,
                        record,
                        req,
                    },
                    ConfirmResult::Declined => {
                        self.msg = Some((Tone::Muted, "cancelled: nothing was sent".into()));
                        Step::Idle
                    }
                    ConfirmResult::Accepted(ack) => {
                        self.accepted(host, ack, intent, prev, target, record)
                    }
                }
            }
            Step::Holding {
                mut job,
                prev,
                target,
                since,
            } => match job.take() {
                None => Step::Holding {
                    job,
                    prev,
                    target,
                    since,
                },
                Some(r) => {
                    self.held(r, prev);
                    Step::Idle
                }
            },
            Step::Setting(mut job, target) => match job.take() {
                None => Step::Setting(job, target),
                Some(Ok(st)) => {
                    self.unrestored = None;
                    self.status = None;
                    self.msg = Some((
                        Tone::Ok,
                        format!(
                            "mode {target} set; the unit reports {} ({} % commanded)",
                            st.mode.map(Mode::as_str).unwrap_or("?"),
                            st.commanded_pct
                        ),
                    ));
                    Step::Idle
                }
                Some(Err(e)) => {
                    self.msg = Some((Tone::Crit, e));
                    Step::Idle
                }
            },
        };
    }

    fn accepted(
        &mut self,
        host: &Host<'_>,
        ack: crate::host::Ack,
        intent: Intent,
        prev: Mode,
        target: Mode,
        record: Vec<u8>,
    ) -> Step {
        let Some(a) = host.adapter() else {
            return Step::Idle;
        };
        if ack.target() != Some(OH) {
            self.msg = Some((
                Tone::Crit,
                "refused: the confirmation does not name the OpenHaldex; nothing was sent".into(),
            ));
            return Step::Idle;
        }
        match intent {
            Intent::Hold => {
                self.hold = HoldShared::new(self.grace);
                self.hold.beat();
                let sh = self.hold.clone();
                let dur = Duration::from_secs(self.hold_s.clamp(1, ops::MAX_HOLD_S).into());
                Step::Holding {
                    job: host.spawn_job("OpenHaldex mode hold", move |_| {
                        ops::hold_op(&a, ack, prev, target, dur, sh)
                    }),
                    prev,
                    target,
                    since: Instant::now(),
                }
            }
            Intent::Keep => {
                let Some(near) = self.records.near(host) else {
                    self.msg = Some((Tone::Crit, "no backup folder; nothing was sent".into()));
                    return Step::Idle;
                };
                let receipt =
                    match host.backup(&near, &format!("openhaldex FDA0 mode {prev}"), &record) {
                        Ok(r) => r,
                        Err(e) => {
                            self.msg =
                                Some((Tone::Crit, format!("backup failed, nothing written: {e}")));
                            return Step::Idle;
                        }
                    };
                self.last_backup = Some(receipt.path.clone());
                Step::Setting(
                    host.spawn_job("OpenHaldex mode set", move |_| {
                        ops::set_keep_op(&a, ack, &receipt, record, target)
                    }),
                    target,
                )
            }
        }
    }

    /// A hold ended: report it; a failed hand-back leaves `unrestored`.
    fn held(&mut self, r: Result<Actuation<HoldReport>, String>, prev: Mode) {
        self.status = None;
        let act = match r {
            Ok(a) => a,
            Err(e) => {
                self.msg = Some((Tone::Crit, format!("hold not started: {e}")));
                return;
            }
        };
        if act.restored.is_err() {
            self.unrestored = Some(prev);
        } else if self.hold.written() {
            self.unrestored = None;
        }
        self.msg = Some(match act.into_result() {
            Ok(rep) => {
                let why = match rep.end {
                    HoldEnd::Stopped => "stopped",
                    HoldEnd::TimeUp => "time up",
                    HoldEnd::PanelLeft => "the panel was left",
                };
                (
                    Tone::Ok,
                    format!(
                        "held {:.1} s ({why}, {} status polls); the unit is back in mode {prev}",
                        rep.held_ms as f64 / 1000.0,
                        rep.polls
                    ),
                )
            }
            Err(e) => (Tone::Crit, e),
        });
    }

    // -- Gen4 / log --------------------------------------------------------

    pub(crate) fn request_gen4(&mut self, host: &Host<'_>) {
        if self.gen4_job.is_some() {
            return;
        }
        let opts = match ops::gen4_options(&self.gen4_groups, self.gen4_dtcs) {
            Ok(o) => o,
            Err(e) => return self.gen4 = Some(Err(e)),
        };
        if self.gen4_sim {
            self.gen4_job =
                Some(host.spawn_job("Haldex Gen4 (simulated)", move |_| Ok(ops::gen4_sim(&opts))));
            return;
        }
        let Some(a) = host.adapter() else { return };
        let driver = host.link().driver;
        self.gen4_job = Some(host.spawn_job("Haldex Gen4 read", move |_| {
            ops::gen4_read_op(&a, &driver, opts)
        }));
    }

    pub(crate) fn request_decode(&mut self, host: &Host<'_>, path: PathBuf) {
        let bus = self.bus;
        self.log_path = Some(path.clone());
        self.log_msg = None;
        self.log_job =
            Some(host.spawn_job("Haldex log decode", move |_| ops::decode_file(&path, bus)));
    }

    pub(crate) fn export_csv(&mut self, path: PathBuf) {
        let Some(Ok(d)) = &self.log else { return };
        if let Some(r) = self
            .saver
            .save("haldex-export", path, d.csv.clone().into_bytes())
        {
            self.log_msg = Some(saved_msg(r));
        }
    }

    // -- UI ----------------------------------------------------------------

    fn openhaldex_ui(&mut self, ui: &mut egui::Ui, host: &mut Host<'_>) {
        let t = *host.tokens();
        pl::note(
            ui,
            host,
            Tone::Muted,
            "OpenHaldex on the Haldex diagnostic pair 0x70F/0x779. From the OBD port only \
             where the gateway routes 0x70F (MQB); on PQ cars connect to the drivetrain CAN.",
        );
        ui.horizontal(|ui| {
            let idle = !self.busy() && self.status_job.is_none();
            if ui
                .add_enabled_ui(idle, |ui| {
                    theme::button(ui, &t, Some(icons::ARROWS_CLOCKWISE), "Read status")
                })
                .inner
                .clicked()
            {
                self.request_status(host);
            }
            ui.checkbox(&mut self.auto, "auto");
        });
        if self.auto && !self.busy() && self.status_at.is_none_or(|s| s.elapsed() >= STATUS_EVERY) {
            self.request_status(host);
        }
        let live = match &self.step {
            Step::Holding { .. } => self.hold.live().map(Ok),
            _ => self.status.clone(),
        };
        match &live {
            Some(Ok(s)) => status_line(ui, host, s),
            Some(Err(e)) => pl::note(ui, host, Tone::Crit, e),
            None => {}
        }
        ui.add_space(6.0);
        theme::section_label(ui, &t, "Mode");
        ui.add_enabled_ui(!self.busy(), |ui| {
            ui.horizontal_wrapped(|ui| {
                egui::ComboBox::from_id_salt("haldex-mode")
                    .selected_text(ops::mode_label(self.mode))
                    .show_ui(ui, |ui| {
                        for m in Mode::ALL {
                            ui.selectable_value(&mut self.mode, m, ops::mode_label(m));
                        }
                    });
                ui.label(RichText::new("hold for (s)").color(t.fg_dim));
                ui.add(egui::DragValue::new(&mut self.hold_s).range(1..=ops::MAX_HOLD_S));
            });
        });
        let can = crate::widgets::caps::gate(ui, host, &[crate::widgets::caps::Need::Can]);
        ui.horizontal(|ui| {
            let ok = can && !self.busy();
            if ui
                .add_enabled_ui(ok, |ui| {
                    theme::primary_button(ui, &t, Some(icons::PLAY), "Hold mode…")
                })
                .inner
                .on_hover_text(
                    "Holds the mode while this panel is open, then always sets the unit \
                     back to the mode it had.",
                )
                .clicked()
            {
                self.request(host, Intent::Hold, self.mode);
            }
            if ui
                .add_enabled_ui(matches!(self.step, Step::Holding { .. }), |ui| {
                    theme::primary_button(ui, &t, Some(icons::STOP), "STOP")
                })
                .inner
                .clicked()
            {
                self.request_stop();
            }
            if ui
                .add_enabled_ui(ok, |ui| {
                    theme::button(ui, &t, Some(icons::FLOPPY_DISK), "Set & keep…")
                })
                .inner
                .on_hover_text(
                    "Sets the mode and leaves it (the unit may keep it). The current record \
                     is backed up first.",
                )
                .clicked()
            {
                self.request(host, Intent::Keep, self.mode);
            }
            if matches!(self.step, Step::PreRead(..) | Step::Setting(..)) {
                ui.spinner();
            }
        });
        if let Step::Holding {
            prev,
            target,
            since,
            ..
        } = &self.step
        {
            let left = (self.hold_s as f64 - since.elapsed().as_secs_f64()).max(0.0);
            pl::note(
                ui,
                host,
                Tone::Warn,
                &format!(
                    "Holding mode {target}; returns to {prev} on STOP, in {left:.0} s, or when \
                     you leave this panel."
                ),
            );
        }
        self.records.ui(ui, host);
        if let Some(b) = &self.last_backup {
            pl::mono(ui, host, &format!("backup: {}", b.display()));
        }
        if let Some(prev) = self.unrestored {
            pl::note(
                ui,
                host,
                Tone::Crit,
                &format!(
                    "The hand-back could not be confirmed: the unit may still be in the held \
                     mode. Set it to {prev} again when the link is back."
                ),
            );
            if ui
                .add_enabled_ui(!self.busy(), |ui| {
                    theme::primary_button(
                        ui,
                        &t,
                        Some(icons::ARROW_COUNTER_CLOCKWISE),
                        &format!("Set {prev} again…"),
                    )
                })
                .inner
                .clicked()
            {
                self.request(host, Intent::Keep, prev);
            }
        }
        if let Some((tone, m)) = &self.msg {
            pl::note(ui, host, *tone, m);
        }
    }

    fn gen4_ui(&mut self, ui: &mut egui::Ui, host: &mut Host<'_>) {
        let t = *host.tokens();
        pl::note(
            ui,
            host,
            Tone::Muted,
            "Read-only: identification, stored codes and measuring groups of a Haldex Gen4 \
             (KWP2000 over VW TP2.0 to 0x0A). Needs a raw-CAN adapter (socketcan, slcan, \
             gsusb).",
        );
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(&mut self.gen4_sim, "simulated unit");
            ui.label(RichText::new("groups").color(t.fg_dim));
            ui.add(
                egui::TextEdit::singleline(&mut self.gen4_groups)
                    .desired_width(120.0)
                    .hint_text("1 3 125 126"),
            );
            ui.checkbox(&mut self.gen4_dtcs, "stored codes");
            let driver = host.link().driver;
            let refusal = (!self.gen4_sim)
                .then(|| ops::gen4_refusal(&driver))
                .flatten();
            if ui
                .add_enabled_ui(self.gen4_job.is_none() && refusal.is_none(), |ui| {
                    theme::primary_button(ui, &t, Some(icons::DOWNLOAD), "Read")
                })
                .inner
                .clicked()
            {
                self.request_gen4(host);
            }
            if self.gen4_job.is_some() {
                ui.spinner();
            }
            if let Some(r) = refusal.filter(|_| !driver.is_empty()) {
                ui.label(RichText::new(r).color(t.warn));
            }
        });
        match &self.gen4 {
            Some(Ok(r)) => gen4_report_ui(ui, host, r),
            Some(Err(e)) => pl::note(ui, host, Tone::Crit, e),
            None => {}
        }
    }

    fn log_ui(&mut self, ui: &mut egui::Ui, host: &mut Host<'_>) {
        let t = *host.tokens();
        pl::note(
            ui,
            host,
            Tone::Muted,
            "Offline: decodes the Haldex frames of a drivetrain CAN capture (candump, OpenTune \
             CSV, SavvyCAN CSV, Vector ASC). Nothing is sent.",
        );
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("bus").color(t.fg_dim));
            egui::ComboBox::from_id_salt("haldex-bus")
                .selected_text(bus_name(self.bus))
                .show_ui(ui, |ui| {
                    for b in [Bus::Auto, Bus::Pq, Bus::Mqb] {
                        ui.selectable_value(&mut self.bus, b, bus_name(b));
                    }
                });
            if theme::button(ui, &t, Some(icons::FOLDER_OPEN), "Open log…").clicked() {
                if let Some(p) =
                    host.pick_file("CAN log", &[("CAN logs", &["log", "csv", "asc", "txt"])])
                {
                    self.request_decode(host, p);
                }
            }
            let ok = matches!(self.log, Some(Ok(_))) && !self.saver.busy();
            if ui
                .add_enabled_ui(ok, |ui| {
                    theme::button(ui, &t, Some(icons::EXPORT), "Export CSV…")
                })
                .inner
                .clicked()
            {
                if let Some(p) = host.pick_save("Live-log CSV", "haldex-series.csv") {
                    self.export_csv(p);
                }
            }
            if self.log_job.is_some() {
                ui.spinner();
            }
        });
        if let Some(p) = &self.log_path {
            pl::mono(ui, host, &p.display().to_string());
        }
        if let Some((tone, m)) = &self.log_msg {
            pl::note(ui, host, *tone, m);
        }
        match &self.log {
            Some(Ok(d)) => decoded_ui(ui, host, d),
            Some(Err(e)) => pl::note(ui, host, Tone::Crit, e),
            None => {}
        }
    }
}

fn saved_msg(r: Result<PathBuf, String>) -> (Tone, String) {
    match r {
        Ok(p) => (Tone::Ok, format!("saved {}", p.display())),
        Err(e) => (Tone::Crit, e),
    }
}

fn bus_name(b: Bus) -> &'static str {
    match b {
        Bus::Auto => "auto",
        Bus::Pq => "PQ (Gen1/2/4)",
        Bus::Mqb => "MQB (Gen5)",
    }
}

fn status_line(ui: &mut egui::Ui, host: &Host<'_>, s: &OhStatus) {
    let t = *host.tokens();
    ui.horizontal_wrapped(|ui| {
        let mode = s.lock.mode.map(ops::mode_label).unwrap_or("unknown");
        theme::stat(ui, &t, icons::GEAR_SIX, "mode", mode, t.accent);
        let c = format!("{} %", s.lock.commanded_pct);
        theme::stat(ui, &t, icons::GAUGE, "commanded", &c, t.fg);
        let e = format!("0x{:02X}", s.lock.engagement_raw);
        theme::stat(ui, &t, icons::PULSE, "engagement", &e, t.fg);
        if let Some(sl) = &s.slip {
            let f = |v: Option<i8>| v.map(|x| x.to_string()).unwrap_or_else(|| "—".into());
            let txt = format!(
                "FL {} FR {} RL {} RR {}",
                f(sl.fl),
                f(sl.fr),
                f(sl.rl),
                f(sl.rr)
            );
            theme::stat(ui, &t, icons::WAVEFORM, "slip", &txt, t.fg);
        }
    });
    if let Some(e) = &s.slip_error {
        pl::note(ui, host, Tone::Muted, &format!("slip: {e}"));
    }
}

fn gen4_report_ui(ui: &mut egui::Ui, host: &Host<'_>, r: &Gen4Report) {
    let t = *host.tokens();
    if let Some(e) = &r.session_error {
        pl::note(ui, host, Tone::Warn, &format!("session: {e}"));
    }
    match (&r.identity, &r.identity_error) {
        (Some(id), _) => {
            theme::section_label(ui, &t, "Identification");
            pl::mono(ui, host, &format!("part      {}", id.part_number));
            pl::mono(ui, host, &format!("software  {}", id.sw_version));
            pl::mono(ui, host, &format!("component {}", id.component));
            pl::mono(ui, host, &format!("generation {:?}", id.generation));
        }
        (None, Some(e)) => pl::note(ui, host, Tone::Crit, &format!("identification: {e}")),
        _ => {}
    }
    match (&r.dtcs, &r.dtc_error) {
        (Some(d), _) => {
            theme::section_label(ui, &t, "Stored codes");
            if d.is_empty() {
                pl::note(ui, host, Tone::Ok, "none");
            }
            for c in d {
                pl::mono(
                    ui,
                    host,
                    &format!(
                        "{:05}  (0x{:04X}, status 0x{:02X})",
                        c.vag_number, c.code, c.status
                    ),
                );
            }
        }
        (None, Some(e)) => pl::note(ui, host, Tone::Warn, &format!("stored codes: {e}")),
        _ => {}
    }
    if !r.groups.is_empty() {
        theme::section_label(ui, &t, "Measuring groups");
    }
    for g in &r.groups {
        if let Some(e) = &g.error {
            pl::mono(ui, host, &format!("group {:3}: {e}", g.group));
            continue;
        }
        for (i, v) in g.values.iter().enumerate() {
            let val = match v.value {
                Some(x) => format!("{x} {}", v.unit),
                None => format!("raw {:02X} {:02X} (formula 0x{:02X})", v.a, v.b, v.formula),
            };
            pl::mono(
                ui,
                host,
                &format!(
                    "group {:3}.{}  {:<28} {val}",
                    g.group,
                    i + 1,
                    v.label.unwrap_or("")
                ),
            );
        }
    }
}

fn decoded_ui(ui: &mut egui::Ui, host: &Host<'_>, d: &Decoded) {
    use egui_plot::{Legend, Line, Plot, PlotPoints};
    let t = *host.tokens();
    let s = &d.summary;
    ui.horizontal_wrapped(|ui| {
        theme::stat(
            ui,
            &t,
            icons::WAVEFORM,
            "frames",
            &s.frames.to_string(),
            t.fg,
        );
        theme::stat(
            ui,
            &t,
            icons::CHECK,
            "decoded",
            &s.decoded.to_string(),
            t.fg,
        );
        theme::stat(
            ui,
            &t,
            icons::WARNING,
            "bad lines",
            &d.bad_lines.to_string(),
            t.fg,
        );
    });
    let range = |name: &str, r: &Option<opentune_haldex::canlog::Range>, unit: &str| {
        r.as_ref().map(|r| {
            format!(
                "{name}: {:.0}..{:.0} {unit} (last {:.0}, {} samples)",
                r.min, r.max, r.last, r.samples
            )
        })
    };
    for l in [
        range("lock request", &s.lock_request_pct, "%"),
        range("lock actual", &s.lock_actual_pct, "%"),
        range("Gen4 temperature", &s.gen4_temp_c, "°C"),
    ]
    .into_iter()
    .flatten()
    {
        pl::mono(ui, host, &l);
    }
    if let Some(v) = s.max_front_rear_delta_kmh {
        pl::mono(
            ui,
            host,
            &format!("max front-rear wheel-speed delta: {v:.1} km/h"),
        );
    }
    for (k, n) in &s.per_kind {
        pl::mono(ui, host, &format!("{k}: {n}"));
    }
    if !s.flags_seen.is_empty() {
        let f: Vec<&str> = s.flags_seen.iter().map(String::as_str).collect();
        pl::mono(ui, host, &format!("flags seen: {}", f.join(", ")));
    }
    if s.fault_stored_seen || s.mqb_fault_reaction {
        pl::note(ui, host, Tone::Warn, "a fault flag was seen in the log");
    }
    if !s.openhaldex_modes.is_empty() {
        let m: Vec<&str> = s.openhaldex_modes.iter().map(String::as_str).collect();
        pl::mono(ui, host, &format!("OpenHaldex modes: {}", m.join(", ")));
    }
    if !d.series.is_empty() {
        Plot::new("haldex-series")
            .height(220.0)
            .legend(Legend::default())
            .x_axis_formatter(|m, _| format!("{:.0} s", m.value))
            .show(ui, |p| {
                for se in &d.series {
                    let pts: Vec<[f64; 2]> =
                        se.t.iter().zip(&se.v).map(|(a, b)| [*a, *b]).collect();
                    p.line(Line::new(
                        format!("{} ({})", se.name, se.units),
                        PlotPoints::new(pts),
                    ));
                }
            });
    }
    if !d.rows.is_empty() {
        theme::section_label(
            ui,
            &t,
            &format!("Decoded frames ({} of {})", d.rows.len(), d.rows_total),
        );
        egui::ScrollArea::vertical()
            .id_salt("haldex-rows")
            .max_height(220.0)
            .show(ui, |ui| {
                for (ts, id, j) in &d.rows {
                    let ts = ts
                        .map(|x| format!("{x:9.3}"))
                        .unwrap_or_else(|| " ".repeat(9));
                    pl::mono(ui, host, &format!("{ts}  {id:03X}  {j}"));
                }
            });
    }
}

impl Drop for HaldexView {
    fn drop(&mut self) {
        // Closing the app: end a hold now and give its hand-back (and a
        // running Set & keep) a bounded moment to finish on the link.
        self.hold.stop();
        let t0 = Instant::now();
        loop {
            let running = match &mut self.step {
                Step::Holding { job, .. } => job.is_running(),
                Step::Setting(job, _) => job.is_running(),
                _ => false,
            };
            if !running || t0.elapsed() > DROP_WAIT {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl NativeView for HaldexView {
    fn id(&self) -> &str {
        "haldex"
    }
    fn label(&self) -> &str {
        "Haldex AWD"
    }
    fn icon(&self) -> &str {
        icons::GEAR_SIX
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
        if self.busy() || self.auto || self.gen4_job.is_some() || self.log_job.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
        theme::panel(
            ui,
            &t,
            "Haldex AWD",
            |_| {},
            |ui| {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    for (tab, name) in [
                        (Tab::OpenHaldex, "OpenHaldex"),
                        (Tab::Gen4, "Gen4 read"),
                        (Tab::Log, "CAN log"),
                    ] {
                        ui.selectable_value(&mut self.tab, tab, name);
                    }
                });
                ui.add_space(4.0);
                egui::ScrollArea::vertical().show(ui, |ui| match self.tab {
                    Tab::OpenHaldex => self.openhaldex_ui(ui, host),
                    Tab::Gen4 => self.gen4_ui(ui, host),
                    Tab::Log => self.log_ui(ui, host),
                });
            },
        );
    }
}
