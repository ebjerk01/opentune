//! Drive view decisions (pure, unit-tested; the Tauri app's
//! `driveview-core.js`): unit-aware formatting, the default tiles of a
//! channel list, warn / crit thresholds from the OBD channel table, stale /
//! no-data / link-lost states, min / max hold, the live history and the
//! replay data and clock over [`opentune_logview::series::replay`].

use opentune_core::obd;
use opentune_logview::{Log, Replay, TimeUnit};
use std::collections::VecDeque;

// ---------------------------------------------------------------------------
// Units and numbers
// ---------------------------------------------------------------------------

/// A unit spelling reduced to one canonical form ("" when there is none).
pub fn norm_unit(u: &str) -> String {
    let s: String = u
        .trim()
        .to_lowercase()
        .trim_start_matches(['[', '('])
        .trim_end_matches([']', ')'])
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    match s.as_str() {
        "°c" | "degc" | "c" | "celsius" => "degc",
        "°f" | "degf" | "f" => "degf",
        "1/min" | "u/min" | "min-1" | "min^-1" | "rpm" => "rpm",
        "km/h" | "kmh" | "kph" | "km/hr" => "km/h",
        "mph" | "mi/h" => "mph",
        "m/s" => "m/s",
        "kpa" => "kpa",
        "bar" => "bar",
        "hpa" | "mbar" => "hpa",
        "psi" => "psi",
        "%" => "%",
        "v" => "v",
        "g/s" => "g/s",
        "λ" | "lambda" => "lambda",
        _ => return s,
    }
    .into()
}

const SPEED_UNITS: &[&str] = &["km/h", "mph", "m/s"];
const TEMP_UNITS: &[&str] = &["degc", "degf", "k"];
const PRESSURE_UNITS: &[&str] = &["kpa", "bar", "hpa", "psi"];

/// Pressure preference: kPa values shown in bar or the other way round.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PUnit {
    #[default]
    Bar,
    KPa,
}

fn decimals(v: Option<f64>, n: &str) -> usize {
    match n {
        "rpm" | "degc" | "degf" | "km/h" | "mph" | "kpa" | "hpa" | "%" => 0,
        "bar" | "lambda" => 2,
        "v" | "g/s" | "psi" | "m/s" => 1,
        _ => {
            let a = v.unwrap_or(0.0).abs();
            if a >= 100.0 {
                0
            } else if a >= 10.0 {
                1
            } else {
                2
            }
        }
    }
}

/// A value in the unit it is shown in: `(value, unit, decimals)`.
pub fn display(v: Option<f64>, unit: &str, p: PUnit) -> (Option<f64>, String, usize) {
    let n = norm_unit(unit);
    let x = v.filter(|v| v.is_finite());
    let (x, u) = match (n.as_str(), p) {
        ("kpa", PUnit::Bar) => (x.map(|v| v / 100.0), "bar".to_string()),
        ("bar", PUnit::KPa) => (x.map(|v| v * 100.0), "kPa".to_string()),
        _ => (x, unit.to_string()),
    };
    let dp = decimals(x, &norm_unit(&u));
    (x, u, dp)
}

/// `v` with `dp` decimals, "—" for none; "-0" is never shown; `signed`
/// adds an explicit "+".
pub fn fmt_num(v: Option<f64>, dp: usize, signed: bool) -> String {
    let Some(v) = v.filter(|v| v.is_finite()) else {
        return "\u{2014}".into();
    };
    let mut s = format!("{v:.dp$}");
    let zero = |s: &str| {
        s.trim_start_matches('-')
            .chars()
            .all(|c| c == '0' || c == '.')
    };
    if s.starts_with('-') && zero(&s) {
        s.remove(0);
    }
    if signed && v > 0.0 && !zero(&s) {
        s.insert(0, '+');
    }
    s
}

// ---------------------------------------------------------------------------
// Channels and roles
// ---------------------------------------------------------------------------

/// A channel a tile can show.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Chan {
    pub key: String,
    pub name: String,
    pub unit: String,
}

