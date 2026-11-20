//! Analyze bin: the full offline report of a dump (the opened one, or any
//! file): structural family with its evidence, identity strings with the
//! offsets they were read at, definition and VEHICLE CATALOG candidates, the
//! checksum blocks of the detected family, a "match HW / SW" box, and the
//! file-target flash (write a data file into an image file at an address).
//!
//! Everything here is offline: nothing is sent to an adapter. Scans and file
//! reads run as host jobs. The only file write (the file-target flash) goes
//! through `Host::confirm` + `Host::write_file` (vault backup first, atomic
//! replace) and is read back afterwards.

mod report;
mod write;

use crate::host::{
    ConfirmRequest, ConfirmResult, DataRoot, Host, HostAction, JobHandle, NativeView, Tier,
    ViewGroup,
};
use crate::icons;
use crate::theme::{self, font, Tokens, Tone, MONO_MEDIUM, UI_MEDIUM, UI_STRONG};
use crate::views::maps::{kv, two_pane};
use eframe::egui::{self, Color32, Label, Margin, RichText, Ui, UiBuilder, Vec2};
use report::{Catalog, CatalogCand, DefCand, Report};
use std::path::{Path, PathBuf};
use write::Prepared;

/// Registration tier, read before the view is built ([`Tier::Bench`]: only
/// alpha / bench builds register it).
pub const TIER: Tier = Tier::Public;

pub fn create() -> Box<dyn NativeView> {
    Box::new(AnalyzeView::default())
}

/// The confirmation key of the file-target flash.
const WRITE_KEY: &str = "analyze-write-region";
/// Definition maps listed under the first candidate at most.
const MAX_MAPS: usize = 80;

/// What the last analysis of the OPENED dump saw (re-analysed on change).
#[derive(Clone, Debug, PartialEq, Eq)]
struct OpenedKey {
    path: Option<PathBuf>,
    len: usize,
    edited: bool,
}

