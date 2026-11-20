//! Drive view: a few big, glanceable tiles for live values or a replayed
//! log (the Tauri app's Drive view, `feat_driveview.js` +
//! `driveview-core.js`).
//!
//! * **Replay** (every build): open any log the Log viewer reads (a Live-log
//!   CSV, a Dashboard recording, a CLI log or a generic CSV) and play it
//!   back at 1–8× ([`opentune_logview::series::replay`]); seek, pause, and
//!   min / max hold that never skips a spike between frames. Offline.
//! * **Live** (alpha / bench builds only): the tiles follow the app's live
//!   feed, which the Dashboard and the Live log publish their polls to. This
//!   view never opens an adapter operation itself; on a stable build no
//!   publisher exists and the mode is not offered.
//!
//! Tiles pick sensible defaults (RPM, speed, coolant, intake air, boost,
//! load) from the channel names and units, colour by the OBD table's warn /
//! critical thresholds when the units agree (never converted), and grey out
//! when a value is stale or the link is lost. Keys: Space pause / play, R
//! reset min / max, F focus (controls hidden), Esc leave focus.

mod core;
mod live;
#[cfg(test)]
mod tests;

use crate::host::{FeedEvent, Host, HostAction, JobHandle, NativeView, Tier, ViewGroup};
use crate::icons;
use crate::theme::{self, font, Tokens, Tone, MONO_MEDIUM, UI_MEDIUM, UI_STRONG};
use core::{Chan, Clock, Level, Link, PUnit, Peak, Prepared, TileState};
use eframe::egui::{self, Margin, Pos2, Rect, RichText, Sense, Shape, Stroke, Ui, Vec2};
use live::LiveState;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Registration tier: Replay is public; Live is offered only when the Bench
/// tier is on.
pub const TIER: Tier = Tier::Public;

pub fn create() -> Box<dyn NativeView> {
    Box::new(DriveView::default())
}

/// A log another view asked to replay here (the Log viewer's "Drive view").
static QUEUED: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Ask the Drive view to replay `path` the next time it is shown.
pub(crate) fn queue_replay(path: PathBuf) {
    *QUEUED.lock().unwrap_or_else(|e| e.into_inner()) = Some(path);
}

fn take_queued() -> Option<PathBuf> {
    QUEUED.lock().unwrap_or_else(|e| e.into_inner()).take()
}

#[cfg(test)]
pub(crate) fn take_queued_for_test() -> Option<PathBuf> {
    take_queued()
}

fn now_s() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Mode {
    #[default]
    Replay,
    Live,
}

/// A loaded replay.
struct Rp {
    path: PathBuf,
    data: Prepared,
    clock: Clock,
    peaks: HashMap<String, Peak>,
}

impl Rp {
    fn new(path: PathBuf, data: Prepared) -> Self {
        let clock = Clock::new(data.t0, data.t1, data.rate);
        Self {
            path,
            data,
            clock,
            peaks: HashMap::new(),
        }
    }

    /// Fold the samples passed between two frames into the peaks of `sel`.
    fn advance(&mut self, wall: f64, sel: &[String]) -> bool {
        let step = self.clock.tick(wall);
        if step.to > step.from {
            for k in sel {
                if let Some(c) = self.data.chan(k) {
                    core::fold(
                        &self.data,
                        c,
                        self.peaks.entry(k.clone()).or_default(),
                        step.from,
                        step.to,
                    );
                }
            }
        }
        step.ended
    }

    fn seek(&mut self, x: f64, wall: f64, sel: &[String]) {
        self.clock.seek(x, wall);
        for k in sel {
            if let Some(c) = self.data.chan(k) {
                if let Some((v, at)) = core::sample(&self.data, c, self.clock.pos) {
                    self.peaks.entry(k.clone()).or_default().add(Some(v), at);
                }
            }
        }
    }
}

