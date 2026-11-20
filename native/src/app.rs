//! The window: persistent global top bar, sidebar navigation, status bar,
//! and the routed view in the middle.

use crate::bench::{self, Bench};
use crate::host::{
    AdapterAccess, Branding, Confirms, Host, HostAction, HostShared, Jobs, NativeConfig,
    NativeView, Tier, ViewGroup,
};
use crate::settings::{Settings, STORAGE_KEY};
use crate::shell::sidebar::{self, NavGroup, NavItem};
use crate::shell::topbar::TopBar;
use crate::theme::{self, font, Tokens, MONO_MEDIUM};
use crate::views::{self, flash::FlashView, View};
use crate::Args;
use eframe::egui::{self, Margin, RichText};
use opentune_core::ecudef::EcuDef;
use std::path::PathBuf;
use std::sync::Arc;
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
    vault: views::vault::VaultView,
    project: views::project::ProjectView,
    logview: views::logview::LogView,
    controllers: views::controllers::ControllersView,
    env: String,
    branding: Branding,
    /// Built-in NativeViews (`views::builtin`), registered for the channel.
    builtin_views: Vec<Box<dyn NativeView>>,
    /// The embedding build's views (Bench-tier ones only on alpha / bench).
    plugin_views: Vec<Box<dyn NativeView>>,
    /// What every view's Host shares: channel, data roots, live feed, GPS.
    shared: HostShared,
    adapter: Option<Arc<dyn AdapterAccess>>,
    /// The built-in adapter (the top bar's link; the views' default adapter).
    hub: Arc<crate::hublink::HubAdapter>,
    diagnostics: Option<Arc<dyn crate::host::DiagnosticsSink>>,
    seedkeys: opentune_hub::seedkeys::SeedKeys,
    jobs: Jobs,
    confirms: Confirms,
    host_status: Option<String>,
    /// Closing with unsaved edits: asking, or the user already agreed.
    quit_ask: bool,
    quit_ok: bool,
    /// Scripted runs (bench / screenshot) never save settings.
    scripted: bool,
    shot: Option<crate::screenshot::Shot>,
    last_frame: Option<Instant>,
    frame_ms: f64,
}

/// The view with `id`: a core view, a built-in, or a plugin view
/// (`HostAction::ShowView`).
fn view_by_id(
    id: &str,
    builtin: &[Box<dyn NativeView>],
    plugin: &[Box<dyn NativeView>],
) -> Option<View> {
    View::parse(id)
        .or_else(|| {
            let i = builtin.iter().position(|v| v.id() == id)?;
            Some(View::Builtin(i as u16))
        })
        .or_else(|| {
            let i = plugin.iter().position(|v| v.id() == id)?;
            Some(View::Plugin(i as u16))
        })
}

/// Open the local diagnostics log (masked records, size caps, rotation) in
/// the app's storage dir and chain the panic hook into it. Local only:
/// nothing is uploaded (see `NativeConfig::diagnostics`). A failure only
/// disables the log.
fn start_diagnostics_log(branding: &crate::host::Branding, channel: &crate::host::BuildChannel) {
    use opentune_diaglog as dl;
    let Some(dir) = eframe::storage_dir(&branding.app_id) else {
        dl::global().disable();
        return;
    };
    let version = env!("CARGO_PKG_VERSION");
    let info = dl::SessionInfo {
        app: branding.name.clone(),
        version: version.to_string(),
        channel: channel.as_str().to_string(),
        webkit: None,
        render: serde_json::json!({ "renderer": "egui/wgpu" }),
    };
    if dl::global()
        .open(
            &dir.join(dl::DIR_NAME),
            dl::session_header_with_core(&info, version),
        )
        .is_err()
    {
        dl::global().disable();
        return;
    }
    dl::install_panic_hook();
}