fn opened_key(host: &Host<'_>) -> Option<OpenedKey> {
    host.image().map(|b| OpenedKey {
        path: host.image_path().map(Path::to_path_buf),
        len: b.len(),
        edited: host.image_edited(),
    })
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

/// The file-target flash form.
#[derive(Default)]
struct WriteForm {
    target: Option<PathBuf>,
    data: Option<PathBuf>,
    addr: String,
    prep: Option<JobHandle<Prepared>>,
    /// Prepared, waiting for the confirmation.
    pending: Option<Prepared>,
    /// Read-back after the write: (target, reopen it in Flash).
    verify: Option<(JobHandle<()>, PathBuf, bool)>,
    msg: Option<(Tone, String)>,
}

#[derive(Default)]
pub struct AnalyzeView {
    /// The vehicle catalog, loaded by the first analysis.
    catalog: Option<Catalog>,
    /// A checksum algorithm chosen by the user (`None`: from the family).
    algo: Option<String>,
    job: Option<JobHandle<(Report, Catalog)>>,
    report: Option<Report>,
    error: Option<String>,
    /// The user analysed a picked file: stop following the opened dump.
    pinned_file: bool,
    analysed: Option<OpenedKey>,
    hw: String,
    sw: String,
    typed: Option<(Option<DefCand>, Vec<CatalogCand>)>,
    cat_job: Option<JobHandle<Catalog>>,
    form: WriteForm,
}

impl NativeView for AnalyzeView {
    fn id(&self) -> &str {
        "analyze"
    }
    fn label(&self) -> &str {
        "Analyze bin"
    }
    fn icon(&self) -> &str {
        icons::MAGNIFYING_GLASS
    }
    fn group(&self) -> ViewGroup {
        ViewGroup::Calibration
    }
    fn tier(&self) -> Tier {
        TIER
    }
    fn ui(&mut self, ui: &mut egui::Ui, host: &mut Host<'_>) {
        let t = *host.tokens();
        self.poll(host);
        if !self.pinned_file && self.job.is_none() {
            if let Some(k) = opened_key(host) {
                if self.analysed.as_ref() != Some(&k) {
                    self.start(host, None);
                }
            }
        }
        let (left, right) = two_pane(ui);
        ui.scope_builder(UiBuilder::new().max_rect(left), |ui| {
            self.side_pane(ui, &t, host)
        });
        ui.scope_builder(UiBuilder::new().max_rect(right), |ui| {
            self.report_pane(ui, &t, host)
        });
        self.confirm_write(host);
    }
}

impl AnalyzeView {
    /// Collect finished jobs.
    fn poll(&mut self, host: &mut Host<'_>) {
        if let Some(r) = self.job.as_mut().and_then(JobHandle::take) {
            self.job = None;
            match r {
                Ok((rep, cat)) => {
                    self.catalog = Some(cat);
                    self.report = Some(rep);
                    self.error = None;
                }
                Err(e) => self.error = Some(e),
            }
        }
        if let Some(r) = self.cat_job.as_mut().and_then(JobHandle::take) {
            self.cat_job = None;
            match r {
                Ok(cat) => {
                    self.catalog = Some(cat);
                    self.match_typed(host);
                }
                Err(e) => self.error = Some(e),
            }
        }
        if let Some(r) = self.form.prep.as_mut().and_then(JobHandle::take) {
            self.form.prep = None;
            match r {
                Ok(p) => self.form.pending = Some(p),
                Err(e) => self.form.msg = Some((Tone::Crit, e)),
            }
        }
        let verified = self.form.verify.as_mut().and_then(|(h, _, _)| h.take());
        if let Some(r) = verified {
            let (_, target, reopen) = self.form.verify.take().expect("verify job");
            match r {
                Ok(()) => {
                    self.form.msg = Some((
                        Tone::Ok,
                        format!("Written and read back OK: {}", file_name(&target)),
                    ));
                    if reopen {
                        // The Flash view holds the old bytes: reload them.
                        host.act(HostAction::OpenImage(target));
                    }
                }
                Err(e) => self.form.msg = Some((Tone::Crit, e)),
            }
        }
    }

    /// Analyse the opened dump (`file` None) or a file, in a job.
    fn start(&mut self, host: &Host<'_>, file: Option<PathBuf>) {
        let (name, bytes) = match &file {
            None => {
                let Some(img) = host.image() else { return };
                self.analysed = opened_key(host);
                self.pinned_file = false;
                let name = host
                    .image_path()
                    .map(file_name)
                    .unwrap_or_else(|| "opened dump".into());
                (name, Some(img.to_vec()))
            }
            Some(p) => {
                self.pinned_file = true;
                (file_name(p), None)
            }
        };
        if let Some(h) = &self.job {
            h.cancel();
        }
        let defs = host.definitions().to_vec();
        let resolvers = host.checksums.to_vec();
        let catalog = self.catalog.clone();
        let dir = host.data_dir(DataRoot::Catalog).map(Path::to_path_buf);
        let algo = self.algo.clone();
        self.error = None;
        self.job = Some(host.spawn_job("Analyze bin", move |p| {
            let catalog = match catalog {
                Some(c) => c,
                None => {
                    p.set(0.05, "loading the vehicle catalog");
                    report::load_catalog(dir.as_deref())
                }
            };
            let bytes = match (bytes, &file) {
                (Some(b), _) => b,
                (None, Some(f)) => {
                    p.set(0.3, format!("reading {}", file_name(f)));
                    write::read_limited(f, "file")?
                }
                (None, None) => return Err("nothing to analyse".into()),
            };
            if p.cancelled() {
                return Err("cancelled".into());
            }
            p.set(0.5, "scanning");
            let rep = report::analyze(&report::Input {
                name,
                file,
                bytes,
                defs,
                catalog: catalog.clone(),
                resolvers,
                algo,
            });
            Ok((rep, catalog))
        }));
    }

    /// Re-run the current analysis (e.g. another checksum algorithm).
    fn rerun(&mut self, host: &Host<'_>) {
        let file = self.report.as_ref().and_then(|r| r.file.clone());
        match file {
            Some(f) => self.start(host, Some(f)),
            None => self.start(host, None),
        }
    }

    fn match_typed(&mut self, host: &Host<'_>) {
        match &self.catalog {
            Some(c) => {
                self.typed = Some(report::match_typed(
                    host.definitions(),
                    &c.entries,
                    &self.hw,
                    &self.sw,
                ));
            }
            None if self.cat_job.is_none() => {
                let dir = host.data_dir(DataRoot::Catalog).map(Path::to_path_buf);
                self.cat_job = Some(host.spawn_job("Load vehicle catalog", move |_| {
                    Ok(report::load_catalog(dir.as_deref()))
                }));
            }
            None => {}
        }
    }

    // -- left column ---------------------------------------------------------

    fn side_pane(&mut self, ui: &mut Ui, t: &Tokens, host: &mut Host<'_>) {
        theme::panel(
            ui,
            t,
            "Analyze",
            |_| {},
            |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("analyze-side")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        egui::Frame::new()
                            .inner_margin(Margin::symmetric(14, 10))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                self.source_section(ui, t, host);
                                ui.add_space(14.0);
                                theme::hline(ui, t.border_soft);
                                ui.add_space(10.0);
                                self.match_section(ui, t, host);
                                ui.add_space(14.0);
                                theme::hline(ui, t.border_soft);
                                ui.add_space(10.0);
                                self.write_section(ui, t, host);
                            });
                    });
            },
        );
    }

    fn source_section(&mut self, ui: &mut Ui, t: &Tokens, host: &mut Host<'_>) {
        theme::section_label(ui, t, "Source");
        ui.add_space(4.0);
        let what = match &self.report {
            Some(r) if r.file.is_some() => format!("{} {}", icons::FILE, r.name),
            Some(r) => format!("{} {} (opened dump)", icons::FOLDER_OPEN, r.name),
            None if host.image().is_some() => "The opened dump".into(),
            None => "No dump is open".into(),
        };
        ui.add(Label::new(RichText::new(what).font(font(UI_MEDIUM, 12.5)).color(t.fg)).truncate());
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            let can = host.image().is_some();
            if ui
                .add_enabled_ui(can, |ui| {
                    theme::button(ui, t, Some(icons::ARROWS_CLOCKWISE), "Opened dump")
                })
                .inner
                .on_hover_text("Analyse the dump open in the Flash view (with its edits)")
                .on_disabled_hover_text("Open a dump first (Ctrl+O or drop a file)")
                .clicked()
            {
                self.start(host, None);
            }
            if theme::button(ui, t, Some(icons::FILE), "A file…")
                .on_hover_text("Analyse any image file (it is only read)")
                .clicked()
            {
                if let Some(p) = host.pick_file(
                    "Image to analyse",
                    &[
                        ("ECU image", &["bin", "ori", "mod", "hex", "rom"]),
                        ("All", &["*"]),
                    ],
                ) {
                    self.start(host, Some(p));
                }
            }
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("CHECKSUM")
                    .font(font(UI_STRONG, 10.5))
                    .color(t.fg_faint),
            );
            let current = self
                .algo
                .clone()
                .unwrap_or_else(|| "Auto (from family)".into());
            let mut pick: Option<Option<String>> = None;
            egui::ComboBox::from_id_salt("analyze-algo")
                .selected_text(current)
                .width(ui.available_width().min(200.0))
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(self.algo.is_none(), "Auto (from family)")
                        .clicked()
                    {
                        pick = Some(None);
                    }
                    for a in opentune_checksum::algorithms() {
                        let on = self.algo.as_deref() == Some(a.id);
                        if ui
                            .selectable_label(on, format!("{}  —  {}", a.id, a.name))
                            .clicked()
                        {
                            pick = Some(Some(a.id.to_string()));
                        }
                    }
                });
            if let Some(p) = pick {
                if p != self.algo {
                    self.algo = p;
                    if self.report.is_some() {
                        self.rerun(host);
                    }
                }
            }
        });
        if let Some(h) = &self.job {
            let (frac, msg) = h.progress();
            ui.add_space(8.0);
            ui.add(
                egui::ProgressBar::new(frac)
                    .desired_height(6.0)
                    .fill(t.accent),
            );
            ui.label(RichText::new(msg).small().color(t.fg_dim));
        }
        if let Some(e) = &self.error {
            ui.add_space(6.0);
            ui.add(Label::new(RichText::new(e).color(t.crit)).wrap());
        }
    }

    fn match_section(&mut self, ui: &mut Ui, t: &Tokens, host: &mut Host<'_>) {
        theme::section_label(ui, t, "Match HW / SW");
        ui.add_space(4.0);
        ui.label(
            RichText::new("Type a hardware / part or software number to look it up in the definitions and the vehicle catalog.")
                .small()
                .color(t.fg_dim),
        );
        ui.add_space(6.0);
        let w = ui.available_width();
        let mut go = false;
        for (v, hint) in [
            (&mut self.hw, "HW / part number"),
            (&mut self.sw, "SW number"),
        ] {
            let r = ui.add(
                egui::TextEdit::singleline(v)
                    .hint_text(hint)
                    .font(font(MONO_MEDIUM, 12.0))
                    .desired_width(w),
            );
            go |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        }
        ui.add_space(4.0);
        let empty = self.hw.trim().is_empty() && self.sw.trim().is_empty();
        if ui
            .add_enabled_ui(!empty, |ui| {
                theme::button(ui, t, Some(icons::MAGNIFYING_GLASS), "Match")
            })
            .inner
            .clicked()
        {
            go = true;
        }
        if go && !empty {
            self.match_typed(host);
        }
        if self.cat_job.is_some() {
            ui.label(
                RichText::new("Loading the vehicle catalog…")
                    .small()
                    .color(t.fg_dim),
            );
        }
        let Some((def, cat)) = &self.typed else {
            return;
        };
        ui.add_space(6.0);
        match def {
            Some(d) => {
                ui.horizontal(|ui| {
                    theme::pill(ui, t, "DEF", Tone::Accent, false);
                    ui.add(Label::new(RichText::new(&d.name).color(t.fg)).truncate())
                        .on_hover_text(&d.id);
                });
                if theme::button(ui, t, None, "Use this definition").clicked() {
                    host.act(HostAction::SelectDefinition(d.id.clone()));
                    host.status(format!("Flash view: definition {} selected", d.name));
                }
            }
            None => {
                ui.label(RichText::new("No definition matches.").color(t.fg_dim));
            }
        }
        ui.add_space(4.0);
        if cat.is_empty() {
            ui.label(RichText::new("No vehicle catalog entry matches.").color(t.fg_dim));
        }
        for c in cat {
            catalog_line(ui, t, c);
        }
    }

    fn write_section(&mut self, ui: &mut Ui, t: &Tokens, host: &mut Host<'_>) {
        theme::section_label(ui, t, "Write file into image");
        ui.add_space(4.0);
        ui.add(
            Label::new(
                RichText::new(
                    "Writes a data file into an image FILE at an address. The file's current \
                     contents go to the vault first; nothing is sent to an ECU.",
                )
                .small()
                .color(t.fg_dim),
            )
            .wrap(),
        );
        ui.add_space(6.0);
        let busy =
            self.form.prep.is_some() || self.form.pending.is_some() || self.form.verify.is_some();
        egui::Grid::new("analyze-write")
            .num_columns(2)
            .spacing(Vec2::new(10.0, 6.0))
            .show(ui, |ui| {
                ui.label(
                    RichText::new("IMAGE")
                        .font(font(UI_STRONG, 10.5))
                        .color(t.fg_faint),
                );
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled_ui(!busy, |ui| {
                            theme::button(ui, t, Some(icons::FILE), "Pick…")
                        })
                        .inner
                        .clicked()
                    {
                        if let Some(p) = host.pick_file(
                            "Target image file",
                            &[
                                ("ECU image", &["bin", "ori", "mod", "rom"]),
                                ("All", &["*"]),
                            ],
                        ) {
                            self.form.target = Some(p);
                        }
                    }
                    if let Some(p) = host.image_path().map(Path::to_path_buf) {
                        if ui
                            .add_enabled_ui(!busy, |ui| theme::button(ui, t, None, "Opened"))
                            .inner
                            .on_hover_text("The opened dump's file on disk")
                            .clicked()
                        {
                            self.form.target = Some(p);
                        }
                    }
                });
                ui.end_row();
                ui.label("");
                path_label(ui, t, self.form.target.as_deref(), "no image chosen");
                ui.end_row();
                ui.label(
                    RichText::new("DATA")
                        .font(font(UI_STRONG, 10.5))
                        .color(t.fg_faint),
                );
                if ui
                    .add_enabled_ui(!busy, |ui| {
                        theme::button(ui, t, Some(icons::FILE_ARROW_UP), "Pick…")
                    })
                    .inner
                    .clicked()
                {
                    if let Some(p) = host.pick_file(
                        "Data file to write",
                        &[("Data", &["bin", "dat", "cal"]), ("All", &["*"])],
                    ) {
                        self.form.data = Some(p);
                    }
                }
                ui.end_row();
                ui.label("");
                path_label(ui, t, self.form.data.as_deref(), "no data file chosen");
                ui.end_row();
                ui.label(
                    RichText::new("ADDRESS")
                        .font(font(UI_STRONG, 10.5))
                        .color(t.fg_faint),
                );
                ui.add_enabled(
                    !busy,
                    egui::TextEdit::singleline(&mut self.form.addr)
                        .hint_text("0x1C000")
                        .font(font(MONO_MEDIUM, 12.0))
                        .desired_width(120.0),
                );
                ui.end_row();
            });
        ui.add_space(6.0);
        let ready = self.form.target.is_some() && self.form.data.is_some() && !busy;
        if ui
            .add_enabled_ui(ready, |ui| {
                theme::primary_button(ui, t, Some(icons::DOWNLOAD_SIMPLE), "Write…")
            })
            .inner
            .on_hover_text("Checks the address, then asks before writing")
            .clicked()
        {
            self.prepare_write(host);
        }
        if busy {
            ui.label(RichText::new("Working…").small().color(t.fg_dim));
        }
        if let Some((tone, msg)) = &self.form.msg {
            let c = match tone {
                Tone::Ok => t.ok,
                Tone::Crit => t.crit,
                Tone::Warn => t.warn,
                _ => t.fg_dim,
            };
            ui.add_space(4.0);
            ui.add(Label::new(RichText::new(msg).color(c)).wrap());
        }
    }

    fn prepare_write(&mut self, host: &Host<'_>) {
        let (Some(target), Some(data)) = (self.form.target.clone(), self.form.data.clone()) else {
            return;
        };
        self.form.msg = None;
        let addr = match write::parse_addr(&self.form.addr) {
            Ok(a) => a,
            Err(e) => {
                self.form.msg = Some((Tone::Crit, e));
                return;
            }
        };
        if host.image_path() == Some(target.as_path()) && host.image_edited() {
            self.form.msg = Some((
                Tone::Crit,
                "The opened dump has unsaved edits in the Flash view: save or revert them first."
                    .into(),
            ));
            return;
        }
        self.form.prep = Some(host.spawn_job("Prepare write", move |_| {
            write::prepare(&target, &data, addr)
        }));
    }

    /// Ask for the prepared write every frame until answered; on yes, write
    /// through the host (vault backup first) and read it back in a job.
    fn confirm_write(&mut self, host: &mut Host<'_>) {
        let Some(p) = &self.form.pending else { return };
        let req = ConfirmRequest {
            key: WRITE_KEY.into(),
            title: "Write the data file into the image?".into(),
            body: format!(
                "{} bytes from {} at 0x{:08X} into {} ({} bytes change, image 0x{:X} bytes).\n\n\
                 The file's current contents are archived to the vault first and the file is \
                 replaced atomically. Nothing is sent to an ECU.",
                p.len,
                p.data_name,
                p.addr,
                p.target.display(),
                p.changed,
                p.image_len
            ),
            typed: None,
            confirm_label: "Write".into(),
        };
        match host.confirm(req) {
            ConfirmResult::Pending => {}
            ConfirmResult::Declined => {
                self.form.pending = None;
                self.form.msg = Some((Tone::Muted, "Write cancelled: nothing was written.".into()));
            }
            ConfirmResult::Accepted(ack) => {
                let Some(p) = self.form.pending.take() else {
                    return;
                };
                let reopen = host.image_path() == Some(p.target.as_path());
                if reopen && host.image_edited() {
                    self.form.msg = Some((
                        Tone::Crit,
                        "The opened dump has unsaved edits: nothing was written.".into(),
                    ));
                    return;
                }
                match host.write_file(ack, &p.target, &p.patched) {
                    Ok(rcpt) => {
                        host.status(match &rcpt.backup {
                            Some(b) => format!(
                                "written; the old image is in the vault: {}",
                                b.path.display()
                            ),
                            None => "written".into(),
                        });
                        let target = p.target.clone();
                        let expected = p.patched;
                        let tgt = target.clone();
                        let h =
                            host.spawn_job("Read back", move |_| write::read_back(&tgt, &expected));
                        self.form.verify = Some((h, target, reopen));
                    }
                    Err(e) => self.form.msg = Some((Tone::Crit, format!("write refused: {e}"))),
                }
            }
        }
    }

    // -- the report ----------------------------------------------------------

    fn report_pane(&mut self, ui: &mut Ui, t: &Tokens, host: &mut Host<'_>) {
        let Some(r) = &self.report else {
            theme::panel(
                ui,
                t,
                "Report",
                |_| {},
                |ui| {
                    let text = if self.job.is_some() {
                        "Scanning…"
                    } else {
                        "Open a dump (Ctrl+O, or drop a file) or pick a file on the left. \
                         The report is built offline: nothing is sent to an adapter."
                    };
                    theme::empty_state(ui, t, icons::MAGNIFYING_GLASS, "No analysis yet", text);
                },
            );
            return;
        };
        let mut acts: Vec<HostAction> = Vec::new();
        let mut status: Option<String> = None;
        theme::panel(
            ui,
            t,
            &format!("Report · {}", r.name),
            |ui| {
                theme::pill(ui, t, &format!("{:.0} ms", r.took_ms), Tone::Muted, true);
            },
            |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("analyze-report")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        egui::Frame::new()
                            .inner_margin(Margin::symmetric(16, 12))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                report_body(ui, t, r, &mut acts, &mut status);
                            });
                    });
            },
        );
        for a in acts {
            host.act(a);
        }
        if let Some(s) = status {
            host.status(s);
        }
    }
}

