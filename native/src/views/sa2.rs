//! SA2 script: load, disassemble and run the user's own VAG SA2 seed/key
//! script offline, and (Bench tier) arm it as seed/key id `sa2` for live
//! sessions. Port of the webview app's SA2 panel (`feat_sa2.rs`,
//! `feat_sa2.js`); the operations are in [`ops`].
//!
//! OpenTune ships **no** SA2 script. A script is typed (hex or assembler),
//! loaded from a file, or picked from the SA2 candidates of the user's own
//! flash container (FRF / ZIP / ODX / SGO; an encrypted FRF needs the user's
//! own FRF key file). Nothing here talks to a vehicle.
//!
//! Arming is gated in [`ops::arm`] (the channel and an explicit
//! acknowledgement), not only hidden here: on a stable channel the card says
//! why and a call is refused. The armed script lives in the app's shared
//! seed/key lookup for this run only, so a service script naming
//! `"seedkey": "sa2"` unlocks with it ([`Host::unlocker`] resolves through the
//! same lookup).

pub(crate) mod ops;

use self::ops::{Inspection, Loaded, Preview, Source};
use crate::host::{Host, JobHandle, NativeView, Tier, ViewGroup};
use crate::icons;
use crate::theme::{self, font, Tokens, Tone, MONO_MEDIUM, UI_MEDIUM, UI_STRONG};
use eframe::egui::{self, Label, RichText, Ui, Vec2};
use std::path::PathBuf;

/// Registration tier, read before the view is built ([`Tier::Bench`]: only
/// alpha / bench builds register it).
pub const TIER: Tier = Tier::Public;

pub fn create() -> Box<dyn NativeView> {
    Box::new(Sa2View::default())
}

/// Trace rows drawn at most (the run keeps up to the limit field).
const TRACE_ROW_H: f32 = 18.0;

pub struct Sa2View {
    text: String,
    frf_key: String,
    origin: String,
    loaded: Option<Loaded>,
    chosen: Option<usize>,
    load_job: Option<JobHandle<Loaded>>,
    load_err: Option<String>,
    inspection: Option<Result<Inspection, String>>,
    seed: String,
    trace: bool,
    limit: String,
    preview: Option<Result<Preview, String>>,
    label: String,
    ack: bool,
    arm_msg: Option<(Tone, String)>,
}

impl Default for Sa2View {
    fn default() -> Self {
        Self {
            text: String::new(),
            frf_key: String::new(),
            origin: String::new(),
            loaded: None,
            chosen: None,
            load_job: None,
            load_err: None,
            inspection: None,
            seed: String::new(),
            trace: false,
            limit: ops::DEFAULT_TRACE_ROWS.to_string(),
            preview: None,
            label: String::new(),
            ack: false,
            arm_msg: None,
        }
    }
}

impl Sa2View {
    fn start_load(&mut self, host: &Host<'_>, src: Source) {
        self.load_err = None;
        self.loaded = None;
        self.chosen = None;
        self.load_job = Some(host.spawn_job("SA2 load", move |_| ops::load(&src)));
    }

    /// A finished load: show its candidates and use the preferred one.
    fn tick(&mut self) {
        let Some(r) = self.load_job.as_mut().and_then(|j| j.take()) else {
            return;
        };
        self.load_job = None;
        match r {
            Ok(l) => {
                self.origin = l.origin.clone();
                let pick = l.preferred();
                let container = l.source == "container";
                self.loaded = Some(l);
                match pick {
                    Some(i) if container => self.use_candidate(i),
                    Some(i) => {
                        self.use_candidate(i);
                        self.label = self.origin.clone();
                    }
                    None => {}
                }
            }
            Err(e) => self.load_err = Some(e),
        }
    }

    fn use_candidate(&mut self, i: usize) {
        let Some(l) = &self.loaded else { return };
        let Some(c) = l.candidates.get(i) else { return };
        if c.truncated {
            return;
        }
        self.text = c.hex.clone();
        if l.source == "container" {
            self.label = format!("{} #{} {}", l.origin, c.index, c.field);
        }
        self.chosen = Some(i);
        self.disassemble();
    }

