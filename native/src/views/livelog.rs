//! Live log: stream logging parameters from the ECU through the hub's
//! live-log engine and record them to CSV (the Tauri app's Live log panel).
//!
//! Pick a logger definition (from a definition pack, or a RomRaider
//! `logger.xml` / pack `LogDef` JSON file), choose channels and options
//! (UDS `0x2C` fast mode with its dynamic DID / record size / session,
//! Subaru SSM over K-line or CAN with module and packet limit, reconnect),
//! then Start. The session is ONE long adapter operation
//! (`AdapterAccess::live_start` / `live_tick` / `live_stop`): it holds the
//! adapter until it stops. Start, the ticks and Stop run in a background
//! job ([`session`]); `ui()` only draws the board the job updates.
//!
//! Each tick updates the live table (value, units, sparkline, note), the
//! plots, the mode / link / rate pills, goes to the CSV recorder (the Tauri
//! live log's columns, so the Log viewer opens the files) and to the Drive
//! view's live feed (`source` "livelog"). Link events (lost, reconnecting,
//! reconnected) are listed, recorded as marker rows and greyed out in the
//! Drive view.
//!
//! The session stops on Stop, when the engine ends it (link lost without
//! reconnect), when the view is left (unless "keep logging in the
//! background" is on) and when the view is dropped (app closed); the engine
//! also stops by itself when no tick arrives for its idle time.
//!
//! This panel never builds a diagnostic request: the live engine (trusted
//! core code) sends the session / `0x2C` requests fast mode needs and never
//! writes ECU data.

mod model;
mod session;
#[cfg(test)]
mod tests;

