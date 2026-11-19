//! The window: persistent global top bar, sidebar navigation, status bar,
//! and the routed view in the middle.

use crate::bench::{self, Bench};
use crate::settings::{Settings, STORAGE_KEY};
use crate::shell::{sidebar, topbar::TopBar};
use crate::theme::{self, font, Tokens, MONO_MEDIUM};
use crate::views::{self, flash::FlashView, View};
use crate::Args;
use eframe::egui::{self, Margin, RichText};
use opentune_core::ecudef::EcuDef;
use std::path::PathBuf;
use std::time::Instant;

pub struct App {
    settings: Settings,
    top: TopBar,
    view: View,
    defs: Vec<EcuDef>,
    defs_note: String,
    flash: FlashView,
    maps: views::maps::MapsView,
    ecudb: views::ecudb::EcuDbView,
    env: String,
    /// Scripted runs (bench / screenshot) never save settings.
    scripted: bool,
    shot: Option<crate::screenshot::Shot>,
    last_frame: Option<Instant>,
    frame_ms: f64,
}

fn find_defs_dir(explicit: Option<PathBuf>) -> Option<PathBuf> {
    explicit.or_else(|| {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()));
        let mut c: Vec<PathBuf> = ["ecudefs", "../ecudefs"]
            .iter()
            .map(PathBuf::from)
            .collect();
        if let Some(d) = exe_dir {
            c.push(d.join("ecudefs"));
        }
        c.into_iter().find(|p| p.is_dir())
    })
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, args: Args) -> Self {
        let mut settings: Settings = cc
            .storage
            .and_then(|s| eframe::get_value(s, STORAGE_KEY))
            .unwrap_or_default();
        let forced = match args.theme.as_deref() {
            Some("light") => Some(theme::ThemeMode::Light),
            Some("dark") => Some(theme::ThemeMode::Dark),
            _ => None,
        };
        theme::setup(&cc.egui_ctx, forced.unwrap_or(settings.theme));
        let scripted = args.bench.is_some() || args.screenshot.is_some();

        let adapter = cc
            .wgpu_render_state
            .as_ref()
            .map(|rs| {
                let i = rs.adapter.get_info();
                format!("{} / {:?}", i.name, i.backend)
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
                    let n = format!("{} definitions", v.len());
                    (v, n)
                }
                Err(e) => (Vec::new(), format!("definitions not loaded: {e}")),
            },
            None => (Vec::new(), "no ecudefs directory (--defs DIR)".into()),
        };
        let mut flash = FlashView::new(args.bench.clone().map(Bench::new));
        let def_id = args.def.clone().or_else(|| settings.last_def.clone());
        if let Some(id) = &def_id {
            flash.select_def(&defs, defs.iter().position(|d| &d.id == id));
        }
        let file = args.file.clone().or_else(|| {
            settings
                .last_file
                .clone()
                .map(PathBuf::from)
                .filter(|p| p.is_file())
        });
        if let Some(f) = file {
            flash.open(&defs, f, &mut settings);
            // An explicit --def wins over the scanner's match.
            if let Some(id) = &args.def {
                flash.select_def(&defs, defs.iter().position(|d| &d.id == id));
            }
        }
        if let Some(b) = &flash.bench {
            eprintln!(
                "bench: {} scroll, {} frames, {} px/frame | {}",
                b.cfg.scene.name(),
                b.cfg.frames,
                b.cfg.px_per_frame,
                env
            );
        }
        Self {
            settings,
            top: TopBar::default(),
            view: args
                .view
                .as_deref()
                .and_then(View::parse)
                .unwrap_or(View::Flash),
            defs,
            defs_note,
            flash,
            maps: Default::default(),
            ecudb: Default::default(),
            env,
            scripted,
            shot: args.screenshot.clone().map(crate::screenshot::Shot::new),
            last_frame: None,
            frame_ms: 0.0,
        }
    }

    fn status_bar(&self, ui: &mut egui::Ui, t: &Tokens) {
        let st = self.flash.status();
        let m = |s: String, c| RichText::new(s).font(font(MONO_MEDIUM, 11.5)).color(c);
        let sep = |ui: &mut egui::Ui| {
            ui.label(m("•".into(), t.border));
        };
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            match &st.path {
                Some(p) => ui.label(m(p.clone(), t.fg_dim)),
                None => ui.label(m("no dump open".into(), t.fg_faint)),
            };
            if let Some(o) = st.offset {
                sep(ui);
                ui.label(m(format!("offset 0x{o:08X}"), t.fg_dim));
            }
            if let Some(f) = &st.family {
                sep(ui);
                ui.label(m(f.clone(), t.fg_dim));
            }
            sep(ui);
            ui.label(m(self.defs_note.clone(), t.fg_faint));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(m(
                    format!("OpenTune native {}", env!("CARGO_PKG_VERSION")),
                    t.accent,
                ));
                sep(ui);
                let fps = if self.frame_ms > 0.0 {
                    1000.0 / self.frame_ms
                } else {
                    0.0
                };
                ui.label(m(format!("egui · wgpu · {fps:.0} fps"), t.fg_faint))
                    .on_hover_text(&self.env);
            });
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let start = Instant::now();
        if let Some(prev) = self.last_frame {
            let dt = start.duration_since(prev).as_secs_f64() * 1000.0;
            self.frame_ms = if self.frame_ms == 0.0 {
                dt
            } else {
                self.frame_ms * 0.9 + dt * 0.1
            };
        }
        self.last_frame = Some(start);
        let t = theme::tokens(&ctx);

        egui::Panel::top("gbar")
            .frame(theme::bar_frame(&t, Margin::symmetric(14, 9)))
            .show(ui, |ui| self.top.ui(ui, &t, &mut self.settings));
        let flash_size = self
            .flash
            .image
            .as_ref()
            .map(|i| views::flash::size_label(i.bytes.len()));
        let map_count = self
            .flash
            .def_idx
            .map(|i| self.defs[i].maps.len())
            .filter(|n| *n > 0);
        let db_count = self.ecudb.count();
        let counts = move |v: View| match v {
            View::Flash => flash_size.clone(),
            View::Maps => map_count.map(|n| n.to_string()),
            View::EcuDb => db_count.map(|n| n.to_string()),
            View::Drivers => Some(crate::shell::topbar::DRIVERS.len().to_string()),
            _ => None,
        };
        egui::Panel::left("nav")
            .exact_size(230.0)
            .resizable(false)
            .frame(egui::Frame::new().fill(t.surface))
            .show(ui, |ui| {
                if let Some(v) = sidebar::ui(ui, &t, self.view, &counts) {
                    self.view = v;
                }
            });
        egui::Panel::bottom("status")
            .frame(theme::bar_frame(&t, Margin::symmetric(14, 6)))
            .show(ui, |ui| self.status_bar(ui, &t));
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(t.bg))
            .show(ui, |ui| match self.view {
                View::Flash => self.flash.ui(ui, &t, &self.defs, &mut self.settings),
                View::Maps => {
                    if let Some((d, m)) =
                        self.maps.ui(ui, &t, &self.defs, self.flash.image.as_ref())
                    {
                        self.flash.select_def(&self.defs, Some(d));
                        self.flash.open_map(m);
                        self.view = View::Flash;
                    }
                }
                View::EcuDb => self.ecudb.ui(ui, &t),
                View::Drivers => views::drivers::ui(ui, &t),
                other => views::placeholder::ui(ui, &t, other),
            });

        if let Some(s) = &mut self.shot {
            s.step(&ctx);
        }
        if let Some(b) = &mut self.flash.bench {
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

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if !self.scripted {
            eframe::set_value(storage, STORAGE_KEY, &self.settings);
        }
    }
}