    fn disassemble(&mut self) {
        self.inspection = Some(ops::inspect(&self.text));
        self.preview = None;
    }

    fn compute(&mut self) {
        let limit = self.limit.trim().parse::<usize>().ok();
        self.preview = Some(
            ops::inspect(&self.text)
                .and_then(|i| ops::preview(&i.bytes, &self.seed, self.trace, limit, None)),
        );
    }

    fn script_card(&mut self, ui: &mut Ui, host: &Host<'_>, t: &Tokens) {
        theme::section_label(ui, t, "Script");
        ui.add_space(4.0);
        let busy = self.load_job.is_some();
        ui.horizontal_wrapped(|ui| {
            let r = ui.add_enabled_ui(!busy, |ui| {
                theme::button(ui, t, Some(icons::FILE_CODE), "Load script file…")
            });
            if r.inner.clicked() {
                if let Some(p) = host.pick_file(
                    "SA2 script",
                    &[
                        ("Script", &["sa2", "txt", "hex", "bin"]),
                        ("All files", &["*"]),
                    ],
                ) {
                    self.start_load(host, Source::File(p));
                }
            }
            let r = ui.add_enabled_ui(!busy, |ui| {
                theme::button(ui, t, Some(icons::FILE_ARCHIVE), "Open container…")
            });
            if r.inner.clicked() {
                if let Some(p) = host.pick_file(
                    "Your flash container",
                    &[
                        ("Container", &["frf", "zip", "odx", "sgo"]),
                        ("All files", &["*"]),
                    ],
                ) {
                    let key = self.frf_key.trim();
                    let frf_key = (!key.is_empty()).then(|| PathBuf::from(key));
                    self.start_load(host, Source::Container { path: p, frf_key });
                }
            }
            ui.label(RichText::new("FRF key").color(t.fg_dim));
            ui.add(
                egui::TextEdit::singleline(&mut self.frf_key)
                    .hint_text("only for an encrypted .frf")
                    .desired_width(200.0),
            );
            if theme::icon_button(ui, t, icons::FOLDER_OPEN, "Choose your FRF key file").clicked() {
                if let Some(p) = host.pick_file("FRF key file", &[("All files", &["*"])]) {
                    self.frf_key = p.display().to_string();
                }
            }
            if busy {
                ui.spinner();
            }
        });
        ui.add(
            egui::TextEdit::multiline(&mut self.text)
                .font(egui::FontId::monospace(12.0))
                .desired_rows(4)
                .desired_width(f32::INFINITY)
                .hint_text(
                    "Hex (87 5A 5A 5A 5A 4C) or assembler, one instruction per line \
                     (eor 0x5A5A5A5A / finish)",
                ),
        );
        ui.horizontal(|ui| {
            if theme::primary_button(ui, t, Some(icons::CODE), "Disassemble").clicked() {
                self.disassemble();
            }
            if !self.origin.is_empty() {
                let mut o = format!("from {}", self.origin);
                if let Some(l) = &self.loaded {
                    if let Some(k) = &l.container_kind {
                        o.push_str(&format!(" · {k}"));
                    }
                }
                ui.label(RichText::new(o).small().color(t.fg_faint));
            }
        });
        if let Some(e) = &self.load_err {
            note(ui, t, Tone::Crit, e);
        }
        let mut pick = None;
        if let Some(l) = self.loaded.as_ref().filter(|l| l.source == "container") {
            if l.candidates.is_empty() {
                note(
                    ui,
                    t,
                    Tone::Muted,
                    l.note.unwrap_or("No SA2 script in this container."),
                );
            } else {
                note(
                    ui,
                    t,
                    Tone::Muted,
                    &format!(
                        "{} SA2 candidate{} in the container. Pick the one that decodes as a script.",
                        l.candidates.len(),
                        if l.candidates.len() == 1 { "" } else { "s" }
                    ),
                );
                egui::Grid::new("sa2-cands")
                    .striped(true)
                    .num_columns(6)
                    .spacing(Vec2::new(14.0, 5.0))
                    .show(ui, |ui| {
                        for h in ["#", "ODX field", "session", "bytes", "status", ""] {
                            head(ui, t, h);
                        }
                        ui.end_row();
                        for (i, c) in l.candidates.iter().enumerate() {
                            mono(ui, t, &c.index.to_string());
                            mono(ui, t, &c.field);
                            ui.label(c.session.as_deref().unwrap_or("–"));
                            mono(ui, t, &c.len.to_string());
                            ui.horizontal(|ui| {
                                if c.decodes {
                                    theme::pill(ui, t, "decodes", Tone::Ok, false);
                                    if c.lints > 0 {
                                        theme::pill(
                                            ui,
                                            t,
                                            &format!("{} lint(s)", c.lints),
                                            Tone::Muted,
                                            false,
                                        );
                                    }
                                } else {
                                    theme::pill(ui, t, "not a script", Tone::Crit, false)
                                        .on_hover_text(c.error.clone().unwrap_or_default());
                                }
                            });
                            let label = if self.chosen == Some(i) {
                                "In use"
                            } else {
                                "Use"
                            };
                            if ui
                                .add_enabled(!c.truncated, egui::Button::new(label))
                                .clicked()
                            {
                                pick = Some(i);
                            }
                            ui.end_row();
                        }
                    });
            }
        }
        if let Some(i) = pick {
            self.use_candidate(i);
        }
    }

