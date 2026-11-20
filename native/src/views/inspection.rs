//! Inspection: readiness, fault codes, freeze frame and mode 09 of every
//! OBD ECU (the Tauri app's OBD inspection, `obd_inspect`).
//!
//! Reads the emissions data a roadside / periodic check looks at, through
//! the core's [`opentune_core::obd::inspect`]: readiness monitors (mode `01`
//! PID `01` / `41`), stored, pending and permanent codes (modes `03`, `07`,
//! `0A`), freeze frame 0 (mode `02`) and vehicle information (mode `09`:
//! VIN, calibration ids, CVNs, ECU name). Either the app's target module or
//! every OBD address `7E0..7E7` (the modules that answer are listed); one
//! READ operation per module, in a background job. Read-only: every request
//! is on the read allow-list; clearing codes lives in the Service and
//! Autoscan views, behind their confirmation.

mod model;
#[cfg(test)]
mod tests;

use crate::host::{Host, JobHandle, NativeView, Tier, ViewGroup};
use crate::icons;
use crate::theme::{self, font, Tokens, Tone, MONO_MEDIUM, UI_STRONG};
use eframe::egui::{self, Color32, Margin, RichText, Ui, Vec2};
use model::{ModuleResult, Scope};
use opentune_core::obd::inspect::{DtcEntry, MonitorState};
use opentune_core::obd::Sections;

/// Registration tier, read before the view is built ([`Tier::Bench`]: only
/// alpha / bench builds register it).
pub const TIER: Tier = Tier::Bench;

pub fn create() -> Box<dyn NativeView> {
    Box::new(InspectionView::default())
}

pub struct InspectionView {
    scope: Scope,
    sections: Sections,
    job: Option<JobHandle<Vec<ModuleResult>>>,
    results: Option<Vec<ModuleResult>>,
    note: Option<(Tone, String)>,
}

impl Default for InspectionView {
    fn default() -> Self {
        Self {
            scope: Scope::Target,
            sections: Sections::ALL,
            job: None,
            results: None,
            note: None,
        }
    }
}

impl InspectionView {
    fn running(&mut self) -> bool {
        self.job.as_mut().is_some_and(JobHandle::is_running)
    }

    fn poll(&mut self) {
        if let Some(r) = self.job.as_mut().and_then(JobHandle::take) {
            self.job = None;
            match r {
                Ok(rs) => {
                    self.note = Some(if rs.is_empty() {
                        (Tone::Warn, "no OBD module answered".into())
                    } else {
                        (Tone::Muted, format!("{} module(s) read", rs.len()))
                    });
                    self.results = Some(rs);
                }
                Err(e) => self.note = Some((Tone::Crit, format!("read failed: {e}"))),
            }
        }
    }

    /// Read the chosen modules in a job.
    fn start(&mut self, host: &Host<'_>) -> Result<(), String> {
        if self.running() {
            return Err("a read is already running".into());
        }
        let s = self.sections;
        if !(s.readiness || s.dtcs || s.freeze || s.info) {
            return Err("choose at least one section".into());
        }
        let adapter = host
            .adapter()
            .ok_or("no adapter: connect a cable in the top bar first")?;
        let app = match (self.scope, host.target()) {
            (_, Ok(t)) => t,
            (Scope::AllObd, Err(_)) => crate::host::DiagTarget::ENGINE,
            (Scope::Target, Err(e)) => return Err(format!("no target module: {e}")),
        };
        let scope = self.scope;
        let mods = model::modules(scope, app);
        self.note = None;
        self.job = Some(host.spawn_job("OBD inspection", move |p| {
            model::read(&adapter, mods, scope == Scope::Target, s, Some(p))
        }));
        Ok(())
    }

