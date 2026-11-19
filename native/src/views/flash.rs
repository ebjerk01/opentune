//! Flash dump workspace: dump identity bar, then resizable panes — hex
//! (left) | map (top right) / checksum (bottom right).

use crate::bench::{Bench, Scene};
use crate::hex::{parse_addr, HexView, BYTES_PER_ROW};
use crate::icons;
use crate::image::{fmt_val, Image};
use crate::mapview::{self, draw_map};
use crate::settings::Settings;
use crate::theme::{self, font, Tokens, Tone, DISPLAY, MONO_MEDIUM, UI_STRONG};
use eframe::egui::{
    self, CornerRadius, Margin, Rect, RichText, Sense, Stroke, Ui, UiBuilder, Vec2,
};
use opentune_checksum::{Recognition, Representation};
use opentune_core::ecudef::EcuDef;
use std::path::{Path, PathBuf};
use std::time::Instant;

const SPLIT: f32 = 14.0;

struct Identity {
    family: String,
    hw: Option<String>,
    sw: Option<String>,
    took_ms: f64,
}

struct Block {
    name: String,
    start: u32,
    stored: u64,
    computed: u64,
    ok: bool,
}

struct ChecksumResult {
    algo: String,
    recognised: bool,
    blocks: Vec<Block>,
    took_ms: f64,
}

/// What the status bar shows about the workspace.
#[derive(Default)]
pub struct Status {
    pub path: Option<String>,
    pub offset: Option<usize>,
    pub family: Option<String>,
}

#[derive(Default)]
pub struct FlashView {
    pub image: Option<Image>,
    identity: Option<Identity>,
    pub def_idx: Option<usize>,
    pub map_idx: usize,
    show_all_maps: bool,
    pending_jump_to_map: bool,
    hex: HexView,
    hex_offset: f32,
    hex_max: f32,
    maps_max: f32,
    algo: String,
    checksum: Option<Result<ChecksumResult, String>>,
    pub bench: Option<Bench>,
    pub error: Option<String>,
    pub note: Option<String>,
}

impl FlashView {
    pub fn new(bench: Option<Bench>) -> Self {
        Self {
            bench,
            ..Default::default()
        }
    }

    /// Show map `idx` of the current definition and jump the hex view to it.
    pub fn open_map(&mut self, idx: usize) {
        self.map_idx = idx;
        self.show_all_maps = false;
        self.pending_jump_to_map = true;
    }

    pub fn select_def(&mut self, defs: &[EcuDef], idx: Option<usize>) {
        self.def_idx = idx;
        self.map_idx = 0;
        if let Some(c) = idx.and_then(|i| defs[i].checksum.clone()) {
            if c != self.algo {
                self.algo = c;
                // Results always belong to the algorithm shown.
                if self.image.is_some() {
                    self.verify();
                }
            }
        }
    }