use crate::host::{Host, JobHandle, NativeView, Tier, ViewGroup};
use crate::icons;
use crate::theme::{self, font, Tokens, Tone, MONO_MEDIUM, UI_MEDIUM, UI_STRONG};
use crate::widgets::caps;
use crate::widgets::csv::CsvRecordControl;
use crate::widgets::gps::GpsCard;
use crate::widgets::packs::{PackKind, PackPicker};
use eframe::egui::{self, Color32, Margin, RichText, Sense, Ui, Vec2};
use egui_plot::{Line, Plot, PlotPoints};
use model::{Board, Options, Param, Phase};
use session::{lock, Ctl, RowSink, Run};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Registration tier, read before the view is built ([`Tier::Bench`]: only
/// alpha / bench builds register it).
pub const TIER: Tier = Tier::Bench;

pub fn create() -> Box<dyn NativeView> {
    Box::new(LivelogView::default())
}

/// Channels charted at most.
const MAX_PLOTS: usize = 6;
/// Plot windows: (label, seconds; 0 = everything kept).
const WINDOWS: [(&str, f64); 4] = [
    ("30 s", 30.0),
    ("2 min", 120.0),
    ("10 min", 600.0),
    ("all", 0.0),
];
/// How long dropping the view waits for the session to stop.
const DROP_WAIT: Duration = Duration::from_secs(3);

/// A loaded logger definition and the channel selection.
struct Def {
    path: PathBuf,
    version: Option<String>,
    params: Vec<Param>,
    checked: Vec<bool>,
}

impl Def {
    /// The chosen channel ids (empty: every enabled parameter).
    fn ids(&self) -> Vec<String> {
        self.params
            .iter()
            .zip(&self.checked)
            .filter(|(_, c)| **c)
            .map(|(p, _)| p.id.clone())
            .collect()
    }
}

/// Read `path` and list its parameters, one per id (a definition can list
/// the same id under several ECU variants; the Tauri `log_params`).
fn load_params(path: &Path) -> Result<Def, String> {
    let def = opentune_hub::livelog::load_def(&path.display().to_string())?;
    let mut seen = std::collections::HashSet::new();
    let params: Vec<Param> = def
        .params()
        .filter(|(_, _, p)| seen.insert(p.id.clone()))
        .map(|(_, _, p)| Param {
            id: p.id.clone(),
            name: p.name.clone(),
            units: p.units.clone(),
        })
        .collect();
    Ok(Def {
        path: path.to_path_buf(),
        version: def.version.clone(),
        checked: vec![false; params.len()],
        params,
    })
}

pub struct LivelogView {
    picker: PackPicker,
    def: Option<Def>,
    load: Option<JobHandle<Def>>,
    note: Option<(Tone, String)>,
    filter: String,
    opts: Options,
    /// Keep logging when the view is left (e.g. for the Drive view).
    keep: bool,
    board: Arc<Mutex<Board>>,
    job: Option<JobHandle<()>>,
    ctl: Option<Arc<Ctl>>,
    rec: Arc<Mutex<CsvRecordControl>>,
    /// Channels charted, in order.
    plotted: Vec<String>,
    window: usize,
    gps: GpsCard,
}

impl Default for LivelogView {
    fn default() -> Self {
        Self {
            picker: PackPicker::new(PackKind::Logger),
            def: None,
            load: None,
            note: None,
            filter: String::new(),
            opts: Options::default(),
            keep: false,
            board: Default::default(),
            job: None,
            ctl: None,
            rec: Default::default(),
            plotted: Vec::new(),
            window: 0,
            gps: GpsCard::default(),
        }
    }
}

impl Drop for LivelogView {
    /// The app is closing (or the view is replaced): end the session so the
    /// adapter is released and the dynamic DIDs are cleared.
    fn drop(&mut self) {
        if let (Some(ctl), Some(job)) = (&self.ctl, &mut self.job) {
            ctl.stop("the Live log view closed");
            let t0 = Instant::now();
            while job.is_running() && t0.elapsed() < DROP_WAIT {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

impl LivelogView {
    fn running(&mut self) -> bool {
        self.job.as_mut().is_some_and(JobHandle::is_running)
    }

    fn open(&mut self, host: &Host<'_>, path: PathBuf) {
        self.note = None;
        self.load = Some(host.spawn_job("reading logger definition", move |_p| load_params(&path)));
    }

    /// Collect finished jobs.
    fn poll(&mut self) {
        if let Some(r) = self.load.as_mut().and_then(JobHandle::take) {
            self.load = None;
            match r {
                Ok(d) => {
                    self.note = Some((
                        Tone::Muted,
                        format!("loaded {} parameter(s)", d.params.len()),
                    ));
                    self.def = Some(d);
                }
                Err(e) => self.note = Some((Tone::Crit, format!("load failed: {e}"))),
            }
        }
        if self.job.as_mut().is_some_and(|j| j.take().is_some()) {
            // The board holds the outcome (also a failed start).
            self.job = None;
            self.ctl = None;
        }
    }

    /// Start a session on the app's adapter and target (in a job: the start
    /// probes and defines, which blocks).
    fn start(&mut self, host: &Host<'_>, ctx: &egui::Context) -> Result<(), String> {
        if self.running() {
            return Err("a session is already running".into());
        }
        let adapter = host
            .adapter()
            .ok_or("no adapter: connect a cable in the top bar first")?;
        let def = self
            .def
            .as_ref()
            .ok_or("choose a logger definition first")?;
        // SSM addresses its own module (K-line address or its CAN ids); the
        // app's target matters for UDS only.
        let target = match host.target() {
            Ok(t) => t,
            Err(_) if model::is_ssm(&self.opts.transport) => crate::host::DiagTarget::ENGINE,
            Err(e) => return Err(format!("no target module: {e}")),
        };
        let ids = def.ids();
        let spec = self
            .opts
            .spec(&def.path.display().to_string(), ids.clone())?;
        *lock(&self.board) = Board::starting(&def.params, &ids);
        let ctl = Arc::new(Ctl::new(
            self.opts.interval_ms,
            self.keep,
            session::frame_nr(ctx),
        ));
        let rec: Arc<Mutex<dyn RowSink>> = self.rec.clone();
        let run = Run {
            adapter,
            spec,
            target,
            ctl: ctl.clone(),
            board: self.board.clone(),
            feed: host.live_feed(),
            rec,
            gps: Some(host.gps()),
            frames: Some(ctx.clone()),
        };
        self.plotted.clear();
        self.job = Some(host.spawn_job("live log session", move |p| session::run(p, run)));
        self.ctl = Some(ctl);
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(c) = &self.ctl {
            c.stop("stopped");
        }
    }

    // -----------------------------------------------------------------------
    // Left column: definition, channels, options, GPS
    // -----------------------------------------------------------------------

    fn setup_ui(&mut self, ui: &mut Ui, host: &Host<'_>, running: bool) {
        let t = *host.tokens();
        card(ui, &t, "Logger definition", |ui| {
            ui.add_enabled_ui(!running, |ui| {
                if let Some(d) = self.picker.ui(ui, host) {
                    self.open(host, d.path);
                }
                ui.horizontal(|ui| {
                    if theme::button(ui, &t, Some(icons::FILE), "Open file…").clicked() {
                        if let Some(p) = host.pick_file(
                            "Logger definition",
                            &[("Logger definition", &["xml", "json"])],
                        ) {
                            self.open(host, p);
                        }
                    }
                    if self.load.is_some() {
                        ui.spinner();
                    }
                });
            });
            if let Some(d) = &self.def {
                let mut line = d.path.display().to_string();
                if let Some(v) = &d.version {
                    line.push_str(&format!("  ·  version {v}"));
                }
                ui.add(
                    egui::Label::new(
                        RichText::new(line)
                            .font(font(MONO_MEDIUM, 11.0))
                            .color(t.fg_dim),
                    )
                    .truncate(),
                );
            }
            if let Some((tone, n)) = &self.note {
                ui.label(RichText::new(n).color(tone_color(&t, *tone)));
            }
        });
        ui.add_space(theme::GAP);
        self.channels_ui(ui, &t, running);
        ui.add_space(theme::GAP);
        self.options_ui(ui, &t, running);
        ui.add_space(theme::GAP);
        egui::CollapsingHeader::new(RichText::new("GPS (optional)").color(t.fg_dim))
            .id_salt("livelog-gps")
            .show(ui, |ui| {
                ui.label(
                    RichText::new(
                        "While a GPS source is open its gps_* channels (and the speed \
                         error against the logged vehicle speed) join the CSV and the \
                         Drive view.",
                    )
                    .color(t.fg_faint),
                );
                self.gps.ui(ui, host);
            });
    }

    fn channels_ui(&mut self, ui: &mut Ui, t: &Tokens, running: bool) {
        let Some(def) = &mut self.def else {
            return;
        };
        let n = def.params.len();
        let chosen = def.checked.iter().filter(|c| **c).count();
        card(ui, t, &format!("Channels ({chosen} of {n})"), |ui| {
            if n == 0 {
                ui.label(RichText::new("No parameters in this definition.").color(t.fg_faint));
                return;
            }
            ui.add_enabled_ui(!running, |ui| {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.filter)
                            .hint_text("filter")
                            .desired_width(150.0),
                    );
                    let f = self.filter.to_lowercase();
                    let matches = |p: &Param| {
                        f.is_empty()
                            || p.name.to_lowercase().contains(&f)
                            || p.id.to_lowercase().contains(&f)
                    };
                    if ui.small_button("All").clicked() {
                        for (p, c) in def.params.iter().zip(def.checked.iter_mut()) {
                            *c |= matches(p);
                        }
                    }
                    if ui.small_button("None").clicked() {
                        def.checked.iter_mut().for_each(|c| *c = false);
                    }
                });
                if chosen == 0 {
                    ui.label(
                        RichText::new("None chosen: Start logs every enabled parameter.")
                            .color(t.fg_faint),
                    );
                }
                let f = self.filter.to_lowercase();
                egui::ScrollArea::vertical()
                    .id_salt("livelog-params")
                    .max_height(260.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for (p, c) in def.params.iter().zip(def.checked.iter_mut()) {
                            if !f.is_empty()
                                && !p.name.to_lowercase().contains(&f)
                                && !p.id.to_lowercase().contains(&f)
                            {
                                continue;
                            }
                            ui.horizontal(|ui| {
                                let label: &str = if p.name.is_empty() { &p.id } else { &p.name };
                                ui.checkbox(c, label);
                                if !p.units.is_empty() {
                                    ui.label(RichText::new(&p.units).color(t.fg_dim));
                                }
                                ui.label(
                                    RichText::new(&p.id)
                                        .font(font(MONO_MEDIUM, 10.5))
                                        .color(t.fg_faint),
                                );
                            });
                        }
                    });
            });
        });
    }

    fn options_ui(&mut self, ui: &mut Ui, t: &Tokens, running: bool) {
        let o = &mut self.opts;
        card(ui, t, "Session options", |ui| {
            ui.add_enabled_ui(!running, |ui| {
                egui::Grid::new("livelog-opts")
                    .num_columns(2)
                    .spacing([10.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Transport");
                        let cur = model::TRANSPORTS
                            .iter()
                            .find(|(id, _)| *id == o.transport)
                            .map_or("UDS / OBD-II", |(_, l)| l);
                        egui::ComboBox::from_id_salt("livelog-transport")
                            .selected_text(cur)
                            .show_ui(ui, |ui| {
                                for (id, label) in model::TRANSPORTS {
                                    ui.selectable_value(&mut o.transport, id.to_string(), label);
                                }
                            });
                        ui.end_row();
                        let ssm = model::is_ssm(&o.transport);
                        if ssm {
                            o.fast = false;
                            ui.label("SSM module");
                            ui.horizontal(|ui| {
                                ui.selectable_value(&mut o.ssm_target, "ecu".into(), "ECU (engine)");
                                ui.selectable_value(
                                    &mut o.ssm_target,
                                    "tcu".into(),
                                    "TCU (transmission)",
                                );
                            });
                            ui.end_row();
                            ui.label("Packet limit");
                            ui.add(
                                egui::TextEdit::singleline(&mut o.ssm_max_packet)
                                    .hint_text("default")
                                    .desired_width(70.0),
                            )
                            .on_hover_text("Bytes per batched A8 request (blank: the engine's default)");
                            ui.end_row();
                        }
                        ui.label("");
                        ui.add_enabled(!ssm, egui::Checkbox::new(&mut o.fast, "Fast (0x2C)"))
                            .on_hover_text(
                                "UDS 0x2C DynamicallyDefineDataIdentifier: one request per record \
                                 instead of one per channel. The ECU decides; otherwise the \
                                 session polls and says why. SSM always batches its reads.",
                            );
                        ui.end_row();
                        let fast = o.fast && !ssm;
                        for (label, val, hint, tip) in [
                            ("Dynamic DID", &mut o.dynamic_did, "F200", "First DDDID, 0xF200–0xF3FF"),
                            ("Max record", &mut o.max_record, "32", "Bytes per dynamic DID (ECU limits vary)"),
                            (
                                "Session",
                                &mut o.session,
                                "keep",
                                "Diagnostic session to enter before defining, e.g. 03 (blank = leave as is)",
                            ),
                        ] {
                            ui.add_enabled(fast, egui::Label::new(label));
                            ui.add_enabled(
                                fast,
                                egui::TextEdit::singleline(val)
                                    .hint_text(hint)
                                    .font(font(MONO_MEDIUM, 12.0))
                                    .desired_width(70.0),
                            )
                            .on_hover_text(tip);
                            ui.end_row();
                        }
                        ui.label("");
                        ui.checkbox(&mut o.reconnect, "Reconnect if the link drops")
                            .on_hover_text(
                                "Hold one session on the adapter; when nothing answers for 5 \
                                 ticks (or the adapter resets) close and re-open it with a \
                                 growing pause (0.5–10 s), re-define the dynamic DIDs and carry \
                                 on. Gives up after 5 minutes down, or after 3 re-opens in a row \
                                 with no answer. Off: the session stops when the link is lost.",
                            );
                        ui.end_row();
                    });
            });
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "Polling speed depends on the adapter: an ELM327 manages only ~1–3 \
                     parameters/s. The dongle's HS/MS-CAN switch must match the module's \
                     bus. SSM over K-line needs a K-line cable.",
                )
                .font(font(UI_MEDIUM, 11.5))
                .color(t.fg_faint),
            );
        });
    }

    // -----------------------------------------------------------------------
    // Right column: controls, status, table, plots
    // -----------------------------------------------------------------------

    fn live_ui(&mut self, ui: &mut Ui, host: &Host<'_>, running: bool) {
        let t = *host.tokens();
        let ctx = ui.ctx().clone();
        card(ui, &t, "Live session", |ui| {
            let gate = caps::gate(ui, host, &model::needs(&self.opts.transport));
            ui.horizontal_wrapped(|ui| {
                if running {
                    let stopping = self.ctl.as_ref().is_some_and(|c| c.stopping());
                    if ui
                        .add_enabled(
                            !stopping,
                            egui::Button::new(
                                RichText::new(format!("{}  Stop", icons::STOP)).color(t.fg),
                            )
                            .fill(t.surface2),
                        )
                        .clicked()
                    {
                        self.stop();
                    }
                } else {
                    let can = gate && self.def.is_some() && self.load.is_none();
                    let label = match &self.def {
                        Some(d) if d.checked.iter().all(|c| !c) => "Start (all channels)",
                        _ => "Start",
                    };
                    let r = ui.add_enabled_ui(can, |ui| {
                        theme::primary_button(ui, &t, Some(icons::PLAY), label)
                    });
                    if r.inner.clicked() {
                        if let Err(e) = self.start(host, &ctx) {
                            self.note = Some((Tone::Crit, e));
                        }
                    }
                }
                ui.label(RichText::new("Interval").color(t.fg_dim));
                let iv = ui.add(
                    egui::DragValue::new(&mut self.opts.interval_ms)
                        .range(20..=10_000)
                        .speed(10)
                        .suffix(" ms"),
                );
                if iv.changed() {
                    if let Some(c) = &self.ctl {
                        c.set_interval(self.opts.interval_ms);
                    }
                }
                if ui
                    .checkbox(&mut self.keep, "Keep logging in the background")
                    .on_hover_text(
                        "Off: leaving this view stops the session (it holds the adapter). \
                         On: it keeps logging, recording and feeding the Drive view until \
                         you come back and press Stop.",
                    )
                    .changed()
                {
                    if let Some(c) = &self.ctl {
                        c.set_keep(self.keep);
                    }
                }
            });
            ui.add_space(4.0);
            let stamp = utc_stamp(model::now_ms());
            lock(&self.rec).ui(
                ui,
                host,
                &format!("opentune-log-{stamp}.csv"),
                &model::CSV_HEAD,
            );
        });
        ui.add_space(theme::GAP);

        let board = self.board.clone();
        let b = lock(&board);
        status_ui(ui, &t, &b);
        ui.add_space(theme::GAP);
        if b.phase == Phase::Idle {
            theme::empty_state(
                ui,
                &t,
                icons::PULSE,
                "No session yet",
                "Choose a logger definition and channels, then press Start.",
            );
            return;
        }
        // Chart the first channels until the user picks.
        if self.plotted.is_empty() && b.ticks > 0 {
            self.plotted = b
                .chans
                .iter()
                .filter(|c| !c.hist.is_empty())
                .take(3)
                .map(|c| c.param.id.clone())
                .collect();
        }
        card(
            ui,
            &t,
            &format!("Values ({} channel(s))", b.chans.len()),
            |ui| {
                table_ui(ui, &t, &b, &mut self.plotted);
            },
        );
        ui.add_space(theme::GAP);
        card(ui, &t, "Plots", |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Window").color(t.fg_dim));
                for (i, (label, _)) in WINDOWS.iter().enumerate() {
                    ui.selectable_value(&mut self.window, i, *label);
                }
            });
            plots_ui(ui, &t, &b, &self.plotted, WINDOWS[self.window].1);
        });
        if !b.events.is_empty() {
            ui.add_space(theme::GAP);
            events_ui(ui, &t, &b);
        }
    }
}

