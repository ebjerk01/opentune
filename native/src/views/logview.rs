//! Datalog viewer: open a CSV log and plot its channels over time.
//!
//! Parsing is the core's `opentune-logview` (the same reader as the webview
//! Log viewer): the OpenTune **Live log** (`obd read --out`, the desktop Live
//! log: `ts,param_id,name,raw,value,units,note,text`; `opentune log`: the same
//! with a leading `round` and a trailing `ts`) and **generic wide CSV**
//! (header row, `[unit]` / `(unit)` in names or a units row, `,` `;` tab `|`,
//! decimal comma, time column in s / ms / µs / min / clock, or the sample
//! index). What the reader inferred is listed under "notes".
//!
//! The Live log's **marker rows** (`param_id` `link`: link lost / reconnected
//! …; `trigger`: trigger windows and `mark`s) are not channels; they are read
//! here and drawn as dashed vertical lines.
//!
//! Left: the channels with a checkbox, their colour and min / max / mean of
//! the visible time range. Right: an `egui_plot` time plot of the selected
//! channels and a readout of every selected channel at the hovered (or the
//! clicked, pinned) time.
//!
//! **Scaling:** channels have unrelated units and ranges (rpm next to a 0–1
//! duty), so each is normalised to its own min…max over the whole log (fixed,
//! so traces do not jump while panning). *Lanes* (default) gives every channel
//! its own horizontal strip labelled with its range; *Overlay* draws them on
//! top of each other on a 0–100 % axis. Real values are in the channel list
//! and the readout.
//!
//! **Large logs:** drawing is decimated per pixel bucket (each bucket keeps its
//! minimum and maximum, so spikes survive); statistics and the readout always
//! use every sample. Parsing runs on a background thread.
//!
//! **Map overlay** ([`overlay`]): bin the log over a calibration map's
//! breakpoints (hits, time in cell, a value channel's range per cell) with
//! the core's `opentune_hub::logoverlay`. **Drive view**: hand the open log
//! to the Drive view for a 1–8× replay.

mod overlay;
#[cfg(test)]
mod overlay_tests;

pub use overlay::LogContext;

use crate::icons;
use crate::image::fmt_val;
use crate::theme::{self, font, Tokens, Tone, MONO_MEDIUM, UI_MEDIUM, UI_STRONG};
use crate::views::maps::two_pane;
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, Label, Layout, Margin, Rect, RichText, Sense,
    Stroke, Ui, UiBuilder, Vec2,
};
use egui_plot::{HLine, Line, LineStyle, Plot, PlotPoint, PlotPoints, Points, Text, VLine};
use opentune_logview::{self as lv, ChannelStats, Log, LogFormat, ParseOptions, TimeUnit};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

/// Height of a channel row in the list.
const ROW_H: f32 = 44.0;
/// Number of distinct trace colours.
const PALETTE_LEN: usize = 8;
/// Channels selected automatically when a log opens.
const AUTO_SELECT: usize = 4;
/// Vertical padding inside a lane, as a fraction of the lane height.
const LANE_PAD: f64 = 0.14;
/// A line breaks where consecutive valid samples are further apart than this
/// many times the channel's median sample interval (link outages, pauses).
const GAP_FACTOR: f64 = 5.0;
/// Most samples the readout scans on each side for a valid value.
const SCAN_LIMIT: usize = 4096;
/// `param_id`s of the Live log's marker rows (link events, triggers).
const MARKER_IDS: [&str; 2] = ["link", "trigger"];

// ---------------------------------------------------------------------------
// Loading (pure, unit-tested)
// ---------------------------------------------------------------------------

/// A marker row of an OpenTune Live log.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Marker {
    /// Time on the log's axis (seconds from the first sample, or samples).
    pub t: f64,
    /// `link` or `trigger`.
    pub source: String,
    /// The row's `name` (e.g. `Link status`, the trigger's name).
    pub title: String,
    /// The row's `note` (what happened).
    pub note: String,
    /// The row's `text` (link state or trigger marker kind).
    pub state: String,
}

impl Marker {
    /// One line for the readout and the marker menu.
    fn label(&self) -> String {
        let mut s = if self.title.is_empty() {
            self.source.clone()
        } else {
            self.title.clone()
        };
        if !self.state.is_empty() {
            s.push_str(&format!(" · {}", self.state));
        }
        if !self.note.is_empty() {
            s.push_str(&format!(": {}", self.note));
        }
        s
    }

    /// Link loss and failed reconnects are drawn in the critical colour.
    fn is_alarm(&self) -> bool {
        self.source == "link" && matches!(self.state.as_str(), "lost" | "gave-up" | "reopen-failed")
    }
}

/// A parsed log plus what the view derives from it once.
pub(crate) struct Loaded {
    path: PathBuf,
    bytes: u64,
    log: Log,
    markers: Vec<Marker>,
    /// Whole-log value range per channel (normalisation); `None` when the
    /// channel has no valid sample.
    range: Vec<Option<(f64, f64)>>,
    /// Per channel: break the line where valid samples are further apart.
    gap: Vec<f64>,
}

/// Parse CSV log text: the channels (via `opentune-logview`) and, for an
/// OpenTune Live log, its marker rows.
pub(crate) fn load_text(text: &str) -> Result<(Log, Vec<Marker>), String> {
    let log = lv::parse(text, &ParseOptions::default()).map_err(|e| e.to_string())?;
    let markers = if log.format == LogFormat::OpentuneLong {
        extract_markers(text, &log)
    } else {
        Vec::new()
    };
    Ok((log, markers))
}

/// The marker rows (`param_id` `link` / `trigger`) of a Live log, on the same
/// time axis as `log.t` (same unit scaling and epoch offset as the parser).
pub(crate) fn extract_markers(text: &str, log: &Log) -> Vec<Marker> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let d = log.delimiter;
    let mut lines = text.lines().map(|l| l.trim_end_matches('\r')).filter(|l| {
        let t = l.trim();
        !t.is_empty() && !t.starts_with('#') && !t.starts_with("//")
    });
    // The header is the first line naming a param_id column.
    let mut header = None;
    for l in lines.by_ref() {
        let cols: Vec<String> = lv::csv::split_line(l, d)
            .iter()
            .map(|c| c.trim().to_ascii_lowercase())
            .collect();
        if cols.iter().any(|c| c == "param_id") {
            header = Some(cols);
            break;
        }
    }
    let Some(cols) = header else {
        return Vec::new();
    };
    let col = |n: &str| cols.iter().position(|c| c == n);
    let (Some(ts), Some(pid)) = (col("ts").or_else(|| col("round")), col("param_id")) else {
        return Vec::new();
    };
    let (name, note, state) = (col("name"), col("note"), col("text"));
    let scale = log.time_unit.to_seconds();
    let offset = log.epoch_start.unwrap_or(0.0);
    let mut out = Vec::new();
    for l in lines {
        let f = lv::csv::split_line(l, d);
        let id = f.get(pid).map(|s| s.trim()).unwrap_or("");
        if !MARKER_IDS.iter().any(|m| id.eq_ignore_ascii_case(m)) {
            continue;
        }
        let Some(raw) = f.get(ts).and_then(|s| lv::csv::num(s, false)) else {
            continue;
        };
        let cell = |c: Option<usize>| {
            c.and_then(|i| f.get(i))
                .map(|s| s.trim().to_string())
                .unwrap_or_default()
        };
        out.push(Marker {
            t: raw * scale - offset,
            source: id.to_ascii_lowercase(),
            title: cell(name),
            note: cell(note),
            state: cell(state),
        });
    }
    out.sort_by(|a, b| a.t.total_cmp(&b.t));
    out
}

/// Min and max of the finite values.
pub(crate) fn value_range(v: &[f64]) -> Option<(f64, f64)> {
    v.iter()
        .filter(|x| x.is_finite())
        .fold(None, |acc, &x| match acc {
            None => Some((x, x)),
            Some((lo, hi)) => Some((lo.min(x), hi.max(x))),
        })
}