pub struct DriveView {
    mode: Mode,
    punit: PUnit,
    focus: bool,
    epoch: Instant,
    rp: Option<Rp>,
    load: Option<(PathBuf, JobHandle<Prepared>)>,
    sel_replay: Vec<String>,
    rx: Option<Receiver<FeedEvent>>,
    live: LiveState,
    /// Live display frozen (Space): what was shown when paused.
    frozen: Option<(LiveState, f64)>,
    sel_live: Vec<String>,
    note: Option<(Tone, String)>,
    /// Asked for during `ui` (switch to the Dashboard).
    pending_action: Option<HostAction>,
}

impl Default for DriveView {
    fn default() -> Self {
        Self {
            mode: Mode::Replay,
            punit: PUnit::Bar,
            focus: false,
            epoch: Instant::now(),
            rp: None,
            load: None,
            sel_replay: Vec::new(),
            rx: None,
            live: LiveState::default(),
            frozen: None,
            sel_live: Vec::new(),
            note: None,
            pending_action: None,
        }
    }
}

impl DriveView {
    fn wall(&self) -> f64 {
        self.epoch.elapsed().as_secs_f64() * 1000.0
    }

    /// Load `path` for replay in a job.
    fn open(&mut self, host: &Host<'_>, path: PathBuf) {
        let p = path.clone();
        self.mode = Mode::Replay;
        self.note = None;
        self.load = Some((
            path,
            host.spawn_job("reading log for replay", move |_| load_replay(&p)),
        ));
    }

    fn poll_load(&mut self) {
        let Some((path, job)) = &mut self.load else {
            return;
        };
        let Some(r) = job.take() else {
            return;
        };
        let path = path.clone();
        self.load = None;
        match r {
            Ok(d) => {
                let mut notes = Vec::new();
                if d.index {
                    notes.push(format!(
                        "no time column: replayed at {} samples per second at 1×",
                        core::INDEX_HZ
                    ));
                }
                if d.stride > 1 {
                    notes.push(format!(
                        "long log: every {} rows replayed as one (latest value of each)",
                        d.stride
                    ));
                }
                self.sel_replay = core::selection_for(&d.list(), &self.sel_replay);
                let mut rp = Rp::new(path, d);
                let wall = self.wall();
                let sel = self.sel_replay.clone();
                rp.seek(rp.data.t0, wall, &sel);
                self.rp = Some(rp);
                self.note = (!notes.is_empty()).then(|| (Tone::Muted, notes.join("; ")));
            }
            Err(e) => self.note = Some((Tone::Crit, format!("{}: {e}", path.display()))),
        }
    }

    /// Follow the live feed while Live is shown.
    fn pump_live(&mut self, host: &Host<'_>) {
        if self.mode != Mode::Live || !host.bench_enabled() {
            self.rx = None;
            return;
        }
        let rx = self.rx.get_or_insert_with(|| host.live_feed().subscribe());
        let now = now_s();
        let evs: Vec<FeedEvent> = rx.try_iter().collect();
        for ev in &evs {
            let t = match ev {
                FeedEvent::Frame { t_ms, .. } if *t_ms > 0 => (*t_ms as f64 / 1000.0).min(now),
                _ => now,
            };
            self.live.apply(ev, t);
        }
        if self.live.fresh {
            self.live.fresh = false;
            self.sel_live = core::selection_for(&self.live.list(), &self.sel_live);
        }
    }

    fn toggle_pause(&mut self) {
        match self.mode {
            Mode::Replay => {
                let wall = self.wall();
                if let Some(rp) = &mut self.rp {
                    if rp.clock.playing {
                        rp.clock.pause(wall);
                    } else {
                        rp.clock.play(wall);
                    }
                }
            }
            Mode::Live => {
                self.frozen = match self.frozen.take() {
                    Some(_) => None,
                    None => Some((self.live.clone(), now_s())),
                };
            }
        }
    }

    fn reset_peaks(&mut self) {
        match self.mode {
            Mode::Replay => {
                let (wall, sel) = (self.wall(), self.sel_replay.clone());
                if let Some(rp) = &mut self.rp {
                    rp.peaks.clear();
                    let pos = rp.clock.pos;
                    rp.seek(pos, wall, &sel);
                }
            }
            Mode::Live => self.live.reset_peaks(),
        }
    }

