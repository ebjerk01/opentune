//! Check my car: a guided, read-only health check for a non-expert (the
//! webview app's `feat_wizard.js` + `wizard-core.js`).
//!
//! Six steps over engines the app already has:
//! * Connect: the top bar's cable, opened on the target (adapter set-up
//!   only), with the battery voltage rated in plain bands;
//! * Identify: VIN and engine-computer identity (OBD mode 09, UDS `22 F190`
//!   fall-back), the VIN decoded by `opentune-vin`;
//! * Scan: readiness, MIL and codes from every OBD address
//!   ([`opentune_core::obd::inspect`]), plus the full module list through
//!   Auto-scan when one is chosen;
//! * Explain: plain language from the DTC table (`opentune-dtc`); never a
//!   cause;
//! * Live check: a short mode-01 snapshot rated with the channel table's own
//!   warn / crit marks (from the live feed when another panel holds the
//!   adapter);
//! * Report: HTML / Markdown to save (the VIN only when ticked).
//!
//! READ-ONLY: nothing is written to the car and no code is cleared — the
//! Explain step points to the Autoscan / Service views, which keep their own
//! confirmation. Every step can be skipped or retried; errors are plain
//! language with the raw detail one click away. The logic is in
//! [`model`] (pure, unit-tested), the adapter work in [`ops`] (background
//! jobs, `AdapterAccess::read` / `scan` only).

mod model;
#[cfg(test)]
mod model_tests;
mod ops;
#[cfg(test)]
mod tests;