impl NativeView for LivelogView {
    fn id(&self) -> &str {
        "livelog"
    }
    fn label(&self) -> &str {
        "Live log"
    }
    fn icon(&self) -> &str {
        icons::PULSE
    }
    fn group(&self) -> ViewGroup {
        ViewGroup::Vehicle
    }
    fn tier(&self) -> Tier {
        TIER
    }
    fn count(&self) -> Option<String> {
        self.ctl.as_ref().map(|_| "live".into())
    }
    fn ui(&mut self, ui: &mut egui::Ui, host: &mut Host<'_>) {
        if let Some(c) = &self.ctl {
            c.seen(session::frame_nr(ui.ctx()));
        }
        self.poll();
        let running = self.running();
        let host: &Host<'_> = host;
        let wide = ui.available_width() >= 900.0;
        egui::Frame::new()
            .inner_margin(Margin::same(theme::GAP as i8))
            .show(ui, |ui| {
                if wide {
                    let h = ui.available_height();
                    let left_w = (ui.available_width() * 0.34).clamp(320.0, 420.0);
                    ui.horizontal_top(|ui| {
                        ui.allocate_ui_with_layout(
                            Vec2::new(left_w, h),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                                egui::ScrollArea::vertical()
                                    .id_salt("livelog-left")
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| self.setup_ui(ui, host, running));
                            },
                        );
                        ui.add_space(theme::GAP);
                        ui.allocate_ui_with_layout(
                            Vec2::new(ui.available_width(), h),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                                egui::ScrollArea::vertical()
                                    .id_salt("livelog-right")
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| self.live_ui(ui, host, running));
                            },
                        );
                    });
                } else {
                    egui::ScrollArea::vertical()
                        .id_salt("livelog-narrow")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            self.live_ui(ui, host, running);
                            ui.add_space(theme::GAP);
                            self.setup_ui(ui, host, running);
                        });
                }
            });
        if running {
            // The job's ticks ask for frames; this keeps the elapsed time and
            // a slow link's state current between them.
            ui.ctx().request_repaint_after(Duration::from_millis(500));
        }
    }
}

