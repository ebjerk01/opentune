//! Debug (row 37, Bench tier): read-only raw requests, the offline UDS
//! simulator, a guided auto-debug self-test (row 22), the session log and,
//! in a build with an upload target, a diagnostics report sent only after a
//! preview (row 39).
//!
//! Gating: the view is registered only on alpha / bench channels (`TIER`),
//! and every operation also checks the channel in its backend
//! ([`backend::gate`]), so nothing runs on stable even if the view were
//! reached.
//!
//! **No write path.** Raw requests go through `AdapterAccess::read`, which
//! admits only `READ_SERVICES`; ECU-changing requests are refused before
//! anything is sent. An ECU change belongs to a module panel, through
//! `Host::confirm_ecu` + `mutate` (with a vault backup) or `actuate`.

pub mod autodebug;
pub mod backend;
pub mod report;
pub mod session;

use crate::host::{ConfirmRequest, ConfirmResult, Host, JobHandle, NativeView, Tier, ViewGroup};
use crate::icons;
use crate::theme::{self, font, Tokens, Tone, DISPLAY_MEDIUM, MONO_MEDIUM, UI_STRONG};
use autodebug::{Outcome, Vehicle, Verdict, STEPS};
use eframe::egui::{self, Margin, RichText, Ui};
use std::path::PathBuf;
use std::time::Duration;

/// Registration tier, read before the view is built ([`Tier::Bench`]: only
/// alpha / bench builds register it).
pub const TIER: Tier = Tier::Bench;

pub fn create() -> Box<dyn NativeView> {
    Box::new(DebugView::default())
}

enum Phase {
    /// No step waiting for the user (a step without a car action starts
    /// on its own; after the last step nothing is pending).
    Idle,
    /// Waiting for the user to do `STEPS[plan[idx]].car` (or skip).
    Prompt,
    Running(JobHandle<Outcome>),
    Done,
}

struct AutoDebug {
    vehicle: Vehicle,
    plan: Vec<usize>,
    idx: usize,
    phase: Phase,
    results: Vec<Outcome>,
}

impl Default for AutoDebug {
    fn default() -> Self {
        Self {
            vehicle: Vehicle::default(),
            plan: Vec::new(),
            idx: 0,
            phase: Phase::Idle,
            results: Vec::new(),
        }
    }
}

impl AutoDebug {
    /// A run is in progress (a step waits, runs or is about to start).
    fn active(&self) -> bool {
        match self.phase {
            Phase::Prompt | Phase::Running(_) => true,
            Phase::Idle => self.idx < self.plan.len(),
            Phase::Done => false,
        }
    }
}

/// A file save that may need a "replace?" confirmation.
struct Save {
    key: &'static str,
    path: PathBuf,
    data: Vec<u8>,
}

pub struct DebugView {
    req: String,
    raw_job: Option<JobHandle<Vec<u8>>>,
    raw_out: Option<Result<String, String>>,
    sim_dids: String,
    sim_reqs: String,
    sim_sec: bool,
    sim_out: Option<Result<backend::Exchanges, String>>,
    ad: AutoDebug,
    save: Option<Save>,
    msg: Option<(Tone, String)>,
    note: String,
    /// The redacted report on screen, waiting for Send / Cancel.
    pending: Option<report::Pending>,
    /// The report the user just clicked Send on (submitted next frame).
    send_now: Option<report::Pending>,
    upload_job: Option<JobHandle<String>>,
    upload_out: Option<Result<String, String>>,
}

impl Default for DebugView {
    fn default() -> Self {
        Self {
            req: "22 F1 90".into(),
            raw_job: None,
            raw_out: None,
            sim_dids: "F190=57 56 57 5A 5A 5A 33 43 5A 39 45 30 30 30 30 30 30".into(),
            sim_reqs: "10 03\n22 F1 90\n22 F1 91\n2E F1 90 41".into(),
            sim_sec: true,
            sim_out: None,
            ad: AutoDebug::default(),
            save: None,
            msg: None,
            note: String::new(),
            pending: None,
            send_now: None,
            upload_job: None,
            upload_out: None,
        }
    }
}