use crate::host::{
    ConfirmRequest, ConfirmResult, Host, HostAction, JobHandle, NativeView, Tier, ViewGroup,
};
use crate::icons;
use crate::theme::{self, font, Tokens, Tone, MONO_MEDIUM, UI_STRONG};
use crate::widgets::packs::{PackKind, PackPicker};
use eframe::egui::{self, Margin, RichText, Ui, Vec2};
use model::{Block, ExplainItem, Level, LiveSource, ReportMeta, Run, Status, StepData, StepId};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Registration tier, read before the view is built ([`Tier::Bench`]: only
/// alpha / bench builds register it).
pub const TIER: Tier = Tier::Bench;

pub fn create() -> Box<dyn NativeView> {
    Box::new(WizardView::default())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// What to do in the car before each step, and the Run button's label.
fn intro(id: StepId) -> (&'static str, &'static str) {
    match id {
        StepId::Connect => (
            "Plug the adapter into the car's OBD socket (under the dashboard, driver's side on \
             most cars) and switch the ignition on. The engine can stay off.",
            "Check the adapter",
        ),
        StepId::Identify => (
            "Reads the vehicle identification number (VIN) and the engine computer's name and \
             calibration IDs. Nothing is changed.",
            "Read identity",
        ),
        StepId::Scan => (
            "Reads the check-engine lamp, the readiness monitors and the trouble codes from \
             every OBD module. With the vehicle's module list chosen below it also asks every \
             module in the list (Auto-scan). Read-only: no code is cleared.",
            "Scan the car",
        ),
        StepId::Explain => (
            "What the scan found, in plain words. Descriptions come from OpenTune's \
             trouble-code table; the wizard does not guess causes.",
            "Explain again",
        ),
        StepId::Live => (
            "Start the engine and let it idle with the car parked. The wizard takes a short \
             snapshot (about three seconds) of coolant temperature, engine speed, fuel trims and \
             voltage.",
            "Take the snapshot",
        ),
        StepId::Report => (
            "A summary of every step. Save it as HTML or Markdown (open the HTML in a browser \
             to print it).",
            "Rebuild the report",
        ),
    }
}

fn tone(l: Level) -> Tone {
    match l {
        Level::Ok => Tone::Ok,
        Level::Warn => Tone::Warn,
        Level::Crit | Level::Bad => Tone::Crit,
        Level::Info | Level::Unknown | Level::Unrated => Tone::Muted,
    }
}

fn status_icon(s: Status) -> &'static str {
    match s {
        Status::Pending => icons::CIRCLE,
        Status::Running => icons::DOTS_THREE,
        Status::Done => icons::CHECK,
        Status::Failed => icons::WARNING_CIRCLE,
        Status::Skipped => icons::MINUS,
    }
}

/// A report waiting for the user to confirm replacing an existing file.
struct PendingSave {
    path: PathBuf,
    data: Vec<u8>,
}

pub struct WizardView {
    run: Run,
    /// The running step's job.
    job: Option<(StepId, JobHandle<StepData>)>,
    /// The vehicle module list for the full scan (optional).
    modules: Option<PathBuf>,
    picker: PackPicker,
    /// Put the VIN in a saved / copied report (off by default).
    include_vin: bool,
    save: Option<PendingSave>,
    msg: Option<(Tone, String)>,
    /// Live check reading gap (shorter in tests).
    gap: Duration,
}

impl Default for WizardView {
    fn default() -> Self {
        Self {
            run: Run::new(now_ms()),
            job: None,
            modules: None,
            picker: PackPicker::new(PackKind::Modules),
            include_vin: false,
            save: None,
            msg: None,
            gap: Duration::from_millis(model::LIVE_GAP_MS),
        }
    }
}

impl WizardView {
    fn meta(&self) -> ReportMeta {
        ReportMeta {
            now_ms: now_ms(),
            app: "OpenTune".into(),
            include_vin: self.include_vin,
        }
    }

    /// Collect a finished job.
    fn poll(&mut self) {
        let Some((id, job)) = self.job.as_mut() else {
            return;
        };
        let id = *id;
        if let Some(r) = job.take() {
            self.job = None;
            match r {
                Ok(d) => self.run.succeed(id, d),
                Err(e) => self.run.fail(id, &e),
            }
        }
    }

    fn running(&mut self) -> bool {
        self.poll();
        self.job.is_some()
    }

    /// Run step `id`: Explain and the report at once, the others in a job.
    fn run_step(&mut self, host: &Host<'_>, id: StepId) {
        if self.running() || self.run.busy() {
            return;
        }
        self.msg = None;
        self.run.begin(id);
        if !id.io() {
            let d = match id {
                StepId::Explain => StepData::Explain(model::explain_run(&self.run)),
                _ => StepData::Report,
            };
            self.run.succeed(id, d);
            return;
        }
        let Some(a) = host.adapter() else {
            self.run
                .fail(id, "no adapter: connect a cable in the top bar first");
            return;
        };
        let target = match host.target() {
            Ok(t) => t,
            // The scan asks the OBD addresses itself.
            Err(_) if id == StepId::Scan => crate::host::DiagTarget::ENGINE,
            Err(e) => {
                self.run.fail(id, &format!("no target module: {e}"));
                return;
            }
        };
        let name = format!("Check my car: {}", id.title());
        let job = match id {
            StepId::Connect => host.spawn_job(&name, move |_p| {
                ops::connect(&a, target).map(StepData::Connect)
            }),
            StepId::Identify => host.spawn_job(&name, move |_p| {
                ops::identify(&a, target).map(StepData::Identify)
            }),
            StepId::Scan => {
                let mods = self.modules.clone();
                host.spawn_job(&name, move |p| ops::scan(&a, mods, p).map(StepData::Scan))
            }
            _ => {
                let feed = host.live_feed();
                let gap = self.gap;
                host.spawn_job(&name, move |p| {
                    ops::live(&a, target, Some(&feed), gap, p).map(StepData::Live)
                })
            }
        };
        self.job = Some((id, job));
    }

    /// Explain and the report need no car: they fill in on arrival.
    fn auto_run(&mut self, host: &Host<'_>) {
        let id = self.run.cur();
        let st = self.run.status(id);
        if !id.io() && (st == Status::Pending || id == StepId::Report) && !self.run.busy() {
            self.run_step(host, id);
        }
    }

    fn skip(&mut self, host: &Host<'_>) {
        let id = self.run.cur();
        self.run.skip(id);
        self.run.next();
        self.auto_run(host);
    }

    fn next(&mut self, host: &Host<'_>) {
        self.run.next();
        self.auto_run(host);
    }

    fn restart(&mut self) {
        if self.running() {
            return;
        }
        self.run = Run::new(now_ms());
        self.msg = None;
    }

    /// The report text (`md` or full HTML).
    fn report_text(&self, md: bool) -> String {
        let m = self.meta();
        if md {
            model::report_markdown(&self.run, &m)
        } else {
            model::report_html(&self.run, &m, true)
        }
    }

    /// Save the report to `path`: a new file directly; an existing one only
    /// after a confirmation, through the host's guarded write (its old
    /// contents go to the vault first).
    fn save_to(&mut self, path: PathBuf, data: Vec<u8>) {
        if path.exists() {
            self.save = Some(PendingSave { path, data });
            return;
        }
        self.msg = Some(saved(&path, crate::host::atomic_write(&path, &data)));
    }

    fn save(&mut self, host: &Host<'_>, md: bool) {
        let ext = if md { "md" } else { "html" };
        let name = model::report_file_name(&self.run, ext, &self.meta());
        if let Some(p) = host.pick_save("Save the car check report", &name) {
            let data = self.report_text(md).into_bytes();
            self.save_to(p, data);
        }
    }

    fn finish_save(&mut self, host: &mut Host<'_>) {
        let Some(s) = &self.save else {
            return;
        };
        let req = ConfirmRequest {
            key: "wizard-save-report".into(),
            title: "Replace the file?".into(),
            body: format!(
                "{} exists. Its current contents are archived in the vault first.",
                s.path.display()
            ),
            typed: None,
            confirm_label: "Replace".into(),
        };
        match host.confirm(req) {
            ConfirmResult::Pending => {}
            ConfirmResult::Declined => self.save = None,
            ConfirmResult::Accepted(ack) => {
                if let Some(s) = self.save.take() {
                    let r = host.write_file(ack, &s.path, &s.data).map(|_| ());
                    self.msg = Some(saved(&s.path, r));
                }
            }
        }
    }
}

fn saved(path: &Path, r: Result<(), String>) -> (Tone, String) {
    match r {
        Ok(()) => (Tone::Ok, format!("Saved {}", path.display())),
        Err(e) => (Tone::Crit, format!("Not saved: {e}")),
    }
}

impl NativeView for WizardView {
    fn id(&self) -> &str {
        "wizard"
    }
    fn label(&self) -> &str {
        "Check my car"
    }
    fn icon(&self) -> &str {
        icons::CAR
    }
    fn group(&self) -> ViewGroup {
        ViewGroup::Vehicle
    }
    fn tier(&self) -> Tier {
        TIER
    }
    fn count(&self) -> Option<String> {
        self.run
            .scan()
            .map(|s| s.summary.dtcs.len())
            .filter(|&n| n > 0)
            .map(|n| n.to_string())
    }
    fn ui(&mut self, ui: &mut egui::Ui, host: &mut Host<'_>) {
        let running = self.running();
        self.finish_save(host);
        let t = *host.tokens();
        egui::ScrollArea::vertical()
            .id_salt("wizard-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Frame::new()
                    .inner_margin(Margin::same(theme::GAP as i8))
                    .show(ui, |ui| {
                        card(ui, &t, |ui| {
                            ui.label(
                                RichText::new("Check my car")
                                    .font(font(theme::DISPLAY_MEDIUM, 15.0))
                                    .color(t.fg),
                            );
                            dim(
                                ui,
                                &t,
                                "A guided, read-only check: connect, identify the car, scan for \
                                 trouble codes, read what they mean, take a short live snapshot \
                                 and save a report. Nothing is written to the car and no codes \
                                 are cleared. Any step can be skipped or retried.",
                            );
                            ui.add_space(8.0);
                            self.steps_ui(ui, &t, host);
                        });
                        ui.add_space(theme::GAP);
                        card(ui, &t, |ui| self.step_ui(ui, &t, host, running));
                    });
            });
        if running {
            ui.ctx().request_repaint_after(Duration::from_millis(150));
        }
    }
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

fn card<R>(ui: &mut Ui, t: &Tokens, body: impl FnOnce(&mut Ui) -> R) -> R {
    theme::panel_frame(t)
        .inner_margin(Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            body(ui)
        })
        .inner
}