// ---------------------------------------------------------------------------
// Drawing helpers
// ---------------------------------------------------------------------------

fn tone_color(t: &Tokens, tone: Tone) -> Color32 {
    match tone {
        Tone::Muted => t.fg_dim,
        Tone::Accent => t.accent,
        Tone::Ok => t.ok,
        Tone::Warn => t.warn,
        Tone::Crit => t.crit,
    }
}

/// A titled panel card that takes the width it is given.
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

fn pill(ui: &mut Ui, t: &Tokens, b: &model::Badge) {
    let r = theme::pill(ui, t, &b.text, b.tone, false);
    if !b.title.is_empty() {
        r.on_hover_text(&b.title);
    }
}

/// Phase / mode / link pills, rate, target, SSM summary and errors.
fn status_ui(ui: &mut Ui, t: &Tokens, b: &Board) {
    ui.horizontal_wrapped(|ui| {
        let (tone, text) = match b.phase {
            Phase::Idle => (Tone::Muted, "idle"),
            Phase::Starting => (Tone::Accent, "starting…"),
            Phase::Running => (Tone::Ok, "live"),
            Phase::Stopping => (Tone::Accent, "stopping…"),
            Phase::Done => (Tone::Muted, "stopped"),
        };
        theme::pill(ui, t, text, tone, false);
        if let Some(m) = model::mode_badge(&b.status) {
            pill(ui, t, &m);
        }
        if let Some(l) = model::link_badge(&b.link, &b.markers) {
            pill(ui, t, &l);
        }
        let rate = model::fmt_rate(&b.rate);
        if !rate.is_empty() {
            ui.label(
                RichText::new(rate)
                    .font(font(MONO_MEDIUM, 11.5))
                    .color(t.fg_dim),
            );
        }
        if b.ticks > 0 {
            ui.label(
                RichText::new(format!("{} tick(s) · {} sample(s)", b.ticks, b.samples))
                    .font(font(MONO_MEDIUM, 11.5))
                    .color(t.fg_faint),
            );
        }
    });
    let target = model::target_text(&b.target);
    if !target.is_empty() {
        ui.label(
            RichText::new(format!("{}  {target}", icons::CROSSHAIR))
                .font(font(MONO_MEDIUM, 11.5))
                .color(t.fg_dim),
        );
    }
    if b.phase == Phase::Starting {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(
                RichText::new(
                    "starting: probing DIDs / defining records / identifying the module…",
                )
                .color(t.fg_dim),
            );
        });
    }
    if let Some(w) = model::mode_why(&b.status) {
        ui.label(RichText::new(w).color(t.fg_faint));
    }
    if let Some(s) = model::ssm_summary(&b.ssm) {
        ui.label(RichText::new(format!("SSM  {} · {}", s.head, s.logged)).color(t.fg));
        if !s.not_logged.is_empty() {
            ui.label(RichText::new(&s.not_logged).color(t.fg_dim));
        }
        if !s.notes.is_empty() {
            egui::CollapsingHeader::new(format!("Left out ({} reason(s))", s.notes.len()))
                .id_salt("livelog-ssm-notes")
                .show(ui, |ui| {
                    for n in &s.notes {
                        ui.label(RichText::new(n).color(t.fg_dim));
                    }
                });
        }
    }
    if let Some(e) = &b.error {
        ui.label(RichText::new(format!("{}  {e}", icons::WARNING)).color(t.crit));
    }
    if let Some(e) = &b.tick_error {
        ui.label(RichText::new(format!("tick: {e}")).color(t.warn));
    }
    if b.phase == Phase::Done {
        if let Some(e) = &b.ended {
            ui.label(RichText::new(format!("Ended: {e}")).color(t.fg_dim));
        }
    }
}

