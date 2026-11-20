//! The Log viewer's map overlay: where on a calibration map did the logged
//! operating points fall? (the Tauri app's `log_map_hits` + the overlay
//! card of `feat_logview.js`).
//!
//! The binning, the axis breakpoints (inline, or stored in the image) and
//! the unit check are the core's ([`opentune_hub::logoverlay::overlay_value`]
//! over [`opentune_logview::overlay::map_hits`]): per-cell hits (nearest
//! cell, or bilinear weights), time in cell and a value channel's range.
//! Units are compared, never converted; a mismatch is refused unless
//! explicitly accepted. Breakpoints that cannot be known (a stored axis
//! without its image, a shared axis) are refused, never guessed.
//!
//! The definition comes from the app (the selected one, when the app passes
//! it), the bundled definitions or a definition file; stored axes need the
//! image (the app's open image, or one chosen here). Nothing is written.

use crate::theme::{self, font, Tokens, MONO_MEDIUM};
use eframe::egui::{self, Color32, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use opentune_core::ecudef::{AxisDef, EcuDef, MapDef};
use opentune_hub::logoverlay::{overlay_value, OverlayReq};
use opentune_logview::{BinMode, Log};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

/// What the app shares with the Log viewer: its selected definition and
/// open image (both optional).
#[derive(Clone, Copy, Default)]
pub struct LogContext<'a> {
    pub def: Option<&'a EcuDef>,
    pub image: Option<(&'a Path, &'a [u8])>,
}