fn path_label(ui: &mut Ui, t: &Tokens, p: Option<&Path>, none: &str) {
    match p {
        Some(p) => {
            ui.add(
                Label::new(
                    RichText::new(file_name(p))
                        .font(font(MONO_MEDIUM, 11.5))
                        .color(t.fg),
                )
                .truncate(),
            )
            .on_hover_text(p.display().to_string());
        }
        None => {
            ui.label(RichText::new(none).small().color(t.fg_faint));
        }
    }
}

fn catalog_line(ui: &mut Ui, t: &Tokens, c: &CatalogCand) {
    let years = c
        .years
        .as_deref()
        .map(|y| format!(" ({y})"))
        .unwrap_or_default();
    ui.add(
        Label::new(
            RichText::new(format!(
                "{} {} {}{years}",
                icons::CAR_PROFILE,
                c.make,
                c.models
            ))
            .font(font(UI_MEDIUM, 12.5))
            .color(t.fg),
        )
        .truncate(),
    )
    .on_hover_text(catalog_hover(c));
}

fn catalog_hover(c: &CatalogCand) -> String {
    let mut s = format!("{} — {}", c.make, c.models);
    for (k, v) in [
        ("years", &c.years),
        ("ECU", &c.ecu),
        ("family", &c.family),
        ("definition", &c.ecudef_id),
        ("source", &c.source),
    ] {
        if let Some(v) = v {
            s.push_str(&format!("\n{k}: {v}"));
        }
    }
    s
}