/// What a default tile is for.
pub struct Role {
    pub role: &'static str,
    pub label: &'static str,
    /// Exact ids (Dashboard keys, Live-log param ids).
    pub keys: &'static [&'static str],
    /// Matches a log header name (lower case).
    pub name: fn(&str) -> bool,
    /// Excludes names (lower case).
    pub not: fn(&str) -> bool,
    /// Canonical units accepted when the channel has a unit.
    pub units: Option<&'static [&'static str]>,
    /// Hidden until it has had a value.
    pub optional: bool,
    /// Shown with an explicit sign.
    pub signed: bool,
}

/// `w` as a whole word of `s` (letters / digits around it end the word).
fn word(s: &str, w: &str) -> bool {
    let b = s.as_bytes();
    s.match_indices(w).any(|(i, _)| {
        let before = i == 0 || !b[i - 1].is_ascii_alphanumeric();
        let j = i + w.len();
        let after = j >= b.len() || !b[j].is_ascii_alphanumeric();
        before && after
    })
}

fn any(s: &str, ws: &[&str]) -> bool {
    ws.iter().any(|w| s.contains(w))
}

fn none(_: &str) -> bool {
    false
}

/// Role order is the default tile order.
pub const ROLES: &[Role] = &[
    Role {
        role: "rpm",
        label: "RPM",
        keys: &["rpm", "engine_speed", "n_engine"],
        name: |s| {
            s.contains("engine speed")
                || s.contains("enginespeed")
                || word(s, "rpm")
                || s.contains("drehzahl")
        },
        not: |s| any(s, &["target", "limit", "idle set"]),
        units: Some(&["rpm"]),
        optional: false,
        signed: false,
    },
    Role {
        role: "speed",
        label: "Speed",
        keys: &[
            "speed",
            "vss",
            "vehicle_speed",
            "vehicle_speed_kmh",
            "veh_speed",
            "obd_speed",
            "obd_vss",
            "spd",
        ],
        name: |s| {
            s.contains("vehicle speed")
                || s.contains("vehiclespeed")
                || (s.starts_with("speed") && !s[5..].starts_with(|c: char| c.is_alphanumeric()))
                || s.contains("geschwindigkeit")
        },
        not: |s| any(s, &["gps", "error"]),
        units: Some(SPEED_UNITS),
        optional: false,
        signed: false,
    },
    Role {
        role: "coolant",
        label: "Coolant",
        keys: &["coolant_temp", "coolant", "ect"],
        name: |s| s.contains("coolant") || word(s, "ect") || s.contains("kühlmittel"),
        not: |s| any(s, &["target", "set point", "setpoint"]),
        units: Some(TEMP_UNITS),
        optional: false,
        signed: false,
    },
    Role {
        role: "iat",
        label: "Intake air",
        keys: &["intake_air_temp", "iat"],
        name: |s| {
            s.contains("intake air temp")
                || word(s, "intake air")
                || word(s, "iat")
                || s.contains("charge air temp")
        },
        not: none,
        units: Some(TEMP_UNITS),
        optional: false,
        signed: false,
    },
    Role {
        role: "boost",
        label: "Boost",
        keys: &["boost_kpa", "boost_bar", "boost"],
        name: |s| s.contains("boost"),
        not: |s| {
            any(
                s,
                &[
                    "command",
                    "target",
                    "set point",
                    "setpoint",
                    "desired",
                    "spec",
                    "soll",
                    "limit",
                ],
            )
        },
        units: Some(PRESSURE_UNITS),
        optional: false,
        signed: false,
    },
    Role {
        role: "load",
        label: "Load",
        keys: &["engine_load", "load", "calc_load"],
        name: |s| s.contains("engine load") || word(s, "load"),
        not: |s| any(s, &["target", "limit"]),
        units: Some(&["%"]),
        optional: false,
        signed: false,
    },
    Role {
        role: "gps_speed",
        label: "GPS speed",
        keys: &["gps_speed_kmh"],
        name: |s| s.contains("gps speed"),
        not: none,
        units: Some(SPEED_UNITS),
        optional: true,
        signed: false,
    },
    Role {
        role: "speed_error",
        label: "Speed error",
        keys: &["speed_error_kmh"],
        name: |s| s.contains("speed error"),
        not: none,
        units: Some(SPEED_UNITS),
        optional: true,
        signed: true,
    },
];

fn unit_fits(r: &Role, unit: &str) -> bool {
    let n = norm_unit(unit);
    n.is_empty() || r.units.is_none_or(|u| u.contains(&n.as_str()))
}