    fn controls_ui(&mut self, ui: &mut Ui, host: &Host<'_>, running: bool) {
        let t = *host.tokens();
        theme::panel_frame(&t)
            .inner_margin(Margin::symmetric(14, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
                    ui.add_enabled_ui(!running, |ui| {
                        if theme::primary_button(ui, &t, Some(icons::MAGNIFYING_GLASS), "Read")
                            .on_hover_text("Read the chosen sections (read-only)")
                            .clicked()
                        {
                            if let Err(e) = self.start(host) {
                                self.note = Some((Tone::Crit, e));
                            }
                        }
                    });
                    if running {
                        if let Some(j) = &self.job {
                            if theme::button(ui, &t, Some(icons::STOP), "Cancel").clicked() {
                                j.cancel();
                            }
                            let (f, m) = j.progress();
                            ui.add(egui::Spinner::new().size(14.0).color(t.accent));
                            ui.label(
                                RichText::new(format!("{m} ({:.0} %)", f * 100.0)).color(t.fg_dim),
                            );
                        }
                    }
                    ui.separator();
                    ui.add_enabled_ui(!running, |ui| {
                        ui.selectable_value(&mut self.scope, Scope::Target, "Target module")
                            .on_hover_text("The app's target module (top bar)");
                        ui.selectable_value(&mut self.scope, Scope::AllObd, "All OBD ECUs")
                            .on_hover_text(
                                "Every emissions ECU address 7E0..7E7 (those that answer)",
                            );
                        ui.separator();
                        let s = &mut self.sections;
                        ui.checkbox(&mut s.readiness, "Readiness");
                        ui.checkbox(&mut s.dtcs, "Fault codes");
                        ui.checkbox(&mut s.freeze, "Freeze frame");
                        ui.checkbox(&mut s.info, "Vehicle info");
                    });
                    if let Some(rs) = self.results.as_ref().filter(|r| !r.is_empty()) {
                        if theme::icon_button(ui, &t, icons::COPY, "Copy the report as text")
                            .clicked()
                        {
                            ui.ctx().copy_text(model::report_text(rs));
                            self.note = Some((Tone::Muted, "report copied".into()));
                        }
                    }
                });
                if let Some((tone, n)) = &self.note {
                    ui.add_space(4.0);
                    let col = match tone {
                        Tone::Crit => t.crit,
                        Tone::Warn => t.warn,
                        _ => t.fg_dim,
                    };
                    ui.add(egui::Label::new(RichText::new(n).color(col)).wrap());
                }
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "Read-only. To clear codes use the Service or Autoscan view (with a \
                         confirmation and a saved report).",
                    )
                    .small()
                    .color(t.fg_faint),
                );
            });
    }
}

impl NativeView for InspectionView {
    fn id(&self) -> &str {
        "inspection"
    }
    fn label(&self) -> &str {
        "Inspection"
    }
    fn icon(&self) -> &str {
        icons::CHECK_CIRCLE
    }
    fn group(&self) -> ViewGroup {
        ViewGroup::Vehicle
    }
    fn tier(&self) -> Tier {
        TIER
    }
    fn count(&self) -> Option<String> {
        let rs = self.results.as_ref()?;
        let s = model::summary(rs);
        let n = s.stored + s.pending + s.permanent;
        (n > 0).then(|| n.to_string())
    }
    fn ui(&mut self, ui: &mut egui::Ui, host: &mut Host<'_>) {
        self.poll();
        let running = self.running();
        let host: &Host<'_> = host;
        let t = *host.tokens();
        egui::ScrollArea::vertical()
            .id_salt("inspection-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Frame::new()
                    .inner_margin(Margin::same(theme::GAP as i8))
                    .show(ui, |ui| {
                        self.controls_ui(ui, host, running);
                        match &self.results {
                            None => {
                                ui.add_space(theme::GAP);
                                theme::empty_state(
                                    ui,
                                    &t,
                                    icons::CHECK_CIRCLE,
                                    "OBD inspection",
                                    "Readiness monitors, fault codes, freeze frame and vehicle \
                                     information of the target module or every OBD ECU.",
                                );
                            }
                            Some(rs) => {
                                ui.add_space(theme::GAP);
                                summary_ui(ui, &t, &model::summary(rs));
                                for r in rs {
                                    ui.add_space(theme::GAP);
                                    module_ui(ui, &t, r);
                                }
                            }
                        }
                    });
            });
        if running {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(150));
        }
    }
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