/// The live table: plot toggle, name, value, units, sparkline, note.
fn table_ui(ui: &mut Ui, t: &Tokens, b: &Board, plotted: &mut Vec<String>) {
    if b.chans.is_empty() {
        ui.label(RichText::new("No parameters selected: nothing to stream.").color(t.fg_faint));
        return;
    }
    egui::ScrollArea::vertical()
        .id_salt("livelog-table")
        .max_height(360.0)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            egui::Grid::new("livelog-values")
                .num_columns(6)
                .striped(true)
                .spacing([12.0, 5.0])
                .show(ui, |ui| {
                    for h in ["", "Parameter", "Value", "Units", "Trend", "Note"] {
                        ui.label(
                            RichText::new(h)
                                .font(font(UI_STRONG, 10.5))
                                .color(t.fg_faint),
                        );
                    }
                    ui.end_row();
                    for c in &b.chans {
                        let id = &c.param.id;
                        let mut on = plotted.contains(id);
                        let can = on || plotted.len() < MAX_PLOTS;
                        if ui
                            .add_enabled(can, egui::Checkbox::without_text(&mut on))
                            .on_hover_text("Chart this channel")
                            .changed()
                        {
                            if on {
                                plotted.push(id.clone());
                            } else {
                                plotted.retain(|p| p != id);
                            }
                        }
                        let name = if c.param.name.is_empty() {
                            id
                        } else {
                            &c.param.name
                        };
                        ui.label(RichText::new(name).color(t.fg)).on_hover_text(id);
                        let (shown, text) =
                            c.last.as_ref().map_or(("—".to_string(), false), |r| {
                                (r.shown(), r.text.is_some())
                            });
                        let layout = if text {
                            egui::Layout::left_to_right(egui::Align::Center)
                        } else {
                            egui::Layout::right_to_left(egui::Align::Center)
                        };
                        ui.allocate_ui_with_layout(Vec2::new(96.0, 18.0), layout, |ui| {
                            ui.label(
                                RichText::new(shown)
                                    .font(font(theme::MONO_STRONG, 13.0))
                                    .color(t.fg),
                            );
                        });
                        ui.label(RichText::new(&c.param.units).color(t.fg_dim));
                        sparkline(ui, t, &c.hist);
                        let note = c.last.as_ref().map(|r| r.note()).unwrap_or("");
                        ui.label(
                            RichText::new(note)
                                .font(font(UI_MEDIUM, 11.5))
                                .color(t.fg_faint),
                        );
                        ui.end_row();
                    }
                });
        });
}