// ---------------------------------------------------------------------------
// The overlay result (typed view of `overlay_value`)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Outside {
    pub x_low: usize,
    pub x_high: usize,
    pub y_low: usize,
    pub y_high: usize,
    pub any: usize,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct CellValues {
    pub min: Vec<Vec<Option<f64>>>,
    pub max: Vec<Vec<Option<f64>>>,
    pub mean: Vec<Vec<Option<f64>>>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct AxisOut {
    pub name: Option<String>,
    pub unit: Option<String>,
    pub values: Vec<f64>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct MapOut {
    pub name: String,
    pub unit: String,
    pub x_axis: AxisOut,
    pub y_axis: AxisOut,
    pub cells: Option<Vec<Vec<Option<f64>>>>,
    pub cells_error: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct UnitOut {
    pub channel: String,
    pub channel_unit: String,
    pub axis: String,
    pub axis_unit: String,
    pub check: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Units {
    pub x: Option<UnitOut>,
    pub y: Option<UnitOut>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct NameUnit {
    pub name: String,
    pub unit: String,
}

/// One overlay.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Hits {
    pub rows: usize,
    pub cols: usize,
    pub mode: String,
    pub hits: Vec<Vec<f64>>,
    pub time: Vec<Vec<f64>>,
    pub values: Option<CellValues>,
    pub samples_total: usize,
    pub samples_used: usize,
    pub samples_skipped: usize,
    pub outside: Outside,
    pub map: MapOut,
    pub units: Units,
    pub value_channel: Option<NameUnit>,
    pub time_label: String,
    pub warnings: Vec<String>,
}

/// Run the overlay of `log` over map `map` of `def`.
pub fn compute(
    log: &Log,
    def: &EcuDef,
    map: &str,
    img: Option<&[u8]>,
    req: &OverlayReq,
) -> Result<Hits, String> {
    let v = overlay_value(log, def, map, img, req)?;
    serde_json::from_value(v).map_err(|e| format!("overlay result: {e}"))
}

/// What a cell is coloured by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Metric {
    #[default]
    Hits,
    Time,
    /// Mean of the value channel.
    Value,
}

/// A cell's metric (`None`: nothing there).
pub fn metric(h: &Hits, m: Metric, r: usize, c: usize) -> Option<f64> {
    match m {
        Metric::Hits => h.hits.get(r)?.get(c).copied(),
        Metric::Time => h.time.get(r)?.get(c).copied(),
        Metric::Value => h.values.as_ref()?.mean.get(r)?.get(c).copied().flatten(),
    }
}

/// The colour range of a metric over the used cells (hits / time from 0).
pub fn metric_range(h: &Hits, m: Metric) -> (f64, f64) {
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for r in 0..h.rows {
        for c in 0..h.cols {
            if let Some(v) = metric(h, m, r, c).filter(|v| m == Metric::Value || *v > 0.0) {
                lo = lo.min(v);
                hi = hi.max(v);
            }
        }
    }
    if m != Metric::Value {
        lo = 0.0;
    }
    if !hi.is_finite() {
        return (0.0, 1.0);
    }
    (lo, hi)
}

/// The Maps panel's heat ramp (blue → green → amber → orange → red).
pub fn heat_color(f: f64) -> Color32 {
    const STOPS: [[f64; 3]; 5] = [
        [43.0, 108.0, 176.0],
        [56.0, 161.0, 105.0],
        [214.0, 158.0, 46.0],
        [221.0, 107.0, 32.0],
        [197.0, 48.0, 48.0],
    ];
    let x = f.clamp(0.0, 1.0) * (STOPS.len() - 1) as f64;
    let i = (x.floor() as usize).min(STOPS.len() - 1);
    let k = x - i as f64;
    let (a, b) = (STOPS[i], STOPS[(i + 1).min(STOPS.len() - 1)]);
    let ch = |j: usize| (a[j] + (b[j] - a[j]) * k).round() as u8;
    Color32::from_rgb(ch(0), ch(1), ch(2))
}

fn norm_unit(u: &str) -> String {
    let s: String = u
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let s = s
        .trim_start_matches(['[', '('])
        .trim_end_matches([']', ')']);
    match s {
        "1/min" | "u/min" | "min-1" => "rpm",
        "mg/hub" | "mg/str" => "mg/stroke",
        "mbar" => "hpa",
        "°c" => "degc",
        x => x,
    }
    .to_string()
}

/// The log channel that best fits a map axis: a matching unit counts 2, each
/// word of the axis name (longer than two letters) found in the channel name
/// counts 1 (the desktop's `guessChannel`). `None` when nothing scores.
pub fn guess_channel(axis: Option<&AxisDef>, chans: &[(String, String)]) -> Option<String> {
    let a = axis?;
    let name = a.name.to_lowercase();
    let words: Vec<&str> = name
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| w.len() > 2)
        .collect();
    let au = norm_unit(&a.unit);
    let mut best = None;
    let mut score = 0;
    for (n, u) in chans {
        let mut s = 0;
        if !a.unit.is_empty() && norm_unit(u) == au {
            s += 2;
        }
        let ln = n.to_lowercase();
        s += words.iter().filter(|w| ln.contains(*w)).count();
        if s > score {
            score = s;
            best = Some(n.clone());
        }
    }
    best
}

/// Maps an overlay can use (more than one cell).
pub fn usable_maps(def: &EcuDef) -> Vec<&MapDef> {
    def.maps.iter().filter(|m| m.rows * m.cols > 1).collect()
}

/// Does the map need the image for its axes?
pub fn needs_image(m: &MapDef) -> bool {
    let stored = |a: &Option<AxisDef>| {
        a.as_ref()
            .is_some_and(|a| a.values.is_empty() && a.address.is_some())
    };
    stored(&m.x_axis) || (m.rows > 1 && stored(&m.y_axis))
}

fn axis_text(a: &Option<AxisDef>) -> String {
    match a {
        Some(a) if !a.unit.is_empty() => {
            format!(
                "{}, {}",
                if a.name.is_empty() { "axis" } else { &a.name },
                a.unit
            )
        }
        Some(a) if !a.name.is_empty() => a.name.clone(),
        _ => "axis".into(),
    }
}

pub fn fmt(v: f64) -> String {
    if !v.is_finite() {
        return "\u{2014}".into();
    }
    let a = v.abs();
    if a != 0.0 && !(1e-3..1e6).contains(&a) {
        format!("{v:.2e}")
    } else if v.fract() == 0.0 {
        format!("{v:.0}")
    } else if a >= 100.0 {
        format!("{v:.1}")
    } else if a >= 10.0 {
        format!("{v:.2}")
    } else {
        format!("{v:.3}")
    }
}

// ---------------------------------------------------------------------------
// State and UI
// ---------------------------------------------------------------------------

/// Where the overlay's definition comes from.
#[derive(Clone, Debug, PartialEq)]
pub enum DefSrc {
    /// The app's selected definition.
    App,
    /// A bundled definition, by id.
    Bundled(String),
    /// A definition file chosen here.
    File,
}

type Bundled = Result<Vec<EcuDef>, String>;

#[derive(Default)]
pub struct Overlay {
    pub open: bool,
    bundled: Option<Bundled>,
    bundled_rx: Option<mpsc::Receiver<Bundled>>,
    file_def: Option<(PathBuf, EcuDef)>,
    src: Option<DefSrc>,
    map: Option<String>,
    x: String,
    y: String,
    val: String,
    mode: BinMode,
    accept: bool,
    window: bool,
    image: Option<(PathBuf, Vec<u8>)>,
    /// `(definition, map, log generation)` the channels were guessed for.
    guessed: Option<(String, String, u64)>,
    pub result: Option<Result<Hits, String>>,
    metric: Metric,
    show_map: bool,
}

impl Overlay {
    /// Start loading the bundled definitions (once).
    fn load_bundled(&mut self, ctx: &egui::Context) {
        if self.bundled.is_some() || self.bundled_rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let r = match crate::paths::find("ecudefs") {
                Some(d) => opentune_core::ecudef::load_dir(&d).map_err(|e| e.to_string()),
                None => Err("no bundled definitions found".into()),
            };
            let _ = tx.send(r);
            ctx.request_repaint();
        });
        self.bundled_rx = Some(rx);
    }

    fn poll(&mut self) {
        if let Some(rx) = &self.bundled_rx {
            match rx.try_recv() {
                Ok(r) => {
                    self.bundled = Some(r);
                    self.bundled_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.bundled = Some(Err("loading the definitions stopped".into()));
                    self.bundled_rx = None;
                }
            }
        }
    }

    /// The chosen definition.
    fn def<'a>(&'a self, cx: &LogContext<'a>) -> Option<&'a EcuDef> {
        match self.src.as_ref()? {
            DefSrc::App => cx.def,
            DefSrc::Bundled(id) => self
                .bundled
                .as_ref()?
                .as_ref()
                .ok()?
                .iter()
                .find(|d| &d.id == id),
            DefSrc::File => self.file_def.as_ref().map(|f| &f.1),
        }
    }

    /// The image used for stored axes and map values: the one chosen here,
    /// else the app's.
    fn image<'a>(&'a self, cx: &LogContext<'a>) -> Option<(&'a Path, &'a [u8])> {
        match &self.image {
            Some((p, b)) => Some((p.as_path(), b.as_slice())),
            None => cx.image,
        }
    }

    /// Pre-select the X / Y channels for the map (once per map and log).
    fn guess(&mut self, def: &EcuDef, chans: &[(String, String)], generation: u64) {
        let Some(m) = self
            .map
            .as_ref()
            .and_then(|n| def.maps.iter().find(|m| &m.name == n))
        else {
            return;
        };
        let key = (def.id.clone(), m.name.clone(), generation);
        if self.guessed.as_ref() == Some(&key) {
            return;
        }
        if let Some(x) = guess_channel(m.x_axis.as_ref(), chans) {
            self.x = x;
        }
        if m.rows > 1 {
            if let Some(y) = guess_channel(m.y_axis.as_ref(), chans) {
                self.y = y;
            }
        }
        self.guessed = Some(key);
    }

    /// Run the overlay with the current choices.
    pub fn run(&mut self, log: &Log, window: Option<(f64, f64)>, cx: &LogContext<'_>) {
        let r = (|| {
            let def = self.def(cx).ok_or("choose a definition and a map")?;
            let map = self.map.as_deref().ok_or("choose a map")?;
            let m = def
                .maps
                .iter()
                .find(|m| m.name == map)
                .ok_or("choose a map")?;
            let blank = |s: &str| (!s.trim().is_empty()).then(|| s.to_string());
            let (t0, t1) = match window.filter(|_| self.window) {
                Some((a, b)) => (Some(a), Some(b)),
                None => (None, None),
            };
            let req = OverlayReq {
                x_channel: self.x.clone(),
                y_channel: if m.rows > 1 { blank(&self.y) } else { None },
                value_channel: blank(&self.val),
                mode: self.mode,
                accept_units: self.accept,
                t0,
                t1,
            };
            compute(log, def, map, self.image(cx).map(|i| i.1), &req)
        })();
        if let Ok(h) = &r {
            if self.metric == Metric::Value && h.values.is_none() {
                self.metric = Metric::Hits;
            }
            self.show_map = self.show_map && h.map.cells.is_some();
        }
        self.result = Some(r);
    }

    /// The overlay card. `generation` changes per opened log; `window` is the
    /// plot's visible time range.
    pub fn ui(
        &mut self,
        ui: &mut Ui,
        t: &Tokens,
        log: &Log,
        generation: u64,
        window: Option<(f64, f64)>,
        cx: &LogContext<'_>,
    ) {
        self.poll();
        self.load_bundled(ui.ctx());
        if self.src.is_none() && cx.def.is_some() {
            self.src = Some(DefSrc::App);
        }
        let chans: Vec<(String, String)> = log
            .channels
            .iter()
            .map(|c| (c.name.clone(), c.unit.clone()))
            .collect();
        let mut run = false;
        let mut pick_def = false;
        let mut pick_img = false;
        egui::Grid::new("lv-ov-form")
            .num_columns(2)
            .spacing(Vec2::new(10.0, 6.0))
            .show(ui, |ui| {
                // Definition.
                ui.label(RichText::new("Definition").color(t.fg_dim));
                ui.horizontal(|ui| {
                    let current = match self.def(cx) {
                        Some(d) => d.name.clone(),
                        None => "(choose)".into(),
                    };
                    let before = self.src.clone();
                    egui::ComboBox::from_id_salt("lv-ov-def")
                        .selected_text(current)
                        .width(280.0)
                        .show_ui(ui, |ui| {
                            if let Some(d) = cx.def {
                                ui.selectable_value(
                                    &mut self.src,
                                    Some(DefSrc::App),
                                    format!("Selected: {}", d.name),
                                );
                            }
                            if let Some((p, _)) = &self.file_def {
                                let n = p.file_name().map(|n| n.to_string_lossy().into_owned());
                                ui.selectable_value(
                                    &mut self.src,
                                    Some(DefSrc::File),
                                    format!("File: {}", n.unwrap_or_default()),
                                );
                            }
                            match &self.bundled {
                                None => {
                                    ui.label("loading the bundled definitions …");
                                }
                                Some(Err(e)) => {
                                    ui.label(e.as_str());
                                }
                                Some(Ok(list)) => {
                                    for d in list.iter().filter(|d| !usable_maps(d).is_empty()) {
                                        ui.selectable_value(
                                            &mut self.src,
                                            Some(DefSrc::Bundled(d.id.clone())),
                                            &d.name,
                                        );
                                    }
                                }
                            }
                        });
                    if self.src != before {
                        self.map = None;
                    }
                    pick_def = theme::button(ui, t, None, "File…")
                        .on_hover_text("Use a definition (.json) file")
                        .clicked();
                });
                ui.end_row();
                // Map.
                ui.label(RichText::new("Map").color(t.fg_dim));
                let def = self.def(cx).cloned();
                let maps: Vec<(String, u32, u32)> = def
                    .as_ref()
                    .map(|d| {
                        usable_maps(d)
                            .into_iter()
                            .map(|m| (m.name.clone(), m.rows, m.cols))
                            .collect()
                    })
                    .unwrap_or_default();
                if self
                    .map
                    .as_ref()
                    .is_some_and(|n| !maps.iter().any(|m| &m.0 == n))
                {
                    self.map = None;
                }
                if self.map.is_none() {
                    self.map = maps.first().map(|m| m.0.clone());
                }
                egui::ComboBox::from_id_salt("lv-ov-map")
                    .selected_text(self.map.clone().unwrap_or_else(|| {
                        if def.is_some() {
                            "(no maps with axes)".into()
                        } else {
                            "—".into()
                        }
                    }))
                    .width(320.0)
                    .show_ui(ui, |ui| {
                        for (n, r, c) in &maps {
                            ui.selectable_value(
                                &mut self.map,
                                Some(n.clone()),
                                format!("{n} — {r}×{c}"),
                            );
                        }
                    });
                ui.end_row();
                let m = def
                    .as_ref()
                    .and_then(|d| {
                        self.map
                            .as_ref()
                            .and_then(|n| d.maps.iter().find(|m| &m.name == n))
                    })
                    .cloned();
                if let Some(d) = &def {
                    self.guess(d, &chans, generation);
                }
                // Channels.
                let xl = m.as_ref().map_or("axis".into(), |m| axis_text(&m.x_axis));
                ui.label(RichText::new(format!("X ({xl})")).color(t.fg_dim));
                channel_combo(ui, "lv-ov-x", &mut self.x, &chans, false);
                ui.end_row();
                let rows = m.as_ref().map_or(1, |m| m.rows);
                let yl = if rows > 1 {
                    format!(
                        "Y ({})",
                        m.as_ref().map_or("axis".into(), |m| axis_text(&m.y_axis))
                    )
                } else {
                    "Y (not used: 1-row curve)".into()
                };
                ui.label(RichText::new(yl).color(t.fg_dim));
                ui.add_enabled_ui(rows > 1, |ui| {
                    channel_combo(ui, "lv-ov-y", &mut self.y, &chans, false)
                });
                ui.end_row();
                ui.label(RichText::new("Value").color(t.fg_dim));
                channel_combo(ui, "lv-ov-v", &mut self.val, &chans, true);
                ui.end_row();
                // Image.
                ui.label(RichText::new("Image").color(t.fg_dim));
                ui.horizontal(|ui| {
                    let (txt, col) = match (&self.image, cx.image) {
                        (Some((p, _)), _) => (base(p), t.fg),
                        (None, Some((p, _))) => (format!("{} (open image)", base(p)), t.fg),
                        (None, None) if m.as_ref().is_some_and(needs_image) => (
                            "this map's axes are stored in the image: choose the .bin it belongs to"
                                .into(),
                            t.warn,
                        ),
                        _ => ("none (inline axes; map values not shown)".into(), t.fg_dim),
                    };
                    ui.label(RichText::new(txt).color(col));
                    pick_img = theme::button(ui, t, None, "Image…").clicked();
                    if self.image.is_some() && ui.small_button("✕").clicked() {
                        self.image = None;
                    }
                });
                ui.end_row();
            });
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(&mut self.mode, BinMode::Nearest, "Nearest cell");
            ui.selectable_value(&mut self.mode, BinMode::Bilinear, "Bilinear")
                .on_hover_text(
                    "Split each sample over the four surrounding cells by ECU interpolation weight",
                );
            ui.checkbox(&mut self.window, "Visible range only");
            ui.checkbox(
                &mut self.accept,
                "Accept a unit mismatch (values as logged)",
            );
            run = theme::primary_button(ui, t, None, "Overlay").clicked();
        });
        if pick_def {
            if let Some(p) = rfd::FileDialog::new()
                .add_filter("definition", &["json"])
                .pick_file()
            {
                match std::fs::read_to_string(&p)
                    .map_err(|e| e.to_string())
                    .and_then(|s| EcuDef::from_json(&s).map_err(|e| e.to_string()))
                {
                    Ok(d) => {
                        self.file_def = Some((p, d));
                        self.src = Some(DefSrc::File);
                        self.map = None;
                    }
                    Err(e) => self.result = Some(Err(format!("{}: {e}", p.display()))),
                }
            }
        }
        if pick_img {
            if let Some(p) = rfd::FileDialog::new().pick_file() {
                match std::fs::read(&p) {
                    Ok(b) => self.image = Some((p, b)),
                    Err(e) => self.result = Some(Err(format!("{}: {e}", p.display()))),
                }
            }
        }
        if run {
            self.run(log, window, cx);
        }
        ui.add_space(6.0);
        match &self.result {
            None => {}
            Some(Err(e)) => {
                ui.add(egui::Label::new(RichText::new(e).color(t.crit)).wrap());
            }
            Some(Ok(_)) => self.result_ui(ui, t),
        }
    }

    fn result_ui(&mut self, ui: &mut Ui, t: &Tokens) {
        let Some(Ok(h)) = &self.result else { return };
        for w in &h.warnings {
            ui.add(egui::Label::new(RichText::new(w).color(t.warn)).wrap());
        }
        if let Some(e) = &h.map.cells_error {
            ui.label(RichText::new(format!("Map values not shown: {e}")).color(t.fg_dim));
        }
        let o = &h.outside;
        let mut s = format!(
            "{} of {} sample(s) placed · {} skipped (missing X/Y) · {} outside the axes",
            h.samples_used, h.samples_total, h.samples_skipped, o.any
        );
        if o.any > 0 {
            s.push_str(&format!(
                " (X low {}, X high {}, Y low {}, Y high {}; counted in the edge cell, as the ECU clamps)",
                o.x_low, o.x_high, o.y_low, o.y_high
            ));
        }
        ui.add(egui::Label::new(RichText::new(s).color(t.fg_dim)).wrap());
        for (k, u) in [("X", &h.units.x), ("Y", &h.units.y)] {
            if let Some(u) = u {
                let (verdict, col) = match u.check.as_str() {
                    "match" => ("units match", t.fg_dim),
                    "mismatch" => ("MISMATCH, not converted", t.warn),
                    _ => ("unit unknown", t.fg_dim),
                };
                ui.label(
                    RichText::new(format!(
                        "{k}: {} ← {} ({} / {}: {verdict})",
                        if u.axis.is_empty() { "axis" } else { &u.axis },
                        u.channel,
                        if u.channel_unit.is_empty() {
                            "no unit"
                        } else {
                            &u.channel_unit
                        },
                        if u.axis_unit.is_empty() {
                            "no unit"
                        } else {
                            &u.axis_unit
                        },
                    ))
                    .small()
                    .color(col),
                );
            }
        }
        let has_values = h.values.is_some();
        let has_cells = h.map.cells.is_some();
        let mut metric = self.metric;
        let mut show_map = self.show_map;
        ui.horizontal(|ui| {
            ui.label(RichText::new("Colour by").color(t.fg_dim));
            ui.selectable_value(&mut metric, Metric::Hits, "Hits");
            ui.selectable_value(&mut metric, Metric::Time, "Time");
            if has_values {
                ui.selectable_value(&mut metric, Metric::Value, "Value");
            }
            if has_cells {
                ui.checkbox(
                    &mut show_map,
                    format!(
                        "Show map values ({})",
                        if h.map.unit.is_empty() {
                            "raw"
                        } else {
                            &h.map.unit
                        }
                    ),
                );
            }
        });
        grid_ui(ui, t, h, metric, show_map);
        let (lo, hi) = metric_range(h, metric);
        let what = match metric {
            Metric::Hits if h.mode == "bilinear" => "hits (bilinear weight)".to_string(),
            Metric::Hits => "hits".into(),
            Metric::Time => format!("time in cell ({})", h.time_label),
            Metric::Value => h
                .value_channel
                .as_ref()
                .map(|v| {
                    format!(
                        "mean {} ({})",
                        v.name,
                        if v.unit.is_empty() {
                            "as logged"
                        } else {
                            &v.unit
                        }
                    )
                })
                .unwrap_or_else(|| "value".into()),
        };
        ui.label(
            RichText::new(format!(
                "{} … {} · {what} · empty cells were never used",
                fmt(lo),
                fmt(hi)
            ))
            .small()
            .color(t.fg_faint),
        );
        self.metric = metric;
        self.show_map = show_map;
    }
}