    fn disasm_card(&self, ui: &mut Ui, t: &Tokens) {
        theme::section_label(ui, t, "Disassembly");
        ui.add_space(4.0);
        let Some(r) = &self.inspection else {
            note(
                ui,
                t,
                Tone::Muted,
                "Load or paste a script, then Disassemble.",
            );
            return;
        };
        let v = match r {
            Ok(v) => v,
            Err(e) => {
                note(ui, t, Tone::Crit, e);
                return;
            }
        };
        let n = v.rows.iter().filter(|r| !r.bytes.is_empty()).count();
        let mut head_line = format!(
            "{} bytes · {n} instructions · read as {}",
            v.bytes.len(),
            v.input
        );
        if let Some(e) = &v.error {
            head_line.push_str(&format!(" · disassembly stopped: {e}"));
        }
        note(
            ui,
            t,
            if v.decodes { Tone::Ok } else { Tone::Crit },
            &head_line,
        );
        for l in &v.lints {
            note(ui, t, Tone::Warn, &format!("lint: {l}"));
        }
        if let Some(src) = &v.source {
            egui::CollapsingHeader::new("As SA2 assembler")
                .id_salt("sa2-source")
                .show(ui, |ui| {
                    let mut s = src.as_str();
                    ui.add(
                        egui::TextEdit::multiline(&mut s)
                            .font(egui::FontId::monospace(12.0))
                            .desired_width(f32::INFINITY),
                    );
                });
        }
        egui::ScrollArea::vertical()
            .id_salt("sa2-disasm")
            .max_height(280.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                egui::Grid::new("sa2-rows")
                    .striped(true)
                    .num_columns(5)
                    .spacing(Vec2::new(14.0, 3.0))
                    .show(ui, |ui| {
                        for h in ["", "offset", "bytes", "instruction", "branch"] {
                            head(ui, t, h);
                        }
                        ui.end_row();
                        for r in &v.rows {
                            mono(
                                ui,
                                t,
                                &r.label
                                    .as_ref()
                                    .map(|l| format!("{l}:"))
                                    .unwrap_or_default(),
                            );
                            mono(ui, t, &format!("{:04X}", r.offset));
                            mono(ui, t, &r.bytes);
                            mono(ui, t, &format!("{}{}", "  ".repeat(r.depth), r.text));
                            mono(
                                ui,
                                t,
                                &r.target.map(|x| format!("→ {x:04X}")).unwrap_or_default(),
                            );
                            ui.end_row();
                        }
                    });
            });
    }

    fn run_card(&mut self, ui: &mut Ui, t: &Tokens) {
        theme::section_label(ui, t, "Seed → key");
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("seed (4 bytes hex)").color(t.fg_dim));
            ui.add(
                egui::TextEdit::singleline(&mut self.seed)
                    .font(egui::FontId::monospace(12.0))
                    .hint_text("e.g. 1A 2B 3C 4D")
                    .desired_width(130.0),
            );
            ui.checkbox(&mut self.trace, "step trace");
            ui.label(RichText::new("rows").color(t.fg_dim));
            ui.add(egui::TextEdit::singleline(&mut self.limit).desired_width(50.0));
            if theme::primary_button(ui, t, Some(icons::PLAY), "Compute key").clicked() {
                self.compute();
            }
        });
        let Some(r) = &self.preview else { return };
        let p = match r {
            Ok(p) => p,
            Err(e) => {
                note(ui, t, Tone::Crit, e);
                return;
            }
        };
        match p.key {
            Some(k) => {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("seed {:08X}  →  key", p.seed)).color(t.fg_dim));
                    ui.label(
                        RichText::new(ops::hexs(&k.to_be_bytes()))
                            .font(font(MONO_MEDIUM, 14.0))
                            .color(t.ok),
                    );
                    ui.label(
                        RichText::new(format!("({} steps)", p.steps))
                            .small()
                            .color(t.fg_faint),
                    );
                });
            }
            None => note(ui, t, Tone::Crit, p.error.as_deref().unwrap_or("no key")),
        }
        let Some(trace) = &p.trace else { return };
        if p.trace_truncated {
            note(
                ui,
                t,
                Tone::Muted,
                &format!("Showing the first {} of {} steps.", trace.len(), p.steps),
            );
        }
        ui.horizontal(|ui| {
            for (h, w) in [
                ("offset", 60.0),
                ("instruction", 200.0),
                ("register", 90.0),
                ("C", 24.0),
                ("loops", 40.0),
            ] {
                ui.add_sized(
                    Vec2::new(w, 16.0),
                    Label::new(
                        RichText::new(h.to_uppercase())
                            .font(font(UI_STRONG, 10.0))
                            .color(t.fg_faint),
                    ),
                );
            }
        });
        egui::ScrollArea::vertical()
            .id_salt("sa2-trace")
            .max_height(260.0)
            .auto_shrink([false, true])
            .show_rows(ui, TRACE_ROW_H, trace.len(), |ui, range| {
                for r in &trace[range] {
                    ui.horizontal(|ui| {
                        for (s, w) in [
                            (format!("{:04X}", r.offset), 60.0),
                            (r.op.clone(), 200.0),
                            (format!("{:08X}", r.register), 90.0),
                            (if r.carry { "1" } else { "0" }.to_string(), 24.0),
                            (r.loops.to_string(), 40.0),
                        ] {
                            ui.add_sized(
                                Vec2::new(w, TRACE_ROW_H - 2.0),
                                Label::new(
                                    RichText::new(s).font(font(MONO_MEDIUM, 11.5)).color(t.fg),
                                ),
                            );
                        }
                    });
                }
            });
    }

    fn arm_card(&mut self, ui: &mut Ui, host: &Host<'_>, t: &Tokens) {
        let st = ops::status(host.seedkeys, host.channel());
        ui.horizontal(|ui| {
            theme::section_label(ui, t, "Use for live sessions");
            theme::pill(ui, t, "Bench", Tone::Crit, false);
        });
        ui.add_space(4.0);
        ui.add(
            Label::new(
                RichText::new(
                    "Arms the script above as seed/key id `sa2` until the app closes: a service \
                     script with \"seedkey\": \"sa2\" (and a plugin asking for sa2) answers \
                     SecurityAccess with it. Not vehicle-tested. A wrong key counts against the \
                     module's attempt limit — check the key against a known seed first.",
                )
                .color(t.fg_dim),
            )
            .wrap(),
        );
        match &st.armed {
            Some(a) => note(
                ui,
                t,
                Tone::Ok,
                &format!(
                    "armed: {} bytes{}",
                    a.script.len(),
                    if a.label.is_empty() {
                        String::new()
                    } else {
                        format!(" · {}", a.label)
                    }
                ),
            ),
            None => note(ui, t, Tone::Muted, "No script armed."),
        }
        if st.shadowed_by_plugin {
            note(
                ui,
                t,
                Tone::Warn,
                "A plugin answers the id sa2 first: the armed script is not used.",
            );
        }
        if !st.live_allowed {
            note(
                ui,
                t,
                Tone::Warn,
                "Arming is a Bench-tier feature (alpha / bench builds) until it is \
                 vehicle-tested; the offline preview above works in every build.",
            );
        }
        let mut action: Option<Option<String>> = None;
        ui.add_enabled_ui(st.live_allowed, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("label").color(t.fg_dim));
                ui.add(
                    egui::TextEdit::singleline(&mut self.label)
                        .hint_text("where the script came from")
                        .desired_width(220.0),
                );
                ui.checkbox(
                    &mut self.ack,
                    "I understand this answers SecurityAccess on real modules",
                );
            });
            ui.horizontal(|ui| {
                if theme::primary_button(ui, t, Some(icons::KEY), "Arm as sa2").clicked() {
                    action = Some(Some(self.text.clone()));
                }
                if theme::button(ui, t, None, "Disarm").clicked() {
                    action = Some(None);
                }
            });
        });
        if let Some(script) = action {
            // The gate is in ops::arm (channel + acknowledgement), whatever
            // the UI enabled.
            // The script checks are in ops::arm; the arming itself (and the
            // channel gate again) in Host::arm_sa2.
            let r = ops::arm(script.as_deref(), &self.label, self.ack, host.channel())
                .and_then(|(armed, lints)| host.arm_sa2(armed).map(|()| lints));
            let armed = ops::status(host.seedkeys, host.channel()).armed.is_some();
            self.arm_msg = Some(match r {
                Ok(lints) if armed => (
                    Tone::Ok,
                    if lints.is_empty() {
                        "Armed as sa2 for this run.".into()
                    } else {
                        format!("Armed as sa2 for this run (lints: {}).", lints.join("; "))
                    },
                ),
                Ok(_) => (Tone::Muted, "Disarmed.".into()),
                Err(e) => (Tone::Crit, e),
            });
        }
        if let Some((tone, m)) = &self.arm_msg {
            note(ui, t, *tone, m);
        }
    }
}