/// A small trend line of the last points.
fn sparkline(ui: &mut Ui, t: &Tokens, hist: &std::collections::VecDeque<[f64; 2]>) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(120.0, 18.0), Sense::hover());
    if hist.len() < 2 || !ui.is_rect_visible(rect) {
        return;
    }
    let pts: Vec<&[f64; 2]> = hist.iter().rev().take(120).collect();
    let (lo, hi) = pts
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), p| {
            (lo.min(p[1]), hi.max(p[1]))
        });
    let span = if hi > lo { hi - lo } else { 1.0 };
    let n = pts.len();
    let line: Vec<egui::Pos2> = pts
        .iter()
        .rev()
        .enumerate()
        .map(|(i, p)| {
            let x = rect.left() + rect.width() * i as f32 / (n - 1).max(1) as f32;
            let y = rect.bottom() - 2.0 - (rect.height() - 4.0) * ((p[1] - lo) / span) as f32;
            egui::pos2(x, y)
        })
        .collect();
    ui.painter()
        .add(egui::Shape::line(line, egui::Stroke::new(1.25, t.accent)));
}

const PALETTE: [Color32; MAX_PLOTS] = [
    Color32::from_rgb(0x4C, 0x9A, 0xFF),
    Color32::from_rgb(0xF5, 0x9E, 0x0B),
    Color32::from_rgb(0x22, 0xC5, 0x5E),
    Color32::from_rgb(0xEC, 0x48, 0x99),
    Color32::from_rgb(0xA8, 0x55, 0xF7),
    Color32::from_rgb(0x14, 0xB8, 0xA6),
];