/// Units must fit either way: a log's "Speed" column in rpm is not the
/// vehicle speed, even though its name is a known id.
fn role_matches(r: &Role, c: &Chan, by_key_only: bool) -> bool {
    if !unit_fits(r, &c.unit) {
        return false;
    }
    if r.keys.contains(&c.key.to_lowercase().as_str()) {
        return true;
    }
    if by_key_only {
        return false;
    }
    let name = if c.name.is_empty() { &c.key } else { &c.name }.to_lowercase();
    (r.name)(&name) && !(r.not)(&name)
}

/// The role a channel plays.
pub fn role_of(c: &Chan) -> Option<&'static Role> {
    ROLES
        .iter()
        .find(|r| role_matches(r, c, true))
        .or_else(|| ROLES.iter().find(|r| role_matches(r, c, false)))
}

/// The default tiles of a channel list: one channel per role, in role
/// order, exact ids first, then header names. Returns keys.
pub fn pick_default(chans: &[Chan]) -> Vec<String> {
    let mut used: Vec<String> = Vec::new();
    for r in ROLES {
        let free = |c: &&Chan| !used.contains(&c.key);
        let hit = chans
            .iter()
            .filter(free)
            .find(|c| role_matches(r, c, true))
            .or_else(|| {
                chans
                    .iter()
                    .filter(free)
                    .find(|c| role_matches(r, c, false))
            });
        if let Some(c) = hit {
            used.push(c.key.clone());
        }
    }
    used
}

/// A saved selection filtered to the channels that exist, else the default.
pub fn selection_for(chans: &[Chan], saved: &[String]) -> Vec<String> {
    let kept: Vec<String> = saved
        .iter()
        .filter(|k| chans.iter().any(|c| &c.key == *k))
        .cloned()
        .collect();
    if kept.is_empty() {
        pick_default(chans)
    } else {
        kept
    }
}

// ---------------------------------------------------------------------------
// Thresholds
// ---------------------------------------------------------------------------

/// Range and high-side thresholds that apply to a channel.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Thresholds {
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub warn: Option<f64>,
    pub crit: Option<f64>,
    /// The OBD table row was found but its unit differs (not converted).
    pub unit_mismatch: bool,
}