#[cfg(test)]
impl Overlay {
    pub fn set_for_test(&mut self, src: DefSrc, map: &str, x: &str, y: &str) {
        self.src = Some(src);
        self.map = Some(map.into());
        self.x = x.into();
        self.y = y.into();
    }
    pub fn set_window_for_test(&mut self, on: bool) {
        self.window = on;
    }
}

fn base(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

fn channel_combo(
    ui: &mut Ui,
    id: &str,
    sel: &mut String,
    chans: &[(String, String)],
    optional: bool,
) {
    let shown = if sel.is_empty() {
        if optional { "(none)" } else { "(choose)" }.to_string()
    } else {
        sel.clone()
    };
    egui::ComboBox::from_id_salt(id)
        .selected_text(shown)
        .width(280.0)
        .show_ui(ui, |ui| {
            if optional {
                ui.selectable_value(sel, String::new(), "(none)");
            }
            for (n, u) in chans {
                let label = if u.is_empty() {
                    n.clone()
                } else {
                    format!("{n} [{u}]")
                };
                ui.selectable_value(sel, n.clone(), label);
            }
        });
}

/// The heat grid: Y breakpoints down the left, X across the top.
fn grid_ui(ui: &mut Ui, t: &Tokens, h: &Hits, m: Metric, show_map: bool) {
    let (lo, hi) = metric_range(h, m);
    let span = if hi - lo > 0.0 { hi - lo } else { 1.0 };
    let cw = 54.0;
    let rh = 20.0;
    let head_w = 64.0;
    let size = Vec2::new(head_w + cw * h.cols as f32, rh * (h.rows as f32 + 1.0));
    egui::ScrollArea::both()
        .id_salt("lv-ov-grid")
        .max_height(360.0)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
            let p = ui.painter_at(rect);
            let f = font(MONO_MEDIUM, 10.5);
            let xs = &h.map.x_axis.values;
            let ys = &h.map.y_axis.values;
            let cell = |r: usize, c: usize| {
                Rect::from_min_size(
                    Pos2::new(
                        rect.left() + head_w + cw * c as f32,
                        rect.top() + rh * (r as f32 + 1.0),
                    ),
                    Vec2::new(cw, rh),
                )
            };
            for c in 0..h.cols {
                let r = Rect::from_min_size(
                    Pos2::new(rect.left() + head_w + cw * c as f32, rect.top()),
                    Vec2::new(cw, rh),
                );
                let txt = xs.get(c).map_or(c.to_string(), |v| fmt(*v));
                p.text(
                    r.center(),
                    egui::Align2::CENTER_CENTER,
                    txt,
                    f.clone(),
                    t.fg_dim,
                );
            }
            for r in 0..h.rows {
                let y = Rect::from_min_size(
                    Pos2::new(rect.left(), rect.top() + rh * (r as f32 + 1.0)),
                    Vec2::new(head_w - 4.0, rh),
                );
                let txt = if h.rows > 1 {
                    ys.get(r).map_or(r.to_string(), |v| fmt(*v))
                } else {
                    String::new()
                };
                p.text(
                    y.right_center(),
                    egui::Align2::RIGHT_CENTER,
                    txt,
                    f.clone(),
                    t.fg_dim,
                );
                for c in 0..h.cols {
                    let b = cell(r, c);
                    let used = h
                        .hits
                        .get(r)
                        .and_then(|x| x.get(c))
                        .is_some_and(|v| *v > 0.0);
                    let v = metric(h, m, r, c);
                    let fill = match v.filter(|_| used) {
                        Some(v) => heat_color((v - lo) / span),
                        None => t.surface2,
                    };
                    p.rect_filled(b.shrink(0.5), 2.0, fill);
                    let mv = h
                        .map
                        .cells
                        .as_ref()
                        .and_then(|m| m.get(r))
                        .and_then(|x| x.get(c))
                        .copied()
                        .flatten();
                    let txt = match (show_map, mv, used, v) {
                        (true, Some(m), _, _) => fmt(m),
                        (_, _, true, Some(v)) => match m {
                            Metric::Hits if h.mode != "bilinear" => format!("{}", v.round()),
                            Metric::Hits | Metric::Time => format!("{v:.1}"),
                            Metric::Value => fmt(v),
                        },
                        _ => String::new(),
                    };
                    let col = if used { Color32::WHITE } else { t.fg_faint };
                    p.text(b.center(), egui::Align2::CENTER_CENTER, txt, f.clone(), col);
                }
            }
            p.line_segment(
                [
                    Pos2::new(rect.left() + head_w - 2.0, rect.top()),
                    Pos2::new(rect.left() + head_w - 2.0, rect.bottom()),
                ],
                Stroke::new(1.0, t.border_soft),
            );
            if let Some(pos) = resp.hover_pos() {
                let c = ((pos.x - rect.left() - head_w) / cw).floor();
                let r = ((pos.y - rect.top()) / rh).floor() - 1.0;
                if c >= 0.0 && r >= 0.0 && (c as usize) < h.cols && (r as usize) < h.rows {
                    let (r, c) = (r as usize, c as usize);
                    let mut tip = format!(
                        "[{r},{c}] · hits {} · time {:.2} {}",
                        fmt(h.hits[r][c]),
                        h.time.get(r).and_then(|x| x.get(c)).copied().unwrap_or(0.0),
                        h.time_label
                    );
                    if let (Some(vals), Some(vc)) = (&h.values, &h.value_channel) {
                        if let Some(mean) =
                            vals.mean.get(r).and_then(|x| x.get(c)).copied().flatten()
                        {
                            let g = |m: &Vec<Vec<Option<f64>>>| {
                                m.get(r)
                                    .and_then(|x| x.get(c))
                                    .copied()
                                    .flatten()
                                    .map_or("—".into(), fmt)
                            };
                            tip.push_str(&format!(
                                "\n{} {} … {} (mean {}) {}",
                                vc.name,
                                g(&vals.min),
                                g(&vals.max),
                                fmt(mean),
                                vc.unit
                            ));
                        }
                    }
                    if let Some(m) = h
                        .map
                        .cells
                        .as_ref()
                        .and_then(|m| m.get(r))
                        .and_then(|x| x.get(c))
                        .copied()
                        .flatten()
                    {
                        tip.push_str(&format!("\nmap {} {}", fmt(m), h.map.unit));
                    }
                    resp.on_hover_text_at_pointer(tip);
                }
            }
        });
}
