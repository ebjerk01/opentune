//! Compare: a stock and a tuned image side by side. The findmaps region diff
//! (changed regions, each with the definition maps it overlaps) and the
//! map-by-map compare against one definition (changed cells, max |Δ| and
//! mean Δ per map, a before / after / Δ grid of the selected map). A map or
//! an address opens in the Flash view.
//!
//! Offline and read-only: files are read (in a job) and nothing is written
//! or sent to an adapter.

mod diff;

use crate::host::{Host, HostAction, JobHandle, NativeView, Tier, ViewGroup};
use crate::icons;
use crate::image::fmt_val;
use crate::mapview::{heat, text_on};
use crate::theme::{self, font, Tokens, Tone, MONO_MEDIUM, UI_MEDIUM, UI_STRONG};
use crate::views::maps::{kv, two_pane};
use diff::{Grid, Outcome, Side};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, Label, Margin, Rect, RichText, Sense, Stroke, Ui,
    UiBuilder, Vec2,
};
use std::path::{Path, PathBuf};

/// Registration tier, read before the view is built ([`Tier::Bench`]: only
/// alpha / bench builds register it).
pub const TIER: Tier = Tier::Public;

pub fn create() -> Box<dyn NativeView> {
    Box::new(CompareView::default())
}

/// Cell height of the before / after grid.
const CELL_H: f32 = 22.0;
/// Axis label column width.
const AXIS_W: f32 = 56.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tab {
    Maps,
    Regions,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GridMode {
    Stock,
    Tuned,
    Delta,
}

pub struct CompareView {
    stock: Option<Side>,
    tuned: Option<Side>,
    /// The definition to compare against (`None`: regions only).
    def_id: Option<String>,
    /// `def_id` was initialised from the Flash view's selection.
    def_init: bool,
    gap: usize,
    min_len: usize,
    job: Option<JobHandle<Outcome>>,
    out: Option<Outcome>,
    error: Option<String>,
    tab: Tab,
    changed_only: bool,
    sel_map: Option<usize>,
    /// The decoded grid of `sel_map` (`None` inside: it does not fit).
    grid: Option<(usize, Option<Grid>)>,
    mode: GridMode,
}

impl Default for CompareView {
    fn default() -> Self {
        Self {
            stock: Some(Side::Opened),
            tuned: None,
            def_id: None,
            def_init: false,
            // The webview app's defaults.
            gap: 16,
            min_len: 8,
            job: None,
            out: None,
            error: None,
            tab: Tab::Maps,
            changed_only: true,
            sel_map: None,
            grid: None,
            mode: GridMode::Tuned,
        }
    }
}

impl NativeView for CompareView {
    fn id(&self) -> &str {
        "compare"
    }
    fn label(&self) -> &str {
        "Compare"
    }
    fn icon(&self) -> &str {
        icons::GIT_DIFF
    }
    fn group(&self) -> ViewGroup {
        ViewGroup::Calibration
    }
    fn tier(&self) -> Tier {
        TIER
    }
    fn count(&self) -> Option<String> {
        self.out
            .as_ref()
            .and_then(|o| o.maps.as_ref())
            .filter(|m| m.changed_maps > 0)
            .map(|m| m.changed_maps.to_string())
    }
    fn ui(&mut self, ui: &mut egui::Ui, host: &mut Host<'_>) {
        let t = *host.tokens();
        if !self.def_init {
            self.def_init = true;
            self.def_id = host.definition().map(|d| d.id.clone());
        }
        if let Some(r) = self.job.as_mut().and_then(JobHandle::take) {
            self.job = None;
            match r {
                Ok(o) => {
                    if o.maps.is_none() {
                        self.tab = Tab::Regions;
                    }
                    self.out = Some(o);
                    self.error = None;
                    self.sel_map = None;
                    self.grid = None;
                }
                Err(e) => self.error = Some(e),
            }
        }
        let (left, right) = two_pane(ui);
        ui.scope_builder(UiBuilder::new().max_rect(left), |ui| {
            self.setup_pane(ui, &t, host)
        });
        ui.scope_builder(UiBuilder::new().max_rect(right), |ui| {
            self.result_pane(ui, &t, host)
        });
    }
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

fn side_name(s: &Side, host: &Host<'_>) -> String {
    match s {
        Side::Opened => host
            .image_path()
            .map(file_name)
            .unwrap_or_else(|| "opened dump".into()),
        Side::File(p) => file_name(p),
    }
}

fn size_text(n: usize) -> String {
    if n >= 1024 {
        format!("{} KiB", n / 1024)
    } else {
        format!("{n} B")
    }
}

fn mono(s: impl Into<String>, c: Color32) -> RichText {
    RichText::new(s.into())
        .font(font(MONO_MEDIUM, 11.5))
        .color(c)
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

/// `+1.5 unit` / `−2 unit`.
fn signed(v: f64, unit: &str) -> String {
    let s = if v > 0.0 {
        format!("+{}", fmt_val(v))
    } else {
        fmt_val(v)
    };
    if unit.is_empty() {
        s
    } else {
        format!("{s} {unit}")
    }
}

impl CompareView {
    fn start(&mut self, host: &Host<'_>) {
        let (Some(s), Some(tu)) = (self.stock.clone(), self.tuned.clone()) else {
            return;
        };
        let opened = |side: &Side| match side {
            Side::Opened => host.image().map(<[u8]>::to_vec),
            Side::File(_) => None,
        };
        if (s == Side::Opened || tu == Side::Opened) && host.image().is_none() {
            self.error = Some("No dump is open in the Flash view.".into());
            return;
        }
        let stock = (side_name(&s, host), s.clone(), opened(&s));
        let tuned = (side_name(&tu, host), tu.clone(), opened(&tu));
        let def = self
            .def_id
            .as_ref()
            .and_then(|id| host.definitions().iter().find(|d| &d.id == id))
            .cloned();
        let (gap, min_len) = (self.gap, self.min_len);
        self.error = None;
        if let Some(h) = &self.job {
            h.cancel();
        }
        self.job = Some(host.spawn_job("Compare", move |p| {
            let load = |(name, side, bytes): (String, Side, Option<Vec<u8>>)| {
                let bytes = match (bytes, &side) {
                    (Some(b), _) => b,
                    (None, Side::File(f)) => {
                        p.set(0.1, format!("reading {name}"));
                        read_image(f)?
                    }
                    (None, Side::Opened) => return Err("no dump is open".to_string()),
                };
                Ok::<_, String>((name, side, bytes))
            };
            let stock = load(stock)?;
            let tuned = load(tuned)?;
            if p.cancelled() {
                return Err("cancelled".into());
            }
            p.set(0.5, "comparing");
            Ok(diff::run(stock, tuned, def, gap, min_len))
        }));
    }

    // -- left column ---------------------------------------------------------

    fn setup_pane(&mut self, ui: &mut Ui, t: &Tokens, host: &mut Host<'_>) {
        theme::panel(
            ui,
            t,
            "Compare",
            |_| {},
            |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("compare-side")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        egui::Frame::new()
                            .inner_margin(Margin::symmetric(14, 10))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                self.setup_body(ui, t, host);
                            });
                    });
            },
        );
    }

    fn side_picker(ui: &mut Ui, t: &Tokens, host: &Host<'_>, label: &str, side: &mut Option<Side>) {
        theme::section_label(ui, t, label);
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            let can = host.image().is_some();
            if ui
                .add_enabled_ui(can, |ui| {
                    theme::button(ui, t, Some(icons::FOLDER_OPEN), "Opened dump")
                })
                .inner
                .on_disabled_hover_text("Open a dump first (Ctrl+O or drop a file)")
                .clicked()
            {
                *side = Some(Side::Opened);
            }
            if theme::button(ui, t, Some(icons::FILE), "File…").clicked() {
                if let Some(p) = host.pick_file(
                    &format!("{label} image"),
                    &[
                        ("ECU image", &["bin", "ori", "mod", "rom"]),
                        ("All", &["*"]),
                    ],
                ) {
                    *side = Some(Side::File(p));
                }
            }
        });
        let (text, c) = match side {
            Some(s @ Side::Opened) => (
                format!("{} {} (opened)", icons::FOLDER_OPEN, side_name(s, host)),
                t.fg,
            ),
            Some(s) => (format!("{} {}", icons::FILE, side_name(s, host)), t.fg),
            None => ("not chosen".to_string(), t.fg_faint),
        };
        let r =
            ui.add(Label::new(RichText::new(text).font(font(UI_MEDIUM, 12.5)).color(c)).truncate());
        if let Some(Side::File(p)) = side {
            r.on_hover_text(p.display().to_string());
        }
    }

    fn setup_body(&mut self, ui: &mut Ui, t: &Tokens, host: &mut Host<'_>) {
        Self::side_picker(ui, t, host, "Stock", &mut self.stock);
        ui.add_space(10.0);
        Self::side_picker(ui, t, host, "Tuned", &mut self.tuned);
        ui.add_space(12.0);
        theme::hline(ui, t.border_soft);
        ui.add_space(10.0);

        theme::section_label(ui, t, "Definition");
        ui.add_space(4.0);
        let defs = host.definitions();
        let current = self
            .def_id
            .as_ref()
            .and_then(|id| defs.iter().find(|d| &d.id == id))
            .map(|d| d.name.clone())
            .unwrap_or_else(|| "None (regions only)".into());
        egui::ComboBox::from_id_salt("compare-def")
            .selected_text(current)
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(self.def_id.is_none(), "None (regions only)")
                    .clicked()
                {
                    self.def_id = None;
                }
                for d in defs.iter().filter(|d| !d.maps.is_empty()) {
                    let on = self.def_id.as_deref() == Some(d.id.as_str());
                    if ui
                        .selectable_label(on, format!("{}  ·  {} maps", d.name, d.maps.len()))
                        .clicked()
                    {
                        self.def_id = Some(d.id.clone());
                    }
                }
            });
        ui.add_space(10.0);
        theme::section_label(ui, t, "Region diff");
        ui.add_space(4.0);
        egui::Grid::new("compare-knobs")
            .num_columns(2)
            .spacing(Vec2::new(12.0, 6.0))
            .show(ui, |ui| {
                ui.label(RichText::new("Merge gap").color(t.fg_dim))
                    .on_hover_text("Changes this many unchanged bytes apart form one region");
                ui.add(
                    egui::DragValue::new(&mut self.gap)
                        .range(0..=4096)
                        .suffix(" B"),
                );
                ui.end_row();
                ui.label(RichText::new("Min length").color(t.fg_dim))
                    .on_hover_text("Shorter regions are not listed");
                ui.add(
                    egui::DragValue::new(&mut self.min_len)
                        .range(1..=65536)
                        .suffix(" B"),
                );
                ui.end_row();
            });
        ui.add_space(12.0);
        let same = self.stock.is_some() && self.stock == self.tuned;
        let ready = self.stock.is_some() && self.tuned.is_some() && !same && self.job.is_none();
        let r = ui
            .add_enabled_ui(ready, |ui| {
                theme::primary_button(ui, t, Some(icons::GIT_DIFF), "Compare")
            })
            .inner;
        let r = if same {
            r.on_disabled_hover_text("Stock and tuned are the same image")
        } else {
            r.on_disabled_hover_text("Choose a stock and a tuned image")
        };
        if r.clicked() {
            self.start(host);
        }
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

        let Some(o) = &self.out else { return };
        ui.add_space(12.0);
        theme::hline(ui, t.border_soft);
        ui.add_space(10.0);
        theme::section_label(ui, t, "Result");
        ui.add_space(4.0);
        egui::Grid::new("compare-summary")
            .num_columns(2)
            .spacing(Vec2::new(14.0, 6.0))
            .show(ui, |ui| {
                kv(
                    ui,
                    t,
                    "Stock",
                    &format!("{} · {}", o.stock_name, size_text(o.a.len())),
                );
                kv(
                    ui,
                    t,
                    "Tuned",
                    &format!("{} · {}", o.tuned_name, size_text(o.b.len())),
                );
                kv(ui, t, "Bytes differ", &o.diff_bytes.to_string());
                kv(
                    ui,
                    t,
                    "Regions",
                    &format!("{} (gap {}, min {})", o.regions.len(), o.gap, o.min_len),
                );
                if let Some(m) = &o.maps {
                    kv(
                        ui,
                        t,
                        "Maps changed",
                        &format!(
                            "{} of {} · {} cells",
                            m.changed_maps,
                            m.rows.len(),
                            m.total_changed
                        ),
                    );
                }
                kv(ui, t, "Took", &format!("{:.0} ms", o.took_ms));
            });
        if o.a.len() != o.b.len() {
            ui.add_space(4.0);
            ui.add(
                Label::new(
                    RichText::new(format!(
                        "{} The images differ in size: only the common part is compared cell by cell.",
                        icons::WARNING
                    ))
                    .color(t.warn),
                )
                .wrap(),
            );
        }
        ui.add_space(8.0);
        let mut open = None;
        ui.horizontal_wrapped(|ui| {
            for (label, side) in [
                ("Open stock in Flash", &o.stock_side),
                ("Open tuned in Flash", &o.tuned_side),
            ] {
                if let Side::File(p) = side {
                    if theme::button(ui, t, Some(icons::ARROW_SQUARE_OUT), label).clicked() {
                        open = Some(p.clone());
                    }
                }
            }
        });
        if let Some(p) = open {
            host.act(HostAction::OpenImage(p));
        }
    }

    // -- results -------------------------------------------------------------

    fn result_pane(&mut self, ui: &mut Ui, t: &Tokens, host: &mut Host<'_>) {
        let counts = self
            .out
            .as_ref()
            .map(|o| (o.regions.len(), o.maps.as_ref().map(|m| m.changed_maps)));
        let tab = self.tab;
        let mut picked = None;
        theme::panel(
            ui,
            t,
            "Changes",
            |ui| {
                if let Some((regions, maps)) = counts {
                    // Right to left: Regions, then Maps.
                    if ui
                        .selectable_label(tab == Tab::Regions, format!("Regions ({regions})"))
                        .clicked()
                    {
                        picked = Some(Tab::Regions);
                    }
                    let m = maps
                        .map(|n| format!("Maps ({n})"))
                        .unwrap_or_else(|| "Maps".into());
                    if ui.selectable_label(tab == Tab::Maps, m).clicked() {
                        picked = Some(Tab::Maps);
                    }
                }
            },
            |ui| {
                if self.out.is_none() {
                    let text = if self.job.is_some() {
                        "Comparing…"
                    } else {
                        "Choose a stock and a tuned image on the left, and a definition to compare map by map. \
                         Files are only read."
                    };
                    theme::empty_state(ui, t, icons::GIT_DIFF, "Nothing compared yet", text);
                    return;
                }
                egui::Frame::new()
                    .inner_margin(Margin::symmetric(14, 10))
                    .show(ui, |ui| {
                        ui.set_min_size(ui.available_size());
                        match tab {
                            Tab::Maps => self.maps_tab(ui, t, host),
                            Tab::Regions => self.regions_tab(ui, t, host),
                        }
                    });
            },
        );
        if let Some(p) = picked {
            self.tab = p;
        }
    }

    fn select_map(&mut self, idx: usize) {
        self.sel_map = Some(idx);
        let g = self.out.as_ref().and_then(|o| {
            let def = o.def.as_ref()?;
            Some(diff::grid(def, idx, &o.a, &o.b))
        });
        self.grid = g.map(|g| (idx, g));
    }

    fn open_map_in_flash(&self, host: &mut Host<'_>, idx: usize) {
        let Some(def) = self.out.as_ref().and_then(|o| o.def.as_ref()) else {
            return;
        };
        host.act(HostAction::SelectDefinition(def.id.clone()));
        host.act(HostAction::SelectMap(idx));
        host.act(HostAction::ShowView("flash".into()));
        if let Some(m) = def.maps.get(idx) {
            host.status(format!("Flash view: map {} of {}", m.name, def.name));
        }
    }

    fn maps_tab(&mut self, ui: &mut Ui, t: &Tokens, host: &mut Host<'_>) {
        let Some(md) = self.out.as_ref().and_then(|o| o.maps.clone()) else {
            ui.label(
                RichText::new("Choose a definition with maps on the left to compare map by map.")
                    .color(t.fg_dim),
            );
            return;
        };
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(&md.def_name)
                    .font(font(UI_STRONG, 12.5))
                    .color(t.fg),
            );
            theme::pill(
                ui,
                t,
                &format!("{} of {} changed", md.changed_maps, md.rows.len()),
                if md.changed_maps > 0 {
                    Tone::Accent
                } else {
                    Tone::Muted
                },
                false,
            );
            ui.checkbox(&mut self.changed_only, "Changed only");
        });
        ui.add_space(6.0);
        let rows: Vec<&diff::MapRow> = md
            .rows
            .iter()
            .filter(|r| !self.changed_only || r.changed > 0)
            .collect();
        let has_grid = self.sel_map.is_some();
        let table_h = if has_grid {
            (ui.available_height() * 0.42).max(120.0)
        } else {
            ui.available_height()
        };
        let mut select = None;
        let mut open = None;
        egui::ScrollArea::vertical()
            .id_salt("compare-maps")
            .max_height(table_h)
            .auto_shrink([false, !has_grid])
            .show(ui, |ui| {
                if rows.is_empty() {
                    ui.label(RichText::new("No map changed.").color(t.fg_dim));
                    return;
                }
                egui::Grid::new("compare-maps-grid")
                    .striped(true)
                    .num_columns(7)
                    .spacing(Vec2::new(16.0, 5.0))
                    .show(ui, |ui| {
                        col_head(
                            ui,
                            t,
                            &["Map", "Address", "Size", "Changed", "Δmax", "Δavg", ""],
                        );
                        for r in &rows {
                            let on = self.sel_map == Some(r.idx);
                            if ui
                                .selectable_label(on, RichText::new(&r.name).color(t.fg))
                                .on_hover_text("Show the before / after grid")
                                .clicked()
                            {
                                select = Some(r.idx);
                            }
                            ui.label(mono(format!("0x{:06X}", r.address), t.fg_dim));
                            ui.label(mono(format!("{}×{}", r.cols, r.rows), t.fg_dim));
                            if r.out_of_range {
                                ui.label(RichText::new("out of range").small().color(t.warn));
                                ui.label("");
                                ui.label("");
                            } else {
                                theme::pill(
                                    ui,
                                    t,
                                    &r.changed.to_string(),
                                    if r.changed > 0 {
                                        Tone::Accent
                                    } else {
                                        Tone::Muted
                                    },
                                    true,
                                );
                                let dash = |v: f64| {
                                    if r.changed > 0 {
                                        signed(v, &r.unit)
                                    } else {
                                        "—".into()
                                    }
                                };
                                ui.label(mono(dash(r.d_max), t.fg));
                                ui.label(mono(dash(r.d_avg), t.fg_dim));
                            }
                            if theme::icon_button(
                                ui,
                                t,
                                icons::ARROW_SQUARE_OUT,
                                "Open this map in the Flash view",
                            )
                            .clicked()
                            {
                                open = Some(r.idx);
                            }
                            ui.end_row();
                        }
                    });
            });
        if let Some(i) = select {
            self.select_map(i);
        }
        if let Some(i) = open {
            self.open_map_in_flash(host, i);
        }
        if has_grid {
            ui.add_space(8.0);
            theme::hline(ui, t.border_soft);
            ui.add_space(8.0);
            self.grid_area(ui, t, host);
        }
    }

    fn grid_area(&mut self, ui: &mut Ui, t: &Tokens, host: &mut Host<'_>) {
        let Some(idx) = self.sel_map else { return };
        let Some(def) = self.out.as_ref().and_then(|o| o.def.clone()) else {
            return;
        };
        let Some(m) = def.maps.get(idx) else { return };
        let mut open = false;
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(&m.name)
                    .font(font(theme::DISPLAY_MEDIUM, 13.5))
                    .color(t.fg),
            );
            if !m.unit.is_empty() {
                theme::pill(ui, t, &m.unit, Tone::Muted, true);
            }
            ui.add_space(8.0);
            for (mode, label) in [
                (GridMode::Stock, "Stock"),
                (GridMode::Tuned, "Tuned"),
                (GridMode::Delta, "Δ"),
            ] {
                if ui.selectable_label(self.mode == mode, label).clicked() {
                    self.mode = mode;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::button(ui, t, Some(icons::ARROW_SQUARE_OUT), "Open in Flash").clicked() {
                    open = true;
                }
            });
        });
        if open {
            self.open_map_in_flash(host, idx);
        }
        ui.add_space(6.0);
        match &self.grid {
            Some((i, Some(g))) if *i == idx => {
                egui::ScrollArea::both()
                    .id_salt("compare-grid")
                    .auto_shrink([false, false])
                    .show(ui, |ui| paint_grid(ui, t, g, self.mode));
            }
            _ => {
                ui.label(RichText::new("This map does not fit inside both images.").color(t.warn));
            }
        }
    }

    fn regions_tab(&mut self, ui: &mut Ui, t: &Tokens, host: &mut Host<'_>) {
        let Some(o) = &self.out else { return };
        if o.regions.is_empty() {
            ui.label(RichText::new("No changed region with these settings.").color(t.fg_dim));
            return;
        }
        ui.label(
            RichText::new(
                "Hex jumps scroll the Flash view's hex pane (the dump opened there). \
                 Map names select the map.",
            )
            .small()
            .color(t.fg_faint),
        );
        ui.add_space(6.0);
        let def = o.def.as_ref();
        let mut jump = None;
        let mut pick_map = None;
        egui::ScrollArea::vertical()
            .id_salt("compare-regions")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Grid::new("compare-regions-grid")
                    .striped(true)
                    .num_columns(6)
                    .spacing(Vec2::new(16.0, 5.0))
                    .show(ui, |ui| {
                        col_head(ui, t, &["#", "Start", "End", "Length", "Shape", "Maps"]);
                        for (i, r) in o.regions.iter().enumerate() {
                            ui.label(mono(format!("{}", i + 1), t.fg_faint));
                            if ui
                                .link(mono(format!("0x{:06X}", r.start), t.accent))
                                .on_hover_text("Show this address in the Flash view's hex pane")
                                .clicked()
                            {
                                jump = Some(r.start);
                            }
                            ui.label(mono(format!("0x{:06X}", r.end), t.fg_dim));
                            ui.label(mono(r.len.to_string(), t.fg));
                            ui.label(mono(
                                if r.hint.is_empty() { "—" } else { &r.hint },
                                t.fg_dim,
                            ));
                            ui.horizontal(|ui| {
                                if r.maps.is_empty() {
                                    ui.label(RichText::new("—").color(t.fg_faint));
                                }
                                for &m in r.maps.iter().take(4) {
                                    let name = def
                                        .and_then(|d| d.maps.get(m))
                                        .map(|m| m.name.as_str())
                                        .unwrap_or("?");
                                    if ui.link(RichText::new(name).color(t.fg)).clicked() {
                                        pick_map = Some(m);
                                    }
                                }
                                if r.maps.len() > 4 {
                                    ui.label(
                                        RichText::new(format!("+{}", r.maps.len() - 4))
                                            .small()
                                            .color(t.fg_faint),
                                    );
                                }
                            });
                            ui.end_row();
                        }
                    });
            });
        if let Some(a) = jump {
            host.act(HostAction::HexJump(a));
            host.act(HostAction::ShowView("flash".into()));
            host.status(format!("Flash view: hex at 0x{a:06X}"));
        }
        if let Some(m) = pick_map {
            self.tab = Tab::Maps;
            self.changed_only = false;
            self.select_map(m);
            self.open_map_in_flash(host, m);
        }
    }
}