/// The OBD channel table row for a channel (same key, same name, or the
/// row of its role); its range and warn / crit apply only when the units
/// agree (never converted).
pub fn thresholds_for(c: &Chan) -> Thresholds {
    let table = obd::channels();
    let name = c.name.to_lowercase();
    let row = table
        .iter()
        .find(|t| t.key == c.key)
        .or_else(|| {
            table
                .iter()
                .find(|t| !name.is_empty() && t.name.to_lowercase() == name)
        })
        .or_else(|| {
            let r = role_of(c)?;
            r.keys
                .iter()
                .find_map(|k| table.iter().find(|t| t.key == *k))
        });
    let Some(row) = row else {
        return Thresholds::default();
    };
    let n = norm_unit(&c.unit);
    if n.is_empty() || n != norm_unit(row.unit) {
        return Thresholds {
            unit_mismatch: !n.is_empty(),
            ..Default::default()
        };
    }
    Thresholds {
        min: Some(row.min),
        max: Some(row.max),
        warn: row.warn,
        crit: row.crit,
        unit_mismatch: false,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Normal,
    Warn,
    Crit,
}

/// The level of `v` against the thresholds (at or above).
pub fn level(v: Option<f64>, th: &Thresholds) -> Level {
    match v.filter(|v| v.is_finite()) {
        Some(v) if th.crit.is_some_and(|c| v >= c) => Level::Crit,
        Some(v) if th.warn.is_some_and(|w| v >= w) => Level::Warn,
        _ => Level::Normal,
    }
}

/// Bar range: the table's range when known, else what has been seen.
pub fn bar_range(th: &Thresholds, peak: &Peak) -> Option<(f64, f64)> {
    if let (Some(lo), Some(hi)) = (th.min, th.max) {
        if hi > lo {
            return Some((lo, hi));
        }
    }
    let (mn, mx) = (peak.min?, peak.max?);
    let lo = mn.min(0.0);
    let hi = if mx > lo { mx } else { lo + 1.0 };
    Some((lo, hi))
}

/// 0..1 position of `v` on a range (clamped).
pub fn frac(v: Option<f64>, rng: Option<(f64, f64)>) -> Option<f64> {
    let (lo, hi) = rng?;
    let v = v.filter(|v| v.is_finite())?;
    Some(((v - lo) / (hi - lo)).clamp(0.0, 1.0))
}

// ---------------------------------------------------------------------------
// Stale / link state
// ---------------------------------------------------------------------------

pub const STALE_MIN_S: f64 = 2.5;

/// A value older than this (seconds) is stale: three update intervals,
/// never less than [`STALE_MIN_S`].
pub fn stale_after(interval_s: f64) -> f64 {
    STALE_MIN_S.max(3.0 * interval_s.max(0.0))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    Up,
    Waiting,
    Lost,
    Stopped,
    /// A replay (no link).
    Replay,
}

/// Link of a live source: lost after a failing poll or no frame for
/// `stale_s`; waiting before the first frame; stopped when it said so.
pub fn link_state(
    stopped: bool,
    failing: bool,
    last_frame: Option<f64>,
    now: f64,
    stale_s: f64,
) -> Link {
    if stopped {
        Link::Stopped
    } else if failing {
        Link::Lost
    } else {
        match last_frame {
            None => Link::Waiting,
            Some(t) if now - t > stale_s => Link::Lost,
            Some(_) => Link::Up,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileState {
    Live,
    Stale,
    NoData,
    /// The link is down: nothing shown is live.
    Lost,
}

pub fn tile_state(link: Link, last_t: Option<f64>, now: f64, stale_s: f64) -> TileState {
    if !matches!(link, Link::Up | Link::Replay) {
        return if last_t.is_none() {
            TileState::NoData
        } else {
            TileState::Lost
        };
    }
    match last_t {
        None => TileState::NoData,
        Some(t) if now - t > stale_s => TileState::Stale,
        Some(_) => TileState::Live,
    }
}

/// "4 s ago" / "2 min ago" for an age in seconds.
pub fn fmt_age(s: f64) -> String {
    if !s.is_finite() || s < 0.0 {
        String::new()
    } else if s < 60.0 {
        format!("{} s ago", s.floor())
    } else if s < 3600.0 {
        format!("{} min ago", (s / 60.0).floor())
    } else {
        format!("{} h ago", (s / 3600.0).floor())
    }
}

// ---------------------------------------------------------------------------
// Peak hold and history
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Peak {
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub t_min: Option<f64>,
    pub t_max: Option<f64>,
    pub n: usize,
}

impl Peak {
    pub fn add(&mut self, v: Option<f64>, t: f64) {
        let Some(v) = v.filter(|v| v.is_finite()) else {
            return;
        };
        if self.min.is_none_or(|m| v < m) {
            self.min = Some(v);
            self.t_min = Some(t);
        }
        if self.max.is_none_or(|m| v > m) {
            self.max = Some(v);
            self.t_max = Some(t);
        }
        self.n += 1;
    }
}

/// Seconds of live history a sparkline shows.
pub const HIST_S: f64 = 60.0;

/// Live history: `(t, v)` with `None` gaps, at most `span` seconds back.
#[derive(Clone, Debug, Default)]
pub struct Hist(pub VecDeque<(f64, Option<f64>)>);

impl Hist {
    pub fn push(&mut self, t: f64, v: Option<f64>, span: f64) {
        self.0.push_back((t, v.filter(|v| v.is_finite())));
        while self.0.front().is_some_and(|p| p.0 < t - span) {
            self.0.pop_front();
        }
    }
}

/// Sparkline polylines of `(t, v)` inside `[ta, tb]`, scaled to `0..1`
/// (x left→right, y bottom→top); `None` values break the line. A flat
/// series sits mid-height.
pub fn spark(pts: &[(f64, Option<f64>)], ta: f64, tb: f64) -> Vec<Vec<[f64; 2]>> {
    let inside = |t: f64| t >= ta && t <= tb;
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for (t, v) in pts {
        if let Some(v) = v.filter(|_| inside(*t)) {
            lo = lo.min(v);
            hi = hi.max(v);
        }
    }
    if !lo.is_finite() {
        return Vec::new();
    }
    if hi - lo < 1e-9 {
        lo -= 1.0;
        hi += 1.0;
    }
    let span = if tb > ta { tb - ta } else { 1.0 };
    let mut out: Vec<Vec<[f64; 2]>> = Vec::new();
    let mut pen = false;
    for (t, v) in pts {
        if !inside(*t) {
            continue;
        }
        match v {
            None => pen = false,
            Some(v) => {
                let p = [(t - ta) / span, (v - lo) / (hi - lo)];
                if pen {
                    if let Some(l) = out.last_mut() {
                        l.push(p);
                    }
                } else {
                    out.push(vec![p]);
                }
                pen = true;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Replay
// ---------------------------------------------------------------------------

/// Index-time logs (no time column) replay at this many samples per
/// second at 1×.
pub const INDEX_HZ: f64 = 5.0;
pub const SPEEDS: [f64; 4] = [1.0, 2.0, 4.0, 8.0];
/// Most rows a replay loads (an hour at 50 Hz).
pub const REPLAY_ROWS: usize = 180_000;

/// One replayed channel: its values per row and, per row, the index of the
/// last valid sample at or before it.
#[derive(Clone, Debug)]
pub struct RChan {
    pub chan: Chan,
    pub v: Vec<Option<f64>>,
    pub lv: Vec<i64>,
}

/// Replay data: one shared time axis.
#[derive(Clone, Debug)]
pub struct Prepared {
    pub t: Vec<f64>,
    pub chans: Vec<RChan>,
    pub t0: f64,
    pub t1: f64,
    /// Log units per wall second at 1×.
    pub rate: f64,
    /// No time column: `t` is the sample index.
    pub index: bool,
    /// Median update interval in log units.
    pub dt: f64,
    pub stride: usize,
}

impl Prepared {
    pub fn chan(&self, key: &str) -> Option<&RChan> {
        self.chans.iter().find(|c| c.chan.key == key)
    }
    pub fn list(&self) -> Vec<Chan> {
        self.chans.iter().map(|c| c.chan.clone()).collect()
    }
}

/// Replay data of `log` (every channel), as the desktop's `log_replay` +
/// `replayPrepare`.
pub fn prepare(log: &Log, max_rows: usize) -> Result<Prepared, String> {
    let r = opentune_logview::replay(log, &[], max_rows).map_err(|e| e.to_string())?;
    let index = log.time_unit == TimeUnit::Index;
    Ok(prepare_replay(&r, index, log.median_dt() * r.stride as f64))
}

/// [`prepare`] from a [`Replay`] (`index`: no time column; `median_dt` of
/// the returned rows).
pub fn prepare_replay(r: &Replay, index: bool, median_dt: f64) -> Prepared {
    let n = r.t.len();
    let chans = r
        .series
        .iter()
        .map(|s| {
            let mut lv = Vec::with_capacity(n);
            let mut last = -1i64;
            for i in 0..n {
                if s.v.get(i).copied().flatten().is_some() {
                    last = i as i64;
                }
                lv.push(last);
            }
            RChan {
                chan: Chan {
                    key: s.name.clone(),
                    name: s.name.clone(),
                    unit: s.unit.clone(),
                },
                v: s.v.clone(),
                lv,
            }
        })
        .collect();
    let mut dt = if median_dt > 0.0 { median_dt } else { 0.0 };
    if dt == 0.0 && n > 1 {
        dt = (r.t[n - 1] - r.t[0]) / (n - 1) as f64;
    }
    Prepared {
        t0: r.t.first().copied().unwrap_or(0.0),
        t1: r.t.last().copied().unwrap_or(0.0),
        t: r.t.clone(),
        chans,
        rate: if index { INDEX_HZ } else { 1.0 },
        index,
        dt,
        stride: r.stride.max(1),
    }
}

/// Last index with `t[i] <= x` (`None` when there is none). `t` ascending.
pub fn idx_at(t: &[f64], x: f64) -> Option<usize> {
    let n = t.partition_point(|v| *v <= x);
    n.checked_sub(1)
}

/// The latest valid value of a channel at log time `x`: `(v, at)`.
pub fn sample(d: &Prepared, c: &RChan, x: f64) -> Option<(f64, f64)> {
    let i = idx_at(&d.t, x)?;
    let j = usize::try_from(*c.lv.get(i)?).ok()?;
    Some((c.v[j]?, d.t[j]))
}

/// Fold every valid sample with `x0 < t <= x1` into `peak` (playback
/// between two frames: no spike is skipped at 8×).
pub fn fold(d: &Prepared, c: &RChan, peak: &mut Peak, x0: f64, x1: f64) {
    let a = idx_at(&d.t, x0).map_or(0, |i| i + 1);
    let Some(b) = idx_at(&d.t, x1) else { return };
    for (t, v) in d.t.iter().zip(&c.v).take(b + 1).skip(a) {
        if let Some(v) = v {
            peak.add(Some(*v), *t);
        }
    }
}

/// Points of a channel in `[xa, xb]`, at most `max_pts` (every k-th plus
/// the last).
pub fn window(
    d: &Prepared,
    c: &RChan,
    xa: f64,
    xb: f64,
    max_pts: usize,
) -> Vec<(f64, Option<f64>)> {
    let a = idx_at(&d.t, xa).unwrap_or(0);
    let Some(b) = idx_at(&d.t, xb) else {
        return Vec::new();
    };
    let step = (b + 1 - a.min(b)).div_ceil(max_pts.max(2)).max(1);
    let mut out: Vec<(f64, Option<f64>)> =
        (a..=b).step_by(step).map(|i| (d.t[i], c.v[i])).collect();
    if out.last().is_some_and(|p| p.0 != d.t[b]) {
        out.push((d.t[b], c.v[b]));
    }
    out
}

/// The replay clock: log position `pos` in `[t0, t1]`, `speed`×, `rate` log
/// units per wall second at 1×. Wall times are milliseconds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Clock {
    pub t0: f64,
    pub t1: f64,
    pub rate: f64,
    pub pos: f64,
    pub speed: f64,
    pub playing: bool,
    pub wall: f64,
}

/// What one [`Clock::tick`] covered.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Step {
    pub from: f64,
    pub to: f64,
    /// Reached the end and stopped.
    pub ended: bool,
}

impl Clock {
    pub fn new(t0: f64, t1: f64, rate: f64) -> Self {
        Self {
            t0,
            t1: t1.max(t0),
            rate: if rate > 0.0 { rate } else { 1.0 },
            pos: t0,
            speed: 1.0,
            playing: false,
            wall: 0.0,
        }
    }
    pub fn pos_at(&self, wall: f64) -> f64 {
        if !self.playing {
            return self.pos;
        }
        (self.pos + (wall - self.wall) / 1000.0 * self.speed * self.rate).min(self.t1)
    }
    /// Play (again from the start when at the end).
    pub fn play(&mut self, wall: f64) {
        if self.pos >= self.t1 {
            self.pos = self.t0;
        }
        self.wall = wall;
        self.playing = true;
    }
    pub fn pause(&mut self, wall: f64) {
        self.pos = self.pos_at(wall);
        self.playing = false;
    }
    pub fn seek(&mut self, x: f64, wall: f64) {
        self.pos = x.clamp(self.t0, self.t1);
        self.wall = wall;
    }
    pub fn set_speed(&mut self, s: f64, wall: f64) {
        self.pos = self.pos_at(wall);
        self.wall = wall;
        self.speed = if SPEEDS.contains(&s) { s } else { 1.0 };
    }
    /// Advance to `wall`.
    pub fn tick(&mut self, wall: f64) -> Step {
        let from = self.pos;
        let to = self.pos_at(wall);
        self.pos = to;
        self.wall = wall;
        let ended = self.playing && to >= self.t1;
        if ended {
            self.playing = false;
        }
        Step { from, to, ended }
    }
}

/// "m:ss" / "h:mm:ss" from seconds, or "#n" for an index log.
pub fn fmt_clock(s: f64, index: bool) -> String {
    if !s.is_finite() {
        return "\u{2014}".into();
    }
    if index {
        return format!("#{}", s.round());
    }
    let x = s.max(0.0).floor() as u64;
    let (h, m, r) = (x / 3600, (x % 3600) / 60, x % 60);
    if h > 0 {
        format!("{h}:{m:02}:{r:02}")
    } else {
        format!("{m}:{r:02}")
    }
}