fn dim(ui: &mut Ui, t: &Tokens, s: &str) {
    ui.add(egui::Label::new(RichText::new(s).color(t.fg_dim)).wrap());
}

fn faint(ui: &mut Ui, t: &Tokens, s: &str) {
    ui.add(egui::Label::new(RichText::new(s).small().color(t.fg_faint)).wrap());
}

fn kv_grid(ui: &mut Ui, t: &Tokens, id: &str, rows: &[(String, String)]) {
    egui::Grid::new(id)
        .num_columns(2)
        .spacing(Vec2::new(18.0, 4.0))
        .show(ui, |ui| {
            for (k, v) in rows {
                ui.label(RichText::new(k).color(t.fg_dim));
                ui.add(egui::Label::new(RichText::new(v).color(t.fg)).wrap());
                ui.end_row();
            }
        });
}

fn error_ui(ui: &mut Ui, t: &Tokens, salt: &str, e: &model::ErrorInfo) {
    ui.add(egui::Label::new(RichText::new(&e.text).color(t.crit)).wrap());
    if !e.raw.is_empty() {
        egui::CollapsingHeader::new("Technical detail")
            .id_salt(("wiz-err", salt))
            .show(ui, |ui| {
                ui.add(
                    egui::Label::new(
                        RichText::new(&e.raw)
                            .font(font(MONO_MEDIUM, 11.5))
                            .color(t.fg_dim),
                    )
                    .wrap(),
                );
            });
    }
}