    fn keys(&mut self, ui: &Ui) {
        if ui.ctx().egui_wants_keyboard_input() {
            return;
        }
        let (space, r, f, esc) = ui.input(|i| {
            let plain = !(i.modifiers.ctrl || i.modifiers.command || i.modifiers.alt);
            (
                plain && i.key_pressed(egui::Key::Space),
                plain && i.key_pressed(egui::Key::R),
                plain && i.key_pressed(egui::Key::F),
                i.key_pressed(egui::Key::Escape),
            )
        });
        if space {
            self.toggle_pause();
        }
        if r {
            self.reset_peaks();
        }
        if f {
            self.focus = !self.focus;
        }
        if esc {
            self.focus = false;
        }
    }

    fn chans(&self) -> Vec<Chan> {
        match self.mode {
            Mode::Replay => self.rp.as_ref().map(|r| r.data.list()).unwrap_or_default(),
            Mode::Live => self.live.list(),
        }
    }

    fn sel_mut(&mut self) -> &mut Vec<String> {
        match self.mode {
            Mode::Replay => &mut self.sel_replay,
            Mode::Live => &mut self.sel_live,
        }
    }

    fn bar_ui(&mut self, ui: &mut Ui, host: &Host<'_>) {
        let t = *host.tokens();
        let bench = host.bench_enabled();
        let mut open = false;
        let mut show_dash = false;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
            if bench {
                ui.selectable_value(&mut self.mode, Mode::Live, "Live")
                    .on_hover_text("Follow the Dashboard / Live log polls");
            }
            ui.selectable_value(&mut self.mode, Mode::Replay, "Replay")
                .on_hover_text("Play a recorded log back");
            ui.separator();
            match self.mode {
                Mode::Replay => {
                    if theme::button(ui, &t, Some(icons::FOLDER_OPEN), "Open log").clicked() {
                        open = true;
                    }
                }
                Mode::Live => {
                    if theme::button(ui, &t, Some(icons::SPEEDOMETER), "Dashboard")
                        .on_hover_text("Start polling there; the tiles follow")
                        .clicked()
                    {
                        show_dash = true;
                    }
                }
            }
            ui.selectable_value(&mut self.punit, PUnit::Bar, "bar");
            ui.selectable_value(&mut self.punit, PUnit::KPa, "kPa");
            let chans = self.chans();
            let sel = self.sel_mut();
            ui.menu_button(
                format!("{}  Tiles ({})", icons::GRID_FOUR, sel.len()),
                |ui| {
                    if chans.is_empty() {
                        ui.label("No channels yet.");
                    }
                    if ui.button("Default tiles").clicked() {
                        *sel = core::pick_default(&chans);
                    }
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .max_height(360.0)
                        .show(ui, |ui| {
                            for c in &chans {
                                let mut on = sel.contains(&c.key);
                                let label = if c.unit.is_empty() {
                                    c.name.clone()
                                } else {
                                    format!("{} [{}]", c.name, c.unit)
                                };
                                if ui.checkbox(&mut on, label).changed() {
                                    if on {
                                        sel.push(c.key.clone());
                                    } else {
                                        sel.retain(|k| k != &c.key);
                                    }
                                }
                            }
                        });
                },
            );
            if theme::icon_button(
                ui,
                &t,
                icons::ARROW_COUNTER_CLOCKWISE,
                "Reset min / max (R)",
            )
            .clicked()
            {
                self.reset_peaks();
            }
            if theme::icon_button(
                ui,
                &t,
                icons::ARROWS_OUT,
                "Focus: hide the controls (F, Esc)",
            )
            .clicked()
            {
                self.focus = true;
            }
        });
        if open {
            if let Some(p) = host.pick_file(
                "Open a log to replay",
                &[("datalog", &["csv", "CSV", "txt", "log", "tsv"])],
            ) {
                self.open(host, p);
            }
        }
        if show_dash {
            self.pending_action = Some(HostAction::ShowView("dashboard".into()));
        }
    }

    fn transport_ui(&mut self, ui: &mut Ui, t: &Tokens) {
        let wall = self.wall();
        let sel = self.sel_replay.clone();
        let Some(rp) = &mut self.rp else {
            return;
        };
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(8.0, 6.0);
            let (icon, tip) = if rp.clock.playing {
                (icons::PAUSE, "Pause (Space)")
            } else {
                (icons::PLAY, "Play (Space)")
            };
            if theme::icon_button(ui, t, icon, tip).clicked() {
                if rp.clock.playing {
                    rp.clock.pause(wall);
                } else {
                    rp.clock.play(wall);
                }
            }
            if theme::icon_button(ui, t, icons::REWIND, "Back to the start").clicked() {
                let t0 = rp.data.t0;
                rp.seek(t0, wall, &sel);
            }
            for s in core::SPEEDS {
                if ui
                    .selectable_label(rp.clock.speed == s, format!("{s}×"))
                    .clicked()
                {
                    rp.clock.set_speed(s, wall);
                }
            }
            let idx = rp.data.index;
            ui.label(
                RichText::new(format!(
                    "{} / {}",
                    core::fmt_clock(rp.clock.pos - if idx { 0.0 } else { rp.data.t0 }, idx),
                    core::fmt_clock(rp.data.t1 - if idx { 0.0 } else { rp.data.t0 }, idx)
                ))
                .font(font(MONO_MEDIUM, 12.5))
                .color(t.fg_dim),
            );
            let mut pos = rp.clock.pos;
            ui.spacing_mut().slider_width = ui.available_width().max(80.0);
            let r = ui.add(
                egui::Slider::new(&mut pos, rp.data.t0..=rp.data.t1.max(rp.data.t0 + 1e-9))
                    .show_value(false),
            );
            if r.changed() {
                rp.seek(pos, wall, &sel);
            }
        });
    }

    fn status(&self, now: f64) -> (Tone, String, String) {
        match self.mode {
            Mode::Replay => match &self.rp {
                None if self.load.is_some() => (Tone::Accent, "Loading".into(), String::new()),
                None => (
                    Tone::Muted,
                    "Replay".into(),
                    "open a log (Live-log CSV, Dashboard recording, CLI log or any CSV the \
                     Log viewer reads)"
                        .into(),
                ),
                Some(rp) => {
                    let name = rp
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    if rp.clock.playing {
                        (Tone::Accent, format!("Replay {}×", rp.clock.speed), name)
                    } else {
                        (Tone::Muted, "Replay paused".into(), name)
                    }
                }
            },
            Mode::Live => {
                if let Some((_, at)) = &self.frozen {
                    return (
                        Tone::Warn,
                        "Paused".into(),
                        format!(
                            "display frozen {} — these values are not live · Space to resume",
                            core::fmt_age(now - at)
                        ),
                    );
                }
                let l = &self.live;
                let src = match l.src.as_deref() {
                    Some("livelog") => "Live log session",
                    Some(_) => "Dashboard polling",
                    None => "no source",
                };
                match l.link(now) {
                    Link::Up => (
                        Tone::Ok,
                        "Live".into(),
                        format!("{src} · every {:.0} ms", l.interval * 1000.0),
                    ),
                    Link::Waiting => (
                        Tone::Muted,
                        "Not running".into(),
                        "start the Dashboard or a Live log session; the tiles follow it".into(),
                    ),
                    Link::Stopped => (
                        Tone::Muted,
                        "Stopped".into(),
                        format!("{src} stopped — values below are not live"),
                    ),
                    Link::Lost | Link::Replay => (
                        Tone::Crit,
                        "Link lost".into(),
                        format!(
                            "{}no fresh values for {:.0} s",
                            if l.last_error.is_empty() {
                                String::new()
                            } else {
                                format!("{} · ", l.last_error)
                            },
                            (now - l.last_frame.unwrap_or(now)).max(0.0).floor()
                        ),
                    ),
                }
            }
        }
    }

    /// The tiles of the current mode.
    fn tiles(&self, now: f64) -> Vec<Tile> {
        match self.mode {
            Mode::Replay => {
                let Some(rp) = &self.rp else {
                    return Vec::new();
                };
                let d = &rp.data;
                let x = rp.clock.pos;
                let stale = core::stale_after(d.dt);
                let win = core::HIST_S * d.rate;
                self.sel_replay
                    .iter()
                    .filter_map(|k| d.chan(k))
                    .map(|c| {
                        let s = core::sample(d, c, x);
                        Tile::new(
                            &c.chan,
                            s.map(|s| s.0),
                            s.map(|s| s.1),
                            x,
                            stale,
                            Link::Replay,
                            rp.peaks.get(&c.chan.key).copied().unwrap_or_default(),
                            core::window(d, c, x - win, x, 120),
                            (x - win, x),
                        )
                    })
                    .collect()
            }
            Mode::Live => {
                let (l, now) = match &self.frozen {
                    Some((l, at)) => (l, *at),
                    None => (&self.live, now),
                };
                let link = l.link(now);
                self.sel_live
                    .iter()
                    .filter_map(|k| l.chan(k))
                    .filter(|c| {
                        // Optional tiles (GPS) stay hidden until they have a value.
                        !core::role_of(&c.chan).is_some_and(|r| r.optional) || c.last.is_some()
                    })
                    .map(|c| {
                        Tile::new(
                            &c.chan,
                            c.last.map(|s| s.0),
                            c.last.map(|s| s.1),
                            now,
                            l.stale_s(),
                            link,
                            c.peak,
                            c.hist.0.iter().copied().collect(),
                            (now - core::HIST_S, now),
                        )
                    })
                    .collect()
            }
        }
    }
}

