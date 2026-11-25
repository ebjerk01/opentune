//! Dashboard: live OBD-II mode 01 values (the Tauri app's Dashboard panel).
//!
//! Two hero gauges (engine speed, boost) and a grid of value tiles with
//! sparklines, min / max and warn / critical colouring from the core's
//! channel table ([`opentune_core::obd::CHANNELS`]). Pick a preset (diesel,
//! petrol, everything supported) or your own set; only PIDs the ECU reports
//! as supported are polled, up to six per request with the core's automatic
//! single-PID fall-back ([`opentune_core::obd::ObdSession`]).
//!
//! Start discovers the supported PIDs and the VIN, then polls in a
//! background job ([`poll`]); each poll is one short READ operation on the
//! app's adapter and target (mode `01`, `09` and the UDS `22 F190` VIN
//! fall-back: all on the read-only allow-list), so nothing here can change
//! the ECU. The learned session is cached per connection. Every poll can be
//! recorded as an OpenTune Live-log CSV (the Log viewer opens it) and is
//! published on the live feed, where the Drive view shows it; polling goes
//! on when you switch to another view, until Stop.

mod model;
mod poll;
#[cfg(test)]
mod tests;

use crate::host::{Host, JobHandle, NativeView, Tier, ViewGroup};
use crate::icons;
use crate::theme::{self, font, Tokens, Tone, MONO_MEDIUM, UI_MEDIUM, UI_STRONG};
use crate::widgets::csv::CsvRecordControl;
use eframe::egui::{self, Color32, Margin, Pos2, RichText, Sense, Shape, Stroke, Ui, Vec2};
use model::{Level, Preset, PressureUnit, Track};
use opentune_core::obd::{self, Group, ObdSession};
use poll::{lock, Board, Ctl, Phase, Run};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Registration tier, read before the view is built ([`Tier::Bench`]: only
/// alpha / bench builds register it).
pub const TIER: Tier = Tier::Bench;

/// How long dropping the view waits for the job to stop.
const DROP_WAIT: Duration = Duration::from_secs(2);
/// Poll intervals offered, milliseconds.
const INTERVALS: [u64; 3] = [250, 500, 1000];
/// Smallest tile width.
const TILE_W: f32 = 210.0;
const TILE_H: f32 = 104.0;

pub fn create() -> Box<dyn NativeView> {
    Box::new(DashboardView::default())
}

pub struct DashboardView {
    preset: Preset,
    custom: Vec<String>,
    punit: PressureUnit,
    interval_ms: u64,
    board: Arc<Mutex<Board>>,
    job: Option<JobHandle<()>>,
    ctl: Option<Arc<Ctl>>,
    rec: Arc<Mutex<CsvRecordControl>>,
    /// Learned OBD sessions per connection (driver, port, target).
    sessions: HashMap<String, Arc<Mutex<Option<ObdSession>>>>,
    note: Option<(Tone, String)>,
    show_picker: bool,
    /// The preferences last loaded / saved (None until the first frame).
    prefs_seen: Option<DashPrefs>,
}

/// What the Dashboard remembers between runs (egui's persisted memory,
/// saved with the app's state).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct DashPrefs {
    preset: Preset,
    custom: Vec<String>,
    punit: PressureUnit,
    interval_ms: u64,
}

impl DashboardView {
    fn prefs(&self) -> DashPrefs {
        DashPrefs {
            preset: self.preset,
            custom: self.custom.clone(),
            punit: self.punit,
            interval_ms: self.interval_ms,
        }
    }

    /// Load the remembered preferences on the first frame, save them when
    /// they change.
    fn sync_prefs(&mut self, ui: &Ui) {
        let id = egui::Id::unique("opentune-dashboard-prefs");
        if self.prefs_seen.is_none() {
            if let Some(p) = ui.data_mut(|d| d.get_persisted::<DashPrefs>(id)) {
                self.preset = p.preset;
                self.custom = p.custom;
                self.punit = p.punit;
                if [250, 500, 1000].contains(&p.interval_ms) {
                    self.interval_ms = p.interval_ms;
                }
            }
            self.prefs_seen = Some(self.prefs());
            return;
        }
        let now = self.prefs();
        if self.prefs_seen.as_ref() != Some(&now) {
            ui.data_mut(|d| d.insert_persisted(id, now.clone()));
            self.prefs_seen = Some(now);
        }
    }
}