fn heading(ui: &mut Ui, t: &Tokens, icon: &str, title: &str) {
    ui.add_space(14.0);
    ui.label(
        RichText::new(format!("{icon}  {title}"))
            .font(font(theme::DISPLAY_MEDIUM, 14.0))
            .color(t.fg),
    );
    ui.add_space(2.0);
    theme::hline(ui, t.border_soft);
    ui.add_space(6.0);
}

fn col_head(ui: &mut Ui, t: &Tokens, cols: &[&str]) {
    for h in cols {
        ui.label(
            RichText::new(h.to_uppercase())
                .font(font(UI_STRONG, 10.5))
                .color(t.fg_faint),
        );
    }
    ui.end_row();
}

fn mono(s: impl Into<String>, c: Color32) -> RichText {
    RichText::new(s.into())
        .font(font(MONO_MEDIUM, 11.5))
        .color(c)
}

/// `s` cut to `n` characters with an ellipsis (grid cells: a truncating
/// label inside a `Grid` collapses to its minimum width).
fn cut(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut c: String = s.chars().take(n.saturating_sub(1)).collect();
        c.push('\u{2026}');
        c
    }
}

fn confidence_tone(c: &str) -> Tone {
    match c.to_ascii_lowercase().as_str() {
        "high" => Tone::Ok,
        "medium" => Tone::Accent,
        "low" => Tone::Warn,
        _ => Tone::Muted,
    }
}