    pub fn open(&mut self, defs: &[EcuDef], path: PathBuf, settings: &mut Settings) {
        match Image::open(path) {
            Ok(img) => {
                let t = Instant::now();
                let ctx = opentune_scan::ScanContext {
                    catalog: Vec::new(),
                    ecudefs: defs.to_vec(),
                };
                let root = img.path.parent().unwrap_or(Path::new(".")).to_path_buf();
                let rec = opentune_scan::scan_file(&ctx, &root, &img.path, &img.bytes);
                let fam = match &rec.ecu_variant {
                    Some(v) if !rec.ecu_family.contains(v.as_str()) => {
                        format!("{} {v}", rec.ecu_family)
                    }
                    _ => rec.ecu_family.to_string(),
                };
                let supplier = rec
                    .ecu_supplier
                    .map(|s| format!("{s} "))
                    .unwrap_or_default();
                self.identity = Some(Identity {
                    family: format!("{supplier}{fam}"),
                    hw: rec
                        .bosch_numbers
                        .iter()
                        .find(|n| n.starts_with("0281") || n.starts_with("0261"))
                        .cloned(),
                    sw: rec.sw_numbers.first().cloned().or_else(|| {
                        rec.bosch_numbers
                            .iter()
                            .find(|n| n.starts_with("1037"))
                            .cloned()
                    }),
                    took_ms: t.elapsed().as_secs_f64() * 1000.0,
                });
                if let Some(idx) = pick_def(
                    defs,
                    rec.ecu_family,
                    rec.ecu_variant.as_deref(),
                    rec.ecudef_match.as_ref().map(|h| h.id.as_str()),
                ) {
                    self.select_def(defs, Some(idx));
                }
                settings.last_file = Some(img.path.display().to_string());
                self.checksum = None;
                self.error = None;
                self.note = None;
                self.image = Some(img);
                // Start the hex view at the selected map, not the boot area.
                self.pending_jump_to_map = self.def_idx.is_some();
                if !self.algo.is_empty() {
                    self.verify();
                }
            }
            Err(e) => self.error = Some(format!("open failed: {e}")),
        }
    }

    fn verify(&mut self) {
        let Some(img) = &self.image else { return };
        let id = self.algo.clone();
        self.checksum = Some(match opentune_checksum::get(&id) {
            None => Err(format!("unknown checksum '{id}'")),
            Some(a) => {
                let t = Instant::now();
                a.verify_report(&img.bytes, Representation::RawFullImage)
                    .map(|v| ChecksumResult {
                        algo: id.clone(),
                        recognised: !v.is_empty()
                            && v.iter().all(|r| r.recognition == Recognition::Recognised),
                        blocks: v
                            .into_iter()
                            .map(|r| Block {
                                name: r.status.name,
                                start: r.status.region.address,
                                stored: r.status.stored,
                                computed: r.status.computed,
                                ok: r.status.ok,
                            })
                            .collect(),
                        took_ms: t.elapsed().as_secs_f64() * 1000.0,
                    })
                    .map_err(|e| e.to_string())
            }
        });
    }