/// [`GAP_FACTOR`] × the median interval between consecutive valid samples;
/// infinite when that is unknown (fewer than two valid samples, zero step).
pub(crate) fn gap_limit(t: &[f64], v: &[f64]) -> f64 {
    let mut prev: Option<f64> = None;
    let mut dts = Vec::new();
    for (&ti, &vi) in t.iter().zip(v) {
        if !vi.is_finite() {
            continue;
        }
        if let Some(p) = prev {
            dts.push(ti - p);
        }
        prev = Some(ti);
    }
    if dts.is_empty() {
        return f64::INFINITY;
    }
    let mid = dts.len() / 2;
    let (_, m, _) = dts.select_nth_unstable_by(mid, |a, b| a.total_cmp(b));
    if *m > 0.0 {
        *m * GAP_FACTOR
    } else {
        f64::INFINITY
    }
}

fn read_text(path: &Path) -> Result<(String, u64), String> {
    let meta =
        std::fs::metadata(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a file", path.display()));
    }
    if meta.len() > lv::csv::MAX_LOG_BYTES {
        return Err(format!(
            "the log is {} MiB; the viewer reads logs up to {} MiB",
            meta.len() >> 20,
            lv::csv::MAX_LOG_BYTES >> 20
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let text = match String::from_utf8(bytes) {
        Ok(s) => s,
        // Logs from Windows tools are often Latin-1 (`°C`); map bytes 1:1
        // like the core reader does.
        Err(e) => e.into_bytes().iter().map(|&b| b as char).collect(),
    };
    Ok((text, meta.len()))
}

fn load_file(path: &Path) -> Result<Loaded, String> {
    let (text, bytes) = read_text(path)?;
    let (log, markers) = load_text(&text)?;
    let range = log
        .channels
        .iter()
        .map(|c| value_range(&c.values))
        .collect();
    let gap = log
        .channels
        .iter()
        .map(|c| gap_limit(&log.t, &c.values))
        .collect();
    Ok(Loaded {
        path: path.to_path_buf(),
        bytes,
        log,
        markers,
        range,
        gap,
    })
}

// ---------------------------------------------------------------------------
// Decimation, scaling, readout (pure, unit-tested)
// ---------------------------------------------------------------------------

/// Collects points into line segments, starting a new one at a gap.
struct Segments {
    gap: f64,
    done: Vec<Vec<[f64; 2]>>,
    cur: Vec<[f64; 2]>,
}

impl Segments {
    fn push(&mut self, x: f64, y: f64) {
        if self.cur.last().is_some_and(|l| x - l[0] > self.gap) {
            self.flush();
        }
        self.cur.push([x, y]);
    }
    fn flush(&mut self) {
        if !self.cur.is_empty() {
            self.done.push(std::mem::take(&mut self.cur));
        }
    }
}

/// Plot points of one channel over `t0..=t1` (`t` sorted), split into line
/// segments wherever valid samples are more than `gap` apart. One sample
/// either side of the range is included so lines reach the plot edges.
///
/// With at most `2 * buckets` samples the samples are returned as they are.
/// Otherwise the range is cut into `buckets` equal-time buckets and each
/// bucket contributes its minimum and its maximum (in time order; one point
/// when they are the same sample), so no spike is lost. Missing (NaN)
/// samples are skipped.
pub(crate) fn decimate(
    t: &[f64],
    v: &[f64],
    t0: f64,
    t1: f64,
    buckets: usize,
    gap: f64,
) -> Vec<Vec<[f64; 2]>> {
    let n = t.len().min(v.len());
    let mut s = Segments {
        gap,
        done: Vec::new(),
        cur: Vec::new(),
    };
    if n == 0 || !t0.is_finite() || !t1.is_finite() || t1 < t0 {
        return s.done;
    }
    let lo = t[..n].partition_point(|&x| x < t0).saturating_sub(1);
    let hi = (t[..n].partition_point(|&x| x <= t1) + 1).min(n);
    let buckets = buckets.max(1);
    if hi - lo <= 2 * buckets || t1 <= t0 {
        for i in lo..hi {
            if v[i].is_finite() {
                s.push(t[i], v[i]);
            }
        }
    } else {
        let w = (t1 - t0) / buckets as f64;
        // Samples before t0 / after t1 fall in buckets -1 and `buckets`.
        let bucket = |x: f64| ((x - t0) / w).floor().clamp(-1.0, buckets as f64) as i64;
        let mut i = lo;
        while i < hi {
            let k = bucket(t[i]);
            let mut mn: Option<usize> = None;
            let mut mx: Option<usize> = None;
            while i < hi && bucket(t[i]) == k {
                if v[i].is_finite() {
                    if mn.is_none_or(|j| v[i] < v[j]) {
                        mn = Some(i);
                    }
                    if mx.is_none_or(|j| v[i] > v[j]) {
                        mx = Some(i);
                    }
                }
                i += 1;
            }
            if let (Some(a), Some(b)) = (mn, mx) {
                let (p, q) = if a <= b { (a, b) } else { (b, a) };
                s.push(t[p], v[p]);
                if q != p {
                    s.push(t[q], v[q]);
                }
            }
        }
    }
    s.flush();
    s.done
}

/// How the selected channels share the plot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Mode {
    /// One horizontal strip per channel.
    #[default]
    Lanes,
    /// All channels on one 0–100 % axis.
    Overlay,
}

/// `v` as a fraction of `range` (0.5 for a constant channel).
pub(crate) fn normalise(v: f64, (lo, hi): (f64, f64)) -> f64 {
    if hi > lo {
        (v - lo) / (hi - lo)
    } else {
        0.5
    }
}

/// Plot y of a normalised value: in lanes mode lane 0 is the top strip
/// `[lanes-1, lanes)`; in overlay mode the value itself.
fn plot_y(mode: Mode, lane: usize, lanes: usize, norm: f64) -> f64 {
    match mode {
        Mode::Overlay => norm,
        Mode::Lanes => {
            let base = lanes.saturating_sub(1 + lane) as f64;
            base + LANE_PAD + norm * (1.0 - 2.0 * LANE_PAD)
        }
    }
}

/// Fixed y range of the plot.
fn y_bounds(mode: Mode, lanes: usize) -> (f64, f64) {
    match mode {
        Mode::Overlay => (-0.05, 1.05),
        Mode::Lanes => (0.0, lanes.max(1) as f64),
    }
}

/// The valid sample nearest to `at` within `max_dist`: `(time, value)`.
pub(crate) fn value_at(t: &[f64], v: &[f64], at: f64, max_dist: f64) -> Option<(f64, f64)> {
    let n = t.len().min(v.len());
    if n == 0 || !at.is_finite() {
        return None;
    }
    let i = t[..n].partition_point(|&x| x < at);
    let mut best: Option<(f64, usize)> = None;
    for j in (i.saturating_sub(SCAN_LIMIT)..i).rev() {
        let d = at - t[j];
        if d > max_dist {
            break;
        }
        if v[j].is_finite() {
            best = Some((d, j));
            break;
        }
    }
    for j in i..n.min(i + SCAN_LIMIT) {
        let d = t[j] - at;
        if d > max_dist {
            break;
        }
        if v[j].is_finite() {
            if best.is_none_or(|b| d < b.0) {
                best = Some((d, j));
            }
            break;
        }
    }
    best.map(|(_, j)| (t[j], v[j]))
}

/// Every channel's statistics over `t0..=t1` (all samples, no decimation).
pub(crate) fn window_stats(log: &Log, t0: f64, t1: f64) -> Vec<ChannelStats> {
    lv::stats(log, Some(t0.min(t1)), Some(t0.max(t1)))
}

/// Case-insensitive: every whitespace-separated token of `q` occurs in the
/// channel name or unit.
pub(crate) fn channel_matches(name: &str, unit: &str, q: &str) -> bool {
    let hay = format!("{name}\n{unit}").to_lowercase();
    q.split_whitespace()
        .all(|tok| hay.contains(&tok.to_lowercase()))
}

/// The first palette slot no selected channel uses (wraps when all are).
fn next_slot(sel: &[Option<u8>]) -> u8 {
    let used: Vec<u8> = sel.iter().flatten().copied().collect();
    (0..PALETTE_LEN as u8)
        .find(|s| !used.contains(s))
        .unwrap_or((used.len() % PALETTE_LEN) as u8)
}

/// Categorical trace colours: the theme's accent, ok and warn plus five fixed
/// hues, toned per theme (deeper on light, lighter on dark; nothing neon).
pub(crate) fn palette(t: &Tokens) -> [Color32; PALETTE_LEN] {
    let c = Color32::from_rgb;
    if t.dark {
        [
            t.accent,
            c(0x6a, 0xa5, 0xff),
            t.ok,
            c(0xb3, 0x94, 0xff),
            c(0x4c, 0xc4, 0xd0),
            t.warn,
            c(0xec, 0x86, 0xb0),
            c(0x9a, 0xa6, 0xbd),
        ]
    } else {
        [
            t.accent,
            c(0x2f, 0x6f, 0xd6),
            t.ok,
            c(0x85, 0x5a, 0xcc),
            c(0x0e, 0x8f, 0x9c),
            t.warn,
            c(0xc2, 0x45, 0x7a),
            c(0x5f, 0x6b, 0x85),
        ]
    }
}

/// A time on the log's axis for labels.
/// An axis tick: only as many decimals as the tick spacing needs.
fn fmt_tick(t: f64, step: f64, unit: TimeUnit) -> String {
    if unit == TimeUnit::Index {
        return format!("{}", t.round() as i64);
    }
    let dec = if step > 0.0 && step.is_finite() {
        (-step.log10().floor()).clamp(0.0, 3.0) as usize
    } else {
        3
    };
    format!("{t:.dec$} s")
}

fn fmt_time(t: f64, unit: TimeUnit) -> String {
    if unit == TimeUnit::Index {
        format!("#{}", t.round() as i64)
    } else if t.abs() >= 600.0 {
        let s = t.abs();
        let sign = if t < 0.0 { "-" } else { "" };
        format!("{sign}{}:{:06.3}", (s / 60.0).floor() as i64, s % 60.0)
    } else {
        format!("{t:.3} s")
    }
}

fn fmt_duration(d: f64, unit: TimeUnit) -> String {
    if unit == TimeUnit::Index {
        return format!("{} samples", d.round() as i64);
    }
    if d >= 3600.0 {
        format!(
            "{}h {:02}m",
            (d / 3600.0) as i64,
            ((d % 3600.0) / 60.0) as i64
        )
    } else if d >= 60.0 {
        format!("{}m {:02}s", (d / 60.0) as i64, (d % 60.0) as i64)
    } else {
        format!("{d:.1} s")
    }
}

fn size_text(n: u64) -> String {
    if n >= 1 << 20 {
        format!("{:.1} MiB", n as f64 / (1u64 << 20) as f64)
    } else if n >= 1 << 10 {
        format!("{:.1} KiB", n as f64 / 1024.0)
    } else {
        format!("{n} B")
    }
}

/// `name [unit]`.
fn name_unit(name: &str, unit: &str) -> String {
    if unit.is_empty() {
        name.to_string()
    } else {
        format!("{name} [{unit}]")
    }
}

/// A value with up to 3 decimals, trailing zeros dropped (integers from
/// 1000 up).
fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 || v.abs() >= 1000.0 {
        fmt_val(v.round())
    } else {
        let s = format!("{v:.3}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// The whole log's time range (never empty).
fn full_range(log: &Log) -> (f64, f64) {
    let a = log.t.first().copied().unwrap_or(0.0);
    let b = log.t.last().copied().unwrap_or(1.0);
    if b > a {
        (a, b)
    } else {
        (a - 0.5, a + 0.5)
    }
}

/// The marker closest to `at` within `within`.
fn nearest_marker(markers: &[Marker], at: f64, within: f64) -> Option<&Marker> {
    markers
        .iter()
        .filter(|m| (m.t - at).abs() <= within)
        .min_by(|a, b| (a.t - at).abs().total_cmp(&(b.t - at).abs()))
}

// ---------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------

struct Pending {
    path: PathBuf,
    rx: mpsc::Receiver<Result<Loaded, String>>,
}

#[derive(Default)]
pub struct LogView {
    path_input: String,
    loading: Option<Pending>,
    error: Option<String>,
    data: Option<Loaded>,
    /// Per channel: its palette slot when selected.
    sel: Vec<Option<u8>>,
    filter: String,
    filter_key: Option<(String, u64)>,
    filtered: Vec<usize>,
    mode: Mode,
    /// Pinned cursor time (click in the plot).
    cursor: Option<f64>,
    /// X range shown in the last frame.
    view: Option<(f64, f64)>,
    stats_key: Option<(u64, u64, u64)>,
    stats: Vec<ChannelStats>,
    /// Bumped per opened log: a fresh plot id (and zoom) per log.
    generation: u64,
    reset: bool,
    center: Option<f64>,
    show_notes: bool,
    /// About six pixels in time units (marker hover snapping).
    snap: f64,
    /// The map overlay card.
    overlay: overlay::Overlay,
    /// "Drive view" was pressed (the app switches view, see
    /// [`LogView::take_show_drive`]).
    show_drive: bool,
    drive_note: Option<String>,
}

impl LogView {
    /// Draw the viewer; `cx` carries the app's selected definition and open
    /// image, which the map overlay offers.
    pub fn ui_with(&mut self, ui: &mut Ui, t: &Tokens, cx: LogContext<'_>) {
        self.poll();
        if self.loading.is_some() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(120));
        }
        self.refresh();
        let (left, right) = two_pane(ui);
        ui.scope_builder(UiBuilder::new().max_rect(left), |ui| {
            self.channel_pane(ui, t)
        });
        ui.scope_builder(UiBuilder::new().max_rect(right), |ui| {
            self.plot_pane(ui, t, &cx)
        });
    }

    /// True once after "Drive view" was pressed: the log is queued there and
    /// the app should switch to the `drive` view.
    pub fn take_show_drive(&mut self) -> bool {
        std::mem::take(&mut self.show_drive)
    }

    /// The map of the app's definition to show in the Maps view, once after
    /// the overlay's "Open in Maps" was pressed.
    pub fn take_open_map(&mut self) -> Option<String> {
        self.overlay.open_maps.take()
    }

    /// Queue the open log for the Drive view's replay.
    fn send_to_drive(&mut self) {
        if let Some(d) = &self.data {
            super::drive::queue_replay(d.path.clone());
            self.show_drive = true;
            self.drive_note = Some(format!(
                "{} is queued in the Drive view (Calibration → Drive view)",
                d.path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ));
        }
    }

    /// Start loading `path` in the background (also used for dropped files).
    pub(crate) fn open(&mut self, ctx: &egui::Context, path: PathBuf) {
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        let p = path.clone();
        std::thread::spawn(move || {
            let _ = tx.send(load_file(&p));
            ctx.request_repaint();
        });
        self.path_input = path.display().to_string();
        self.loading = Some(Pending { path, rx });
        self.error = None;
    }

    fn browse(&mut self, ctx: &egui::Context) {
        let mut dlg = rfd::FileDialog::new()
            .add_filter("datalog", &["csv", "CSV", "txt", "log", "tsv"])
            .add_filter("any file", &["*"]);
        if let Some(dir) = self.data.as_ref().and_then(|d| d.path.parent()) {
            dlg = dlg.set_directory(dir);
        }
        if let Some(p) = dlg.pick_file() {
            self.open(ctx, p);
        }
    }

    fn poll(&mut self) {
        let Some(p) = &self.loading else {
            return;
        };
        let res = match p.rx.try_recv() {
            Ok(r) => r,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                Err(format!("reading {} stopped unexpectedly", p.path.display()))
            }
        };
        self.loading = None;
        match res {
            Ok(d) => self.install(d),
            Err(e) => self.error = Some(e),
        }
    }

    fn install(&mut self, d: Loaded) {
        self.sel = vec![None; d.log.channels.len()];
        let mut slot = 0u8;
        for (i, r) in d.range.iter().enumerate() {
            if r.is_some() && (slot as usize) < AUTO_SELECT {
                self.sel[i] = Some(slot);
                slot += 1;
            }
        }
        self.path_input = d.path.display().to_string();
        self.data = Some(d);
        self.generation += 1;
        self.view = None;
        self.cursor = None;
        self.center = None;
        self.stats_key = None;
        self.filter_key = None;
        self.error = None;
        self.overlay.result = None;
        self.drive_note = None;
    }

    /// Filtered channel list and the statistics of the visible range.
    fn refresh(&mut self) {
        let Some(d) = &self.data else {
            return;
        };
        let fk = (self.filter.clone(), self.generation);
        if self.filter_key.as_ref() != Some(&fk) {
            self.filtered = d
                .log
                .channels
                .iter()
                .enumerate()
                .filter(|(_, c)| channel_matches(&c.name, &c.unit, &self.filter))
                .map(|(i, _)| i)
                .collect();
            self.filter_key = Some(fk);
        }
        let (t0, t1) = self.view.unwrap_or_else(|| full_range(&d.log));
        let key = (t0.to_bits(), t1.to_bits(), self.generation);
        if self.stats_key != Some(key) {
            self.stats = window_stats(&d.log, t0, t1);
            self.stats_key = Some(key);
        }
    }

    fn channel_pane(&mut self, ui: &mut Ui, t: &Tokens) {
        let total = self.data.as_ref().map_or(0, |d| d.log.channels.len());
        let selected = self.sel.iter().filter(|s| s.is_some()).count();
        let mut clear = false;
        theme::panel(
            ui,
            t,
            "Channels",
            |ui| {
                if total == 0 {
                    return;
                }
                if selected > 0
                    && theme::icon_button(ui, t, icons::ERASER, "Clear the selection").clicked()
                {
                    clear = true;
                }
                theme::pill(ui, t, &format!("{selected} / {total}"), Tone::Muted, true)
                    .on_hover_text("selected / channels");
            },
            |ui| {
                let Some(d) = &self.data else {
                    ui.add_space(18.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new("Open a log to list its channels.").color(t.fg_dim));
                    });
                    return;
                };
                egui::Frame::new()
                    .inner_margin(Margin::symmetric(12, 10))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(icons::MAGNIFYING_GLASS).color(t.fg_faint));
                            ui.add(
                                egui::TextEdit::singleline(&mut self.filter)
                                    .hint_text("Filter by name or unit")
                                    .desired_width(f32::INFINITY),
                            );
                        });
                    });
                theme::hline(ui, t.border_soft);
                if self.filtered.is_empty() {
                    ui.add_space(18.0);
                    ui.vertical_centered(|ui| {
                        let msg = if total == 0 {
                            "The log has no numeric channels.".to_string()
                        } else {
                            format!("No channel matches \u{201c}{}\u{201d}.", self.filter.trim())
                        };
                        ui.label(RichText::new(msg).color(t.fg_dim));
                    });
                    return;
                }
                let pal = palette(t);
                let mut toggle = None;
                egui::ScrollArea::vertical()
                    .id_salt("log-channels")
                    .auto_shrink([false, false])
                    .show_rows(ui, ROW_H, self.filtered.len(), |ui, rows| {
                        for &i in &self.filtered[rows] {
                            let w = ui.available_width();
                            let (r, resp) =
                                ui.allocate_exact_size(Vec2::new(w, ROW_H), Sense::click());
                            let c = &d.log.channels[i];
                            let row = ChannelRow {
                                name: &c.name,
                                unit: &c.unit,
                                color: self.sel[i].map(|s| pal[s as usize % PALETTE_LEN]),
                                stats: self.stats.get(i),
                                has_data: d.range[i].is_some(),
                            };
                            channel_row(ui, t, r, &resp, &row);
                            if resp.clicked() && row.has_data {
                                toggle = Some(i);
                            }
                        }
                    });
                if let Some(i) = toggle {
                    self.sel[i] = match self.sel[i] {
                        Some(_) => None,
                        None => Some(next_slot(&self.sel)),
                    };
                }
            },
        );
        if clear {
            self.sel.iter_mut().for_each(|s| *s = None);
        }
    }

    fn plot_pane(&mut self, ui: &mut Ui, t: &Tokens, cx: &LogContext<'_>) {
        let title = self
            .data
            .as_ref()
            .and_then(|d| d.path.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Log viewer".into());
        let ctx = ui.ctx().clone();
        let has_data = self.data.is_some();
        let mut mode = self.mode;
        let mut browse = false;
        let mut reset = false;
        let mut drive = false;
        let mut toggle_overlay = false;
        let overlay_open = self.overlay.open;
        theme::panel(
            ui,
            t,
            &title,
            |ui| {
                if theme::primary_button(ui, t, Some(icons::FOLDER_OPEN), "Open log")
                    .on_hover_text("Open a CSV datalog")
                    .clicked()
                {
                    browse = true;
                }
                if !has_data {
                    return;
                }
                if theme::icon_button(
                    ui,
                    t,
                    icons::STEERING_WHEEL,
                    "Play this log back in the Drive view",
                )
                .clicked()
                {
                    drive = true;
                }
                let tip = if overlay_open {
                    "Hide the map overlay"
                } else {
                    "Map overlay: which map cells the log used"
                };
                if theme::icon_button(ui, t, icons::GRID_FOUR, tip).clicked() {
                    toggle_overlay = true;
                }
                if theme::icon_button(ui, t, icons::CORNERS_OUT, "Show the whole log").clicked() {
                    reset = true;
                }
                // Right-to-left: the last added sits leftmost.
                ui.selectable_value(&mut mode, Mode::Overlay, "Overlay")
                    .on_hover_text("All channels on one axis, each scaled to its own min…max");
                ui.selectable_value(&mut mode, Mode::Lanes, "Lanes")
                    .on_hover_text("One strip per channel, each scaled to its own min…max");
            },
            |ui| {
                self.path_bar(ui, t, &ctx);
                theme::hline(ui, t.border_soft);
                if self.data.is_none() {
                    theme::empty_state(
                        ui,
                        t,
                        icons::CHART_LINE,
                        "Open a datalog",
                        "An OpenTune Live log (obd read --out, opentune log, the desktop Live \
                         log) or any CSV with a header row: comma, semicolon or tab separated.",
                    );
                    return;
                }
                egui::Frame::new()
                    .inner_margin(Margin::symmetric(14, 8))
                    .show(ui, |ui| {
                        self.info_row(ui, t);
                        if self.show_notes {
                            self.notes(ui, t);
                        }
                    });
                theme::hline(ui, t.border_soft);
                if let Some(n) = &self.drive_note {
                    egui::Frame::new()
                        .inner_margin(Margin::symmetric(14, 4))
                        .show(ui, |ui| {
                            ui.label(RichText::new(n).small().color(t.fg_dim));
                        });
                }
                egui::Frame::new()
                    .inner_margin(Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        let readout_h = 58.0;
                        let avail = ui.available_height();
                        let plot_h = if self.overlay.open {
                            (avail * 0.42 - readout_h).max(120.0)
                        } else {
                            (avail - readout_h).max(120.0)
                        };
                        let hover = self.plot(ui, t, plot_h);
                        ui.add_space(4.0);
                        self.readout(ui, t, hover);
                        if self.overlay.open {
                            ui.add_space(6.0);
                            theme::hline(ui, t.border_soft);
                            ui.add_space(6.0);
                            egui::ScrollArea::vertical()
                                .id_salt("lv-overlay")
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    ui.label(
                                        RichText::new("Map overlay")
                                            .font(font(theme::DISPLAY_MEDIUM, 13.5))
                                            .color(t.fg),
                                    );
                                    ui.add_space(4.0);
                                    if let Some(d) = &self.data {
                                        self.overlay.ui(
                                            ui,
                                            t,
                                            &d.log,
                                            self.generation,
                                            self.view,
                                            cx,
                                        );
                                    }
                                });
                        }
                    });
            },
        );
        if drive {
            self.send_to_drive();
        }
        if toggle_overlay {
            self.overlay.open = !self.overlay.open;
        }
        if mode != self.mode {
            self.mode = mode;
            ctx.request_repaint();
        }
        if reset {
            self.reset = true;
            ctx.request_repaint();
        }
        if browse {
            self.browse(&ctx);
        }
    }

    fn path_bar(&mut self, ui: &mut Ui, t: &Tokens, ctx: &egui::Context) {
        egui::Frame::new()
            .inner_margin(Margin::symmetric(12, 10))
            .show(ui, |ui| {
                let loading = self.loading.is_some();
                let mut go = false;
                ui.horizontal(|ui| {
                    ui.label(RichText::new(icons::FILE_CSV).color(t.fg_faint));
                    let edit = ui.add_enabled(
                        !loading,
                        egui::TextEdit::singleline(&mut self.path_input)
                            .hint_text("Paste or type a log path and press Enter")
                            .font(font(MONO_MEDIUM, 12.0))
                            .desired_width((ui.available_width() - 74.0).max(80.0)),
                    );
                    go = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    go |= ui
                        .add_enabled_ui(!loading, |ui| theme::button(ui, t, None, "Load"))
                        .inner
                        .clicked();
                });
                let typed = self.path_input.trim().trim_matches('"').to_string();
                if go && !typed.is_empty() {
                    self.open(ctx, PathBuf::from(typed));
                }
                if let Some(p) = &self.loading {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(14.0).color(t.accent));
                        ui.label(
                            RichText::new(format!("Reading {} …", p.path.display()))
                                .color(t.fg_dim),
                        );
                    });
                }
                if let Some(e) = &self.error {
                    ui.add_space(4.0);
                    ui.add(
                        Label::new(RichText::new(format!("{}  {e}", icons::WARNING)).color(t.crit))
                            .wrap(),
                    );
                }
            });
    }

    fn info_row(&mut self, ui: &mut Ui, t: &Tokens) {
        let Some(d) = &self.data else {
            return;
        };
        let log = &d.log;
        let unit = log.time_unit;
        let mut toggle_notes = false;
        let mut jump = None;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
            let fmt = match log.format {
                LogFormat::OpentuneLong => "OpenTune Live log",
                LogFormat::Wide => "CSV",
            };
            theme::pill(ui, t, fmt, Tone::Accent, false);
            theme::pill(ui, t, &format!("{} rows", log.len()), Tone::Muted, true);
            theme::pill(
                ui,
                t,
                &format!("{} channels", log.channels.len()),
                Tone::Muted,
                true,
            );
            theme::pill(
                ui,
                t,
                &fmt_duration(log.duration(), unit),
                Tone::Muted,
                true,
            );
            let dt = log.median_dt();
            if dt > 0.0 && unit != TimeUnit::Index {
                theme::pill(ui, t, &format!("{:.1} Hz", 1.0 / dt), Tone::Muted, true)
                    .on_hover_text("median sample rate");
            }
            let delim = match log.delimiter {
                '\t' => "tab".to_string(),
                c => format!("\u{201c}{c}\u{201d}"),
            };
            theme::pill(ui, t, &format!("sep {delim}"), Tone::Muted, true);
            theme::pill(ui, t, &size_text(d.bytes), Tone::Muted, true);
            if !d.markers.is_empty() {
                jump = marker_menu(ui, t, &d.markers, unit);
            }
            if !log.notes.is_empty() {
                let label = format!(
                    "{}  {} note{}",
                    if self.show_notes {
                        icons::CARET_UP
                    } else {
                        icons::INFO
                    },
                    log.notes.len(),
                    if log.notes.len() == 1 { "" } else { "s" }
                );
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new(label)
                                .font(font(UI_STRONG, 11.0))
                                .color(t.fg_dim),
                        )
                        .frame(false),
                    )
                    .on_hover_text("What the reader inferred or repaired")
                    .clicked()
                {
                    toggle_notes = true;
                }
            }
        });
        if toggle_notes {
            self.show_notes = !self.show_notes;
        }
        if let Some(x) = jump {
            self.center = Some(x);
            self.cursor = Some(x);
        }
    }

    fn notes(&self, ui: &mut Ui, t: &Tokens) {
        let Some(d) = &self.data else {
            return;
        };
        ui.add_space(4.0);
        for n in &d.log.notes {
            ui.add(Label::new(RichText::new(format!("•  {n}")).small().color(t.fg_dim)).wrap());
        }
    }

    /// The plot; returns the hovered time.
    fn plot(&mut self, ui: &mut Ui, t: &Tokens, height: f32) -> Option<f64> {
        let d = self.data.as_ref()?;
        let log = &d.log;
        let unit = log.time_unit;
        let pal = palette(t);
        let mode = self.mode;
        let lanes_idx: Vec<usize> = (0..log.channels.len())
            .filter(|&i| self.sel[i].is_some() && d.range[i].is_some())
            .collect();
        let lanes = lanes_idx.len();
        let (ylo, yhi) = y_bounds(mode, lanes);
        let (tf, tl) = full_range(log);
        let width_px = ui.available_width().max(1.0);
        let center = self.center.take();
        let cursor = self.cursor;
        let sel = &self.sel;

        let mut plot = Plot::new(("log-plot", self.generation))
            .height(height)
            .default_x_bounds(tf, tl)
            .default_y_bounds(ylo, yhi)
            .allow_zoom([true, false])
            .allow_drag([true, false])
            .allow_scroll(false)
            .allow_axis_zoom_drag([true, false])
            .show_y(false)
            .cursor_color(t.fg_faint)
            .show_grid([true, mode == Mode::Overlay])
            .grid_color(t.fg_faint)
            .x_axis_formatter(move |m, _| fmt_tick(m.value, m.step_size, unit))
            .y_axis_formatter(move |m, _| match mode {
                Mode::Overlay if (-1e-9..=1.0 + 1e-9).contains(&m.value) => {
                    format!("{:.0} %", m.value * 100.0)
                }
                _ => String::new(),
            });
        if std::mem::take(&mut self.reset) {
            plot = plot.reset();
        }
        let resp = plot.show(ui, |pu| {
            let b = pu.plot_bounds();
            let (mut x0, mut x1) = (b.min()[0], b.max()[0]);
            if !(b.is_finite_x() && x1 > x0) {
                (x0, x1) = (tf, tl);
            }
            if let Some(c) = center {
                let w = x1 - x0;
                (x0, x1) = (c - w / 2.0, c + w / 2.0);
                pu.set_plot_bounds_x(x0..=x1);
            }
            pu.set_plot_bounds_y(ylo..=yhi);
            // Plain wheel zooms time around the pointer (Ctrl+wheel and
            // pinch are egui_plot's own zoom).
            if pu.response().hovered() {
                let (dy, ctrl) = pu
                    .ctx()
                    .input(|i| (i.smooth_scroll_delta.y, i.modifiers.command));
                if dy != 0.0 && !ctrl {
                    pu.zoom_bounds_around_hovered(Vec2::new((dy * 0.004).exp(), 1.0));
                }
            }
            // Decimate a range twice as wide as the view (one bucket per
            // pixel), so a pan never shows an undrawn edge before the next
            // frame catches up.
            let w = x1 - x0;
            let (a, bb) = (x0 - w * 0.5, x1 + w * 0.5);
            let buckets = (width_px * 2.0) as usize;

            if mode == Mode::Lanes {
                for k in 1..lanes {
                    pu.hline(
                        HLine::new("", k as f64)
                            .color(t.border)
                            .width(1.0)
                            .allow_hover(false),
                    );
                }
            }
            for m in d.markers.iter().filter(|m| m.t >= a && m.t <= bb) {
                let col = if m.is_alarm() { t.crit } else { t.fg_dim };
                pu.vline(
                    VLine::new("", m.t)
                        .color(col.gamma_multiply(0.8))
                        .width(1.0)
                        .style(LineStyle::dashed_dense())
                        .allow_hover(false),
                );
            }
            if let Some(c) = cursor {
                pu.vline(
                    VLine::new("", c)
                        .color(t.accent)
                        .width(1.25)
                        .allow_hover(false),
                );
            }
            for (lane, &ci) in lanes_idx.iter().enumerate() {
                let (Some(slot), Some(range)) = (sel[ci], d.range[ci]) else {
                    continue;
                };
                let col = pal[slot as usize % PALETTE_LEN];
                let ch = &log.channels[ci];
                for seg in decimate(&log.t, &ch.values, a, bb, buckets, d.gap[ci]) {
                    let pts: Vec<[f64; 2]> = seg
                        .iter()
                        .map(|p| [p[0], plot_y(mode, lane, lanes, normalise(p[1], range))])
                        .collect();
                    if pts.len() == 1 {
                        pu.points(
                            Points::new("", pts)
                                .color(col)
                                .radius(1.75)
                                .allow_hover(false),
                        );
                    } else {
                        pu.line(
                            Line::new("", PlotPoints::new(pts))
                                .color(col)
                                .width(1.5)
                                .allow_hover(false),
                        );
                    }
                }
                if mode == Mode::Lanes {
                    let top = plot_y(mode, lane, lanes, 1.0) + LANE_PAD * 0.9;
                    let label = format!(
                        "{}   {} … {}",
                        name_unit(&ch.name, &ch.unit),
                        fmt_num(range.0),
                        fmt_num(range.1)
                    );
                    pu.text(
                        Text::new(
                            "",
                            PlotPoint::new(x0 + w * 0.004, top),
                            RichText::new(label).font(font(UI_MEDIUM, 11.0)).color(col),
                        )
                        .anchor(Align2::LEFT_TOP)
                        .allow_hover(false),
                    );
                }
            }
            (x0, x1)
        });
        let (x0, x1) = resp.inner;
        if self.view != Some((x0, x1)) {
            self.view = Some((x0, x1));
            ui.ctx().request_repaint();
        }
        self.snap = resp.transform.dvalue_dpos()[0].abs() * 6.0;
        let hover = resp
            .response
            .hover_pos()
            .map(|p| resp.transform.value_from_position(p).x);
        if resp.response.clicked() {
            self.cursor = hover;
        }
        if lanes == 0 {
            ui.painter().text(
                resp.response.rect.center(),
                Align2::CENTER_CENTER,
                "Tick channels on the left to plot them",
                font(UI_MEDIUM, 13.0),
                t.fg_faint,
            );
        }
        hover
    }

    fn readout(&mut self, ui: &mut Ui, t: &Tokens, hover: Option<f64>) {
        let Some(d) = &self.data else {
            return;
        };
        let log = &d.log;
        let unit = log.time_unit;
        let at = hover.or(self.cursor);
        let pal = palette(t);
        let mut clear = false;
        egui::ScrollArea::vertical()
            .id_salt("log-readout")
            .auto_shrink([false, false])
            .max_height(ui.available_height())
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(14.0, 4.0);
                    let Some(at) = at else {
                        ui.label(
                            RichText::new(
                                "Hover the plot to read values · click pins a cursor · drag or \
                                 wheel to pan and zoom time · double-click shows the whole log",
                            )
                            .small()
                            .color(t.fg_faint),
                        );
                        return;
                    };
                    let tag = if hover.is_some() { "hover" } else { "cursor" };
                    ui.label(
                        RichText::new(format!(
                            "{}  {tag} {}",
                            icons::CROSSHAIR,
                            fmt_time(at, unit)
                        ))
                        .font(font(MONO_MEDIUM, 12.0))
                        .color(t.accent),
                    );
                    if hover.is_none()
                        && ui
                            .add(
                                egui::Button::new(RichText::new(icons::X).color(t.fg_faint))
                                    .frame(false),
                            )
                            .on_hover_text("Remove the cursor")
                            .clicked()
                    {
                        clear = true;
                    }
                    for (i, c) in log.channels.iter().enumerate() {
                        let Some(slot) = self.sel[i] else {
                            continue;
                        };
                        let col = pal[slot as usize % PALETTE_LEN];
                        let v = value_at(&log.t, &c.values, at, d.gap[i])
                            .map(|(_, v)| {
                                if c.unit.is_empty() {
                                    fmt_num(v)
                                } else {
                                    format!("{} {}", fmt_num(v), c.unit)
                                }
                            })
                            .unwrap_or_else(|| "–".into());
                        readout_item(ui, t, col, &c.name, &v);
                    }
                    if let Some(m) = nearest_marker(&d.markers, at, self.snap) {
                        let col = if m.is_alarm() { t.crit } else { t.fg_dim };
                        ui.label(
                            RichText::new(format!("{}  {}", icons::FLAG, m.label()))
                                .font(font(UI_MEDIUM, 12.0))
                                .color(col),
                        );
                    }
                });
            });
        if clear {
            self.cursor = None;
        }
    }
}