/// A badge plus text in one row.
fn rated(ui: &mut Ui, t: &Tokens, level: Level, text: &str) {
    ui.horizontal_wrapped(|ui| {
        theme::pill(ui, t, level.word(), tone(level), false);
        ui.add(egui::Label::new(RichText::new(text).color(t.fg)).wrap());
    });
}

impl WizardView {
    fn steps_ui(&mut self, ui: &mut Ui, t: &Tokens, host: &Host<'_>) {
        let busy = self.run.busy();
        let mut go = None;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
            for (i, s) in StepId::ALL.iter().enumerate() {
                let st = self.run.status(*s);
                let col = match st {
                    Status::Done => t.ok,
                    Status::Failed => t.crit,
                    Status::Running => t.accent,
                    _ => t.fg_faint,
                };
                let text = RichText::new(format!("{}  {}  {}", i + 1, s.title(), status_icon(st)))
                    .color(if i == self.run.current { t.accent } else { col });
                let ok = self.run.reachable(i) && !busy;
                let r = ui
                    .add_enabled(ok, egui::Button::selectable(i == self.run.current, text))
                    .on_hover_text(format!("{}: {}", s.title(), st.key()));
                if r.clicked() {
                    go = Some(i);
                }
            }
        });
        if let Some(i) = go {
            self.run.go_to(i);
            self.auto_run(host);
        }
    }

    fn step_ui(&mut self, ui: &mut Ui, t: &Tokens, host: &mut Host<'_>, running: bool) {
        let id = self.run.cur();
        let st = self.run.status(id);
        let (lead, run_label) = intro(id);
        ui.label(
            RichText::new(format!(
                "Step {} of {}: {}",
                self.run.current + 1,
                StepId::ALL.len(),
                id.title()
            ))
            .font(font(theme::DISPLAY_MEDIUM, 14.0))
            .color(t.fg),
        );
        ui.add_space(2.0);
        dim(ui, t, lead);
        ui.add_space(8.0);
        self.controls_ui(ui, t, host, id);
        ui.add_space(6.0);
        self.result_ui(ui, t, id, running);
        if let Some((tone, m)) = &self.msg {
            let col = match tone {
                Tone::Crit => t.crit,
                Tone::Ok => t.ok,
                _ => t.fg_dim,
            };
            ui.add_space(4.0);
            ui.add(egui::Label::new(RichText::new(m).color(col)).wrap());
        }
        ui.add_space(10.0);
        let busy = self.run.busy();
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
            if ui
                .add_enabled_ui(!busy && self.run.current > 0, |ui| {
                    theme::button(ui, t, Some(icons::ARROW_LEFT), "Back")
                })
                .inner
                .clicked()
            {
                self.run.back();
            }
            let skip_ok =
                !busy && !matches!(st, Status::Done | Status::Skipped) && id != StepId::Report;
            if ui
                .add_enabled_ui(skip_ok, |ui| {
                    theme::button(ui, t, Some(icons::SKIP_FORWARD), "Skip this step")
                })
                .inner
                .clicked()
            {
                self.skip(host);
            }
            let label = match st {
                Status::Failed => "Retry",
                Status::Done if id.io() => "Run again",
                _ => run_label,
            };
            if ui
                .add_enabled_ui(!busy, |ui| {
                    theme::primary_button(ui, t, Some(icons::PLAY), label)
                })
                .inner
                .clicked()
            {
                self.run_step(host, id);
            }
            if running {
                if let Some((_, j)) = &self.job {
                    if theme::button(ui, t, Some(icons::STOP), "Stop").clicked() {
                        j.cancel();
                    }
                }
            }
            if ui
                .add_enabled_ui(self.run.can_next(), |ui| {
                    theme::button(ui, t, Some(icons::ARROW_RIGHT), "Next")
                })
                .inner
                .clicked()
            {
                self.next(host);
            }
        });
    }

    fn controls_ui(&mut self, ui: &mut Ui, t: &Tokens, host: &mut Host<'_>, id: StepId) {
        match id {
            StepId::Connect => {
                let l = host.link();
                let now = if l.driver.is_empty() {
                    "none chosen".to_string()
                } else {
                    let mut s = ops::driver_name(&l.driver);
                    if !l.port.is_empty() {
                        s += &format!(" on {}", l.port);
                    }
                    s
                };
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("Cable:").color(t.fg_dim));
                    ui.label(RichText::new(now).font(font(UI_STRONG, 13.0)).color(t.fg));
                    if host.adapter().is_some() && l.connected {
                        theme::pill(ui, t, "connected", Tone::Ok, false);
                    }
                });
                faint(
                    ui,
                    t,
                    "Choose a different cable (and its port and HS/MS bus) in the top bar. The \
                     Virtual car runs the whole check without a car.",
                );
            }
            StepId::Scan => {
                faint(
                    ui,
                    t,
                    "Full module scan (optional): the vehicle's module list.",
                );
                let busy = self.run.busy();
                ui.add_enabled_ui(!busy, |ui| {
                    if let Some(d) = self.picker.ui(ui, host) {
                        self.modules = Some(d.path);
                    }
                    ui.horizontal_wrapped(|ui| {
                        if theme::button(ui, t, Some(icons::FOLDER_OPEN), "Open module list…")
                            .clicked()
                        {
                            if let Some(p) = host.pick_file("Module list", &[("JSON", &["json"])]) {
                                self.modules = Some(p);
                            }
                        }
                        match &self.modules {
                            Some(p) => {
                                let n = p
                                    .file_name()
                                    .map(|f| f.to_string_lossy().into_owned())
                                    .unwrap_or_default();
                                theme::pill(ui, t, &n, Tone::Muted, true)
                                    .on_hover_text(p.display().to_string());
                                if theme::icon_button(ui, t, icons::X, "Scan the OBD modules only")
                                    .clicked()
                                {
                                    self.modules = None;
                                }
                            }
                            None => {
                                ui.label(
                                    RichText::new("none: only the OBD modules are asked")
                                        .color(t.fg_faint),
                                );
                            }
                        }
                    });
                });
            }
            StepId::Explain => {
                let codes = self.run.scan().is_some_and(|s| !s.summary.dtcs.is_empty());
                if codes {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(icons::INFO).color(t.accent));
                        ui.add(
                            egui::Label::new(
                                RichText::new(
                                    "Clearing codes is not part of this check. After a repair, \
                                     codes can be cleared in the Autoscan or Service view, which \
                                     ask for confirmation first.",
                                )
                                .color(t.fg_dim),
                            )
                            .wrap(),
                        );
                        if theme::button(ui, t, Some(icons::ARROW_SQUARE_OUT), "Open Autoscan")
                            .clicked()
                        {
                            host.act(HostAction::ShowView("autoscan".into()));
                        }
                    });
                }
            }
            StepId::Report if self.run.status(StepId::Report) == Status::Done => {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
                    if theme::button(ui, t, Some(icons::FILE_HTML), "Save HTML…").clicked() {
                        self.save(host, false);
                    }
                    if theme::button(ui, t, Some(icons::FILE_MD), "Save Markdown…").clicked() {
                        self.save(host, true);
                    }
                    if theme::icon_button(ui, t, icons::COPY, "Copy the report as Markdown")
                        .clicked()
                    {
                        ui.ctx().copy_text(self.report_text(true));
                        self.msg = Some((Tone::Muted, "Report copied".into()));
                    }
                    if theme::button(
                        ui,
                        t,
                        Some(icons::ARROW_COUNTER_CLOCKWISE),
                        "Start a new check",
                    )
                    .clicked()
                    {
                        self.restart();
                    }
                });
                ui.checkbox(&mut self.include_vin, "Include the VIN in the report")
                    .on_hover_text(
                        "Off by default, so a report you share does not identify the car.",
                    );
            }
            _ => {}
        }
    }

    fn result_ui(&mut self, ui: &mut Ui, t: &Tokens, id: StepId, running: bool) {
        let step = self.run.step(id);
        match step.status {
            Status::Running => {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(14.0).color(t.accent));
                    let m = match (&self.job, running) {
                        (Some((_, j)), true) => {
                            let (_, m) = j.progress();
                            if m.is_empty() {
                                "Working…".to_string()
                            } else {
                                m
                            }
                        }
                        _ => "Working…".to_string(),
                    };
                    ui.label(RichText::new(m).color(t.fg_dim));
                });
                return;
            }
            Status::Failed => {
                if let Some(e) = &step.error {
                    error_ui(ui, t, "step", e);
                }
                faint(
                    ui,
                    t,
                    "Press Retry after checking the above, or Skip this step.",
                );
                return;
            }
            Status::Skipped => {
                faint(ui, t, "Skipped. Press Run to do it after all.");
                return;
            }
            Status::Pending => return,
            Status::Done => {}
        }
        match id {
            StepId::Connect => {
                if let Some(c) = self.run.connect() {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new("Adapter").color(t.fg_dim));
                        ui.label(RichText::new(&c.name).color(t.fg));
                        theme::pill(ui, t, "opened", Tone::Ok, false);
                    });
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new("Battery").color(t.fg_dim));
                    });
                    rated(ui, t, c.voltage.level, &c.voltage.text);
                }
            }
            StepId::Identify => {
                if let Some(d) = self.run.identify() {
                    let mut rows = vec![(
                        "VIN".to_string(),
                        match &d.vin {
                            Some(v) => format!("{v}  ({})", d.vin_source.unwrap_or_default()),
                            None => "not reported by the car".into(),
                        },
                    )];
                    if let Some(dec) = &d.decoded {
                        if let Some(m) = &dec.manufacturer {
                            rows.push(("Manufacturer".into(), format!("{m} (from the VIN)")));
                        }
                        if let Some(y) = &dec.model_year {
                            rows.push(("Model year".into(), format!("{y} (from the VIN)")));
                        }
                    }
                    if let Some(n) = &d.ecu_name {
                        rows.push(("Engine computer".into(), n.clone()));
                    }
                    if !d.calids.is_empty() {
                        rows.push(("Calibration IDs".into(), d.calids.join(", ")));
                    }
                    if !d.cvns.is_empty() {
                        rows.push(("Verification numbers".into(), d.cvns.join(", ")));
                    }
                    if let Some(tg) = &d.target {
                        rows.push(("Asked".into(), tg.clone()));
                    }
                    kv_grid(ui, t, "wiz-identify", &rows);
                    if !d.errors.is_empty() {
                        egui::CollapsingHeader::new("Not read")
                            .id_salt("wiz-id-errors")
                            .show(ui, |ui| {
                                for (k, e) in &d.errors {
                                    faint(ui, t, &format!("{k}: {e}"));
                                }
                            });
                    }
                    faint(
                        ui,
                        t,
                        "The VIN is shown here but left out of a saved report unless you tick \
                         it on the Report step.",
                    );
                }
            }
            StepId::Scan => {
                if let Some(d) = self.run.scan() {
                    let sm = &d.summary;
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
                        match sm.mil {
                            None => theme::pill(ui, t, "MIL not read", Tone::Muted, false),
                            Some(true) => theme::pill(ui, t, "MIL ON", Tone::Crit, false),
                            Some(false) => theme::pill(ui, t, "MIL off", Tone::Ok, false),
                        };
                        let n = sm.dtcs.len();
                        theme::pill(
                            ui,
                            t,
                            &format!("{n} trouble code(s)"),
                            if n > 0 { Tone::Crit } else { Tone::Ok },
                            false,
                        );
                        if let Some(r) = &sm.readiness {
                            let k = r.incomplete.len();
                            theme::pill(
                                ui,
                                t,
                                &format!("{k} monitor(s) not finished"),
                                if k > 0 { Tone::Warn } else { Tone::Ok },
                                false,
                            );
                        }
                    });
                    let mut rows = vec![(
                        "OBD modules".to_string(),
                        if sm.obd_modules.is_empty() {
                            "none answered".into()
                        } else {
                            sm.obd_modules.join(", ")
                        },
                    )];
                    if !sm.dtcs.is_empty() {
                        rows.push((
                            "Trouble codes".into(),
                            sm.dtcs
                                .iter()
                                .map(|x| x.code.as_str())
                                .collect::<Vec<_>>()
                                .join(", "),
                        ));
                    }
                    if let Some(r) = &sm.readiness {
                        if !r.incomplete.is_empty() {
                            rows.push(("Monitors not finished".into(), r.incomplete.join(", ")));
                        }
                    }
                    if let Some(m) = &sm.modules {
                        rows.push((
                            "Module list".into(),
                            format!("{}: {} of {} answered", m.vehicle, m.present.len(), m.total),
                        ));
                    } else if let Some(n) = &d.modules_note {
                        rows.push(("Module list".into(), n.clone()));
                    }
                    ui.add_space(4.0);
                    kv_grid(ui, t, "wiz-scan", &rows);
                    for (i, p) in d.problems.iter().enumerate() {
                        ui.add_space(4.0);
                        ui.label(RichText::new(format!("{}:", p.what)).color(t.fg_dim));
                        error_ui(ui, t, &format!("problem-{i}"), &p.info);
                    }
                    ui.add_space(4.0);
                    faint(ui, t, "Next: Explain says what this means.");
                }
            }
            StepId::Explain => {
                if let Some(items) = self.run.explain() {
                    for it in items {
                        explain_item_ui(ui, t, it);
                        ui.add_space(6.0);
                    }
                }
            }
            StepId::Live => {
                if let Some(d) = self.run.live() {
                    live_ui(ui, t, d);
                }
            }
            StepId::Report => {
                let m = model::report_model(&self.run, &self.meta());
                report_ui(ui, t, &m);
            }
        }
    }
}