fn size_text(n: usize) -> String {
    if n >= 1 << 20 && n.is_multiple_of(1 << 10) {
        format!("{:.2} MiB", n as f64 / (1 << 20) as f64)
    } else if n >= 1024 {
        format!("{} KiB", n / 1024)
    } else {
        format!("{n} B")
    }
}

fn report_body(
    ui: &mut Ui,
    t: &Tokens,
    r: &Report,
    acts: &mut Vec<HostAction>,
    status: &mut Option<String>,
) {
    let opened = r.file.is_none();
    // Headline stats.
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
        let fam = match &r.variant {
            Some(v) if !r.family.contains(v.as_str()) => format!("{} {v}", r.family),
            _ => r.family.clone(),
        };
        theme::stat(ui, t, icons::CPU, "Family", &fam, t.fg);
        if let Some(s) = r.supplier {
            theme::stat(ui, t, icons::IDENTIFICATION_CARD, "Supplier", s, t.fg);
        }
        theme::stat(ui, t, icons::HASH, "Size", &size_text(r.size), t.fg);
        let (cc, cv) = match r.confidence {
            "High" => (t.ok, "High"),
            "Medium" => (t.accent, "Medium"),
            "Low" => (t.warn, "Low"),
            other => (t.fg_dim, other),
        };
        theme::stat(ui, t, icons::CAR, "Car", cv, cc);
        if let Some(ck) = &r.checksum {
            let (ok, n) = ck.tally();
            let (txt, c) = match &ck.blocks {
                Err(_) => ("error".to_string(), t.crit),
                Ok(_) if n == 0 => ("no blocks".to_string(), t.warn),
                Ok(_) if ok == n => (format!("{ok}/{n} OK"), t.ok),
                Ok(_) => (format!("{}/{n} BAD", n - ok), t.crit),
            };
            theme::stat(ui, t, icons::SHIELD_CHECK, "Checksum", &txt, c);
        }
    });

    // Family.
    heading(ui, t, icons::CPU, "ECU family");
    egui::Grid::new("an-family")
        .num_columns(2)
        .spacing(Vec2::new(18.0, 6.0))
        .show(ui, |ui| {
            kv(ui, t, "Family", &r.family);
            if let Some(v) = &r.variant {
                kv(ui, t, "Variant", v);
            }
            if let Some(s) = r.supplier {
                kv(ui, t, "Supplier", s);
            }
            if let Some(c) = r.family_confidence {
                kv(ui, t, "Recognition", c);
            }
            kv(
                ui,
                t,
                "Image",
                &format!("{} · {}", r.image_kind, r.size_class),
            );
            kv(
                ui,
                t,
                "Size",
                &format!("{} (0x{:X} bytes)", size_text(r.size), r.size),
            );
        });
    if r.contested {
        ui.label(
            RichText::new(format!(
                "{} Contested: two incompatible families tie at the top.",
                icons::WARNING
            ))
            .color(t.warn),
        );
    }
    if !r.family_how.is_empty() {
        ui.add_space(4.0);
        egui::CollapsingHeader::new(
            RichText::new(format!("Evidence ({})", r.family_how.len())).color(t.fg_dim),
        )
        .id_salt("an-evidence")
        .show(ui, |ui| {
            for h in &r.family_how {
                ui.add(Label::new(mono(h.clone(), t.fg_dim)).wrap());
            }
        });
    }

    // Identity.
    heading(ui, t, icons::FINGERPRINT, "Identity");
    if r.identity.is_empty() {
        ui.label(RichText::new("No identity strings were found.").color(t.fg_dim));
    } else {
        egui::Grid::new("an-identity")
            .striped(true)
            .num_columns(4)
            .spacing(Vec2::new(18.0, 5.0))
            .show(ui, |ui| {
                col_head(ui, t, &["Kind", "Value", "Offset", "Seen"]);
                for f in &r.identity {
                    ui.label(
                        RichText::new(report::kind_label(f.kind))
                            .font(font(UI_MEDIUM, 12.0))
                            .color(t.fg_dim),
                    );
                    ui.add(Label::new(mono(cut(&f.value, 44), t.fg)))
                        .on_hover_text(&f.value);
                    let off = format!("0x{:06X}", f.offset);
                    if opened {
                        if ui
                            .link(mono(off, t.accent))
                            .on_hover_text("Show this offset in the Flash view's hex pane")
                            .clicked()
                        {
                            acts.push(HostAction::HexJump(f.offset as usize));
                            acts.push(HostAction::ShowView("flash".into()));
                            *status = Some(format!("Flash view: hex at 0x{:06X}", f.offset));
                        }
                    } else {
                        ui.label(mono(off, t.fg_dim));
                    }
                    ui.label(mono(format!("×{}", f.count), t.fg_faint));
                    ui.end_row();
                }
            });
    }
    if r.vin.is_some() || r.part.is_some() {
        ui.add_space(8.0);
        egui::Grid::new("an-decoded")
            .num_columns(2)
            .spacing(Vec2::new(18.0, 6.0))
            .show(ui, |ui| {
                if let Some(v) = &r.vin {
                    let mut s = v.vin.clone();
                    for x in [&v.make, &v.country].into_iter().flatten() {
                        s.push_str(&format!(" · {x}"));
                    }
                    if let Some(y) = v.year {
                        s.push_str(&format!(" · {y}"));
                    }
                    kv(ui, t, "VIN", &s);
                }
                if let Some((pn, rows)) = &r.part {
                    kv(ui, t, "Part number", pn);
                    for (k, v) in rows {
                        kv(ui, t, k, v);
                    }
                }
            });
    }

    // Vehicle.
    heading(ui, t, icons::CAR_PROFILE, "Vehicle");
    ui.horizontal(|ui| {
        ui.label(RichText::new("Assignment").color(t.fg_dim));
        theme::pill(ui, t, r.confidence, confidence_tone(r.confidence), false);
        if !r.basis.is_empty() {
            ui.label(
                RichText::new(format!("via {}", r.basis))
                    .small()
                    .color(t.fg_faint),
            );
        }
    });
    if !r.assignment.is_empty() {
        egui::Grid::new("an-assign")
            .num_columns(2)
            .spacing(Vec2::new(18.0, 6.0))
            .show(ui, |ui| {
                for (k, v) in &r.assignment {
                    kv(ui, t, k, v);
                }
            });
    }
    ui.add_space(6.0);
    if r.catalog.is_empty() {
        ui.label(
            RichText::new(match &r.catalog_note {
                Some(n) => n.clone(),
                None => format!(
                    "No vehicle catalog candidate ({} catalog entries searched).",
                    r.catalog_size
                ),
            })
            .color(t.fg_dim),
        );
    } else {
        ui.label(
            RichText::new(format!(
                "{} catalog candidate(s) from {} entries; same family first",
                r.catalog.len(),
                r.catalog_size
            ))
            .small()
            .color(t.fg_faint),
        );
        egui::Grid::new("an-catalog")
            .striped(true)
            .num_columns(6)
            .spacing(Vec2::new(16.0, 5.0))
            .show(ui, |ui| {
                col_head(
                    ui,
                    t,
                    &["Make", "Models", "Years", "ECU", "Family", "Source"],
                );
                for c in &r.catalog {
                    ui.label(
                        RichText::new(&c.make)
                            .font(font(UI_STRONG, 12.0))
                            .color(t.fg),
                    );
                    ui.add(Label::new(RichText::new(cut(&c.models, 40)).color(t.fg)))
                        .on_hover_text(catalog_hover(c));
                    let dash = || "—".to_string();
                    ui.label(mono(c.years.clone().unwrap_or_else(dash), t.fg_dim));
                    ui.label(RichText::new(c.ecu.clone().unwrap_or_else(dash)).color(t.fg_dim));
                    let fam = c.family.clone().unwrap_or_else(dash);
                    if c.family_agrees {
                        ui.label(mono(format!("{} {fam}", icons::CHECK), t.ok));
                    } else {
                        ui.label(mono(fam, t.fg_dim));
                    }
                    ui.label(
                        RichText::new(c.source.clone().unwrap_or_else(dash))
                            .small()
                            .color(t.fg_faint),
                    );
                    ui.end_row();
                }
            });
    }

    // Definitions.
    heading(ui, t, icons::STACK, "Definition");
    if r.defs.is_empty() {
        ui.label(RichText::new("No loaded definition matches this image.").color(t.fg_dim));
    }
    for (i, d) in r.defs.iter().enumerate() {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(&d.name)
                    .font(font(UI_STRONG, 12.5))
                    .color(t.fg),
            )
            .on_hover_text(&d.id);
            theme::pill(ui, t, &format!("{} maps", d.maps), Tone::Muted, true);
            if theme::button(ui, t, Some(icons::ARROW_SQUARE_OUT), "Use")
                .on_hover_text("Select this definition in the Flash view")
                .clicked()
            {
                acts.push(HostAction::SelectDefinition(d.id.clone()));
                *status = Some(format!("Flash view: definition {} selected", d.name));
            }
        });
        ui.label(
            RichText::new(&d.why)
                .small()
                .color(if i == 0 { t.fg_dim } else { t.fg_faint }),
        );
        ui.add_space(4.0);
    }
    if let Some(d) = r.defs.first() {
        if opened && d.maps > 0 {
            ui.add_space(4.0);
            egui::CollapsingHeader::new(
                RichText::new(format!("Maps of {} ({})", d.name, d.maps)).color(t.fg_dim),
            )
            .id_salt("an-maps")
            .show(ui, |ui| {
                ui.label(
                    RichText::new("Click a map to open it in the Flash view.")
                        .small()
                        .color(t.fg_faint),
                );
                for (i, name) in r.def_map_names.iter().enumerate().take(MAX_MAPS) {
                    if ui.link(RichText::new(name).color(t.fg)).clicked() {
                        acts.push(HostAction::SelectDefinition(d.id.clone()));
                        acts.push(HostAction::SelectMap(i));
                        acts.push(HostAction::ShowView("flash".into()));
                        *status = Some(format!("Flash view: map {name}"));
                    }
                }
                if r.def_map_names.len() > MAX_MAPS {
                    ui.label(
                        RichText::new(format!(
                            "+{} more in the Maps view",
                            r.def_map_names.len() - MAX_MAPS
                        ))
                        .small()
                        .color(t.fg_faint),
                    );
                }
            });
        }
    }

    // Checksum.
    heading(ui, t, icons::SHIELD_CHECK, "Checksum");
    match &r.checksum {
        None => {
            ui.label(
                RichText::new(format!(
                    "No checksum algorithm is known for family {}. Pick one on the left to verify anyway.",
                    r.family
                ))
                .color(t.fg_dim),
            );
        }
        Some(ck) => checksum_body(ui, t, ck),
    }
    ui.add_space(10.0);
}