impl Default for DashboardView {
    fn default() -> Self {
        Self {
            preset: Preset::Diesel,
            custom: model::DIESEL.iter().map(|s| s.to_string()).collect(),
            punit: PressureUnit::Bar,
            interval_ms: 500,
            board: Default::default(),
            job: None,
            ctl: None,
            rec: Default::default(),
            sessions: HashMap::new(),
            note: None,
            show_picker: false,
            prefs_seen: None,
        }
    }
}

impl Drop for DashboardView {
    /// The app is closing: stop polling so the adapter is free.
    fn drop(&mut self) {
        if let (Some(ctl), Some(job)) = (&self.ctl, &mut self.job) {
            ctl.stop();
            let t0 = Instant::now();
            while job.is_running() && t0.elapsed() < DROP_WAIT {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

impl DashboardView {
    fn running(&mut self) -> bool {
        self.job.as_mut().is_some_and(JobHandle::is_running)
    }

    fn tiles(&self) -> Vec<String> {
        let b = lock(&self.board);
        model::selection(self.preset, &self.custom, b.supported.as_ref())
    }

    fn poll_job(&mut self) {
        if let Some(r) = self.job.as_mut().and_then(JobHandle::take) {
            self.job = None;
            self.ctl = None;
            if let Err(e) = r {
                self.note = Some((Tone::Crit, e));
            }
        }
    }

    /// Start polling on the app's adapter and target.
    fn start(&mut self, host: &Host<'_>) -> Result<(), String> {
        if self.running() {
            return Err("already polling".into());
        }
        let adapter = host
            .adapter()
            .ok_or("no adapter: connect a cable in the top bar first")?;
        let target = host
            .target()
            .map_err(|e| format!("no target module: {e}"))?;
        let link = host.link();
        let key = format!("{}|{}|{target}", link.driver, link.port);
        let session = self.sessions.entry(key).or_default().clone();
        *lock(&self.board) = Board::default();
        let ctl = Arc::new(Ctl::new(self.interval_ms, self.tiles()));
        let run = Run {
            adapter,
            target,
            ctl: ctl.clone(),
            board: self.board.clone(),
            feed: host.live_feed(),
            rec: self.rec.clone(),
            session,
        };
        self.job = Some(host.spawn_job("dashboard polling", move |p| poll::run(p, run)));
        self.ctl = Some(ctl);
        self.note = None;
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(c) = &self.ctl {
            c.stop();
        }
    }

    /// Forget the learned sessions (e.g. after swapping the car).
    fn forget(&mut self) {
        self.sessions.clear();
        self.note = Some((Tone::Muted, "learned PID support forgotten".into()));
    }

    fn bar_ui(&mut self, ui: &mut Ui, host: &Host<'_>, running: bool) {
        let t = *host.tokens();
        theme::panel_frame(&t)
            .inner_margin(Margin::symmetric(14, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
                    if running {
                        if theme::button(ui, &t, Some(icons::STOP), "Stop").clicked() {
                            self.stop();
                        }
                    } else if theme::primary_button(ui, &t, Some(icons::PLAY), "Start")
                        .on_hover_text("Discover the supported values, then poll them")
                        .clicked()
                    {
                        if let Err(e) = self.start(host) {
                            self.note = Some((Tone::Crit, e));
                        }
                    }
                    ui.label(RichText::new("Values").color(t.fg_dim));
                    egui::ComboBox::from_id_salt("dash-preset")
                        .selected_text(self.preset.label())
                        .show_ui(ui, |ui| {
                            for p in Preset::ALL {
                                ui.selectable_value(&mut self.preset, p, p.label());
                            }
                        });
                    if theme::icon_button(ui, &t, icons::LIST_CHECKS, "Choose your own values")
                        .clicked()
                    {
                        self.show_picker = !self.show_picker;
                    }
                    ui.label(RichText::new("Every").color(t.fg_dim));
                    egui::ComboBox::from_id_salt("dash-rate")
                        .selected_text(format!("{} ms", self.interval_ms))
                        .width(80.0)
                        .show_ui(ui, |ui| {
                            for ms in INTERVALS {
                                ui.selectable_value(&mut self.interval_ms, ms, format!("{ms} ms"));
                            }
                        });
                    ui.selectable_value(&mut self.punit, PressureUnit::Bar, "bar");
                    ui.selectable_value(&mut self.punit, PressureUnit::KPa, "kPa");
                    let header = model::csv_header();
                    lock(&self.rec).ui(ui, host, "dashboard.csv", &header);
                    if !running
                        && !self.sessions.is_empty()
                        && theme::icon_button(
                            ui,
                            &t,
                            icons::ARROW_COUNTER_CLOCKWISE,
                            "Forget the learned PID support (next Start discovers again)",
                        )
                        .clicked()
                    {
                        self.forget();
                    }
                });
                ui.add_space(6.0);
                self.status_ui(ui, &t, host);
            });
    }

    fn status_ui(&self, ui: &mut Ui, t: &Tokens, host: &Host<'_>) {
        let b = lock(&self.board);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
            let (txt, tone) = match &b.phase {
                Phase::Idle => ("idle", Tone::Muted),
                Phase::Discovering => ("discovering", Tone::Accent),
                Phase::Polling if b.poll_error.is_some() => ("polling (errors)", Tone::Warn),
                Phase::Polling => ("polling", Tone::Ok),
                Phase::Stopped => ("stopped", Tone::Muted),
                Phase::Failed(_) => ("failed", Tone::Crit),
            };
            theme::pill(ui, t, txt, tone, false);
            match host.target() {
                Ok(tg) => theme::pill(ui, t, &tg.to_string(), Tone::Muted, true),
                Err(_) => theme::pill(ui, t, "no target", Tone::Warn, false),
            };
            if let Some(s) = &b.supported {
                theme::pill(ui, t, &format!("{} PIDs", s.len()), Tone::Muted, true)
                    .on_hover_text("mode-01 PIDs the ECU reports as supported");
            }
            match b.multi_pid {
                Some(true) => {
                    theme::pill(ui, t, "multi-PID", Tone::Muted, false)
                        .on_hover_text("up to six PIDs per request");
                }
                Some(false) => {
                    theme::pill(ui, t, "single PID", Tone::Muted, false)
                        .on_hover_text("the ECU rejects multi-PID requests: one PID per request");
                }
                None => {}
            }
            if let Some(r) = b.rate_hz {
                theme::pill(ui, t, &format!("{r:.1} Hz"), Tone::Muted, true);
            }
            if b.polls > 0 {
                theme::pill(ui, t, &format!("{} polls", b.polls), Tone::Muted, true);
            }
            if let Some(v) = &b.vin {
                theme::pill(ui, t, &format!("VIN {v}"), Tone::Muted, true);
            }
        });
        let err = match &b.phase {
            Phase::Failed(e) => Some(e.clone()),
            _ => b.poll_error.clone(),
        };
        if let Some(e) = err.or_else(|| self.note.as_ref().map(|n| n.1.clone())) {
            let col = match (&b.phase, &self.note) {
                (Phase::Failed(_), _) => t.crit,
                (_, Some((Tone::Muted, _))) if b.poll_error.is_none() => t.fg_dim,
                _ => t.warn,
            };
            ui.add_space(4.0);
            ui.add(egui::Label::new(RichText::new(e).color(col)).wrap());
        }
    }