/// "⚑ N markers" menu; returns the time of the marker picked.
fn marker_menu(ui: &mut Ui, t: &Tokens, markers: &[Marker], unit: TimeUnit) -> Option<f64> {
    let alarms = markers.iter().filter(|m| m.is_alarm()).count();
    let col = if alarms > 0 { t.crit } else { t.fg_dim };
    let r = ui
        .add(
            egui::Button::new(
                RichText::new(format!("{}  {} markers", icons::FLAG, markers.len()))
                    .font(font(UI_STRONG, 11.0))
                    .color(col),
            )
            .frame(false),
        )
        .on_hover_text("Link events and trigger marks: pick one to jump to it");
    let mut jump = None;
    egui::Popup::menu(&r).width(380.0).show(|ui| {
        egui::ScrollArea::vertical()
            .max_height(320.0)
            .show(ui, |ui| {
                for m in markers {
                    let c = if m.is_alarm() { t.crit } else { t.fg };
                    let txt = format!("{}   {}", fmt_time(m.t, unit), m.label());
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new(txt).font(font(MONO_MEDIUM, 11.5)).color(c),
                            )
                            .frame(false)
                            .truncate(),
                        )
                        .clicked()
                    {
                        jump = Some(m.t);
                    }
                }
            });
    });
    jump
}