impl NativeView for Sa2View {
    fn id(&self) -> &str {
        "sa2"
    }
    fn label(&self) -> &str {
        "SA2 script"
    }
    fn icon(&self) -> &str {
        icons::CODE
    }
    fn group(&self) -> ViewGroup {
        ViewGroup::Reference
    }
    fn tier(&self) -> Tier {
        TIER
    }
    fn ui(&mut self, ui: &mut Ui, host: &mut Host<'_>) {
        self.tick();
        if self.load_job.is_some() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(100));
        }
        let t = *host.tokens();
        theme::panel(
            ui,
            &t,
            "SA2 script",
            |_| {},
            |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("sa2-scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        egui::Frame::new()
                            .inner_margin(egui::Margin::symmetric(16, 12))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.add(
                                    Label::new(
                                        RichText::new(
                                            "Run your own VAG SA2 seed/key script offline: \
                                             disassemble it, check it and compute the key for a \
                                             seed. OpenTune ships no script — it comes from your \
                                             own flash container or your own notes.",
                                        )
                                        .font(font(UI_MEDIUM, 12.5))
                                        .color(t.fg_dim),
                                    )
                                    .wrap(),
                                );
                                ui.add_space(10.0);
                                self.script_card(ui, host, &t);
                                ui.add_space(14.0);
                                self.disasm_card(ui, &t);
                                ui.add_space(14.0);
                                self.run_card(ui, &t);
                                ui.add_space(14.0);
                                self.arm_card(ui, host, &t);
                            });
                    });
            },
        );
    }
}