fn card<R>(ui: &mut Ui, t: &Tokens, title: &str, body: impl FnOnce(&mut Ui) -> R) -> R {
    theme::panel_frame(t)
        .inner_margin(Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                RichText::new(title)
                    .font(font(theme::DISPLAY_MEDIUM, 13.5))
                    .color(t.fg),
            );
            ui.add_space(6.0);
            body(ui)
        })
        .inner
}

fn summary_ui(ui: &mut Ui, t: &Tokens, s: &model::Summary) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
        theme::pill(
            ui,
            t,
            &format!("{} module(s)", s.modules),
            Tone::Muted,
            false,
        );
        theme::pill(
            ui,
            t,
            if s.mil_on { "MIL on" } else { "MIL off" },
            if s.mil_on { Tone::Crit } else { Tone::Ok },
            false,
        );
        let tone = |n: usize, bad: Tone| if n > 0 { bad } else { Tone::Muted };
        theme::pill(
            ui,
            t,
            &format!("{} stored", s.stored),
            tone(s.stored, Tone::Crit),
            false,
        );
        theme::pill(
            ui,
            t,
            &format!("{} pending", s.pending),
            tone(s.pending, Tone::Warn),
            false,
        );
        theme::pill(
            ui,
            t,
            &format!("{} permanent", s.permanent),
            tone(s.permanent, Tone::Warn),
            false,
        );
        theme::pill(
            ui,
            t,
            &format!("{} monitor(s) incomplete", s.incomplete),
            tone(s.incomplete, Tone::Warn),
            false,
        );
    });
}

fn state_color(t: &Tokens, s: MonitorState) -> Color32 {
    match s {
        MonitorState::Complete => t.ok,
        MonitorState::Incomplete => t.warn,
        MonitorState::NotSupported => t.fg_faint,
    }
}

fn kv(ui: &mut Ui, t: &Tokens, k: &str, v: &str) {
    ui.label(RichText::new(k).color(t.fg_dim));
    ui.label(RichText::new(v).font(font(MONO_MEDIUM, 12.0)).color(t.fg));
    ui.end_row();
}

fn dtc_rows(ui: &mut Ui, t: &Tokens, what: &str, list: &[DtcEntry], err: Option<&String>) {
    ui.label(
        RichText::new(what)
            .font(font(UI_STRONG, 12.0))
            .color(t.fg_dim),
    );
    if let Some(e) = err {
        ui.label(RichText::new(format!("not read: {e}")).color(t.fg_faint));
    } else if list.is_empty() {
        ui.label(RichText::new("none").color(t.fg_faint));
    } else {
        for c in list {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(&c.code)
                        .font(font(MONO_MEDIUM, 12.5))
                        .color(t.fg),
                );
                ui.label(RichText::new(&c.description).color(t.fg_dim));
            });
        }
    }
    ui.add_space(4.0);
}