fn explain_item_ui(ui: &mut Ui, t: &Tokens, it: &ExplainItem) {
    egui::Frame::new()
        .fill(t.surface2)
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                theme::pill(ui, t, it.level.word(), tone(it.level), false);
                ui.add(
                    egui::Label::new(
                        RichText::new(&it.title)
                            .font(font(UI_STRONG, 13.0))
                            .color(t.fg),
                    )
                    .wrap(),
                );
            });
            dim(ui, t, &it.text);
            if let Some(s) = &it.system {
                faint(ui, t, &format!("System: {s}"));
            }
            for x in it.seen.iter().chain(&it.list) {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("•").color(t.fg_faint));
                    ui.add(egui::Label::new(RichText::new(x).color(t.fg_dim)).wrap());
                });
            }
            if let Some(e) = &it.extra {
                faint(ui, t, e);
            }
        });
}

fn live_ui(ui: &mut Ui, t: &Tokens, d: &model::LiveData) {
    let s = &d.summary;
    egui::Grid::new("wiz-live")
        .num_columns(5)
        .striped(true)
        .spacing(Vec2::new(16.0, 4.0))
        .show(ui, |ui| {
            for h in ["Value", "Last", "Min – max", "Rating", "Note"] {
                ui.label(RichText::new(h).color(t.fg_faint));
            }
            ui.end_row();
            for r in &s.rows {
                let mono = |v: String| RichText::new(v).font(font(MONO_MEDIUM, 12.0)).color(t.fg);
                ui.label(RichText::new(r.name).color(t.fg));
                ui.label(mono(match r.last {
                    Some(_) => format!("{} {}", model::fmt_val(r.last, r.unit), r.unit),
                    None => "—".into(),
                }));
                ui.label(mono(match r.last {
                    Some(_) => format!(
                        "{} – {} {}",
                        model::fmt_val(r.min, r.unit),
                        model::fmt_val(r.max, r.unit),
                        r.unit
                    ),
                    None => "—".into(),
                }));
                if r.level == Level::Unrated {
                    ui.label(RichText::new("—").color(t.fg_faint));
                } else {
                    theme::pill(ui, t, r.level.word(), tone(r.level), false);
                }
                ui.add(egui::Label::new(RichText::new(&r.note).small().color(t.fg_dim)).wrap());
                ui.end_row();
            }
        });
    ui.add_space(6.0);
    ui.label(RichText::new("Battery").color(t.fg_dim));
    rated(ui, t, s.battery.level, &s.battery.text);
    kv_grid(
        ui,
        t,
        "wiz-live-kv",
        &[
            ("Idle".into(), s.idle.text.clone()),
            ("Readings".into(), s.samples.to_string()),
        ],
    );
    if let LiveSource::Feed(src) = &d.source {
        faint(
            ui,
            t,
            &format!(
                "The adapter was in use, so these are the {src} panel's live values (nothing \
                 was sent by this check)."
            ),
        );
    }
    if d.stopped {
        faint(ui, t, "Stopped early: the readings taken so far are shown.");
    }
}