fn note(ui: &mut Ui, t: &Tokens, tone: Tone, text: &str) {
    let c = match tone {
        Tone::Ok => t.ok,
        Tone::Warn => t.warn,
        Tone::Crit => t.crit,
        Tone::Accent => t.accent,
        Tone::Muted => t.fg_dim,
    };
    ui.add(Label::new(RichText::new(text).font(font(UI_MEDIUM, 12.5)).color(c)).wrap());
}

fn mono(ui: &mut Ui, t: &Tokens, s: &str) {
    ui.label(RichText::new(s).font(font(MONO_MEDIUM, 11.5)).color(t.fg));
}

fn head(ui: &mut Ui, t: &Tokens, s: &str) {
    ui.label(
        RichText::new(s.to_uppercase())
            .font(font(UI_STRONG, 10.0))
            .color(t.fg_faint),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::DiagTarget;
    use crate::host::{BuildChannel, HostShared};
    use crate::views::coding::simkit::Fixture;

    fn wait(view: &mut Sa2View) {
        for _ in 0..500 {
            view.tick();
            if view.load_job.is_none() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("load job did not finish");
    }

    #[test]
    fn a_loaded_file_is_disassembled_and_previewed() {
        let mut fx = Fixture::new("sa2-view");
        let p = fx.dir.join("s.sa2");
        std::fs::write(&p, "eor 0xFFFFFFFF\nfinish\n").unwrap();
        let mut v = Sa2View::default();
        fx.host(DiagTarget::ENGINE, |h| {
            v.start_load(h, Source::File(p.clone()))
        });
        wait(&mut v);
        assert_eq!(v.text, "87 FF FF FF FF 4C");
        assert!(v.label.starts_with("s.sa2"));
        assert!(matches!(&v.inspection, Some(Ok(i)) if i.decodes));
        v.seed = "00000001".into();
        v.compute();
        assert!(matches!(&v.preview, Some(Ok(p)) if p.key == Some(0xFFFF_FFFE)));
        // A bad seed is an error line, not a panic.
        v.seed = "01".into();
        v.compute();
        assert!(matches!(&v.preview, Some(Err(_))));
    }

    #[test]
    fn arming_through_the_host_lookup_follows_the_channel() {
        let mut fx = Fixture::new("sa2-arm");
        fx.shared = HostShared::new(BuildChannel::new(BuildChannel::STABLE), None);
        let keys = fx.keys.clone();
        // A script that passed the view's checks (as if on a bench build).
        let (script, _) = ops::arm(
            Some("87 FF FF FF FF 4C"),
            "t",
            true,
            &BuildChannel::new(BuildChannel::BENCH),
        )
        .unwrap();
        // The Host gate refuses it on a stable channel, whatever the caller
        // checked.
        let s1 = script.clone();
        let r = fx.host(DiagTarget::ENGINE, |h| h.arm_sa2(s1));
        assert!(r.unwrap_err().contains("Bench"));
        assert!(keys.armed_sa2().is_none());
        // Disarming is always allowed.
        assert!(fx.host(DiagTarget::ENGINE, |h| h.arm_sa2(None)).is_ok());
        fx.shared = HostShared::new(BuildChannel::new(BuildChannel::BENCH), None);
        fx.host(DiagTarget::ENGINE, |h| {
            h.arm_sa2(script).unwrap();
            // The app's unlocker path resolves the armed script.
            assert!(h.unlocker_for("sa2").is_ok());
        });
        assert!(keys.armed_sa2().is_some(), "clones share the armed script");
    }
}
