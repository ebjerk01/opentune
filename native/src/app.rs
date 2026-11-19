//! The window: top bar (open, identity, definition), left panel (maps,
//! checksum), central view (hex / map / all maps), status bar (frame time).

use crate::bench::{self, Bench, Scene};
use crate::hex::HexView;
use crate::image::Image;
use crate::mapview::draw_map;
use crate::Args;
use eframe::egui::{self, Color32, RichText};
use opentune_checksum::{Recognition, Representation};
use opentune_core::ecudef::EcuDef;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Hex,
    Map,
    AllMaps,
}

struct Identity {
    family: String,
    def_match: Option<String>,
    took_ms: f64,
}

struct Block {
    name: String,
    ok: bool,
    recognised: bool,
}

struct ChecksumResult {
    algo: String,
    blocks: Vec<Block>,
    took_ms: f64,
}

pub struct App {
    defs: Vec<EcuDef>,
    defs_note: String,
    image: Option<Image>,
    identity: Option<Identity>,
    def_idx: Option<usize>,
    map_idx: usize,
    tab: Tab,
    hex: HexView,
    maps_scroll_to: Option<f32>,
    maps_max: f32,
    hex_max: f32,
    algo: String,
    checksum: Option<Result<ChecksumResult, String>>,
    bench: Option<Bench>,
    env: String,
    error: Option<String>,
    last_frame: Option<Instant>,
    frame_ms: f64,
}