fn load_replay(path: &std::path::Path) -> Result<Prepared, String> {
    let log = opentune_logview::parse_file(path, &opentune_logview::ParseOptions::default())
        .map_err(|e| e.to_string())?;
    core::prepare(&log, core::REPLAY_ROWS)
}

impl NativeView for DriveView {
    fn id(&self) -> &str {
        "drive"
    }
    fn label(&self) -> &str {
        "Drive view"
    }
    fn icon(&self) -> &str {
        icons::STEERING_WHEEL
    }
    fn group(&self) -> ViewGroup {
        ViewGroup::Calibration
    }
    fn tier(&self) -> Tier {
        TIER
    }
    fn count(&self) -> Option<String> {
        match self.mode {
            Mode::Replay if self.rp.as_ref().is_some_and(|r| r.clock.playing) => Some("▶".into()),
            _ => None,
        }
    }
    fn ui(&mut self, ui: &mut egui::Ui, host: &mut Host<'_>) {
        if let Some(p) = take_queued() {
            self.open(host, p);
        }
        if !host.bench_enabled() {
            self.mode = Mode::Replay;
        }
        self.poll_load();
        self.pump_live(host);
        self.keys(ui);
        let wall = self.wall();
        let sel = self.sel_replay.clone();
        let mut playing = false;
        if self.mode == Mode::Replay {
            if let Some(rp) = &mut self.rp {
                rp.advance(wall, &sel);
                playing = rp.clock.playing;
            }
        }
        let now = now_s();
        let t = *host.tokens();
        {
            let host: &Host<'_> = host;
            egui::Frame::new()
                .inner_margin(Margin::same(theme::GAP as i8))
                .show(ui, |ui| {
                    if !self.focus {
                        self.bar_ui(ui, host);
                        ui.add_space(6.0);
                    }
                    let (tone, head, sub) = self.status(now);
                    ui.horizontal_wrapped(|ui| {
                        theme::pill(ui, &t, &head, tone, false);
                        if !sub.is_empty() {
                            ui.label(RichText::new(sub).color(t.fg_dim));
                        }
                        if self.focus {
                            ui.label(
                                RichText::new("Esc: show controls")
                                    .small()
                                    .color(t.fg_faint),
                            );
                        }
                    });
                    if let Some((tone, n)) = &self.note {
                        let col = if *tone == Tone::Crit {
                            t.crit
                        } else {
                            t.fg_dim
                        };
                        ui.add(egui::Label::new(RichText::new(n).color(col)).wrap());
                    }
                    if self.mode == Mode::Replay && !self.focus {
                        ui.add_space(4.0);
                        self.transport_ui(ui, &t);
                    }
                    ui.add_space(theme::GAP);
                    let tiles = self.tiles(now);
                    if tiles.is_empty() {
                        let text = match self.mode {
                            Mode::Replay => "Open a log to replay it here.",
                            Mode::Live => {
                                "No channels yet: start the Dashboard or a Live log session."
                            }
                        };
                        theme::empty_state(ui, &t, icons::STEERING_WHEEL, "Drive view", text);
                    } else {
                        tiles_ui(ui, &t, &tiles, self.punit);
                    }
                });
        }
        if let Some(a) = self.pending_action.take() {
            host.act(a);
        }
        if playing {
            ui.ctx().request_repaint_after(Duration::from_millis(33));
        } else if self.mode == Mode::Live || self.load.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }
    }
}