fn read_image(p: &PathBuf) -> Result<Vec<u8>, String> {
    const MAX: u64 = 64 << 20;
    let len = std::fs::metadata(p)
        .map_err(|e| format!("{}: {e}", p.display()))?
        .len();
    if len > MAX {
        return Err(format!(
            "{} is {len} bytes: larger than any raw ECU image",
            p.display()
        ));
    }
    std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()))
}

/// Paint the selected map: stock or tuned on the shared heat scale with the
/// changed cells outlined, or the Δ (blue below, red above, unchanged
/// cells plain).
fn paint_grid(ui: &mut Ui, t: &Tokens, g: &Grid, mode: GridMode) {
    let delta;
    let (vals, lo, hi) = match mode {
        GridMode::Stock => {
            let (lo, hi) = g.range();
            (&g.stock, lo, hi)
        }
        GridMode::Tuned => {
            let (lo, hi) = g.range();
            (&g.tuned, lo, hi)
        }
        GridMode::Delta => {
            delta = g.delta();
            let m = delta
                .iter()
                .flatten()
                .fold(0.0f64, |a, v| a.max(v.abs()))
                .max(f64::MIN_POSITIVE);
            (&delta, -m, m)
        }
    };
    let rows = vals.len();
    let cols = vals.first().map_or(0, Vec::len);
    if rows == 0 || cols == 0 {
        return;
    }
    let txt = |v: f64| {
        if mode == GridMode::Delta && v > 0.0 {
            format!("+{}", fmt_val(v))
        } else {
            fmt_val(v)
        }
    };
    let chars = vals
        .iter()
        .flatten()
        .chain(&g.x)
        .map(|v| txt(*v).len())
        .max()
        .unwrap_or(3);
    let cw = (chars as f32 * 6.6 + 12.0).max(36.0);
    let gap = 2.0;
    let size = Vec2::new(
        AXIS_W + cols as f32 * (cw + gap),
        CELL_H + gap + rows as f32 * (CELL_H + gap),
    );
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let p = ui.painter_at(rect.expand(1.0));
    let f = font(MONO_MEDIUM, 11.0);
    let origin = rect.min + Vec2::new(AXIS_W, CELL_H + gap);
    for c in 0..cols {
        let x = g.x.get(c).copied().unwrap_or(c as f64);
        let r = Rect::from_min_size(
            egui::pos2(origin.x + c as f32 * (cw + gap), rect.top()),
            Vec2::new(cw, CELL_H),
        );
        p.text(
            r.center(),
            Align2::CENTER_CENTER,
            fmt_val(x),
            f.clone(),
            t.fg_faint,
        );
    }
    let clip = ui.clip_rect();
    for (ri, row) in vals.iter().enumerate() {
        let y0 = origin.y + ri as f32 * (CELL_H + gap);
        let yv = g.y.get(ri).copied().unwrap_or(ri as f64);
        p.text(
            egui::pos2(rect.left() + AXIS_W - 8.0, y0 + CELL_H / 2.0),
            Align2::RIGHT_CENTER,
            fmt_val(yv),
            f.clone(),
            t.fg_faint,
        );
        for (ci, v) in row.iter().enumerate() {
            let r = Rect::from_min_size(
                egui::pos2(origin.x + ci as f32 * (cw + gap), y0),
                Vec2::new(cw, CELL_H),
            );
            if !clip.intersects(r) {
                continue;
            }
            let changed = g
                .stock
                .get(ri)
                .and_then(|s| s.get(ci))
                .zip(g.tuned.get(ri).and_then(|s| s.get(ci)))
                .is_some_and(|(a, b)| (a - b).abs() > 1e-9);
            let tt = ((v - lo) / (hi - lo)) as f32;
            let (bg, fg) = if mode == GridMode::Delta && !changed {
                (t.surface2, t.fg_faint)
            } else {
                (heat(tt), text_on(tt))
            };
            p.rect_filled(r, CornerRadius::same(4), bg);
            if changed && mode != GridMode::Delta {
                p.rect_stroke(
                    r,
                    CornerRadius::same(4),
                    Stroke::new(1.5, t.fg),
                    egui::StrokeKind::Inside,
                );
            }
            p.text(r.center(), Align2::CENTER_CENTER, txt(*v), f.clone(), fg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::analyze::tests::Rig;

    fn write_tmp(tag: &str, data: &[u8]) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "ot-native-compare-{tag}-{}.bin",
            std::process::id()
        ));
        std::fs::write(&p, data).unwrap();
        p
    }

    #[test]
    fn opened_vs_file_compares_in_a_job_and_jumps_to_flash() {
        let stock = vec![10u8; 64];
        let mut tuned = stock.clone();
        tuned[1] = 14;
        tuned[3] = 8;
        let tuned_path = write_tmp("tuned", &tuned);
        let mut rig = Rig::new(Some(stock), vec![diff::tests::syn_def()]);
        let mut v = CompareView {
            tuned: Some(Side::File(tuned_path.clone())),
            def_id: Some("syn".into()),
            def_init: true,
            min_len: 1,
            ..Default::default()
        };
        rig.frame(&mut v);
        assert!(v.job.is_none(), "nothing runs before Compare is clicked");
        v.start_for_test(&mut rig);
        rig.until(&mut v, |v| v.job.is_none());
        let o = v.out.as_ref().expect("outcome");
        assert_eq!(o.diff_bytes, 2);
        assert_eq!(o.regions.len(), 1);
        assert_eq!(o.maps.as_ref().unwrap().changed_maps, 1);
        assert_eq!(v.count().as_deref(), Some("1"));

        // Select the changed map: its grid decodes; render the grid in every mode.
        v.select_map(0);
        assert!(matches!(&v.grid, Some((0, Some(_)))));
        for mode in [GridMode::Stock, GridMode::Tuned, GridMode::Delta] {
            v.mode = mode;
            rig.frame(&mut v);
        }
        v.tab = Tab::Regions;
        rig.frame(&mut v);

        // Open in Flash selects the definition, then the map, and switches
        // to the Flash view.
        rig.actions.clear();
        v.open_for_test(&mut rig, 0);
        assert_eq!(
            rig.actions,
            vec![
                HostAction::SelectDefinition("syn".into()),
                HostAction::SelectMap(0),
                HostAction::ShowView("flash".into()),
            ]
        );
        let _ = std::fs::remove_file(&tuned_path);
    }

    #[test]
    fn a_missing_file_is_an_error_not_a_panic() {
        let mut rig = Rig::new(Some(vec![0u8; 16]), Vec::new());
        let mut v = CompareView {
            tuned: Some(Side::File("/nonexistent/opentune-tuned.bin".into())),
            def_init: true,
            ..Default::default()
        };
        v.start_for_test(&mut rig);
        rig.until(&mut v, |v| v.job.is_none());
        assert!(v.out.is_none());
        assert!(v.error.as_deref().unwrap_or("").contains("nonexistent"));
    }

    #[test]
    fn signed_values_and_sizes() {
        assert_eq!(signed(1.5, "bar"), "+1.50 bar");
        assert_eq!(signed(-2.0, ""), "-2");
        assert_eq!(size_text(2048), "2 KiB");
        assert_eq!(size_text(10), "10 B");
    }

    impl CompareView {
        /// Press Compare (through a real host frame).
        fn start_for_test(&mut self, rig: &mut Rig) {
            rig.with_host(|h| self.start(h));
        }
        fn open_for_test(&mut self, rig: &mut Rig, idx: usize) {
            rig.with_host(|h| self.open_map_in_flash(h, idx));
        }
    }
}