/// The report preview (the same model the HTML / Markdown are made from).
fn report_ui(ui: &mut Ui, t: &Tokens, m: &model::ReportModel) {
    egui::Frame::new()
        .fill(t.surface2)
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                RichText::new(m.title)
                    .font(font(theme::DISPLAY_MEDIUM, 15.0))
                    .color(t.fg),
            );
            let vin = m
                .vehicle
                .as_ref()
                .map(|v| format!(" · VIN {v}"))
                .unwrap_or_default();
            faint(ui, t, &format!("{} · {}{vin}", m.when, m.app));
            ui.add_space(4.0);
            dim(ui, t, m.disclaimer);
            for (si, s) in m.sections.iter().enumerate() {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(s.title)
                            .font(font(UI_STRONG, 13.5))
                            .color(t.fg),
                    );
                    let tn = match s.status {
                        Status::Done => Tone::Ok,
                        Status::Failed => Tone::Crit,
                        _ => Tone::Muted,
                    };
                    theme::pill(ui, t, s.status.text(), tn, false);
                });
                if let (Status::Failed, Some(e)) = (s.status, &s.error) {
                    error_ui(ui, t, &format!("report-{si}"), e);
                }
                match s.status {
                    Status::Skipped => faint(ui, t, "This step was skipped."),
                    Status::Pending => faint(ui, t, "This step was not run."),
                    _ => {}
                }
                for (bi, b) in s.blocks.iter().enumerate() {
                    match b {
                        Block::P(p) => dim(ui, t, p),
                        Block::Kv(rows) => kv_grid(ui, t, &format!("wiz-rep-{si}-{bi}"), rows),
                        Block::List(l) => {
                            for x in l {
                                ui.horizontal_wrapped(|ui| {
                                    ui.label(RichText::new("•").color(t.fg_faint));
                                    ui.add(
                                        egui::Label::new(RichText::new(x).color(t.fg_dim)).wrap(),
                                    );
                                });
                            }
                        }
                        Block::Table { head, rows } => {
                            egui::Grid::new(format!("wiz-rep-t-{si}-{bi}"))
                                .num_columns(head.len())
                                .striped(true)
                                .spacing(Vec2::new(14.0, 3.0))
                                .show(ui, |ui| {
                                    for h in head {
                                        ui.label(RichText::new(h).color(t.fg_faint));
                                    }
                                    ui.end_row();
                                    for r in rows {
                                        for c in r {
                                            ui.add(
                                                egui::Label::new(RichText::new(c).color(t.fg))
                                                    .wrap(),
                                            );
                                        }
                                        ui.end_row();
                                    }
                                });
                        }
                    }
                }
            }
        });
}