    fn picker_ui(&mut self, ui: &mut Ui, t: &Tokens) {
        let sup = lock(&self.board).supported.clone();
        theme::panel_frame(t)
            .inner_margin(Margin::symmetric(14, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new("My selection")
                        .font(font(theme::DISPLAY_MEDIUM, 13.5))
                        .color(t.fg),
                );
                ui.label(
                    RichText::new(
                        "Tick the values to show. Greyed out: the ECU did not report them \
                         (known after Start).",
                    )
                    .color(t.fg_dim),
                );
                let mut changed = false;
                for g in [
                    Group::Engine,
                    Group::Air,
                    Group::Fuel,
                    Group::Exhaust,
                    Group::Electrical,
                    Group::Vehicle,
                ] {
                    ui.add_space(4.0);
                    theme::section_label(ui, t, g.as_str());
                    ui.horizontal_wrapped(|ui| {
                        for c in obd::channels().iter().filter(|c| c.group == g) {
                            if c.key == model::RPM || c.key == model::BOOST {
                                continue;
                            }
                            let ok = sup.as_ref().is_none_or(|s| model::is_supported(c, s));
                            let mut on = self.custom.iter().any(|k| k == c.key);
                            let r = ui
                                .add_enabled(ok, egui::Checkbox::new(&mut on, c.name))
                                .on_hover_text(format!("{} [{}]", c.key, c.unit));
                            if r.changed() {
                                changed = true;
                                if on {
                                    self.custom.push(c.key.to_string());
                                } else {
                                    self.custom.retain(|k| k != c.key);
                                }
                            }
                        }
                    });
                }
                if changed {
                    self.preset = Preset::Custom;
                }
            });
    }

    fn gauges_ui(&self, ui: &mut Ui, t: &Tokens) {
        let b = lock(&self.board);
        let (top, red) = self.preset.redline();
        let rpm = model::scalar(b.values.get(model::RPM).unwrap_or(&None));
        let boost = model::scalar(b.values.get(model::BOOST).unwrap_or(&None));
        let boost_ok = b
            .supported
            .as_ref()
            .is_none_or(|s| obd::channel(model::BOOST).is_some_and(|c| model::is_supported(c, s)));
        let w = ((ui.available_width() - theme::GAP) / 2.0).clamp(180.0, 340.0);
        ui.horizontal(|ui| {
            gauge(
                ui,
                t,
                w,
                &Gauge {
                    title: "Engine speed",
                    v: rpm,
                    lo: 0.0,
                    hi: top,
                    red: Some(red),
                    text: model::fmt_num(rpm, 0),
                    unit: "rpm".into(),
                    level: if rpm.is_some_and(|v| v >= red) {
                        Level::Crit
                    } else {
                        Level::Normal
                    },
                },
            );
            ui.add_space(theme::GAP);
            let (v, unit, dp) = model::shown("kPa", boost, self.punit);
            let (lo, hi) = match self.punit {
                PressureUnit::Bar => (-0.2, 2.5),
                PressureUnit::KPa => (-20.0, 250.0),
            };
            gauge(
                ui,
                t,
                w,
                &Gauge {
                    title: if boost_ok {
                        "Boost"
                    } else {
                        "Boost (not reported)"
                    },
                    v,
                    lo,
                    hi,
                    red: None,
                    text: model::fmt_num(v, dp),
                    unit,
                    level: Level::Normal,
                },
            );
        });
    }

    fn tiles_ui(&self, ui: &mut Ui, t: &Tokens) {
        let keys = self.tiles();
        let b = lock(&self.board);
        if keys.is_empty() {
            ui.label(RichText::new("No values to show for this ECU.").color(t.fg_dim));
            return;
        }
        let avail = ui.available_width();
        let cols = ((avail + 8.0) / (TILE_W + 8.0)).floor().max(1.0) as usize;
        let w = (avail - 8.0 * (cols as f32 - 1.0)) / cols as f32;
        let empty = Track::default();
        for row in keys.chunks(cols) {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                for k in row {
                    let tr = b.tracks.get(k).unwrap_or(&empty);
                    tile(ui, t, w, k, tr, b.errors.get(k), self.punit);
                }
            });
            ui.add_space(8.0);
        }
    }
}