// ---------------------------------------------------------------------------
// Tiles
// ---------------------------------------------------------------------------

/// Everything one tile shows.
struct Tile {
    label: String,
    unit: String,
    v: Option<f64>,
    signed: bool,
    level: Level,
    state: TileState,
    age: Option<f64>,
    peak: Peak,
    th: core::Thresholds,
    spark: Vec<Vec<[f64; 2]>>,
}

impl Tile {
    #[allow(clippy::too_many_arguments)]
    fn new(
        c: &Chan,
        v: Option<f64>,
        at: Option<f64>,
        now: f64,
        stale_s: f64,
        link: Link,
        peak: Peak,
        hist: Vec<(f64, Option<f64>)>,
        (ta, tb): (f64, f64),
    ) -> Self {
        let role = core::role_of(c);
        let th = core::thresholds_for(c);
        Tile {
            label: match role {
                Some(r) if !c.name.is_empty() && c.name.to_lowercase() != r.role => {
                    format!("{} · {}", r.label, c.name)
                }
                Some(r) => r.label.to_string(),
                None => c.name.clone(),
            },
            unit: c.unit.clone(),
            v,
            signed: role.is_some_and(|r| r.signed),
            level: core::level(v, &th),
            state: core::tile_state(link, at, now, stale_s),
            age: at.map(|a| now - a),
            peak,
            th,
            spark: core::spark(&hist, ta, tb),
        }
    }
}