impl NativeView for DebugView {
    fn id(&self) -> &str {
        "debug"
    }
    fn label(&self) -> &str {
        "Debug"
    }
    fn icon(&self) -> &str {
        icons::BUG
    }
    fn group(&self) -> ViewGroup {
        ViewGroup::Bench
    }
    fn tier(&self) -> Tier {
        TIER
    }
    fn ui(&mut self, ui: &mut egui::Ui, host: &mut Host<'_>) {
        let t = *host.tokens();
        self.poll(host);
        self.finish_save(host);
        theme::panel(
            ui,
            &t,
            "Debug",
            |ui| {
                theme::pill(ui, &t, "Alpha · read-only", Tone::Warn, false);
            },
            |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        egui::Frame::new()
                            .inner_margin(Margin::same(14))
                            .show(ui, |ui| self.body(ui, &t, host));
                    });
            },
        );
        self.upload_modal(ui.ctx(), &t);
    }
}

fn card(ui: &mut Ui, t: &Tokens, icon: &str, title: &str, add: impl FnOnce(&mut Ui)) {
    egui::Frame::new()
        .fill(t.surface)
        .stroke(egui::Stroke::new(1.0, t.border))
        .corner_radius(egui::CornerRadius::same(theme::RADIUS_SM))
        .inner_margin(Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                RichText::new(format!("{icon}  {title}"))
                    .font(font(DISPLAY_MEDIUM, 14.0))
                    .color(t.fg),
            );
            ui.add_space(6.0);
            add(ui);
        });
    ui.add_space(theme::GAP);
}

fn muted(ui: &mut Ui, t: &Tokens, s: &str) {
    ui.add(egui::Label::new(RichText::new(s).size(12.0).color(t.fg_faint)).wrap());
}

fn mono_line(ui: &mut Ui, s: &str, color: egui::Color32) {
    ui.add(egui::Label::new(RichText::new(s).font(font(MONO_MEDIUM, 12.0)).color(color)).wrap());
}

impl DebugView {
    fn poll(&mut self, host: &mut Host<'_>) {
        if let Some(r) = self.raw_job.as_mut().and_then(JobHandle::take) {
            self.raw_job = None;
            self.raw_out = Some(r.map(|b| backend::hexs(&b)));
        }
        if let Phase::Running(j) = &mut self.ad.phase {
            if let Some(r) = j.take() {
                let step = &STEPS[self.ad.plan[self.ad.idx]];
                let o = r.unwrap_or_else(|e| Outcome::new(step.title, Verdict::Fail, e));
                self.ad.results.push(o);
                self.next_step();
            }
        }
        if let Some(p) = self.send_now.take() {
            self.upload_out = None;
            self.upload_job = Some(host.spawn_job("diagnostics upload", move |_| p.send()));
        }
        if let Some(r) = self.upload_job.as_mut().and_then(JobHandle::take) {
            self.upload_job = None;
            match &r {
                Ok(id) => session::push(format!("diagnostics report sent: {id}")),
                Err(e) => session::push(format!("diagnostics report not sent: {e}")),
            }
            self.upload_out = Some(r);
        }
    }

    fn body(&mut self, ui: &mut Ui, t: &Tokens, host: &mut Host<'_>) {
        crate::shell::topbar::help::strip(ui, t, "debug");
        ui.add_space(theme::GAP);
        if let Some((tone, m)) = &self.msg {
            let c = match tone {
                Tone::Crit => t.crit,
                Tone::Ok => t.ok,
                _ => t.fg_dim,
            };
            ui.label(RichText::new(m).color(c));
            ui.add_space(6.0);
        }
        self.raw_card(ui, t, host);
        self.sim_card(ui, t, host);
        self.autodebug_card(ui, t, host);
        self.log_card(ui, t, host);
        self.upload_card(ui, t, host);
    }

    // -- raw request ------------------------------------------------------