impl NativeView for DashboardView {
    fn id(&self) -> &str {
        "dashboard"
    }
    fn label(&self) -> &str {
        "Dashboard"
    }
    fn icon(&self) -> &str {
        icons::SPEEDOMETER
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
        self.sync_prefs(ui);
        self.poll_job();
        let running = self.running();
        if let Some(c) = &self.ctl {
            c.set_interval(self.interval_ms);
            c.set_keys(self.tiles());
        }
        let host: &Host<'_> = host;
        let t = *host.tokens();
        egui::ScrollArea::vertical()
            .id_salt("dash-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Frame::new()
                    .inner_margin(Margin::same(theme::GAP as i8))
                    .show(ui, |ui| {
                        self.bar_ui(ui, host, running);
                        if self.show_picker {
                            ui.add_space(theme::GAP);
                            self.picker_ui(ui, &t);
                        }
                        ui.add_space(theme::GAP);
                        self.gauges_ui(ui, &t);
                        ui.add_space(theme::GAP);
                        self.tiles_ui(ui, &t);
                    });
            });
        if running {
            ui.ctx()
                .request_repaint_after(Duration::from_millis(self.interval_ms.min(500)));
        }
    }
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

fn level_color(t: &Tokens, l: Level) -> Color32 {
    match l {
        Level::Normal => t.fg,
        Level::Warn => t.warn,
        Level::Crit => t.crit,
    }
}