fn find_defs_dir(explicit: Option<PathBuf>) -> Option<PathBuf> {
    explicit.or_else(|| {
        ["ecudefs", "../ecudefs"]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_dir())
    })
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, args: Args) -> Self {
        let adapter = cc
            .wgpu_render_state
            .as_ref()
            .map(|rs| {
                let i = rs.adapter.get_info();
                format!(
                    "{} / {:?} / {} {}",
                    i.name, i.backend, i.driver, i.driver_info
                )
            })
            .unwrap_or_else(|| "no wgpu adapter".into());
        let session = if std::env::var("WAYLAND_DISPLAY").is_ok_and(|v| !v.is_empty()) {
            "wayland"
        } else {
            "x11"
        };
        let env = format!(
            "{adapter} | {session} | vsync {}",
            if args.vsync { "on" } else { "off" }
        );

        let (defs, defs_note) = match find_defs_dir(args.defs.clone()) {
            Some(d) => match opentune_core::ecudef::load_dir(&d) {
                Ok(v) => {
                    let n = format!("{} definitions from {}", v.len(), d.display());
                    (v, n)
                }
                Err(e) => (Vec::new(), format!("definitions not loaded: {e}")),
            },
            None => (Vec::new(), "no ecudefs directory found (--defs DIR)".into()),
        };
        let mut app = Self {
            defs,
            defs_note,
            image: None,
            identity: None,
            def_idx: None,
            map_idx: 0,
            tab: Tab::Hex,
            hex: HexView::default(),
            maps_scroll_to: None,
            maps_max: 0.0,
            hex_max: 0.0,
            algo: String::new(),
            checksum: None,
            bench: args.bench.map(Bench::new),
            env,
            error: None,
            last_frame: None,
            frame_ms: 0.0,
        };
        if let Some(id) = &args.def {
            app.select_def(app.defs.iter().position(|d| &d.id == id));
        }
        if let Some(f) = args.file {
            app.open(f);
        }
        if let Some(b) = &app.bench {
            app.tab = match b.cfg.scene {
                Scene::Hex => Tab::Hex,
                Scene::Maps => Tab::AllMaps,
            };
            eprintln!(
                "bench: {} scroll, {} frames, {} px/frame | {}",
                b.cfg.scene.name(),
                b.cfg.frames,
                b.cfg.px_per_frame,
                app.env
            );
        }
        app
    }

    fn select_def(&mut self, idx: Option<usize>) {
        self.def_idx = idx;
        self.map_idx = 0;
        if let Some(c) = idx.and_then(|i| self.defs[i].checksum.clone()) {
            self.algo = c;
        }
    }

    fn open(&mut self, path: PathBuf) {
        match Image::open(path) {
            Ok(img) => {
                // Identify in-process with the same scanner the CLI and the
                // webview app use.
                let t = Instant::now();
                let ctx = opentune_scan::ScanContext {
                    catalog: Vec::new(),
                    ecudefs: self.defs.clone(),
                };
                let root = img.path.parent().unwrap_or(Path::new(".")).to_path_buf();
                let rec = opentune_scan::scan_file(&ctx, &root, &img.path, &img.bytes);
                let def_match = rec.ecudef_match.as_ref().map(|h| h.id.clone());
                let conf = rec
                    .family_confidence
                    .map(|c| format!(" [{c:?}]"))
                    .unwrap_or_default();
                self.identity = Some(Identity {
                    family: format!(
                        "{}{}{}",
                        rec.ecu_family,
                        rec.ecu_variant
                            .map(|v| format!(" ({v})"))
                            .unwrap_or_default(),
                        conf
                    ),
                    def_match: def_match.clone(),
                    took_ms: t.elapsed().as_secs_f64() * 1000.0,
                });
                if let Some(id) = def_match {
                    let idx = self.defs.iter().position(|d| d.id == id);
                    self.select_def(idx);
                }
                self.checksum = None;
                self.error = None;
                self.image = Some(img);
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
                        blocks: v
                            .into_iter()
                            .map(|r| Block {
                                name: r.status.name,
                                ok: r.status.ok,
                                recognised: r.recognition == Recognition::Recognised,
                            })
                            .collect(),
                        took_ms: t.elapsed().as_secs_f64() * 1000.0,
                    })
                    .map_err(|e| e.to_string())
            }
        });
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("Open…").clicked() {
                if let Some(p) = rfd::FileDialog::new()
                    .add_filter("flash image", &["bin", "ori", "mod", "hex"])
                    .pick_file()
                {
                    self.open(p);
                }
            }
            match &self.image {
                Some(img) => {
                    ui.label(RichText::new(img.name()).strong());
                    ui.label(format!("{} KiB", img.bytes.len() / 1024));
                }
                None => {
                    ui.label("no image open");
                }
            }
            if let Some(id) = &self.identity {
                ui.separator();
                ui.label(format!("Family: {}", id.family))
                    .on_hover_text(format!(
                        "identified in-process in {:.1} ms; definition match: {}",
                        id.took_ms,
                        id.def_match.as_deref().unwrap_or("none")
                    ));
            }
            ui.separator();
            let current = self
                .def_idx
                .map(|i| self.defs[i].name.clone())
                .unwrap_or_else(|| "(no definition)".into());
            let mut pick = None;
            egui::ComboBox::from_id_salt("def")
                .selected_text(current)
                .width(220.0)
                .show_ui(ui, |ui| {
                    for (i, d) in self.defs.iter().enumerate() {
                        if ui
                            .selectable_label(self.def_idx == Some(i), &d.name)
                            .clicked()
                        {
                            pick = Some(i);
                        }
                    }
                });
            if pick.is_some() {
                self.select_def(pick);
            }
            ui.separator();
            ui.selectable_value(&mut self.tab, Tab::Hex, "Hex");
            ui.selectable_value(&mut self.tab, Tab::Map, "Map");
            ui.selectable_value(&mut self.tab, Tab::AllMaps, "All maps");
        });
    }

    fn side_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Maps");
        match self.def_idx.map(|i| &self.defs[i]) {
            Some(d) if !d.maps.is_empty() => {
                egui::ScrollArea::vertical()
                    .id_salt("maplist")
                    .max_height(260.0)
                    .show(ui, |ui| {
                        for (i, m) in d.maps.iter().enumerate() {
                            let r = ui.selectable_label(self.map_idx == i, &m.name);
                            if r.clicked() {
                                self.map_idx = i;
                                if self.tab == Tab::Hex {
                                    self.hex.scroll_to = Some(
                                        (m.address as usize / crate::hex::BYTES_PER_ROW) as f32
                                            * self.hex.row_height,
                                    );
                                } else {
                                    self.tab = Tab::Map;
                                }
                            }
                        }
                    });
            }
            Some(_) => {
                ui.label("this definition has no maps");
            }
            None => {
                ui.label("pick a definition");
            }
        }
        ui.separator();
        ui.heading("Checksum");
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("algo")
                .selected_text(if self.algo.is_empty() {
                    "(algorithm)".to_string()
                } else {
                    self.algo.clone()
                })
                .show_ui(ui, |ui| {
                    for a in opentune_checksum::algorithms() {
                        ui.selectable_value(&mut self.algo, a.id.to_string(), a.id)
                            .on_hover_text(a.name);
                    }
                });
            let can = self.image.is_some() && !self.algo.is_empty();
            if ui.add_enabled(can, egui::Button::new("Verify")).clicked() {
                self.verify();
            }
        });
        match &self.checksum {
            Some(Ok(r)) => {
                let bad = r.blocks.iter().filter(|b| !b.ok).count();
                let rec = r.blocks.iter().all(|b| b.recognised) && !r.blocks.is_empty();
                let (col, txt) = if r.blocks.is_empty() {
                    (Color32::LIGHT_RED, "no protected blocks found".to_string())
                } else if bad == 0 {
                    (
                        Color32::LIGHT_GREEN,
                        format!("{}: all {} blocks OK", r.algo, r.blocks.len()),
                    )
                } else {
                    (
                        Color32::LIGHT_RED,
                        format!("{}: {bad} of {} blocks BAD", r.algo, r.blocks.len()),
                    )
                };
                ui.colored_label(col, txt);
                ui.label(format!(
                    "{} · {:.1} ms in-process",
                    if rec {
                        "recognised family"
                    } else {
                        "consistent only"
                    },
                    r.took_ms
                ));
                egui::ScrollArea::vertical()
                    .id_salt("blocks")
                    .max_height(220.0)
                    .show(ui, |ui| {
                        for b in &r.blocks {
                            ui.label(
                                RichText::new(format!(
                                    "{} {}",
                                    if b.ok { "OK " } else { "BAD" },
                                    b.name
                                ))
                                .monospace()
                                .color(if b.ok {
                                    ui.visuals().text_color()
                                } else {
                                    Color32::LIGHT_RED
                                }),
                            );
                        }
                    });
            }
            Some(Err(e)) => {
                ui.colored_label(Color32::LIGHT_RED, e);
            }
            None => {}
        }
        ui.separator();
        ui.small(&self.defs_note);
    }

    fn central(&mut self, ui: &mut egui::Ui) {
        let Some(img) = &self.image else {
            ui.centered_and_justified(|ui| {
                ui.label("Open a flash image (File → Open…, or pass it on the command line).")
            });
            return;
        };
        let def = self.def_idx.map(|i| &self.defs[i]);
        let sel = def.and_then(|d| d.maps.get(self.map_idx));
        match self.tab {
            Tab::Hex => {
                let hl = sel.map(|m| m.address as usize..(m.address + m.byte_len()) as usize);
                if let Some(b) = &self.bench {
                    let y = wrap(b.offset(), self.hex_max);
                    self.hex.scroll_to = Some(y);
                }
                let rows = img.bytes.len().div_ceil(crate::hex::BYTES_PER_ROW) as f32;
                self.hex.show(ui, &img.bytes, hl);
                self.hex_max = (rows * self.hex.row_height - ui.available_height()).max(1.0);
            }
            Tab::Map => match sel {
                Some(m) => {
                    ui.label(RichText::new(&m.name).heading());
                    ui.label(format!(
                        "{}×{} {:?} {:?} @0x{:06X} · {}",
                        m.rows, m.cols, m.data_type, m.endian, m.address, m.unit
                    ));
                    egui::ScrollArea::both()
                        .id_salt("map")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            draw_map(ui, &img.bytes, m);
                        });
                }
                None => {
                    ui.label("pick a definition and a map");
                }
            },
            Tab::AllMaps => {
                let Some(d) = def else {
                    ui.label("pick a definition");
                    return;
                };
                if let Some(b) = &self.bench {
                    self.maps_scroll_to = Some(wrap(b.offset(), self.maps_max));
                }
                let mut area = egui::ScrollArea::both()
                    .id_salt("allmaps")
                    .auto_shrink([false, false]);
                if let Some(y) = self.maps_scroll_to.take() {
                    area = area.vertical_scroll_offset(y);
                }
                let out = area.show(ui, |ui| {
                    for m in &d.maps {
                        ui.label(
                            RichText::new(format!(
                                "{} — {}×{} @0x{:06X} {}",
                                m.name, m.rows, m.cols, m.address, m.unit
                            ))
                            .strong(),
                        );
                        draw_map(ui, &img.bytes, m);
                        ui.add_space(12.0);
                    }
                });
                self.maps_max = (out.content_size.y - out.inner_rect.height()).max(1.0);
            }
        }
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

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let start = Instant::now();
        if let Some(prev) = self.last_frame {
            let dt = start.duration_since(prev).as_secs_f64() * 1000.0;
            self.frame_ms = self.frame_ms * 0.9 + dt * 0.1;
        }
        self.last_frame = Some(start);

        egui::Panel::top("top").show(ui, |ui| self.top_bar(ui));
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.small(format!(
                    "frame {:.1} ms (~{:.0} fps)",
                    self.frame_ms,
                    if self.frame_ms > 0.0 {
                        1000.0 / self.frame_ms
                    } else {
                        0.0
                    }
                ));
                ui.separator();
                ui.small(&self.env);
                if let Some(e) = &self.error {
                    ui.separator();
                    ui.colored_label(Color32::LIGHT_RED, e);
                }
            });
        });
        egui::Panel::left("side")
            .default_size(290.0)
            .show(ui, |ui| self.side_panel(ui));
        egui::CentralPanel::default().show(ui, |ui| self.central(ui));

        if let Some(b) = &mut self.bench {
            if !b.done {
                let cpu = start.elapsed().as_secs_f64() * 1000.0;
                if b.record(start, cpu) {
                    let st = bench::stats(&b.samples);
                    let line = bench::summary_line(b.cfg.scene, &st, &self.env);
                    println!("{line}");
                    if let Some(p) = &b.cfg.csv {
                        if let Err(e) = bench::write_csv(p, &b.samples, &line) {
                            eprintln!("csv: {e}");
                        }
                    }
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            ctx.request_repaint();
        }
    }
}