    /// Correct the checksums into a NEW file (never the opened one).
    fn fix_and_save(&mut self) {
        let Some(img) = &self.image else { return };
        let Some(algo) = opentune_checksum::get(&self.algo) else {
            return;
        };
        let mut fixed = img.bytes.clone();
        match algo.correct(&mut fixed) {
            Err(e) => self.error = Some(format!("checksum fix refused: {e}")),
            Ok(fixes) if fixes.is_empty() => self.note = Some("No corrections needed.".into()),
            Ok(fixes) => {
                let stem = img
                    .path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "image".into());
                let dir = img.path.parent().map(Path::to_path_buf).unwrap_or_default();
                if let Some(out) = rfd::FileDialog::new()
                    .set_directory(dir)
                    .set_file_name(format!("{stem}_fixed.bin"))
                    .save_file()
                {
                    if out == img.path {
                        self.error = Some(
                            "Pick a new file name: the opened dump is never overwritten.".into(),
                        );
                        return;
                    }
                    match std::fs::write(&out, &fixed) {
                        Ok(()) => {
                            self.note = Some(format!(
                                "Saved {} with {} corrected block(s).",
                                out.display(),
                                fixes.len()
                            ))
                        }
                        Err(e) => self.error = Some(format!("save failed: {e}")),
                    }
                }
            }
        }
    }

    pub fn status(&self) -> Status {
        Status {
            path: self.image.as_ref().map(|i| i.path.display().to_string()),
            offset: self
                .image
                .as_ref()
                .map(|_| (self.hex_offset / self.hex.row_height.max(1.0)) as usize * BYTES_PER_ROW),
            family: self.identity.as_ref().map(|i| i.family.clone()),
        }
    }

    // -----------------------------------------------------------------------

    pub fn ui(&mut self, ui: &mut Ui, t: &Tokens, defs: &[EcuDef], settings: &mut Settings) {
        egui::Frame::new()
            .fill(t.surface)
            .inner_margin(Margin::symmetric(18, 11))
            .show(ui, |ui| {
                self.dump_bar(ui, t, defs, settings);
            });
        theme::hline(ui, t.border);
        if let Some(msg) = self.error.clone().or_else(|| self.note.clone()) {
            let crit = self.error.is_some();
            egui::Frame::new()
                .fill(if crit { t.crit_soft } else { t.accent_soft })
                .inner_margin(Margin::symmetric(18, 6))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(if crit { icons::WARNING } else { icons::INFO })
                                .color(if crit { t.crit } else { t.accent }),
                        );
                        ui.label(RichText::new(msg).color(t.fg));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button(icons::X).clicked() {
                                self.error = None;
                                self.note = None;
                            }
                        });
                    });
                });
        }
        let Some(_) = &self.image else {
            theme::empty_state(
                ui,
                t,
                icons::FOLDER_OPEN,
                "Open a flash dump",
                "Open a .bin read of an ECU to see its hex, maps and checksums.",
            );
            return;
        };

        let outer = ui.available_rect_before_wrap().shrink(theme::GAP);
        ui.allocate_rect(ui.available_rect_before_wrap(), Sense::hover());
        let mut panes = settings.panes.clamp();
        let left_w = ((outer.width() - SPLIT) * panes.hex).round();
        let hex_rect = Rect::from_min_size(outer.min, Vec2::new(left_w, outer.height()));
        let vsplit = Rect::from_min_size(
            egui::pos2(hex_rect.right(), outer.top()),
            Vec2::new(SPLIT, outer.height()),
        );
        let right = Rect::from_min_max(egui::pos2(vsplit.right(), outer.top()), outer.max);
        let map_h = ((right.height() - SPLIT) * panes.map).round();
        let map_rect = Rect::from_min_size(right.min, Vec2::new(right.width(), map_h));
        let hsplit = Rect::from_min_size(
            egui::pos2(right.left(), map_rect.bottom()),
            Vec2::new(right.width(), SPLIT),
        );
        let cks_rect = Rect::from_min_max(egui::pos2(right.left(), hsplit.bottom()), right.max);

        splitter(ui, t, vsplit, true, &mut panes.hex, outer.width() - SPLIT);
        splitter(ui, t, hsplit, false, &mut panes.map, right.height() - SPLIT);
        settings.panes = panes.clamp();

        ui.scope_builder(UiBuilder::new().max_rect(hex_rect), |ui| {
            self.hex_pane(ui, t, defs)
        });
        ui.scope_builder(UiBuilder::new().max_rect(map_rect), |ui| {
            self.map_pane(ui, t, defs)
        });
        ui.scope_builder(UiBuilder::new().max_rect(cks_rect), |ui| {
            self.checksum_pane(ui, t)
        });
    }

    fn dump_bar(&mut self, ui: &mut Ui, t: &Tokens, defs: &[EcuDef], settings: &mut Settings) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 11.0;
            // Chip icon tile.
            let (r, _) = ui.allocate_exact_size(Vec2::splat(34.0), Sense::hover());
            ui.painter().rect(r, CornerRadius::same(theme::RADIUS_SM), t.surface3, Stroke::new(1.0, t.border), egui::StrokeKind::Inside);
            ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, icons::CPU, egui::FontId::proportional(19.0), t.accent);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                match &self.image {
                    Some(img) => {
                        ui.label(RichText::new(img.name()).font(font(UI_STRONG, 14.0)).color(t.fg));
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 12.0;
                            if let Some(id) = &self.identity {
                                ui.label(RichText::new(&id.family).font(font(theme::MONO_STRONG, 12.0)).color(t.fg))
                                    .on_hover_text(format!("identified in-process in {:.1} ms", id.took_ms));
                                if let Some(hw) = &id.hw {
                                    ui.label(RichText::new(format!("HW {hw}")).font(font(MONO_MEDIUM, 12.0)).color(t.fg_dim));
                                }
                                if let Some(sw) = &id.sw {
                                    ui.label(RichText::new(format!("SW {sw}")).font(font(MONO_MEDIUM, 12.0)).color(t.fg_dim));
                                }
                            }
                            ui.label(RichText::new(size_text(img.bytes.len())).font(font(MONO_MEDIUM, 12.0)).color(t.fg_dim));
                        });
                    }
                    None => {
                        ui.label(RichText::new("No dump open").font(font(UI_STRONG, 14.0)).color(t.fg));
                        ui.label(RichText::new("Open a read of your ECU to start").color(t.fg_dim));
                    }
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let can_fix = matches!(&self.checksum, Some(Ok(r)) if r.blocks.iter().any(|b| !b.ok));
                if ui.add_enabled_ui(can_fix, |ui| theme::primary_button(ui, t, Some(icons::CHECK), "Fix & save")).inner.on_hover_text("Write a copy with corrected checksums (the opened dump is never overwritten)").clicked() {
                    self.fix_and_save();
                }
                if theme::button(ui, t, Some(icons::FOLDER_OPEN), "Open dump").clicked() {
                    if let Some(p) = rfd::FileDialog::new().add_filter("flash image", &["bin", "ori", "mod", "hex", "BIN"]).pick_file() {
                        self.open(defs, p, settings);
                    }
                }
                let current = self.def_idx.map(|i| defs[i].name.clone()).unwrap_or_else(|| "No definition".into());
                let mut pick = None;
                egui::ComboBox::from_id_salt("def").selected_text(current).width(210.0).show_ui(ui, |ui| {
                    for (i, d) in defs.iter().enumerate() {
                        if ui.selectable_label(self.def_idx == Some(i), &d.name).clicked() {
                            pick = Some(i);
                        }
                    }
                });
                if pick.is_some() {
                    self.select_def(defs, pick);
                    settings.last_def = pick.map(|i| defs[i].id.clone());
                    if self.image.is_some() && !self.algo.is_empty() {
                        self.verify();
                    }
                }
            });
        });
    }

    fn hex_pane(&mut self, ui: &mut Ui, t: &Tokens, defs: &[EcuDef]) {
        let sel = self.def_idx.and_then(|i| defs[i].maps.get(self.map_idx));
        if std::mem::take(&mut self.pending_jump_to_map) {
            if let Some(m) = sel {
                self.hex.jump(m.address as usize);
            }
        }
        let hl = sel.map(|m| m.address as usize..(m.address + m.byte_len()) as usize);
        let top = (self.hex_offset / self.hex.row_height.max(1.0)) as usize * BYTES_PER_ROW;
        let mut jump = None;
        let mut jump_text = std::mem::take(&mut self.hex.jump_text);
        theme::panel(
            ui,
            t,
            "Hex view",
            |ui| {
                let r = ui.add(
                    egui::TextEdit::singleline(&mut jump_text)
                        .desired_width(86.0)
                        .font(egui::FontId::monospace(12.0))
                        .hint_text("go to 0x…"),
                );
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    jump = parse_addr(&jump_text);
                }
                theme::pill(ui, t, &format!("0x{top:08X}"), Tone::Muted, true);
            },
            |ui| {
                if let Some(b) = &self.bench {
                    if b.cfg.scene == Scene::Hex {
                        self.hex.scroll_to = Some(wrap(b.offset(), self.hex_max));
                    }
                }
                let img = self.image.as_ref().expect("checked by ui()");
                let legend_h = 32.0;
                let body = ui.available_rect_before_wrap();
                let rows_rect = Rect::from_min_max(
                    body.min + Vec2::new(14.0, 6.0),
                    egui::pos2(body.right() - 6.0, body.bottom() - legend_h),
                );
                ui.scope_builder(UiBuilder::new().max_rect(rows_rect), |ui| {
                    self.hex_offset = self.hex.show(ui, t, &img.bytes, hl.clone());
                });
                let rows = img.bytes.len().div_ceil(BYTES_PER_ROW) as f32;
                self.hex_max = (rows * self.hex.row_height - rows_rect.height()).max(1.0);
                let legend =
                    Rect::from_min_max(egui::pos2(body.left(), body.bottom() - legend_h), body.max);
                ui.scope_builder(UiBuilder::new().max_rect(legend), |ui| {
                    theme::hline(ui, t.border_soft);
                    egui::Frame::new()
                        .inner_margin(Margin::symmetric(14, 7))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let (r, _) =
                                    ui.allocate_exact_size(Vec2::splat(11.0), Sense::hover());
                                ui.painter().rect(
                                    r,
                                    CornerRadius::same(3),
                                    t.sel,
                                    Stroke::new(1.0, t.accent_line),
                                    egui::StrokeKind::Inside,
                                );
                                match sel {
                                    Some(m) => {
                                        ui.label(RichText::new("bytes of").small().color(t.fg_dim));
                                        ui.label(
                                            RichText::new(&m.name).small().strong().color(t.fg),
                                        );
                                        ui.label(
                                            RichText::new("highlighted").small().color(t.fg_dim),
                                        );
                                    }
                                    None => {
                                        ui.label(
                                            RichText::new("select a map to highlight its bytes")
                                                .small()
                                                .color(t.fg_dim),
                                        );
                                    }
                                }
                            });
                        });
                });
            },
        );
        self.after_hex_header(jump_text, jump);
    }

    fn after_hex_header(&mut self, jump_text: String, jump: Option<usize>) {
        self.hex.jump_text = jump_text;
        if let Some(a) = jump {
            self.hex.jump(a);
        }
    }

    fn map_pane(&mut self, ui: &mut Ui, t: &Tokens, defs: &[EcuDef]) {
        let def = self.def_idx.map(|i| &defs[i]);
        let maps = def.map(|d| d.maps.as_slice()).unwrap_or(&[]);
        let bench_maps = self
            .bench
            .as_ref()
            .is_some_and(|b| b.cfg.scene == Scene::Maps);
        let show_all = self.show_all_maps || bench_maps;
        let sel = maps.get(self.map_idx);
        let title = if show_all {
            "All maps".to_string()
        } else {
            sel.map(|m| m.name.clone()).unwrap_or_else(|| "Map".into())
        };
        let mut map_idx = self.map_idx;
        let mut all = self.show_all_maps;
        let mut jump_to = None;
        theme::panel(
            ui,
            t,
            &title,
            |ui| {
                if !maps.is_empty() {
                    egui::ComboBox::from_id_salt("mapsel")
                        .selected_text(format!("{} maps", maps.len()))
                        .width(120.0)
                        .show_ui(ui, |ui| {
                            if ui.selectable_label(all, "All maps").clicked() {
                                all = true;
                            }
                            ui.separator();
                            for (i, m) in maps.iter().enumerate() {
                                if ui.selectable_label(!all && i == map_idx, &m.name).clicked() {
                                    map_idx = i;
                                    all = false;
                                    jump_to = Some(m.address as usize);
                                }
                            }
                        });
                }
                if let (Some(m), false) = (sel, show_all) {
                    if !m.unit.is_empty() {
                        theme::pill(ui, t, &m.unit, Tone::Muted, true);
                    }
                    theme::pill(
                        ui,
                        t,
                        &format!("{} × {}", m.cols, m.rows),
                        Tone::Muted,
                        true,
                    );
                }
            },
            |ui| {
                let Some(img) = &self.image else { return };
                if maps.is_empty() {
                    theme::empty_state(
                        ui,
                        t,
                        icons::STACK,
                        "No maps",
                        "Pick an ECU definition with a map library.",
                    );
                    return;
                }
                let mut area = egui::ScrollArea::both()
                    .id_salt("mappane")
                    .auto_shrink([false, false]);
                if let Some(b) = &self.bench {
                    if bench_maps {
                        area = area.vertical_scroll_offset(wrap(b.offset(), self.maps_max));
                    }
                }
                let out = area.show(ui, |ui| {
                    egui::Frame::new()
                        .inner_margin(Margin::symmetric(14, 12))
                        .show(ui, |ui| {
                            let list: Vec<&opentune_core::ecudef::MapDef> = if show_all {
                                maps.iter().collect()
                            } else {
                                sel.into_iter().collect()
                            };
                            for m in list {
                                ui.horizontal_wrapped(|ui| {
                                    ui.spacing_mut().item_spacing.x = 14.0;
                                    if show_all {
                                        ui.label(
                                            RichText::new(&m.name)
                                                .font(font(DISPLAY, 13.0))
                                                .color(t.fg),
                                        );
                                    }
                                    let ax =
                                        |a: Option<&opentune_core::ecudef::AxisDef>, n: &str| {
                                            a.map(|a| {
                                                if a.name.is_empty() {
                                                    n.to_string()
                                                } else {
                                                    a.name.clone()
                                                }
                                            })
                                            .unwrap_or_else(|| n.to_string())
                                        };
                                    meta(ui, t, "X", &ax(m.x_axis.as_ref(), "x"));
                                    meta(ui, t, "Y", &ax(m.y_axis.as_ref(), "y"));
                                    meta(ui, t, "@", &format!("0x{:06X}", m.address));
                                });
                                ui.add_space(6.0);
                                let range = draw_map(ui, t, &img.bytes, m);
                                ui.add_space(8.0);
                                if let Some((lo, hi)) = range {
                                    ui.horizontal(|ui| {
                                        mapview::legend(ui, t, 160.0, &fmt_val(lo), &fmt_val(hi));
                                        ui.label(
                                            RichText::new("hover a cell for its value")
                                                .small()
                                                .color(t.fg_faint),
                                        );
                                    });
                                }
                                ui.add_space(if show_all { 18.0 } else { 0.0 });
                            }
                        });
                });
                self.maps_max = (out.content_size.y - out.inner_rect.height()).max(1.0);
            },
        );
        self.show_all_maps = all;
        if map_idx != self.map_idx {
            self.map_idx = map_idx;
        }
        if let Some(a) = jump_to {
            self.hex.jump(a);
        }
    }

    fn checksum_pane(&mut self, ui: &mut Ui, t: &Tokens) {
        let mut algo = self.algo.clone();
        let mut run = false;
        let recognised = matches!(&self.checksum, Some(Ok(r)) if r.recognised);
        let has = matches!(&self.checksum, Some(Ok(_)));
        theme::panel(
            ui,
            t,
            "Checksum",
            |ui| {
                egui::ComboBox::from_id_salt("algo")
                    .selected_text(if algo.is_empty() {
                        "algorithm".to_string()
                    } else {
                        algo.clone()
                    })
                    .width(110.0)
                    .show_ui(ui, |ui| {
                        for a in opentune_checksum::algorithms() {
                            if ui
                                .selectable_value(&mut algo, a.id.to_string(), a.id)
                                .on_hover_text(a.name)
                                .clicked()
                            {
                                run = true;
                            }
                        }
                    });
                if has {
                    if recognised {
                        theme::pill(
                            ui,
                            t,
                            &format!("{} Recognised", icons::CHECK),
                            Tone::Ok,
                            false,
                        );
                    } else {
                        theme::pill(ui, t, "Consistent only", Tone::Warn, false).on_hover_text(
                            "The blocks verify, but the image was not recognised as this family.",
                        );
                    }
                }
            },
            |ui| {
                egui::Frame::new()
                    .inner_margin(Margin::symmetric(14, 12))
                    .show(ui, |ui| match &self.checksum {
                        None => {
                            ui.label(
                                RichText::new("Pick an algorithm to verify this dump.")
                                    .color(t.fg_dim),
                            );
                        }
                        Some(Err(e)) => {
                            ui.colored_label(t.crit, e);
                        }
                        Some(Ok(r)) => checksum_body(ui, t, r),
                    });
            },
        );
        if algo != self.algo || run {
            self.algo = algo;
            self.verify();
        }
    }
}