struct Gauge<'a> {
    title: &'a str,
    v: Option<f64>,
    lo: f64,
    hi: f64,
    red: Option<f64>,
    text: String,
    unit: String,
    level: Level,
}

/// Start angle and sweep of a gauge arc (degrees, screen coordinates).
const A0: f32 = 150.0;
const SWEEP: f32 = 240.0;

fn arc_points(c: Pos2, r: f32, f0: f32, f1: f32) -> Vec<Pos2> {
    let (f0, f1) = (f0.clamp(0.0, 1.0), f1.clamp(0.0, 1.0));
    let n = ((f1 - f0) * 64.0).ceil().max(1.0) as usize;
    (0..=n)
        .map(|i| {
            let f = f0 + (f1 - f0) * i as f32 / n as f32;
            let a = (A0 + SWEEP * f).to_radians();
            c + Vec2::new(a.cos(), a.sin()) * r
        })
        .collect()
}

fn gauge(ui: &mut Ui, t: &Tokens, w: f32, g: &Gauge<'_>) {
    let h = w * 0.78;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, h), Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, theme::RADIUS as f32, t.surface);
    p.rect_stroke(
        rect,
        theme::RADIUS as f32,
        Stroke::new(1.0, t.border),
        egui::StrokeKind::Inside,
    );
    let r = (w * 0.36).min(h * 0.46);
    let c = Pos2::new(rect.center().x, rect.top() + 18.0 + r);
    let frac = |v: f64| ((v - g.lo) / (g.hi - g.lo)) as f32;
    let width = (r * 0.12).max(6.0);
    p.add(Shape::line(
        arc_points(c, r, 0.0, 1.0),
        Stroke::new(width, t.surface3),
    ));
    if let Some(red) = g.red {
        p.add(Shape::line(
            arc_points(c, r, frac(red), 1.0),
            Stroke::new(width, t.crit_soft),
        ));
    }
    if let Some(v) = g.v {
        let col = if g.level == Level::Normal {
            t.accent
        } else {
            level_color(t, g.level)
        };
        p.add(Shape::line(
            arc_points(c, r, 0.0, frac(v)),
            Stroke::new(width, col),
        ));
    }
    p.text(
        c,
        egui::Align2::CENTER_CENTER,
        &g.text,
        font(MONO_MEDIUM, (r * 0.42).clamp(18.0, 44.0)),
        if g.v.is_some() {
            level_color(t, g.level)
        } else {
            t.fg_faint
        },
    );
    p.text(
        c + Vec2::new(0.0, r * 0.38),
        egui::Align2::CENTER_CENTER,
        &g.unit,
        font(UI_MEDIUM, 12.0),
        t.fg_dim,
    );
    p.text(
        Pos2::new(rect.center().x, rect.bottom() - 14.0),
        egui::Align2::CENTER_CENTER,
        g.title,
        font(UI_STRONG, 12.5),
        t.fg_dim,
    );
}