/// One small plot per charted channel, sharing the time axis; the last
/// `window` seconds (0: everything kept).
fn plots_ui(ui: &mut Ui, t: &Tokens, b: &Board, plotted: &[String], window: f64) {
    let shown: Vec<_> = plotted
        .iter()
        .filter_map(|id| b.chan(id))
        .filter(|c| !c.hist.is_empty())
        .collect();
    if shown.is_empty() {
        ui.label(
            RichText::new("Tick a channel's box in the table to chart it (numeric channels).")
                .color(t.fg_faint),
        );
        return;
    }
    let t_end = shown
        .iter()
        .filter_map(|c| c.hist.back().map(|p| p[0]))
        .fold(0.0_f64, f64::max);
    let t_start = if window > 0.0 {
        (t_end - window).max(0.0)
    } else {
        0.0
    };
    let x1 = t_end.max(t_start + 1.0);
    let link = egui::Id::unique("livelog-plot-x");
    for (i, c) in shown.iter().enumerate() {
        let col = PALETTE[i % PALETTE.len()];
        let pts: Vec<[f64; 2]> = c.hist.iter().filter(|p| p[0] >= t_start).copied().collect();
        let (lo, hi) = pts
            .iter()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), p| {
                (lo.min(p[1]), hi.max(p[1]))
            });
        let pad = if hi > lo { (hi - lo) * 0.08 } else { 1.0 };
        let last = c.last.as_ref().map(model::Row::shown).unwrap_or_default();
        ui.horizontal(|ui| {
            ui.label(RichText::new("■").color(col));
            ui.label(RichText::new(&c.param.name).color(t.fg));
            ui.label(
                RichText::new(format!("{last} {}", c.param.units))
                    .font(font(MONO_MEDIUM, 12.0))
                    .color(t.fg_dim),
            );
        });
        Plot::new(("livelog-plot", &c.param.id))
            .height(96.0)
            .link_axis(link, [true, false])
            .link_cursor(link, [true, false])
            .allow_drag(false)
            .allow_zoom(false)
            .allow_scroll(false)
            .allow_boxed_zoom(false)
            .allow_double_click_reset(false)
            .show_x(true)
            .show_y(true)
            .y_axis_min_width(36.0)
            .x_axis_formatter(|m, _| format!("{:.0} s", m.value))
            .show(ui, |pu| {
                pu.set_plot_bounds_x(t_start..=x1);
                if lo.is_finite() {
                    pu.set_plot_bounds_y((lo - pad)..=(hi + pad));
                }
                pu.line(Line::new("", PlotPoints::new(pts)).color(col).width(1.5));
            });
        ui.add_space(4.0);
    }
}

/// The session's link events, newest first.
fn events_ui(ui: &mut Ui, t: &Tokens, b: &Board) {
    let t0 = b.t0.unwrap_or(0);
    egui::CollapsingHeader::new(
        RichText::new(format!("Link events ({})", b.events.len())).color(t.fg),
    )
    .id_salt("livelog-events")
    .default_open(true)
    .show(ui, |ui| {
        for (ts, state, what) in b.events.iter().rev().take(50) {
            let tone = match state.as_str() {
                "recovered" | "reconnected" => t.ok,
                "silent" => t.warn,
                _ => t.crit,
            };
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("+{:.1} s", ts.saturating_sub(t0) as f64 / 1000.0))
                        .font(font(MONO_MEDIUM, 11.5))
                        .color(t.fg_faint),
                );
                ui.label(RichText::new(state).font(font(UI_STRONG, 11.5)).color(tone));
                ui.label(RichText::new(what).color(t.fg_dim));
            });
        }
    });
}

/// `YYYYMMDD-HHMMSS` (UTC) of a Unix ms time, for file names.
fn utc_stamp(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970-01-01 (H. Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}