fn module_ui(ui: &mut Ui, t: &Tokens, r: &ModuleResult) {
    let i = &r.ins;
    card(ui, t, &r.module.label, |ui| {
        if !i.responded {
            ui.label(RichText::new("no answer").color(t.fg_faint));
            return;
        }
        let id = ui.make_persistent_id(r.module.label.as_str());
        if let Some(rd) = &i.readiness {
            ui.horizontal_wrapped(|ui| {
                theme::pill(
                    ui,
                    t,
                    if rd.status.mil { "MIL on" } else { "MIL off" },
                    if rd.status.mil { Tone::Crit } else { Tone::Ok },
                    false,
                );
                theme::pill(
                    ui,
                    t,
                    &format!("{} stored code(s)", rd.status.dtc_count),
                    Tone::Muted,
                    false,
                );
                theme::pill(
                    ui,
                    t,
                    &format!("{:?} ignition", rd.status.ignition).to_lowercase(),
                    Tone::Muted,
                    false,
                );
            });
            egui::CollapsingHeader::new("Readiness monitors")
                .id_salt(id.with("rd"))
                .default_open(true)
                .show(ui, |ui| {
                    egui::Grid::new(id.with("rd-grid"))
                        .num_columns(3)
                        .spacing(Vec2::new(18.0, 4.0))
                        .show(ui, |ui| {
                            ui.label(RichText::new("Monitor").color(t.fg_faint));
                            ui.label(RichText::new("Since clear").color(t.fg_faint));
                            ui.label(RichText::new("This drive cycle").color(t.fg_faint));
                            ui.end_row();
                            for m in &rd.status.monitors {
                                ui.label(RichText::new(m.name).color(t.fg));
                                ui.label(
                                    RichText::new(model::state_text(m.state))
                                        .color(state_color(t, m.state)),
                                );
                                match rd
                                    .this_cycle
                                    .as_ref()
                                    .and_then(|c| c.monitors.iter().find(|x| x.key == m.key))
                                {
                                    Some(x) => ui.label(
                                        RichText::new(model::state_text(x.state))
                                            .color(state_color(t, x.state)),
                                    ),
                                    None => ui.label(RichText::new("—").color(t.fg_faint)),
                                };
                                ui.end_row();
                            }
                        });
                    if let Some(e) = &rd.this_cycle_error {
                        ui.label(
                            RichText::new(format!("this drive cycle not reported: {e}"))
                                .small()
                                .color(t.fg_faint),
                        );
                    }
                });
        }
        if let Some(d) = &i.dtcs {
            egui::CollapsingHeader::new("Fault codes")
                .id_salt(id.with("dtc"))
                .default_open(true)
                .show(ui, |ui| {
                    dtc_rows(ui, t, "Stored", &d.stored, d.errors.get("stored"));
                    dtc_rows(ui, t, "Pending", &d.pending, d.errors.get("pending"));
                    dtc_rows(ui, t, "Permanent", &d.permanent, d.errors.get("permanent"));
                });
        }
        if let Some(f) = &i.freeze_frame {
            egui::CollapsingHeader::new("Freeze frame")
                .id_salt(id.with("ff"))
                .default_open(f.stored)
                .show(ui, |ui| {
                    if !f.stored {
                        ui.label(RichText::new("none stored").color(t.fg_faint));
                        return;
                    }
                    if let Some(d) = &f.dtc {
                        ui.label(
                            RichText::new(format!("stored by {} {}", d.code, d.description))
                                .color(t.fg_dim),
                        );
                    }
                    egui::Grid::new(id.with("ff-grid"))
                        .num_columns(2)
                        .spacing(Vec2::new(18.0, 3.0))
                        .show(ui, |ui| {
                            for v in &f.values {
                                let val = match v.value.as_ref().and_then(|x| x.first()) {
                                    Some(x) => format!("{} {}", crate::image::fmt_val(x), v.unit),
                                    None => format!("raw {}", v.raw),
                                };
                                kv(ui, t, &v.name, &val);
                            }
                        });
                    for (k, e) in &f.errors {
                        ui.label(RichText::new(format!("{k}: {e}")).small().color(t.fg_faint));
                    }
                });
        }
        if let Some(v) = &i.info {
            egui::CollapsingHeader::new("Vehicle information")
                .id_salt(id.with("info"))
                .default_open(true)
                .show(ui, |ui| {
                    egui::Grid::new(id.with("info-grid"))
                        .num_columns(2)
                        .spacing(Vec2::new(18.0, 3.0))
                        .show(ui, |ui| {
                            if let Some(n) = &v.ecu_name {
                                kv(ui, t, "ECU name", n);
                            }
                            if let Some(x) = &v.vin {
                                kv(ui, t, "VIN", x);
                            }
                            for (k, c) in v.calids.iter().enumerate() {
                                kv(ui, t, &format!("CALID {}", k + 1), c);
                                if let Some(cvn) = v.cvns.get(k) {
                                    kv(ui, t, &format!("CVN {}", k + 1), cvn);
                                }
                            }
                            for (k, c) in v.cvns.iter().enumerate().skip(v.calids.len()) {
                                kv(ui, t, &format!("CVN {}", k + 1), c);
                            }
                        });
                    for (k, e) in &v.errors {
                        ui.label(RichText::new(format!("{k}: {e}")).small().color(t.fg_faint));
                    }
                });
        }
        for (k, e) in &i.errors {
            ui.label(RichText::new(format!("{k}: {e}")).color(t.warn));
        }
    });
}