fn tile(
    ui: &mut Ui,
    t: &Tokens,
    w: f32,
    key: &str,
    tr: &Track,
    err: Option<&String>,
    punit: PressureUnit,
) {
    let ch = obd::channel(key);
    let name = ch.map_or(key, |c| c.name);
    let unit = ch.map_or("", |c| c.unit);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, TILE_H), Sense::hover());
    let p = ui.painter_at(rect);
    let level = model::level(ch, tr.last);
    let edge = match level {
        Level::Normal => t.border,
        l => level_color(t, l),
    };
    p.rect_filled(rect, theme::RADIUS_SM as f32, t.surface);
    p.rect_stroke(
        rect,
        theme::RADIUS_SM as f32,
        Stroke::new(if level == Level::Normal { 1.0 } else { 2.0 }, edge),
        egui::StrokeKind::Inside,
    );
    let inner = rect.shrink2(Vec2::new(12.0, 8.0));
    p.text(
        inner.left_top(),
        egui::Align2::LEFT_TOP,
        name,
        font(UI_STRONG, 11.5),
        t.fg_dim,
    );
    let (v, shown_unit, dp) = model::shown(unit, tr.last, punit);
    let col = if v.is_some() {
        level_color(t, level)
    } else {
        t.fg_faint
    };
    let num = p.text(
        inner.left_top() + Vec2::new(0.0, 18.0),
        egui::Align2::LEFT_TOP,
        model::fmt_num(v, dp),
        font(MONO_MEDIUM, 26.0),
        col,
    );
    p.text(
        Pos2::new(num.right() + 6.0, num.bottom() - 4.0),
        egui::Align2::LEFT_BOTTOM,
        &shown_unit,
        font(UI_MEDIUM, 11.5),
        t.fg_dim,
    );
    // min / max since Start.
    let (mn, _, _) = model::shown(unit, tr.min, punit);
    let (mx, _, _) = model::shown(unit, tr.max, punit);
    if mn.is_some() {
        p.text(
            inner.right_top(),
            egui::Align2::RIGHT_TOP,
            format!("{} … {}", model::fmt_num(mn, dp), model::fmt_num(mx, dp)),
            font(MONO_MEDIUM, 10.5),
            t.fg_faint,
        );
    }
    // Sparkline.
    let spark = egui::Rect::from_min_max(
        Pos2::new(inner.left(), inner.bottom() - 26.0),
        inner.right_bottom(),
    );
    if tr.hist.len() > 1 {
        let (t0, t1) = (tr.hist[0][0], tr.hist[tr.hist.len() - 1][0]);
        let (lo, hi) = tr
            .hist
            .iter()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), p| {
                (a.min(p[1]), b.max(p[1]))
            });
        let (lo, hi) = if hi - lo < 1e-9 {
            (lo - 1.0, hi + 1.0)
        } else {
            (lo, hi)
        };
        let span = (t1 - t0).max(1e-9);
        let pts: Vec<Pos2> = tr
            .hist
            .iter()
            .map(|q| {
                Pos2::new(
                    spark.left() + ((q[0] - t0) / span) as f32 * spark.width(),
                    spark.bottom() - ((q[1] - lo) / (hi - lo)) as f32 * spark.height(),
                )
            })
            .collect();
        p.add(Shape::line(pts, Stroke::new(1.5, t.accent_line)));
    } else if let Some(e) = err.filter(|_| tr.last.is_none()) {
        p.text(
            spark.left_bottom(),
            egui::Align2::LEFT_BOTTOM,
            e,
            font(UI_MEDIUM, 10.5),
            t.fg_faint,
        );
    }
    let mut tip = format!("{key} [{unit}]");
    if let Some(c) = ch {
        if let Some(w) = c.warn {
            tip.push_str(&format!("\nwarn ≥ {w} {unit}"));
        }
        if let Some(x) = c.crit {
            tip.push_str(&format!("\ncritical ≥ {x} {unit}"));
        }
    }
    if let Some(e) = err {
        tip.push_str(&format!("\n{e}"));
    }
    resp.on_hover_text(tip);
}