fn tiles_ui(ui: &mut Ui, t: &Tokens, tiles: &[Tile], p: PUnit) {
    let n = tiles.len();
    let w = ui.available_width();
    let cols = ((w / 300.0).floor() as usize).clamp(1, 4).min(n);
    let rows = n.div_ceil(cols);
    let gap = 10.0;
    let tw = (w - gap * (cols as f32 - 1.0)) / cols as f32;
    let th =
        ((ui.available_height() - gap * (rows as f32 - 1.0)) / rows as f32).clamp(150.0, 320.0);
    for row in tiles.chunks(cols) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for tile in row {
                let (rect, _) = ui.allocate_exact_size(Vec2::new(tw, th), Sense::hover());
                paint_tile(ui, t, rect, tile, p);
            }
        });
        ui.add_space(gap);
    }
}

fn paint_tile(ui: &Ui, t: &Tokens, rect: Rect, tile: &Tile, p: PUnit) {
    let painter = ui.painter_at(rect);
    let edge = match tile.level {
        Level::Crit => t.crit,
        Level::Warn => t.warn,
        Level::Normal => t.border,
    };
    painter.rect_filled(rect, theme::RADIUS as f32, t.surface);
    painter.rect_stroke(
        rect,
        theme::RADIUS as f32,
        Stroke::new(
            if tile.level == Level::Normal {
                1.0
            } else {
                2.5
            },
            edge,
        ),
        egui::StrokeKind::Inside,
    );
    let inner = rect.shrink2(Vec2::new(16.0, 12.0));
    painter.text(
        inner.left_top(),
        egui::Align2::LEFT_TOP,
        &tile.label,
        font(UI_STRONG, 13.0),
        t.fg_dim,
    );
    let sub = match tile.state {
        TileState::Live => String::new(),
        TileState::Stale => format!("stale · {}", core::fmt_age(tile.age.unwrap_or(0.0))),
        TileState::NoData => "no data".into(),
        TileState::Lost => "link lost".into(),
    };
    painter.text(
        inner.right_top(),
        egui::Align2::RIGHT_TOP,
        sub,
        font(UI_MEDIUM, 11.5),
        t.fg_faint,
    );
    let (v, unit, dp) = core::display(tile.v, &tile.unit, p);
    let col = match (tile.state, tile.level) {
        (TileState::NoData | TileState::Lost, _) => t.fg_faint,
        (TileState::Stale, _) => t.fg_dim,
        (_, Level::Crit) => t.crit,
        (_, Level::Warn) => t.warn,
        _ => t.fg,
    };
    let size = (inner.height() * 0.36)
        .min(inner.width() * 0.22)
        .clamp(26.0, 96.0);
    let num = painter.text(
        inner.left_top() + Vec2::new(0.0, 22.0),
        egui::Align2::LEFT_TOP,
        core::fmt_num(v, dp, tile.signed),
        font(MONO_MEDIUM, size),
        col,
    );
    painter.text(
        Pos2::new(num.right() + 8.0, num.bottom() - size * 0.18),
        egui::Align2::LEFT_BOTTOM,
        unit,
        font(UI_MEDIUM, (size * 0.3).clamp(12.0, 22.0)),
        t.fg_dim,
    );
    // Bar with the thresholds.
    let bar = Rect::from_min_size(
        Pos2::new(inner.left(), num.bottom() + 8.0),
        Vec2::new(inner.width(), 6.0),
    );
    if let Some(rng) = core::bar_range(&tile.th, &tile.peak) {
        painter.rect_filled(bar, 3.0, t.surface3);
        if let Some(f) = core::frac(tile.v, Some(rng)) {
            let mut fill = bar;
            fill.set_width(bar.width() * f as f32);
            painter.rect_filled(fill, 3.0, if col == t.fg { t.accent } else { col });
        }
        for (x, c) in [(tile.th.warn, t.warn), (tile.th.crit, t.crit)] {
            if let Some(fx) = core::frac(x, Some(rng)) {
                let px = bar.left() + bar.width() * fx as f32;
                painter.line_segment(
                    [
                        Pos2::new(px, bar.top() - 3.0),
                        Pos2::new(px, bar.bottom() + 3.0),
                    ],
                    Stroke::new(2.0, c),
                );
            }
        }
    }
    // Sparkline.
    let foot = 16.0;
    let sp = Rect::from_min_max(
        Pos2::new(inner.left(), bar.bottom() + 10.0),
        Pos2::new(inner.right(), inner.bottom() - foot),
    );
    if sp.height() > 8.0 {
        for line in &tile.spark {
            let pts: Vec<Pos2> = line
                .iter()
                .map(|q| {
                    Pos2::new(
                        sp.left() + q[0] as f32 * sp.width(),
                        sp.bottom() - q[1] as f32 * sp.height(),
                    )
                })
                .collect();
            if pts.len() > 1 {
                painter.add(Shape::line(pts, Stroke::new(1.6, t.accent_line)));
            }
        }
    }
    if tile.peak.n > 0 {
        let (mn, _, _) = core::display(tile.peak.min, &tile.unit, p);
        let (mx, _, _) = core::display(tile.peak.max, &tile.unit, p);
        painter.text(
            inner.right_bottom(),
            egui::Align2::RIGHT_BOTTOM,
            format!(
                "min {} · max {}",
                core::fmt_num(mn, dp, tile.signed),
                core::fmt_num(mx, dp, tile.signed)
            ),
            font(MONO_MEDIUM, 11.5),
            t.fg_faint,
        );
    }
    if tile.th.unit_mismatch {
        painter.text(
            inner.left_bottom(),
            egui::Align2::LEFT_BOTTOM,
            "unit differs from the OBD table: no thresholds",
            font(UI_MEDIUM, 10.5),
            t.fg_faint,
        );
    }
}