fn readout_item(ui: &mut Ui, t: &Tokens, col: Color32, name: &str, value: &str) {
    use egui::text::{LayoutJob, TextFormat};
    let mut job = LayoutJob::default();
    let f = |font_id, color| TextFormat {
        font_id,
        color,
        valign: Align::Center,
        ..Default::default()
    };
    job.append("■ ", 0.0, f(font(UI_MEDIUM, 12.0), col));
    job.append(name, 0.0, f(font(UI_MEDIUM, 12.0), t.fg_dim));
    job.append(value, 6.0, f(font(MONO_MEDIUM, 12.5), t.fg));
    // One unit per item: a wrapping row moves the whole item down.
    ui.add(Label::new(job).wrap_mode(egui::TextWrapMode::Extend));
}

struct ChannelRow<'a> {
    name: &'a str,
    unit: &'a str,
    color: Option<Color32>,
    stats: Option<&'a ChannelStats>,
    has_data: bool,
}

fn channel_row(ui: &mut Ui, t: &Tokens, r: Rect, resp: &egui::Response, row: &ChannelRow) {
    let on = row.color.is_some();
    let bg = r.shrink2(Vec2::new(6.0, 1.0));
    if on || (resp.hovered() && row.has_data) {
        ui.painter()
            .rect_filled(bg, CornerRadius::same(theme::RADIUS_SM), t.surface2);
    }
    let inner = r.shrink2(Vec2::new(16.0, 6.0));
    // Checkbox.
    let cb = Rect::from_center_size(
        egui::pos2(inner.left() + 7.0, inner.top() + 8.0),
        Vec2::splat(14.0),
    );
    ui.painter().rect(
        cb,
        CornerRadius::same(4),
        if on { t.accent } else { t.surface2 },
        Stroke::new(1.0, if on { t.accent } else { t.border }),
        egui::StrokeKind::Inside,
    );
    if on {
        ui.painter().text(
            cb.center(),
            Align2::CENTER_CENTER,
            icons::CHECK,
            egui::FontId::proportional(11.0),
            t.on_accent,
        );
    }
    // Colour swatch: the trace colour while plotted.
    let sw = Rect::from_center_size(
        egui::pos2(cb.right() + 15.0, cb.center().y),
        Vec2::new(12.0, 4.0),
    );
    match row.color {
        Some(c) => {
            ui.painter().rect_filled(sw, CornerRadius::same(2), c);
        }
        None => {
            ui.painter().rect_stroke(
                sw,
                CornerRadius::same(2),
                Stroke::new(1.0, t.border),
                egui::StrokeKind::Inside,
            );
        }
    }
    let text = Rect::from_min_max(egui::pos2(sw.right() + 9.0, inner.top()), inner.max);
    let meta = match (row.has_data, row.stats) {
        (false, _) => "no numeric samples".to_string(),
        (true, Some(s)) if s.count > 0 => format!(
            "min {} · max {} · mean {}",
            s.min.map(fmt_num).unwrap_or_default(),
            s.max.map(fmt_num).unwrap_or_default(),
            s.mean.map(fmt_num).unwrap_or_default(),
        ),
        (true, _) => "no samples in view".to_string(),
    };
    ui.scope_builder(
        UiBuilder::new()
            .max_rect(text)
            .layout(Layout::top_down(Align::Min)),
        |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            let fg = if row.has_data { t.fg } else { t.fg_faint };
            ui.add(
                Label::new(
                    RichText::new(name_unit(row.name, row.unit))
                        .font(font(if on { UI_STRONG } else { UI_MEDIUM }, 13.0))
                        .color(fg),
                )
                .truncate()
                .selectable(false),
            );
            ui.add(
                Label::new(
                    RichText::new(meta)
                        .font(font(MONO_MEDIUM, 10.5))
                        .color(t.fg_faint),
                )
                .truncate()
                .selectable(false),
            );
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPOCH_MS: f64 = 1_763_000_000_000.0;

    /// A synthetic OpenTune Live log (desktop / `obd read --out` layout): two
    /// channels at 10 Hz, a slow third one, a dead round, a link lost /
    /// reconnected pair and a trigger mark.
    fn live_log() -> String {
        let mut s = String::from("ts,param_id,name,raw,value,units,note,text\n");
        for k in 0..20 {
            let ts = EPOCH_MS + k as f64 * 100.0;
            s.push_str(&format!(
                "{ts},0c,Engine speed,{k},{},rpm,,\n",
                800 + k * 10
            ));
            if k == 5 {
                // Dead round: empty value, the error in `note`.
                s.push_str(&format!("{ts},05,Coolant temperature,,,°C,timeout,\n"));
            } else {
                s.push_str(&format!(
                    "{ts},05,Coolant temperature,{k},{},°C,,\n",
                    60 + k
                ));
            }
            if k % 10 == 0 {
                s.push_str(&format!("{ts},33,Baro,{k},101,kPa,,\n"));
            }
            if k == 12 {
                s.push_str(&format!(
                    "{ts},link,Link status,,,,link lost after 3 dead round(s): timeout,lost\n"
                ));
            }
            if k == 15 {
                s.push_str(&format!(
                    "{ts},link,Link status,,,,reconnected after 0.3 s (attempt 1),reconnected\n"
                ));
                s.push_str(&format!("{ts},trigger,hot,,,,ect > 75,mark\n"));
            }
        }
        s
    }

    #[test]
    fn opentune_live_log_channels_units_and_markers() {
        let (log, markers) = load_text(&live_log()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(log.format, LogFormat::OpentuneLong);
        assert_eq!(log.len(), 20);
        assert_eq!(log.epoch_start, Some(EPOCH_MS / 1000.0));
        assert!((log.t[19] - 1.9).abs() < 1e-6);
        let rpm = log.channel("Engine speed").unwrap_or_else(|| panic!("rpm"));
        assert_eq!(rpm.unit, "rpm");
        assert_eq!(rpm.values[3], 830.0);
        let ect = log
            .channel("Coolant temperature")
            .unwrap_or_else(|| panic!("ect"));
        assert_eq!(ect.unit, "°C");
        assert!(ect.values[5].is_nan(), "a missing value stays missing");
        let baro = log.channel("Baro").unwrap_or_else(|| panic!("baro"));
        assert_eq!(baro.values.iter().filter(|v| v.is_finite()).count(), 2);
        // Marker rows are not plotted as channels.
        assert!(log
            .channels
            .iter()
            .all(|c| c.name != "Link status" && c.name != "hot"));
        assert_eq!(markers.len(), 3);
        assert!((markers[0].t - 1.2).abs() < 1e-6);
        assert_eq!(markers[0].source, "link");
        assert_eq!(markers[0].state, "lost");
        assert!(markers[0].is_alarm());
        assert!(markers[0].label().contains("link lost after 3"));
        assert!((markers[1].t - 1.5).abs() < 1e-6);
        let mark = markers
            .iter()
            .find(|m| m.source == "trigger")
            .unwrap_or_else(|| panic!("trigger"));
        assert!(!mark.is_alarm());
        assert_eq!(mark.label(), "hot · mark: ect > 75");
        assert_eq!(
            nearest_marker(&markers, 1.21, 0.05).map(|m| m.state.as_str()),
            Some("lost")
        );
        assert!(nearest_marker(&markers, 0.5, 0.05).is_none());
    }

    #[test]
    fn cli_log_layout_with_round_and_ts() {
        let mut s = String::from("round,param_id,name,raw,value,units,note,text,ts\n");
        for k in 0..5 {
            let ts = EPOCH_MS + k as f64 * 200.0;
            s.push_str(&format!("{k},0c,RPM,{k},{},rpm,,,{ts}\n", 900 + k));
        }
        s.push_str(&format!(
            "5,link,Link status,,,,link lost,lost,{}\n",
            EPOCH_MS + 1000.0
        ));
        let (log, markers) = load_text(&s).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(log.format, LogFormat::OpentuneLong);
        assert!((log.t[1] - 0.2).abs() < 1e-6, "time from ts, not round");
        assert_eq!(markers.len(), 1);
        assert!((markers[0].t - 1.0).abs() < 1e-6);
    }

    #[test]
    fn generic_csv_comma_with_units_in_header() {
        let csv = "Time [s],RPM [1/min],Boost (hPa),Lambda\n\
                   0.0,800,1000,1.0\n\
                   0.1,850,,0.98\n\
                   0.2,900,1100,0.97\n";
        let (log, markers) = load_text(csv).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(log.format, LogFormat::Wide);
        assert!(markers.is_empty());
        assert_eq!(log.len(), 3);
        let names: Vec<&str> = log.channels.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["RPM", "Boost", "Lambda"]);
        assert_eq!(log.channels[0].unit, "1/min");
        assert_eq!(log.channels[1].unit, "hPa");
        assert!(
            log.channels[1].values[1].is_nan(),
            "an empty cell is missing"
        );
        let st = window_stats(&log, 0.0, 0.2);
        assert_eq!(st[1].count, 2);
        assert_eq!(st[1].invalid, 1);
        assert_eq!(st[1].mean, Some(1050.0));
    }

    #[test]
    fn generic_csv_semicolon_decimal_comma_and_units_row() {
        let csv = "Zeit;Drehzahl;Temperatur\n\
                   s;1/min;°C\n\
                   0,0;800;80,5\n\
                   0,5;;81,0\n\
                   1,0;1200;81,5\n";
        let (log, _) = load_text(csv).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(log.delimiter, ';');
        assert!(log.decimal_comma);
        assert_eq!(log.t, vec![0.0, 0.5, 1.0]);
        assert_eq!(log.channels[0].unit, "1/min");
        assert_eq!(log.channels[1].values, vec![80.5, 81.0, 81.5]);
        assert!(log.channels[0].values[1].is_nan());
    }

    #[test]
    fn markers_need_a_live_log_header() {
        let (log, m) = load_text("a,b\n1,2\n3,4\n").unwrap_or_else(|e| panic!("{e}"));
        assert!(m.is_empty());
        assert!(extract_markers("a,b\n1,2\n", &log).is_empty());
        assert!(load_text("").is_err());
    }

    /// 100k samples of a sine with two one-sample spikes.
    fn big() -> (Vec<f64>, Vec<f64>) {
        let n = 100_000;
        let t: Vec<f64> = (0..n).map(|i| i as f64 * 0.01).collect();
        let mut v: Vec<f64> = t.iter().map(|x| (x * 0.7).sin()).collect();
        v[12_345] = 9.0;
        v[87_654] = -7.0;
        (t, v)
    }

    #[test]
    fn decimation_keeps_every_bucket_min_and_max() {
        let (t, v) = big();
        let (t0, t1) = (t[0], t[t.len() - 1]);
        let buckets = 800;
        let segs = decimate(&t, &v, t0, t1, buckets, f64::INFINITY);
        assert_eq!(segs.len(), 1, "no gaps");
        let pts = &segs[0];
        assert!(pts.len() <= 2 * (buckets + 2), "{} points", pts.len());
        assert!(pts.windows(2).all(|w| w[0][0] <= w[1][0]), "time order");
        let ys: Vec<f64> = pts.iter().map(|p| p[1]).collect();
        assert!(ys.contains(&9.0) && ys.contains(&-7.0), "spikes survive");
        // Each bucket's true min and max are among its points (and nothing
        // else is).
        let w = (t1 - t0) / buckets as f64;
        for k in [0usize, 17, 123, 400, 798] {
            let inb: Vec<f64> = t
                .iter()
                .zip(&v)
                .filter(|(x, _)| ((**x - t0) / w).floor() as usize == k)
                .map(|(_, y)| *y)
                .collect();
            let (mn, mx) = value_range(&inb).unwrap_or_else(|| panic!("bucket {k}"));
            let got: Vec<f64> = pts
                .iter()
                .filter(|p| ((p[0] - t0) / w).floor() as usize == k)
                .map(|p| p[1])
                .collect();
            assert!(got.contains(&mn) && got.contains(&mx), "bucket {k}");
            assert!(got.len() <= 2, "bucket {k}");
        }
    }

    #[test]
    fn decimation_window_small_ranges_and_gaps() {
        let (t, v) = big();
        // A narrow window: the raw samples plus one either side.
        let segs = decimate(&t, &v, 10.0, 10.5, 800, f64::INFINITY);
        let pts = &segs[0];
        assert_eq!(pts.len(), 51 + 2);
        assert!((pts[0][0] - 9.99).abs() < 1e-9);
        assert!((pts[52][0] - 10.51).abs() < 1e-9);
        // NaN samples are skipped; a time gap beyond the limit splits lines.
        let t2 = vec![0.0, 1.0, 2.0, 3.0, 10.0, 11.0];
        let v2 = vec![1.0, f64::NAN, 3.0, 4.0, 5.0, 6.0];
        let segs = decimate(&t2, &v2, 0.0, 11.0, 100, 2.5);
        assert_eq!(
            segs,
            vec![
                vec![[0.0, 1.0], [2.0, 3.0], [3.0, 4.0]],
                vec![[10.0, 5.0], [11.0, 6.0]]
            ]
        );
        assert!(decimate(&t2, &v2, 5.0, 1.0, 10, 1.0).is_empty());
        assert!(decimate(&[], &[], 0.0, 1.0, 10, 1.0).is_empty());
    }

    #[test]
    fn gap_limit_from_the_channels_own_rate() {
        let t: Vec<f64> = (0..10).map(|i| i as f64 * 0.1).collect();
        let mut v = vec![1.0; 10];
        assert!((gap_limit(&t, &v) - 0.5).abs() < 1e-9);
        // A slow channel (every 5th sample) gets a 5× wider limit.
        for (i, x) in v.iter_mut().enumerate() {
            if i % 5 != 0 {
                *x = f64::NAN;
            }
        }
        assert!((gap_limit(&t, &v) - 2.5).abs() < 1e-9);
        assert!(gap_limit(&t, &[f64::NAN; 10]).is_infinite());
    }

    #[test]
    fn statistics_cover_the_window_only() {
        let csv = "t,a\n0,1\n1,2\n2,3\n3,100\n4,\n";
        let (log, _) = load_text(csv).unwrap_or_else(|e| panic!("{e}"));
        let st = window_stats(&log, 0.0, 2.0);
        assert_eq!(
            (st[0].min, st[0].max, st[0].mean),
            (Some(1.0), Some(3.0), Some(2.0))
        );
        // Reversed bounds are accepted.
        let st = window_stats(&log, 4.0, 2.5);
        assert_eq!((st[0].count, st[0].invalid, st[0].max), (1, 1, Some(100.0)));
        assert_eq!(value_range(&log.channels[0].values), Some((1.0, 100.0)));
        assert_eq!(value_range(&[f64::NAN]), None);
    }

    #[test]
    fn readout_takes_the_nearest_valid_sample() {
        let t = vec![0.0, 1.0, 2.0, 3.0, 4.0];
        let v = vec![10.0, f64::NAN, f64::NAN, 13.0, 14.0];
        assert_eq!(value_at(&t, &v, 1.2, 5.0), Some((0.0, 10.0)));
        assert_eq!(value_at(&t, &v, 2.2, 5.0), Some((3.0, 13.0)));
        assert_eq!(value_at(&t, &v, 3.6, 5.0), Some((4.0, 14.0)));
        assert_eq!(value_at(&t, &v, 9.0, 5.0), Some((4.0, 14.0)));
        assert_eq!(value_at(&t, &v, 1.5, 0.4), None, "beyond the gap limit");
        assert_eq!(value_at(&[], &[], 1.0, 1.0), None);
    }

    #[test]
    fn scaling_lanes_and_overlay() {
        assert_eq!(normalise(5.0, (0.0, 10.0)), 0.5);
        assert_eq!(normalise(3.0, (3.0, 3.0)), 0.5);
        // Lane 0 is the top strip.
        assert!(plot_y(Mode::Lanes, 0, 3, 0.0) >= 2.0);
        assert!(plot_y(Mode::Lanes, 0, 3, 1.0) < 3.0);
        assert!(plot_y(Mode::Lanes, 2, 3, 1.0) < 1.0);
        assert_eq!(plot_y(Mode::Overlay, 2, 3, 0.25), 0.25);
        assert_eq!(y_bounds(Mode::Lanes, 0), (0.0, 1.0));
        assert_eq!(y_bounds(Mode::Lanes, 4), (0.0, 4.0));
        let (log, _) = load_text("t,a\n5,1\n").unwrap_or_else(|e| panic!("{e}"));
        let (a, b) = full_range(&log);
        assert!(b > a, "a one-sample log still has a range");
    }

    #[test]
    fn channel_filter_and_colour_slots() {
        assert!(channel_matches("Engine speed", "rpm", ""));
        assert!(channel_matches("Engine speed", "rpm", "ENG RPM"));
        assert!(!channel_matches("Engine speed", "rpm", "boost"));
        assert_eq!(next_slot(&[None, None]), 0);
        assert_eq!(next_slot(&[Some(0), None, Some(2)]), 1);
        let full: Vec<Option<u8>> = (0..PALETTE_LEN as u8).map(Some).collect();
        assert_eq!(next_slot(&full), 0, "wraps");
        for t in [theme::LIGHT, theme::DARK] {
            let p = palette(&t);
            for (i, a) in p.iter().enumerate() {
                assert_eq!(a.a(), 255);
                assert!(p[i + 1..].iter().all(|b| b != a), "distinct");
            }
        }
    }

    #[test]
    fn labels() {
        assert_eq!(fmt_time(1.5, TimeUnit::Seconds), "1.500 s");
        assert_eq!(fmt_tick(20.0, 10.0, TimeUnit::Seconds), "20 s");
        assert_eq!(fmt_tick(1.25, 0.05, TimeUnit::Seconds), "1.25 s");
        assert_eq!(fmt_tick(1.0, 1e-6, TimeUnit::Seconds), "1.000 s");
        assert_eq!(fmt_tick(300.0, 100.0, TimeUnit::Index), "300");
        assert_eq!(fmt_time(754.25, TimeUnit::Seconds), "12:34.250");
        assert_eq!(fmt_time(41.6, TimeUnit::Index), "#42");
        assert_eq!(fmt_duration(3725.0, TimeUnit::Seconds), "1h 02m");
        assert_eq!(fmt_duration(75.0, TimeUnit::Milliseconds), "1m 15s");
        assert_eq!(fmt_num(0.12345), "0.123");
        assert_eq!(fmt_num(2.5), "2.5");
        assert_eq!(fmt_num(1234.6), "1235");
        assert_eq!(name_unit("RPM", ""), "RPM");
        assert_eq!(size_text(2048), "2.0 KiB");
    }
}