fn checksum_body(ui: &mut Ui, t: &Tokens, ck: &report::ChecksumReport) {
    ui.horizontal(|ui| {
        theme::pill(ui, t, &ck.algo, Tone::Accent, true);
        ui.label(RichText::new(format!("from {}", ck.why)).small().color(t.fg_faint));
        if ck.blocks.as_ref().is_ok_and(|b| !b.is_empty()) {
            if ck.recognised {
                theme::pill(ui, t, "layout recognised", Tone::Ok, false);
            } else {
                theme::pill(ui, t, "math consistent only", Tone::Warn, false)
                    .on_hover_text("The math checks out but the layout was not confirmed: the match may be coincidental.");
            }
        }
    });
    ui.add_space(4.0);
    match &ck.blocks {
        Err(e) => {
            ui.add(Label::new(RichText::new(e).color(t.crit)).wrap());
        }
        Ok(b) if b.is_empty() => {
            ui.label(
                RichText::new("The algorithm found no checksum blocks in this image.")
                    .color(t.warn),
            );
        }
        Ok(blocks) => {
            egui::Grid::new("an-cks")
                .striped(true)
                .num_columns(6)
                .spacing(Vec2::new(18.0, 5.0))
                .show(ui, |ui| {
                    col_head(
                        ui,
                        t,
                        &["#", "Region", "Length", "Stored", "Computed", "Status"],
                    );
                    for (i, b) in blocks.iter().enumerate() {
                        ui.label(mono(format!("{}", i + 1), t.fg_faint));
                        ui.label(mono(format!("0x{:06X}", b.start), t.fg))
                            .on_hover_text(&b.name);
                        ui.label(mono(format!("0x{:X}", b.len), t.fg_dim));
                        ui.label(mono(format!("0x{:08X}", b.stored), t.fg_dim));
                        ui.label(mono(
                            format!("0x{:08X}", b.computed),
                            if b.ok { t.fg_dim } else { t.crit },
                        ));
                        if b.ok {
                            ui.label(
                                RichText::new(format!("{} OK", icons::CHECK))
                                    .font(font(UI_STRONG, 11.5))
                                    .color(t.ok),
                            );
                        } else {
                            ui.label(
                                RichText::new(format!("{} BAD", icons::X))
                                    .font(font(UI_STRONG, 11.5))
                                    .color(t.crit),
                            );
                        }
                        ui.end_row();
                    }
                });
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "To correct checksums, open the dump and use Fix & save in the Flash view.",
                )
                .small()
                .color(t.fg_faint),
            );
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::host::{Confirms, DiagTarget, HostShared, Jobs};
    use opentune_core::catalog::CatalogEntry;
    use opentune_core::ecudef::EcuDef;
    use std::time::{Duration, Instant};

    /// What a `Host` borrows, owned by a test; `frame` renders one frame.
    pub(crate) struct Rig {
        pub ctx: egui::Context,
        pub jobs: Jobs,
        pub confirms: Confirms,
        pub actions: Vec<HostAction>,
        pub status: Option<String>,
        pub shared: HostShared,
        pub image: Option<Vec<u8>>,
        pub path: Option<PathBuf>,
        pub defs: Vec<EcuDef>,
    }

    impl Rig {
        pub(crate) fn new(image: Option<Vec<u8>>, defs: Vec<EcuDef>) -> Self {
            let ctx = egui::Context::default();
            theme::setup(&ctx, theme::ThemeMode::Dark);
            Self {
                jobs: Jobs::new(ctx.clone()),
                ctx,
                confirms: Confirms::default(),
                actions: Vec::new(),
                status: None,
                shared: HostShared::default(),
                path: image
                    .as_ref()
                    .map(|_| PathBuf::from("/nonexistent/synthetic.bin")),
                image,
                defs,
            }
        }

        /// Run `f` with a host over this rig (outside a frame).
        pub(crate) fn with_host<R>(&mut self, f: impl FnOnce(&mut Host<'_>) -> R) -> R {
            let keys = opentune_hub::seedkeys::SeedKeys::default();
            let mut host = Host {
                tokens: theme::DARK,
                image_path: self.path.as_deref(),
                image: self.image.as_deref(),
                image_edited: false,
                defs: &self.defs,
                def_idx: None,
                map_idx: 0,
                checksums: &[],
                adapter: None,
                diagnostics: None,
                seedkeys: &keys,
                target: Ok(DiagTarget::ENGINE),
                jobs: &self.jobs,
                confirms: &mut self.confirms,
                actions: &mut self.actions,
                status: &mut self.status,
                shared: &self.shared,
            };
            f(&mut host)
        }

        pub(crate) fn frame(&mut self, v: &mut dyn NativeView) {
            let ctx = self.ctx.clone();
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                self.with_host(|host| v.ui(ui, host));
            });
            // Headless: nothing uploads the font atlas.
            out.textures_delta.clear();
        }

        /// Render frames until `done` (jobs finish on their own threads).
        pub(crate) fn until<V: NativeView>(&mut self, v: &mut V, done: impl Fn(&V) -> bool) {
            let t0 = Instant::now();
            loop {
                self.frame(v);
                if done(v) {
                    return;
                }
                // Generous: the analysis job loads the real catalog, which is
                // slow when the build machine is busy (a 10 s limit flaked).
                assert!(t0.elapsed() < Duration::from_secs(60), "timed out");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "ot-native-analyze-view-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn synthetic_view() -> AnalyzeView {
        AnalyzeView {
            // A synthetic catalog: the test never reads the real one.
            catalog: Some(Catalog::of(vec![CatalogEntry {
                make: "Synthetic".into(),
                models: vec!["Testcar".into()],
                part_numbers: vec!["0261209".into()],
                family: Some("ME7".into()),
                ..Default::default()
            }])),
            ..Default::default()
        }
    }

    #[test]
    fn the_opened_dump_is_analysed_in_a_job_with_catalog_candidates() {
        let img = opentune_scan::synth::build(opentune_scan::synth::SynthFamily::Me7);
        let mut rig = Rig::new(Some(img), Vec::new());
        let mut v = synthetic_view();
        rig.frame(&mut v);
        assert!(v.job.is_some(), "auto-analysis of the opened dump");
        rig.until(&mut v, |v| v.job.is_none());
        let r = v.report.as_ref().expect("report");
        assert_eq!(r.family, "ME7");
        assert_eq!(r.catalog.len(), 1);
        assert_eq!(r.catalog[0].models, "Testcar");
        assert!(r.file.is_none());
        // The same image is not analysed again.
        rig.frame(&mut v);
        assert!(v.job.is_none());
        // Typed match uses the loaded catalog.
        v.hw = "0261209999".into();
        let host_free = report::match_typed(&[], &v.catalog.as_ref().unwrap().entries, &v.hw, "");
        assert_eq!(host_free.1.len(), 1);
        assert!(rig.actions.is_empty(), "rendering asks nothing of the app");
    }

    #[test]
    fn long_cells_are_cut_with_an_ellipsis() {
        assert_eq!(cut("abc", 3), "abc");
        assert_eq!(cut("abcdef", 4), "abc\u{2026}");
        assert_eq!(size_text(512 * 1024), "512 KiB");
        assert_eq!(confidence_tone("High"), Tone::Ok);
    }

    #[test]
    fn without_a_dump_nothing_runs() {
        let mut rig = Rig::new(None, Vec::new());
        let mut v = synthetic_view();
        rig.frame(&mut v);
        assert!(v.job.is_none() && v.report.is_none());
    }

    #[test]
    fn the_file_write_is_confirmed_backed_up_and_read_back() {
        let dir = tmp("write");
        let target = dir.join("target.bin");
        let data = dir.join("data.dat");
        let original: Vec<u8> = (0..=255u8).collect();
        std::fs::write(&target, &original).unwrap();
        std::fs::write(&data, [0xA5u8; 4]).unwrap();
        let mut rig = Rig::new(None, Vec::new());
        let mut v = synthetic_view();

        // Declined: nothing is written.
        v.form.pending = Some(write::prepare(&target, &data, 0x10).unwrap());
        rig.frame(&mut v);
        assert!(v.form.pending.is_some(), "the dialog is open");
        rig.confirms.answer(WRITE_KEY, false);
        rig.frame(&mut v);
        assert!(v.form.pending.is_none());
        assert_eq!(std::fs::read(&target).unwrap(), original);

        // Accepted: the vault gets the old image, then the write, then the
        // read-back.
        v.form.pending = Some(write::prepare(&target, &data, 0x10).unwrap());
        rig.frame(&mut v);
        assert_eq!(
            std::fs::read(&target).unwrap(),
            original,
            "not before the yes"
        );
        rig.confirms.answer(WRITE_KEY, true);
        rig.until(&mut v, |v| {
            v.form.verify.is_none() && v.form.pending.is_none()
        });
        let now = std::fs::read(&target).unwrap();
        assert_eq!(&now[0x10..0x14], &[0xA5; 4]);
        assert_eq!(&now[..0x10], &original[..0x10]);
        assert!(
            matches!(&v.form.msg, Some((Tone::Ok, m)) if m.contains("read back OK")),
            "{:?}",
            v.form.msg
        );
        assert!(dir.join("opentune-vault").is_dir(), "backup first");
        assert!(rig.status.as_deref().unwrap_or("").contains("vault"));
        // Not the opened dump: nothing is reopened.
        assert!(rig.actions.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