    fn raw_card(&mut self, ui: &mut Ui, t: &Tokens, host: &mut Host<'_>) {
        card(ui, t, icons::TERMINAL, "Raw request (read-only)", |ui| {
            let target = host.target();
            ui.horizontal(|ui| {
                ui.label(RichText::new("Target").color(t.fg_dim));
                match &target {
                    Ok(tg) => ui.label(
                        RichText::new(tg.to_string())
                            .font(font(MONO_MEDIUM, 12.5))
                            .color(t.fg),
                    ),
                    Err(e) => ui.label(RichText::new(e).color(t.crit)),
                };
                muted(ui, t, "(gear menu → Adapter settings)");
            });
            let busy = self.raw_job.is_some();
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.req)
                        .font(font(MONO_MEDIUM, 13.0))
                        .hint_text("22 F1 90")
                        .desired_width(260.0),
                );
                let go = ui
                    .add_enabled_ui(!busy && target.is_ok(), |ui| {
                        theme::primary_button(ui, t, Some(icons::PAPER_PLANE_RIGHT), "Send")
                    })
                    .inner;
                if go.clicked() {
                    self.send_raw(host);
                }
                if busy {
                    ui.spinner();
                }
            });
            match &self.raw_out {
                Some(Ok(h)) => mono_line(ui, &format!("<- {h}"), t.ok),
                Some(Err(e)) => mono_line(ui, e, t.crit),
                None => {}
            }
            muted(
                ui,
                t,
                &format!(
                    "Read services only ({}); anything else is refused before it is sent. \
                     This panel has no write path: ECU changes go through a module panel's \
                     confirmation and vault backup.",
                    backend::read_services()
                ),
            );
        });
    }

    fn send_raw(&mut self, host: &mut Host<'_>) {
        let req = match backend::parse_hex(&self.req) {
            Ok(r) => r,
            Err(e) => {
                self.raw_out = Some(Err(e));
                return;
            }
        };
        let (Some(adapter), Ok(target)) = (host.adapter(), host.target()) else {
            self.raw_out = Some(Err("no adapter or no target module".into()));
            return;
        };
        let channel = host.channel().clone();
        self.raw_out = None;
        self.raw_job = Some(host.spawn_job("debug raw request", move |_| {
            backend::raw_request(&channel, &*adapter, target, req)
        }));
    }

    // -- simulator --------------------------------------------------------

    fn sim_card(&mut self, ui: &mut Ui, t: &Tokens, host: &mut Host<'_>) {
        card(ui, t, icons::FLASK, "Offline UDS simulator", |ui| {
            muted(
                ui,
                t,
                "An in-memory UDS server: try requests without a car. Seed DIDs as ID=HEX, \
                 one request per line.",
            );
            ui.columns(2, |c| {
                c[0].label(
                    RichText::new("DIDs")
                        .font(font(UI_STRONG, 12.0))
                        .color(t.fg_dim),
                );
                c[0].add(
                    egui::TextEdit::multiline(&mut self.sim_dids)
                        .font(font(MONO_MEDIUM, 12.0))
                        .desired_rows(4)
                        .desired_width(f32::INFINITY),
                );
                c[1].label(
                    RichText::new("Requests")
                        .font(font(UI_STRONG, 12.0))
                        .color(t.fg_dim),
                );
                c[1].add(
                    egui::TextEdit::multiline(&mut self.sim_reqs)
                        .font(font(MONO_MEDIUM, 12.0))
                        .desired_rows(4)
                        .desired_width(f32::INFINITY),
                );
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.sim_sec, "Writes need security access");
                if theme::button(ui, t, Some(icons::PLAY), "Run").clicked() {
                    self.sim_out = Some(backend::simulate(
                        host.channel(),
                        &self.sim_dids,
                        &self.sim_reqs,
                        self.sim_sec,
                    ));
                }
            });
            match &self.sim_out {
                Some(Ok(rows)) => {
                    for (q, a) in rows {
                        let col = if a.first() == Some(&0x7F) {
                            t.warn
                        } else {
                            t.fg
                        };
                        let line = format!("-> {}   <- {}", backend::hexs(q), backend::hexs(a));
                        mono_line(ui, &line, col);
                    }
                }
                Some(Err(e)) => mono_line(ui, e, t.crit),
                None => {}
            }
        });
    }

    // -- auto-debug -------------------------------------------------------

    fn start_autodebug(&mut self, host: &Host<'_>) {
        self.ad.plan = autodebug::plan(host.capabilities().as_ref());
        self.ad.idx = 0;
        self.ad.results.clear();
        session::push(format!(
            "auto-debug start: {} ({} steps)",
            self.ad.vehicle.label(),
            self.ad.plan.len()
        ));
        self.enter_step();
    }

    fn enter_step(&mut self) {
        self.ad.phase = match self.ad.plan.get(self.ad.idx) {
            None => Phase::Done,
            Some(&i) if STEPS[i].car.is_some() => Phase::Prompt,
            Some(_) => Phase::Idle,
        };
    }

    fn next_step(&mut self) {
        self.ad.idx += 1;
        self.enter_step();
        if matches!(self.ad.phase, Phase::Done) {
            let n = |v| self.ad.results.iter().filter(|o| o.verdict == v).count();
            session::push(format!(
                "auto-debug done: {} pass, {} fail, {} info",
                n(Verdict::Pass),
                n(Verdict::Fail),
                n(Verdict::Info)
            ));
        }
    }

    fn run_step(&mut self, host: &Host<'_>) {
        let i = self.ad.plan[self.ad.idx];
        let (Some(adapter), Ok(target)) = (host.adapter(), host.target()) else {
            self.ad.results.push(Outcome::new(
                STEPS[i].title,
                Verdict::Fail,
                "no adapter or no target module",
            ));
            self.next_step();
            return;
        };
        let channel = host.channel().clone();
        self.ad.phase = Phase::Running(host.spawn_job("auto-debug step", move |_| {
            autodebug::run(
                &autodebug::Ctx {
                    channel: &channel,
                    adapter: &*adapter,
                    target,
                    pace: Duration::from_millis(400),
                },
                &STEPS[i],
            )
        }));
    }

    fn autodebug_card(&mut self, ui: &mut Ui, t: &Tokens, host: &mut Host<'_>) {
        card(ui, t, icons::LIST_CHECKS, "Auto-debug", |ui| {
            muted(
                ui,
                t,
                "A guided read-only self-test of the adapter and the car, matched to what the \
                 adapter can do. It says what each step does and what to do in the car; every \
                 request lands in the session log.",
            );
            let v = &mut self.ad.vehicle;
            ui.horizontal_wrapped(|ui| {
                for (s, hint, w) in [
                    (&mut v.make, "Make", 110.0),
                    (&mut v.model, "Model", 130.0),
                    (&mut v.year, "Year", 60.0),
                    (&mut v.engine, "Engine / code", 150.0),
                    (&mut v.notes, "Notes", 200.0),
                ] {
                    ui.add(
                        egui::TextEdit::singleline(s)
                            .hint_text(hint)
                            .desired_width(w),
                    );
                }
            });
            ui.add_space(4.0);
            let active = self.ad.active();
            ui.horizontal(|ui| {
                let start = ui
                    .add_enabled_ui(!active, |ui| {
                        theme::primary_button(ui, t, Some(icons::PLAY), "Start auto-debug")
                    })
                    .inner;
                if start.clicked() {
                    self.start_autodebug(host);
                }
                let done = matches!(self.ad.phase, Phase::Done) && !self.ad.results.is_empty();
                let save = ui
                    .add_enabled_ui(done, |ui| {
                        theme::button(ui, t, Some(icons::DOWNLOAD), "Save report…")
                    })
                    .inner;
                if save.clicked() {
                    self.save_report(host);
                }
            });
            // Steps without a car action start on their own.
            if matches!(self.ad.phase, Phase::Idle) && self.ad.idx < self.ad.plan.len() {
                self.run_step(host);
            }
            if let Some(&i) = self.ad.plan.get(self.ad.idx) {
                let s = &STEPS[i];
                ui.add_space(6.0);
                ui.label(
                    RichText::new(format!(
                        "Step {}/{}: {}",
                        self.ad.idx + 1,
                        self.ad.plan.len(),
                        s.title
                    ))
                    .font(font(UI_STRONG, 13.0))
                    .color(t.fg),
                );
                muted(ui, t, s.doing);
                match &self.ad.phase {
                    Phase::Prompt => {
                        ui.label(
                            RichText::new(format!(
                                "{} Do this in the car: {}",
                                icons::CAR,
                                s.car.unwrap_or_default()
                            ))
                            .color(t.accent),
                        );
                        ui.horizontal(|ui| {
                            if theme::primary_button(ui, t, None, "Done: run the check").clicked() {
                                self.run_step(host);
                            } else if theme::button(ui, t, None, "Skip").clicked() {
                                self.ad.results.push(Outcome::new(
                                    s.title,
                                    Verdict::Info,
                                    "skipped",
                                ));
                                self.next_step();
                            }
                        });
                    }
                    Phase::Running(_) => {
                        ui.spinner();
                    }
                    _ => {}
                }
            }
            if !self.ad.results.is_empty() {
                ui.add_space(6.0);
                egui::Grid::new("autodebug-results")
                    .num_columns(3)
                    .spacing([12.0, 4.0])
                    .striped(true)
                    .show(ui, |ui| {
                        for o in &self.ad.results {
                            ui.label(RichText::new(&o.title).color(t.fg));
                            let tone = match o.verdict {
                                Verdict::Pass => Tone::Ok,
                                Verdict::Fail => Tone::Crit,
                                Verdict::Info => Tone::Muted,
                            };
                            theme::pill(ui, t, o.verdict.label(), tone, true);
                            ui.label(
                                RichText::new(&o.detail)
                                    .font(font(MONO_MEDIUM, 11.5))
                                    .color(t.fg_dim),
                            );
                            ui.end_row();
                        }
                    });
            }
        });
    }

    fn save_report(&mut self, host: &mut Host<'_>) {
        let when = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let caps = host.capabilities();
        let rep = autodebug::report(
            when,
            &self.ad.vehicle,
            &host.link(),
            caps.as_ref(),
            &self.ad.results,
        );
        let name: String = (self.ad.vehicle.label().to_ascii_lowercase().chars())
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let Some(path) = host.pick_save(
            "Save the auto-debug report",
            &format!("opentune-autodebug-{name}.json"),
        ) else {
            return;
        };
        let data = serde_json::to_vec_pretty(&rep).unwrap_or_default();
        self.start_save("debug-save-report", path, data);
    }

    // -- session log ------------------------------------------------------

    fn log_card(&mut self, ui: &mut Ui, t: &Tokens, host: &mut Host<'_>) {
        card(ui, t, icons::LIST_DASHES, "Session log", |ui| {
            let lines = session::lines();
            ui.horizontal(|ui| {
                if theme::button(ui, t, Some(icons::COPY), "Copy").clicked() {
                    ui.ctx().copy_text(session::text());
                    self.msg = Some((Tone::Ok, "Session log copied".into()));
                }
                if theme::button(ui, t, Some(icons::DOWNLOAD), "Save…").clicked() {
                    if let Some(p) = host.pick_save("Save the session log", "opentune-debug.log") {
                        self.start_save("debug-save-log", p, session::text().into_bytes());
                    }
                }
                if theme::button(ui, t, Some(icons::TRASH), "Clear").clicked() {
                    session::clear();
                }
                muted(ui, t, &format!("{} lines, this run only", lines.len()));
            });
            ui.add_space(4.0);
            egui::Frame::new()
                .fill(t.surface2)
                .corner_radius(egui::CornerRadius::same(6))
                .inner_margin(Margin::same(8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    egui::ScrollArea::vertical()
                        .id_salt("debug-session-log")
                        .max_height(180.0)
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            if lines.is_empty() {
                                muted(ui, t, "Nothing yet.");
                            }
                            for l in &lines {
                                mono_line(ui, l, t.fg_dim);
                            }
                        });
                });
            muted(
                ui,
                t,
                "SecurityAccess seeds and keys and long hex payloads are masked before a \
                 line is kept. The local diagnostics log (crashes, startup steps) is not in \
                 the native app yet.",
            );
        });
    }

    fn start_save(&mut self, key: &'static str, path: PathBuf, data: Vec<u8>) {
        if path.exists() {
            self.save = Some(Save { key, path, data });
            return;
        }
        self.msg = Some(match std::fs::write(&path, &data) {
            Ok(()) => (Tone::Ok, format!("Saved {}", path.display())),
            Err(e) => (
                Tone::Crit,
                format!("Could not save {}: {e}", path.display()),
            ),
        });
    }

    /// Replacing an existing file: confirm, then the host's guarded write
    /// (the old contents go to the vault first).
    fn finish_save(&mut self, host: &mut Host<'_>) {
        let Some(s) = &self.save else {
            return;
        };
        let req = ConfirmRequest {
            key: s.key.into(),
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
                    self.msg = Some(match host.write_file(ack, &s.path, &s.data) {
                        Ok(_) => (Tone::Ok, format!("Saved {}", s.path.display())),
                        Err(e) => (Tone::Crit, format!("Not saved: {e}")),
                    });
                }
            }
        }
    }

    // -- diagnostics report -----------------------------------------------

    fn upload_card(&mut self, ui: &mut Ui, t: &Tokens, host: &mut Host<'_>) {
        card(
            ui,
            t,
            icons::PAPER_PLANE_RIGHT,
            "Diagnostics report",
            |ui| {
                let Some(sink) = host.diagnostics() else {
                    muted(
                        ui,
                        t,
                        "This build has no upload target: nothing is ever sent. Copy or save the \
                     session log to share it yourself.",
                    );
                    return;
                };
                muted(
                    ui,
                    t,
                    &format!(
                        "This build can send a report to {}. You see the redacted report and the \
                     destination first, every time; nothing is sent without your click.",
                        sink.destination()
                    ),
                );
                ui.add(
                    egui::TextEdit::multiline(&mut self.note)
                        .hint_text("What happened? (optional)")
                        .desired_rows(2)
                        .desired_width(f32::INFINITY),
                );
                ui.horizontal(|ui| {
                    let busy = self.upload_job.is_some() || self.send_now.is_some();
                    let preview = ui
                        .add_enabled_ui(!busy && self.pending.is_none(), |ui| {
                            theme::button(ui, t, Some(icons::MAGNIFYING_GLASS), "Preview report…")
                        })
                        .inner;
                    if preview.clicked() {
                        let raw = report::assemble(
                            host.channel(),
                            &host.link(),
                            &self.note,
                            &session::lines(),
                        );
                        self.pending = Some(report::prepare(sink, &raw, &report::redact_context()));
                    }
                    if busy {
                        ui.spinner();
                    }
                });
                match &self.upload_out {
                    Some(Ok(id)) => {
                        ui.label(RichText::new(format!("Sent: {id}")).color(t.ok));
                    }
                    Some(Err(e)) => {
                        ui.label(RichText::new(format!("Not sent: {e}")).color(t.crit));
                    }
                    None => {}
                }
            },
        );
    }

    fn upload_modal(&mut self, ctx: &egui::Context, t: &Tokens) {
        let Some(p) = &self.pending else {
            return;
        };
        let (mut send, mut cancel) = (false, false);
        let r = egui::Modal::new(egui::Id::unique("debug-upload-consent"))
            .frame(theme::panel_frame(t).inner_margin(Margin::same(18)))
            .show(ctx, |ui| {
                ui.set_width(560.0);
                ui.label(
                    RichText::new("Send this diagnostics report?")
                        .font(font(DISPLAY_MEDIUM, 16.0))
                        .color(t.fg),
                );
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("Destination").color(t.fg_dim));
                    ui.label(
                        RichText::new(&p.destination)
                            .font(font(MONO_MEDIUM, 12.5))
                            .color(t.fg),
                    );
                });
                muted(
                    ui,
                    t,
                    "Below is exactly what is sent, after redaction (paths, user and host \
                     names, addresses, VINs, part numbers, seeds and keys).",
                );
                ui.add_space(4.0);
                egui::Frame::new()
                    .fill(t.surface2)
                    .inner_margin(Margin::same(8))
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .max_height(320.0)
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                mono_line(ui, &p.report, t.fg);
                            });
                    });
                ui.add_space(12.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::primary_button(ui, t, Some(icons::PAPER_PLANE_RIGHT), "Send")
                        .clicked()
                    {
                        send = true;
                    }
                    if theme::button(ui, t, None, "Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if send {
            // Submitted at the start of the next frame (it needs the host).
            self.send_now = self.pending.take();
            ctx.request_repaint();
        } else if cancel || r.should_close() {
            self.pending = None;
            session::push("diagnostics report: cancelled, nothing sent");
        }
    }
}