fn find_defs_dir(explicit: Option<PathBuf>) -> Option<PathBuf> {
    explicit.or_else(|| crate::paths::find("ecudefs"))
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, args: Args, config: NativeConfig) -> Self {
        let channel = config.channel.clone();
        let hub =
            Arc::new(crate::hublink::HubAdapter::with_registry().with_channel(channel.clone()));
        // The tier is decided at registration: a Bench view is never built on
        // a stable channel.
        let builtin_views = views::builtin::views(&channel);
        let plugin_views: Vec<Box<dyn NativeView>> = config
            .views
            .into_iter()
            .filter(|v| views::builtin::admitted(v.tier(), &channel))
            .collect();
        let loader = opentune_hub::seedkeys::loader_for(config.seedkey_loader.as_ref(), &channel)
            .unwrap_or_else(|e| {
                eprintln!("{e}");
                None
            });
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
        if !scripted {
            start_diagnostics_log(&config.branding, &channel);
        }

        let adapter = cc
            .wgpu_render_state
            .as_ref()
            .map(|rs| {
                let i = rs.adapter.get_info();
                format!("{} / {:?}", i.name, i.backend)
            })
            .unwrap_or_else(|| "no wgpu adapter".into());
        let session = if !cfg!(target_os = "linux") {
            std::env::consts::OS
        } else if std::env::var("WAYLAND_DISPLAY").is_ok_and(|v| !v.is_empty()) {
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
        flash.checksums = config.checksums.clone();
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
        let view = args
            .view
            .as_deref()
            .and_then(|s| {
                View::parse(s)
                    .or_else(|| {
                        let i = builtin_views.iter().position(|v| v.id() == s)?;
                        Some(View::Builtin(i as u16))
                    })
                    .or_else(|| {
                        let i = plugin_views.iter().position(|v| v.id() == s)?;
                        Some(View::Plugin(i as u16))
                    })
            })
            .unwrap_or(View::Flash);
        Self {
            settings,
            top: TopBar::new(hub.clone(), channel.clone()),
            view,
            defs,
            defs_note,
            flash,
            maps: Default::default(),
            ecudb: Default::default(),
            vault: Default::default(),
            project: Default::default(),
            logview: Default::default(),
            controllers: Default::default(),
            env,
            branding: config.branding,
            builtin_views,
            plugin_views,
            shared: HostShared::new(channel, Some(cc.egui_ctx.clone())),
            // An embedding build may bring its own adapter for the views;
            // by default they share the top bar's hub session.
            adapter: config
                .adapter
                .or_else(|| Some(hub.clone() as Arc<dyn AdapterAccess>)),
            hub,
            diagnostics: config.diagnostics,
            seedkeys: opentune_hub::seedkeys::SeedKeys::new(config.seedkeys).with_loader(loader),
            jobs: Jobs::new(cc.egui_ctx.clone()),
            confirms: Confirms::default(),
            quit_ask: false,
            quit_ok: false,
            host_status: None,
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
            // Running background jobs: name, bar, message.
            for (name, frac, msg) in self.jobs.active() {
                sep(ui);
                ui.label(m(name, t.fg));
                ui.add(
                    egui::ProgressBar::new(frac)
                        .desired_width(90.0)
                        .desired_height(8.0)
                        .fill(t.accent),
                );
                if !msg.is_empty() {
                    ui.label(m(msg, t.fg_dim));
                }
            }
            if let Some(s) = &self.host_status {
                sep(ui);
                ui.label(m(s.clone(), t.fg_dim));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(m(
                    format!("{} {}", self.branding.name, env!("CARGO_PKG_VERSION")),
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

impl App {
    /// Carry out what a plugin view asked for.
    fn view_by_id(&self, id: &str) -> Option<View> {
        view_by_id(id, &self.builtin_views, &self.plugin_views)
    }

    fn apply(&mut self, actions: Vec<HostAction>) {
        for a in actions {
            match a {
                HostAction::OpenImage(p) => {
                    self.flash.open(&self.defs, p, &mut self.settings);
                    self.view = View::Flash;
                }
                HostAction::SelectDefinition(id) => {
                    let idx = self.defs.iter().position(|d| d.id == id);
                    self.flash.select_def(&self.defs, idx);
                }
                HostAction::SelectMap(m) => self.flash.open_map(m),
                HostAction::HexJump(addr) => self.flash.hex_jump(addr),
                HostAction::ShowView(id) => match self.view_by_id(&id) {
                    Some(v) => self.view = v,
                    None => self.host_status = Some(format!("no view '{id}'")),
                },
                HostAction::EditCells { map, cells } => {
                    match self.flash.apply_edits(&self.defs, map, &cells) {
                        Ok(n) => self.host_status = Some(format!("{n} cell(s) edited")),
                        Err(e) => self.host_status = Some(format!("edit refused: {e}")),
                    }
                }
                HostAction::AddDefinition(def) => {
                    let def = *def;
                    let id = def.id.clone();
                    match self.defs.iter().position(|d| d.id == id) {
                        Some(i) => {
                            self.defs[i] = def;
                            // The selected definition changed: re-select it
                            // so the Flash view re-reads its maps.
                            if self.flash.def_idx == Some(i) {
                                self.flash.select_def(&self.defs, Some(i));
                            }
                        }
                        None => self.defs.push(def),
                    }
                    self.defs_note = format!("{} definitions", self.defs.len());
                    self.host_status = Some(format!("definition '{id}' added for this session"));
                }
            }
        }
    }
}

/// Dropped / opened files with these extensions go to the Log viewer;
/// everything else is opened as a flash dump.
fn is_log_file(p: &std::path::Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "csv" | "tsv" | "log"))
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

        // Drop a dump on the window, or Ctrl+O, to open it.
        let dropped = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .find(|p| p.is_file())
        });
        let ctrl_o = ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::O));
        let pick = dropped.or_else(|| {
            ctrl_o
                .then(|| {
                    rfd::FileDialog::new()
                        .add_filter("flash image", &["bin", "ori", "mod", "hex", "BIN"])
                        .add_filter("datalog", &["csv", "tsv", "log"])
                        .pick_file()
                })
                .flatten()
        });
        if let Some(p) = pick {
            if is_log_file(&p) {
                self.logview.open(&ctx, p);
                self.view = View::Log;
            } else {
                self.flash.open(&self.defs, p, &mut self.settings);
                self.view = View::Flash;
            }
        }
        let hovering_file = ctx.input(|i| !i.raw.hovered_files.is_empty());

        // The one diagnostic target, for the hub link and every view.
        let target = crate::host::resolve_target_with(
            &self.settings.adapter.transport,
            &self.settings.adapter.module,
            &self.settings.adapter.tx,
            &self.settings.adapter.rx,
            self.flash.def_idx.map(|i| &self.defs[i]),
        );
        if let Ok(t) = &target {
            self.hub.set_target(*t);
        }

        // Closing the window with unsaved map edits asks first.
        if ctx.input(|i| i.viewport().close_requested()) && self.flash.is_dirty() && !self.quit_ok {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.quit_ask = true;
        }
        if self.quit_ask {
            match self.confirms.ask(crate::host::ConfirmRequest {
                key: "quit-unsaved".into(),
                title: "Quit without saving?".into(),
                body: "The opened dump has unsaved map edits. Quitting discards them.".into(),
                typed: None,
                confirm_label: "Discard & quit".into(),
            }) {
                crate::host::ConfirmResult::Pending => {}
                crate::host::ConfirmResult::Declined => self.quit_ask = false,
                crate::host::ConfirmResult::Accepted(_) => {
                    self.quit_ask = false;
                    self.quit_ok = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }

        // The top bar's help opens the panel help of the view on screen.
        let view_id = match self.view {
            View::Builtin(i) => self
                .builtin_views
                .get(i as usize)
                .map(|v| v.id().to_string()),
            View::Plugin(i) => self
                .plugin_views
                .get(i as usize)
                .map(|v| v.id().to_string()),
            v => Some(v.id().to_string()),
        };
        self.top.set_view(view_id.as_deref().unwrap_or(""));
        egui::Panel::top("gbar")
            .frame(theme::bar_frame(&t, Margin::symmetric(14, 9)))
            .show(ui, |ui| {
                self.top.ui(ui, &t, &mut self.settings, &self.branding)
            });
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
        let count = |v: View| match v {
            View::Flash => flash_size.clone(),
            View::Maps => map_count.map(|n| n.to_string()),
            View::EcuDb => db_count.map(|n| n.to_string()),
            View::Drivers => Some(views::drivers::count().to_string()),
            _ => None,
        };
        // Every destination with its group and NAV_ORDER rank; a Bench-tier
        // view sits in the Bench group whatever its home group.
        let mut entries: Vec<(ViewGroup, usize, NavItem)> = View::CORE
            .iter()
            .map(|&v| {
                let item = NavItem {
                    view: v,
                    label: v.label().into(),
                    icon: v.icon().into(),
                    count: count(v),
                };
                (v.group(), views::nav_rank(v.id()), item)
            })
            .collect();
        let natives = (self.builtin_views.iter().enumerate())
            .map(|(i, v)| (View::Builtin(i as u16), v))
            .chain(
                (self.plugin_views.iter().enumerate()).map(|(i, v)| (View::Plugin(i as u16), v)),
            );
        for (view, pv) in natives {
            let group = match pv.tier() {
                Tier::Bench => ViewGroup::Bench,
                Tier::Public => pv.group(),
            };
            let item = NavItem {
                view,
                label: pv.label().into(),
                icon: pv.icon().into(),
                count: pv.count(),
            };
            entries.push((group, views::nav_rank(pv.id()), item));
        }
        // Stable sort: unlisted (plugin) views keep their own order.
        entries.sort_by_key(|(g, rank, _)| (views::group_rank(g), *rank));
        let mut groups: Vec<NavGroup> = Vec::new();
        for (g, _, item) in entries {
            let label = g.label().to_string();
            match groups.iter_mut().find(|x| x.label == label) {
                Some(x) => x.items.push(item),
                None => groups.push(NavGroup {
                    label,
                    items: vec![item],
                }),
            }
        }
        let footer = format!("{} {}", self.branding.name, env!("CARGO_PKG_VERSION"));
        egui::Panel::left("nav")
            .exact_size(230.0)
            .resizable(false)
            .frame(egui::Frame::new().fill(t.surface))
            .show(ui, |ui| {
                if let Some(v) = sidebar::ui(ui, &t, self.view, &groups, &footer) {
                    self.view = v;
                }
            });
        egui::Panel::bottom("status")
            .frame(theme::bar_frame(&t, Margin::symmetric(14, 6)))
            .show(ui, |ui| self.status_bar(ui, &t));
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(t.bg))
            .show(ui, |ui| match self.view {
                View::Flash => {
                    self.flash
                        .ui(ui, &t, &self.defs, &mut self.settings, &mut self.confirms)
                }
                View::Maps => {
                    if let Some((d, m)) =
                        self.maps.ui(ui, &t, &self.defs, self.flash.image.as_ref())
                    {
                        self.flash.select_def(&self.defs, Some(d));
                        self.flash.open_map(m);
                        self.view = View::Flash;
                    }
                    // Imported / promoted definitions (HostAction::AddDefinition).
                    let acts = self.maps.take_actions();
                    self.apply(acts);
                }
                View::EcuDb => self.ecudb.ui(ui, &t),
                View::Drivers => views::drivers::ui(ui, &t),
                View::Vault => {
                    let img = self.flash.image.as_ref().map(|i| i.path.clone());
                    if let Some(p) = self.vault.ui(ui, &t, img.as_deref()) {
                        self.flash.open(&self.defs, p, &mut self.settings);
                        self.view = View::Flash;
                    }
                }
                View::Project => {
                    let img = self.flash.image.as_ref().map(|i| i.path.clone());
                    let def_id = self.flash.def_idx.map(|i| self.defs[i].id.clone());
                    if let Some(views::project::ProjectAction::Open { image, def_id }) =
                        self.project.ui(ui, &t, img.as_deref(), def_id.as_deref())
                    {
                        self.flash.open(&self.defs, image, &mut self.settings);
                        if let Some(id) = def_id {
                            let idx = self.defs.iter().position(|d| d.id == id);
                            self.flash.select_def(&self.defs, idx);
                        }
                        self.view = View::Flash;
                    }
                }
                View::Log => {
                    let cx = views::logview::LogContext {
                        def: self.flash.def_idx.map(|i| &self.defs[i]),
                        image: self
                            .flash
                            .image
                            .as_ref()
                            .map(|i| (i.path.as_path(), i.bytes.as_slice())),
                    };
                    self.logview.ui_with(ui, &t, cx);
                    if self.logview.take_show_drive() {
                        self.apply(vec![HostAction::ShowView("drive".into())]);
                    }
                }
                View::Controllers => self.controllers.ui(ui, &t),
                // Built-in and plugin NativeViews: the same Host path.
                View::Builtin(_) | View::Plugin(_) => {
                    let mut actions = Vec::new();
                    let pv = match self.view {
                        View::Builtin(i) => self.builtin_views.get_mut(i as usize),
                        View::Plugin(i) => self.plugin_views.get_mut(i as usize),
                        _ => None,
                    };
                    if let Some(pv) = pv {
                        let img = self.flash.image.as_ref();
                        let mut host = Host {
                            tokens: t,
                            image_path: img.map(|i| i.path.as_path()),
                            image: img.map(|i| i.bytes.as_slice()),
                            image_edited: img.is_some_and(|i| i.is_dirty()),
                            defs: &self.defs,
                            def_idx: self.flash.def_idx,
                            map_idx: self.flash.map_idx,
                            checksums: &self.flash.checksums,
                            adapter: self.adapter.as_ref(),
                            diagnostics: self.diagnostics.as_ref(),
                            seedkeys: &self.seedkeys,
                            target: target.clone(),
                            jobs: &self.jobs,
                            confirms: &mut self.confirms,
                            actions: &mut actions,
                            status: &mut self.host_status,
                            shared: &self.shared,
                        };
                        pv.ui(ui, &mut host);
                    }
                    self.apply(actions);
                }
            });
        self.confirms.ui(&ctx);

        if hovering_file {
            let r = ctx.content_rect();
            let p = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::unique("drop"),
            ));
            p.rect_filled(r, 0.0, t.bg.gamma_multiply(0.85));
            p.rect_stroke(
                r.shrink(18.0),
                egui::CornerRadius::same(theme::RADIUS),
                egui::Stroke::new(2.0, t.accent),
                egui::StrokeKind::Inside,
            );
            p.text(
                r.center(),
                egui::Align2::CENTER_CENTER,
                format!("{}  Drop to open the dump", crate::icons::FOLDER_OPEN),
                theme::font(theme::DISPLAY, 22.0),
                t.accent,
            );
        }
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

#[cfg(test)]
mod show_view_tests {
    use super::*;

    #[test]
    fn show_view_resolves_core_builtin_and_plugin_ids() {
        let builtin = views::builtin::views(&crate::host::BuildChannel::compiled());
        let analyze = builtin.iter().position(|v| v.id() == "analyze").unwrap();
        let plugin: Vec<Box<dyn NativeView>> = Vec::new();
        assert_eq!(view_by_id("flash", &builtin, &plugin), Some(View::Flash));
        assert_eq!(view_by_id("maps", &builtin, &plugin), Some(View::Maps));
        assert_eq!(
            view_by_id("analyze", &builtin, &plugin),
            Some(View::Builtin(analyze as u16))
        );
        assert_eq!(view_by_id("no-such-view", &builtin, &plugin), None);
        assert_eq!(
            HostAction::ShowView("flash".into()),
            HostAction::ShowView("flash".into())
        );
        assert_ne!(
            HostAction::ShowView("flash".into()),
            HostAction::ShowView("maps".into())
        );
    }
}