fn checksum_body(ui: &mut Ui, t: &Tokens, r: &ChecksumResult) {
    let bad = r.blocks.iter().filter(|b| !b.ok).count();
    let n = r.blocks.len();
    let (col, soft, icon, big, sub) = if n == 0 {
        (
            t.crit,
            t.crit_soft,
            icons::WARNING,
            "No protected blocks".to_string(),
            format!("{} found nothing to check in this image", r.algo),
        )
    } else if bad == 0 {
        (
            t.ok,
            t.ok_soft,
            icons::CHECK_CIRCLE,
            format!("{n} / {n} blocks OK"),
            format!(
                "stored == computed · {} · {:.1} ms in-process",
                r.algo, r.took_ms
            ),
        )
    } else {
        (
            t.crit,
            t.crit_soft,
            icons::X_CIRCLE,
            format!("{bad} of {n} blocks BAD"),
            "Fix & save writes a corrected copy".to_string(),
        )
    };
    egui::Frame::new()
        .fill(soft)
        .stroke(Stroke::new(1.0, col.gamma_multiply(0.4)))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon).size(30.0).color(col));
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(RichText::new(big).font(font(DISPLAY, 17.0)).color(t.fg));
                    ui.label(RichText::new(sub).small().color(t.fg_dim));
                });
            });
        });
    ui.add_space(10.0);
    egui::ScrollArea::vertical()
        .id_salt("cks")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            egui::Grid::new("cks-table")
                .striped(true)
                .num_columns(5)
                .spacing(Vec2::new(18.0, 6.0))
                .show(ui, |ui| {
                    for h in ["#", "Address", "Stored", "Computed", "Status"] {
                        ui.label(
                            RichText::new(h.to_uppercase())
                                .font(font(UI_STRONG, 10.5))
                                .color(t.fg_faint),
                        );
                    }
                    ui.end_row();
                    let m = |s: String, c| RichText::new(s).font(font(MONO_MEDIUM, 11.5)).color(c);
                    for (i, b) in r.blocks.iter().enumerate() {
                        ui.label(m(format!("{}", i + 1), t.fg_faint));
                        ui.label(m(format!("0x{:06X}", b.start), t.fg))
                            .on_hover_text(&b.name);
                        ui.label(m(format!("0x{:08X}", b.stored), t.fg_dim));
                        ui.label(m(
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
        });
}

/// The definition for a scanned image. The scanner's structural variant
/// (e.g. `EDC15C6`, from the checksum layout) wins over its identify-key
/// match, whose keys can be as broad as a Bosch `1037` software prefix; an
/// identify match is only taken when its family agrees with the structure.
fn pick_def(
    defs: &[EcuDef],
    family: &str,
    variant: Option<&str>,
    matched: Option<&str>,
) -> Option<usize> {
    let norm = |s: &str| s.to_ascii_lowercase().replace([' ', '-', '_'], "");
    if let Some(v) = variant.map(norm).filter(|v| !v.is_empty()) {
        if let Some(i) = defs
            .iter()
            .position(|d| norm(&d.name).contains(&v) || norm(&d.id).contains(&v))
        {
            return Some(i);
        }
    }
    let i = defs.iter().position(|d| Some(d.id.as_str()) == matched)?;
    let fam = norm(family);
    (fam.is_empty() || fam == "unknown" || norm(&defs[i].family) == fam).then_some(i)
}

fn meta(ui: &mut Ui, t: &Tokens, k: &str, v: &str) {
    ui.label(
        RichText::new(format!("{k} "))
            .font(font(MONO_MEDIUM, 11.5))
            .color(t.fg_faint),
    );
    ui.add_space(-12.0);
    ui.label(
        RichText::new(v)
            .font(font(theme::MONO_STRONG, 11.5))
            .color(t.fg),
    );
}

fn size_text(n: usize) -> String {
    if n >= 1 << 20 && n.is_multiple_of(1 << 20) {
        format!("{} MiB", n >> 20)
    } else if n >= 1024 {
        format!("{} KiB", n / 1024)
    } else {
        format!("{n} B")
    }
}

/// Scripted scroll position, wrapping back to the top at the end.
fn wrap(y: f32, max: f32) -> f32 {
    if max <= 1.0 {
        0.0
    } else {
        y % max
    }
}

/// A draggable splitter: `vertical` = a vertical bar moved left/right.
fn splitter(ui: &mut Ui, t: &Tokens, rect: Rect, vertical: bool, frac: &mut f32, total: f32) {
    let id = ui.make_persistent_id(("split", vertical));
    let resp = ui.interact(rect, id, Sense::drag());
    let active = resp.hovered() || resp.dragged();
    if active {
        ui.ctx().set_cursor_icon(if vertical {
            egui::CursorIcon::ResizeHorizontal
        } else {
            egui::CursorIcon::ResizeVertical
        });
    }
    if resp.dragged() && total > 1.0 {
        let d = if vertical {
            resp.drag_delta().x
        } else {
            resp.drag_delta().y
        };
        *frac += d / total;
    }
    let grip = if vertical {
        Vec2::new(4.0, 36.0)
    } else {
        Vec2::new(36.0, 4.0)
    };
    ui.painter().rect_filled(
        Rect::from_center_size(rect.center(), grip),
        CornerRadius::same(3),
        if active { t.accent } else { t.border },
    );
}

pub fn size_label(n: usize) -> String {
    size_text(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(id: &str, name: &str, family: &str) -> EcuDef {
        let mut d = EcuDef::from_json(&format!(
            r#"{{"id":"{id}","name":"{name}","maker":"Bosch","family":"{family}","protocol":"iso14230_kline","diag_layer":"kwp2000","bitrate":10400,"tx_id":1,"rx_id":0,"flash_size":1048576,"regions":[],"full_read":"obd"}}"#
        ))
        .expect("minimal def");
        d.maps.clear();
        d
    }

    #[test]
    fn structural_variant_beats_a_broad_identify_match() {
        let defs = [
            def("bosch-edc16u34", "Bosch EDC16U34", "EDC16"),
            def("bosch-edc15c6", "Bosch EDC15C6", "EDC15"),
        ];
        // The real case: an EDC15C6 dump whose SW 1037… hit EDC16U34's key.
        assert_eq!(
            pick_def(&defs, "EDC15", Some("EDC15C6"), Some("bosch-edc16u34")),
            Some(1)
        );
        // No variant: an identify match from another family is not taken.
        assert_eq!(pick_def(&defs, "EDC15", None, Some("bosch-edc16u34")), None);
        // Same family: taken.
        assert_eq!(
            pick_def(&defs, "EDC16", None, Some("bosch-edc16u34")),
            Some(0)
        );
        // Unknown structure: the identify match stands.
        assert_eq!(
            pick_def(&defs, "Unknown", None, Some("bosch-edc16u34")),
            Some(0)
        );
    }

    #[test]
    fn sizes_and_wrap() {
        assert_eq!(size_text(1 << 20), "1 MiB");
        assert_eq!(size_text(512 * 1024), "512 KiB");
        assert_eq!(size_text(10), "10 B");
        assert_eq!(wrap(250.0, 100.0), 50.0);
        assert_eq!(wrap(5.0, 0.0), 0.0);
    }
}
