//! The Live log panel's pure logic (no egui widgets, no adapter): the
//! engine's JSON (start report, tick, stop summary) read into what the panel
//! shows ([`Board`]), the CSV row of a sample (the Tauri live log's columns,
//! which the Log viewer reads), the link / mode badges, the SSM summary, the
//! Drive view's feed event and the panel's options turned into a
//! [`LiveSpec`]. A port of the webview's `livelog-core.js` and the live-log
//! helpers of `util.js`.

use crate::host::{FeedEvent, FeedValue, LiveSpec};
use crate::theme::Tone;
use crate::widgets::caps::Need;
use opentune_logdef::LINK_PARAM_ID;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};

/// The live log's CSV columns (the webview's `LIVE_LOG_HEAD`; the Log viewer
/// reads them as "OpenTune Live log": one row per channel per tick, `ts` in
/// Unix ms, `text` last).
pub const CSV_HEAD: [&str; 8] = [
    "ts", "param_id", "name", "raw", "value", "units", "note", "text",
];

/// The feed source name (the Drive view tells publishers apart by it).
pub const SOURCE: &str = "livelog";

/// Per-request timeout of the session (the webview's `timeoutMs`).
pub const TIMEOUT_MS: u64 = 1000;

/// Points kept per channel for the sparklines and plots.
pub const HISTORY: usize = 1200;

/// Link events kept in the panel's list.
pub const EVENTS: usize = 200;

/// Link states in which the link does not answer.
const DOWN: [&str; 4] = ["silent", "lost", "reconnecting", "gave-up"];

// ---------------------------------------------------------------------------
// Samples
// ---------------------------------------------------------------------------

/// One sample as the engine reports it (`opentune_hub::livelog::sample_json`).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Row {
    pub param_id: String,
    pub name: String,
    pub raw: Option<u64>,
    pub value: Option<f64>,
    pub units: String,
    pub note: Option<String>,
    /// Text channels (identification DIDs) and link markers.
    pub text: Option<String>,
}

impl Row {
    /// A link-event marker (`paramId` `link`), not a channel.
    pub fn is_marker(&self) -> bool {
        self.param_id == LINK_PARAM_ID
    }

    pub fn note(&self) -> &str {
        self.note.as_deref().unwrap_or("")
    }

    /// A logdef sample (GPS speed-error input).
    pub fn sample(&self) -> opentune_logdef::Sample {
        opentune_logdef::Sample {
            param_id: self.param_id.clone(),
            name: self.name.clone(),
            raw: self.raw,
            value: self.value,
            units: self.units.clone(),
            note: self.note().to_string(),
            text: self.text.clone(),
        }
    }

    pub fn from_sample(s: &opentune_logdef::Sample) -> Self {
        Row {
            param_id: s.param_id.clone(),
            name: s.name.clone(),
            raw: s.raw,
            value: s.value,
            units: s.units.clone(),
            note: (!s.note.is_empty()).then(|| s.note.clone()),
            text: s.text.clone(),
        }
    }

    /// What the table shows: the text of a text channel, the value, else the
    /// raw number, else a dash (the webview's `liveShown`).
    pub fn shown(&self) -> String {
        if let Some(t) = &self.text {
            return t.clone();
        }
        match (self.value, self.raw) {
            (Some(v), _) => fmt_num(v),
            (None, Some(r)) => r.to_string(),
            _ => "—".into(),
        }
    }

    /// The CSV cells of this sample at `ts` (the webview's `liveLogRow`).
    pub fn csv(&self, ts: u64) -> [String; 8] {
        [
            ts.to_string(),
            self.param_id.clone(),
            self.name.clone(),
            self.raw.map(|r| r.to_string()).unwrap_or_default(),
            self.value.map(js_num).unwrap_or_default(),
            self.units.clone(),
            self.note().to_string(),
            self.text.clone().unwrap_or_default(),
        ]
    }
}

/// A number as JavaScript's `String(n)` writes it for the values a sensor
/// gives (`1726`, `0.25`, `-3.5`), so both apps write the same CSV.
fn js_num(v: f64) -> String {
    if v == 0.0 {
        "0".into()
    } else {
        v.to_string()
    }
}

/// A value for the table: integers as such, else at most 3 decimals.
pub fn fmt_num(v: f64) -> String {
    if !v.is_finite() {
        return v.to_string();
    }
    if v.fract() == 0.0 && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    let s = format!("{v:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// The samples in `v` (a tick's `samples` or a stop summary's `events`).
pub fn rows(v: &Value) -> Vec<Row> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| serde_json::from_value(s.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Unix ms now.
pub fn now_ms() -> u64 {
    opentune_hub::livelog::chrono_millis()
}

// ---------------------------------------------------------------------------
// Badges
// ---------------------------------------------------------------------------

/// A pill: tone, text and hover text.
#[derive(Clone, Debug, PartialEq)]
pub struct Badge {
    pub tone: Tone,
    pub text: String,
    pub title: String,
}

fn badge(tone: Tone, text: impl Into<String>, title: impl Into<String>) -> Badge {
    Badge {
        tone,
        text: text.into(),
        title: title.into(),
    }
}

fn s<'v>(v: &'v Value, k: &str) -> &'v str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn n(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(Value::as_u64).unwrap_or(0)
}

/// Is the link down (no answer) in this link view?
pub fn link_down(link: &Value) -> bool {
    DOWN.contains(&s(link, "state"))
}

/// The link pill for a tick's `link` view and its marker samples, `None`
/// while the link is up and nothing happened (the webview's `linkBadge`).
pub fn link_badge(link: &Value, markers: &[Row]) -> Option<Badge> {
    let last = markers.last();
    let ev = last.and_then(|m| m.text.as_deref()).unwrap_or("");
    let last_note = last.map(|m| m.note()).filter(|n| !n.is_empty());
    let st = match s(link, "state") {
        "" => "up",
        x => x,
    };
    let err = s(link, "error");
    let or = |a: Option<&str>, b: &str| a.unwrap_or(b).to_string();
    match st {
        "up" | "reconnected" => {
            if matches!(ev, "reconnected" | "recovered") || st == "reconnected" {
                let ev_line = Some(s(link, "last_event")).filter(|e| !e.is_empty());
                Some(badge(
                    Tone::Ok,
                    "reconnected",
                    or(last_note.or(ev_line), "the link is back"),
                ))
            } else {
                None
            }
        }
        "silent" => {
            let k = match (n(link, "dead_ticks"), n(link, "dead_after")) {
                (0, _) => String::new(),
                (d, 0) => format!(" {d}"),
                (d, a) => format!(" {d}/{a}"),
            };
            Some(badge(
                Tone::Warn,
                format!("no answer{k}"),
                if err.is_empty() {
                    "nothing answered this tick"
                } else {
                    err
                },
            ))
        }
        "lost" => {
            let reconnect = link.get("reconnect").and_then(Value::as_bool) == Some(true);
            Some(badge(
                Tone::Crit,
                if reconnect {
                    "link lost — reconnecting…"
                } else {
                    "link lost"
                },
                or(
                    last_note.or(Some(err).filter(|e| !e.is_empty())),
                    "the link is down",
                ),
            ))
        }
        "reconnecting" => {
            let retry = match link.get("retry_in_ms").and_then(Value::as_u64) {
                Some(ms) if ms > 0 => format!("; next try in {:.1} s", ms as f64 / 1000.0),
                _ => String::new(),
            };
            Some(badge(
                Tone::Crit,
                format!("reconnecting (attempt {})", n(link, "attempt").max(1)),
                format!(
                    "{}{retry}",
                    if err.is_empty() {
                        "re-opening the adapter"
                    } else {
                        err
                    }
                ),
            ))
        }
        "gave-up" => Some(badge(
            Tone::Crit,
            "gave up reconnecting",
            or(last_note, err),
        )),
        other => Some(badge(Tone::Muted, other, err)),
    }
}

/// The link as one status line, `""` while it is up (the webview's
/// `linkText`).
pub fn link_text(link: &Value, markers: &[Row]) -> String {
    match link_badge(link, markers) {
        Some(b) if b.tone != Tone::Ok => {
            if b.title.is_empty() {
                b.text
            } else {
                format!("{}: {}", b.text, b.title)
            }
        }
        _ => String::new(),
    }
}

/// The read-mode pill of a `LiveStatus` (the webview's `logModeBadge`).
pub fn mode_badge(st: &Value) -> Option<Badge> {
    match s(st, "mode") {
        "" => None,
        "fast-2c" => {
            let dids: Vec<String> = st
                .get("dynamic_dids")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .map(|d| {
                            format!(
                                "0x{:04X} ({} B, {} ch)",
                                n(d, "did"),
                                n(d, "record_len"),
                                d.get("channels")
                                    .and_then(Value::as_array)
                                    .map_or(0, Vec::len)
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            let polled = st
                .get("polled")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            let mut title = format!(
                "Dynamic DIDs: {}",
                if dids.is_empty() {
                    "none".into()
                } else {
                    dids.join(", ")
                }
            );
            if polled > 0 {
                title.push_str(&format!("; {polled} channel(s) polled alongside"));
            }
            if n(st, "redefines") > 0 {
                title.push_str(&format!("; re-defined {}×", n(st, "redefines")));
            }
            Some(badge(Tone::Ok, "fast-2c", title))
        }
        _ => {
            let reason = s(st, "reason");
            let tone = if reason == "fast mode not requested" {
                Tone::Muted
            } else {
                Tone::Accent
            };
            Some(badge(
                tone,
                "polling",
                if reason.is_empty() { "polling" } else { reason },
            ))
        }
    }
}

/// Why the session polls, or which channels fast mode polls alongside its
/// dynamic DIDs (the webview's `logModeWhy` line).
pub fn mode_why(st: &Value) -> Option<String> {
    let polled: Vec<&str> = st
        .get("polled")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    match s(st, "mode") {
        "polling" => {
            let why = s(st, "reason");
            (!why.is_empty()).then(|| format!("Polling: {why}"))
        }
        "fast-2c" if !polled.is_empty() => Some(format!(
            "Polled alongside the dynamic DIDs: {}",
            polled.join(", ")
        )),
        _ => None,
    }
}

/// "12.3 samples/s · 1.00 req/sample" from a tick's `rate` (the webview's
/// `fmtLogRate`); `""` before the first tick.
pub fn fmt_rate(r: &Value) -> String {
    if n(r, "ticks") == 0 {
        return String::new();
    }
    let f = |k| r.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    let sps = f("samples_per_s");
    let sps = if sps >= 10.0 {
        format!("{sps:.0}")
    } else {
        format!("{sps:.1}")
    };
    format!(
        "{sps} samples/s · {:.2} req/sample",
        f("requests_per_sample")
    )
}

/// The module the engine reports it logs: `tx 7E0 / rx 7E8`, flagged when
/// the engine was assumed; an SSM module with its wire.
pub fn target_text(t: &Value) -> String {
    if t.is_null() {
        return String::new();
    }
    let ids = format!("tx {} / rx {}", s(t, "tx"), s(t, "rx"));
    let mut out = match s(t, "ssm") {
        "" => ids,
        m => {
            let wire = if s(t, "wire") == "can" {
                "CAN"
            } else {
                "K-line"
            };
            format!("SSM {} over {wire} ({ids})", m.to_uppercase())
        }
    };
    if t.get("assumed").and_then(Value::as_bool) == Some(true) {
        out.push_str(" · assumed engine (no module ids given)");
    }
    out
}

/// The panel's SSM summary from the start report's `ssm` (the webview's
/// `ssmSummary`).
#[derive(Clone, Debug, PartialEq)]
pub struct SsmSummary {
    pub head: String,
    pub logged: String,
    pub notes: Vec<String>,
    /// `""` when every requested id is logged.
    pub not_logged: String,
}

pub fn ssm_summary(ssm: &Value) -> Option<SsmSummary> {
    if !ssm.is_object() {
        return None;
    }
    let who = if s(ssm, "target") == "tcu" {
        "TCU"
    } else {
        "ECU"
    };
    let wire = if s(ssm, "wire") == "can" {
        "CAN"
    } else {
        "K-line"
    };
    let strs = |k: &str| -> Vec<String> {
        ssm.get(k)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let nl = strs("not_logged");
    let logged = ssm.get("logged").map_or_else(String::new, |v| match v {
        Value::Array(a) => a.len().to_string(),
        other => other.to_string(),
    });
    Some(SsmSummary {
        head: format!(
            "{who} over {wire} · SSM ID {} · ROM ID {} · {} capability byte(s)",
            s(ssm, "ssm_id"),
            s(ssm, "rom_id"),
            n(ssm, "capability_bytes")
        ),
        logged: format!("{logged} parameter(s) logged"),
        notes: strs("notes"),
        not_logged: if nl.is_empty() {
            String::new()
        } else {
            format!(
                "Selected but not logged (no supported SSM parameter by that id): {}",
                nl.join(", ")
            )
        },
    })
}

// ---------------------------------------------------------------------------
// Drive view feed
// ---------------------------------------------------------------------------

/// What a tick publishes to the Drive view: a frame of the channel samples
/// (plus `extra`, e.g. GPS) while the link answers, an error with the link
/// state while it does not, so a dead link never shows as live (the
/// webview's `driveEvent`).
pub fn feed_event(tick: &Value, samples: &[Row], extra: &[Row]) -> FeedEvent {
    let link = tick.get("link").cloned().unwrap_or(Value::Null);
    if link_down(&link) {
        let markers: Vec<Row> = samples.iter().filter(|r| r.is_marker()).cloned().collect();
        let mut message = link_text(&link, &markers);
        if message.is_empty() {
            message = "link down".into();
        }
        return FeedEvent::Error {
            source: SOURCE.into(),
            message,
            busy: false,
        };
    }
    FeedEvent::Frame {
        source: SOURCE.into(),
        t_ms: tick
            .get("ts")
            .and_then(Value::as_u64)
            .unwrap_or_else(now_ms),
        values: samples
            .iter()
            .chain(extra)
            .filter(|r| !r.is_marker())
            .map(|r| FeedValue {
                key: r.param_id.clone(),
                name: r.name.clone(),
                unit: r.units.clone(),
                value: r.value.filter(|_| r.text.is_none()),
            })
            .collect(),
    }
}

/// A failed tick for the Drive view (`busy`: another operation holds the
/// adapter).
pub fn feed_error(message: &str) -> FeedEvent {
    FeedEvent::Error {
        source: SOURCE.into(),
        message: message.into(),
        busy: message.to_ascii_lowercase().contains("busy"),
    }
}

/// The vehicle-speed sample among `rows` (for the GPS speed error): a
/// known speed id or a name containing "vehicle speed" (the webview's
/// `vehicleSpeedKmh` choice).
pub fn vehicle_speed(rows: &[Row]) -> Option<&Row> {
    const IDS: [&str; 9] = [
        "vss",
        "speed",
        "vehicle_speed",
        "vehicle_speed_kmh",
        "veh_speed",
        "obd_speed",
        "obd_vss",
        "spd",
        "obd01_0d_vss",
    ];
    rows.iter().find(|r| {
        let id = r.param_id.to_ascii_lowercase();
        !id.is_empty()
            && !id.starts_with("gps_")
            && (IDS.contains(&id.as_str()) || r.name.to_ascii_lowercase().contains("vehicle speed"))
    })
}

// ---------------------------------------------------------------------------
// Options → LiveSpec
// ---------------------------------------------------------------------------

/// The transports the panel offers: `(id, label)`.
pub const TRANSPORTS: [(&str, &str); 3] = [
    ("uds", "UDS / OBD-II"),
    ("ssm", "Subaru SSM (K-line)"),
    ("ssm-can", "Subaru SSM (CAN)"),
];

pub fn is_ssm(transport: &str) -> bool {
    matches!(transport, "ssm" | "ssm-can")
}

/// What the transport needs from the adapter (the panel's gate): a raw
/// K-line for SSM over K-line, else a diagnostic link.
pub fn needs(transport: &str) -> [Need; 1] {
    if transport == "ssm" {
        [Need::KLine]
    } else {
        [Need::Diag]
    }
}

/// The panel's session options (the webview's Live stream card).
#[derive(Clone, Debug, PartialEq)]
pub struct Options {
    /// Pause between ticks, ms (a tick never overlaps the previous one).
    pub interval_ms: u64,
    /// UDS `0x2C` fast mode.
    pub fast: bool,
    pub dynamic_did: String,
    pub max_record: String,
    /// Diagnostic session for fast mode (hex, blank = keep).
    pub session: String,
    /// `uds`, `ssm` or `ssm-can`.
    pub transport: String,
    /// SSM module: `ecu` or `tcu`.
    pub ssm_target: String,
    /// SSM packet limit (blank = the engine's default).
    pub ssm_max_packet: String,
    pub reconnect: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            interval_ms: 500,
            fast: false,
            dynamic_did: "F200".into(),
            max_record: "32".into(),
            session: String::new(),
            transport: "uds".into(),
            ssm_target: "ecu".into(),
            ssm_max_packet: String::new(),
            reconnect: false,
        }
    }
}

fn opt_usize(s: &str, what: &str) -> Result<Option<usize>, String> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(None);
    }
    t.parse::<usize>()
        .map(Some)
        .map_err(|_| format!("{what} '{t}' is not a whole number"))
}

impl Options {
    /// The session request for `def_path` and the chosen channel ids (empty:
    /// every enabled parameter). Fast mode is UDS only (SSM always batches);
    /// its fields are sent only when it is on.
    pub fn spec(&self, def_path: &str, ids: Vec<String>) -> Result<LiveSpec, String> {
        let ssm = is_ssm(&self.transport);
        let fast = self.fast && !ssm;
        let some = |s: &str| Some(s.trim().to_string()).filter(|s| !s.is_empty());
        Ok(LiveSpec {
            def_path: def_path.to_string(),
            timeout_ms: TIMEOUT_MS,
            ids,
            fast,
            dynamic_did: if fast { some(&self.dynamic_did) } else { None },
            max_record: if fast {
                opt_usize(&self.max_record, "max record")?
            } else {
                None
            },
            fast_session: if fast { some(&self.session) } else { None },
            transport: Some(if ssm {
                self.transport.clone()
            } else {
                "uds".into()
            }),
            ssm_target: ssm.then(|| self.ssm_target.clone()),
            ssm_max_packet: if ssm {
                opt_usize(&self.ssm_max_packet, "SSM packet limit")?
            } else {
                None
            },
            reconnect: self.reconnect,
        })
    }
}

// ---------------------------------------------------------------------------
// The board: what the panel shows of the running (or last) session
// ---------------------------------------------------------------------------

/// One logger parameter (`{ id, name, units }`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Param {
    pub id: String,
    pub name: String,
    pub units: String,
}

impl Param {
    fn of(v: &Value) -> Option<Self> {
        let id = s(v, "id");
        (!id.is_empty()).then(|| Param {
            id: id.into(),
            name: s(v, "name").into(),
            units: s(v, "units").into(),
        })
    }
}

/// The channels to show: what the session reports it logs, else the
/// definition's parameters filtered by the selection (the webview's
/// `tableParams`).
pub fn table_params(session: &[Param], def: &[Param], ids: &[String]) -> Vec<Param> {
    if !session.is_empty() {
        return session.to_vec();
    }
    if ids.is_empty() {
        return def.to_vec();
    }
    def.iter()
        .filter(|p| ids.contains(&p.id))
        .cloned()
        .collect()
}

/// One table row with its history.
#[derive(Clone, Debug, Default)]
pub struct Chan {
    pub param: Param,
    pub last: Option<Row>,
    /// `[seconds since the first tick, value]`.
    pub hist: VecDeque<[f64; 2]>,
}

impl Chan {
    fn push(&mut self, t: f64, r: &Row) {
        if let Some(v) = r.value.filter(|v| v.is_finite() && r.text.is_none()) {
            if self.hist.len() == HISTORY {
                self.hist.pop_front();
            }
            self.hist.push_back([t, v]);
        }
        if !r.units.is_empty() {
            self.param.units.clone_from(&r.units);
        }
        self.last = Some(r.clone());
    }
}

/// Where the session is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    /// Nothing started yet.
    #[default]
    Idle,
    /// `live_start` is running (probing DIDs, defining, identifying).
    Starting,
    Running,
    /// `live_stop` is running (clearing dynamic DIDs).
    Stopping,
    /// Stopped (or never started: see `error`).
    Done,
}

/// What the panel shows of the current (or last) session. The session job
/// updates it; the panel draws it.
#[derive(Clone, Debug, Default)]
pub struct Board {
    pub phase: Phase,
    pub target: Value,
    pub ssm: Value,
    pub status: Value,
    pub rate: Value,
    pub link: Value,
    /// The last tick's link markers (the link pill).
    pub markers: Vec<Row>,
    /// Link events, newest last: `(Unix ms, state, description)`.
    pub events: VecDeque<(u64, String, String)>,
    pub chans: Vec<Chan>,
    index: HashMap<String, usize>,
    pub ticks: u64,
    /// Samples received (markers included).
    pub samples: u64,
    /// Unix ms of the first tick.
    pub t0: Option<u64>,
    /// The last failed tick (cleared by a good one).
    pub tick_error: Option<String>,
    /// The start failed (nothing ran) or the stop did.
    pub error: Option<String>,
    /// Why the session ended (its own reason, or the panel's).
    pub ended: Option<String>,
}

impl Board {
    /// A board for a session about to start with `def` / `ids` (the table
    /// shows them until the engine says what it logs).
    pub fn starting(def: &[Param], ids: &[String]) -> Self {
        let mut b = Board {
            phase: Phase::Starting,
            ..Default::default()
        };
        b.set_chans(table_params(&[], def, ids));
        b
    }

    /// The table's rows, one per id (a definition can list an id under
    /// several ECU variants).
    fn set_chans(&mut self, ps: Vec<Param>) {
        self.index.clear();
        self.chans.clear();
        for param in ps {
            if self.index.contains_key(&param.id) {
                continue;
            }
            self.index.insert(param.id.clone(), self.chans.len());
            self.chans.push(Chan {
                param,
                ..Default::default()
            });
        }
    }

    pub fn chan(&self, id: &str) -> Option<&Chan> {
        self.index.get(id).map(|&i| &self.chans[i])
    }

    /// The start report (`live_start`).
    pub fn apply_start(&mut self, v: &Value) {
        self.phase = Phase::Running;
        self.target = v.get("target").cloned().unwrap_or(Value::Null);
        self.ssm = v.get("ssm").cloned().unwrap_or(Value::Null);
        self.status = v.get("status").cloned().unwrap_or(Value::Null);
        self.link = v.get("link").cloned().unwrap_or(Value::Null);
        let session: Vec<Param> = v
            .get("params")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Param::of).collect())
            .unwrap_or_default();
        if !session.is_empty() {
            self.set_chans(session);
        }
    }

    fn markers_in(&mut self, ts: u64, rows: &[Row]) {
        for m in rows.iter().filter(|r| r.is_marker()) {
            if self.events.len() == EVENTS {
                self.events.pop_front();
            }
            let what = if m.note().is_empty() {
                m.name.clone()
            } else {
                m.note().to_string()
            };
            self.events
                .push_back((ts, m.text.clone().unwrap_or_default(), what));
        }
    }

    /// One tick (`live_tick`) and its samples.
    pub fn apply_tick(&mut self, v: &Value, rows: &[Row]) {
        let ts = v.get("ts").and_then(Value::as_u64).unwrap_or_else(now_ms);
        let t0 = *self.t0.get_or_insert(ts);
        let t = ts.saturating_sub(t0) as f64 / 1000.0;
        self.ticks += 1;
        self.samples += rows.len() as u64;
        for r in rows.iter().filter(|r| !r.is_marker()) {
            match self.index.get(&r.param_id) {
                Some(&i) => self.chans[i].push(t, r),
                None => {
                    // A channel the start report did not list: show it too.
                    self.index.insert(r.param_id.clone(), self.chans.len());
                    let mut c = Chan {
                        param: Param {
                            id: r.param_id.clone(),
                            name: r.name.clone(),
                            units: r.units.clone(),
                        },
                        ..Default::default()
                    };
                    c.push(t, r);
                    self.chans.push(c);
                }
            }
        }
        self.markers = rows.iter().filter(|r| r.is_marker()).cloned().collect();
        self.markers_in(ts, rows);
        for k in ["status", "rate", "link", "target"] {
            if let Some(x) = v.get(k).filter(|x| !x.is_null()) {
                match k {
                    "status" => self.status = x.clone(),
                    "rate" => self.rate = x.clone(),
                    "link" => self.link = x.clone(),
                    _ => self.target = x.clone(),
                }
            }
        }
        if let Some(e) = v.get("ended").and_then(Value::as_str) {
            self.ended = Some(e.to_string());
        }
        self.tick_error = None;
    }

    /// The stop summary (`live_stop`): final status / rate / link, and the
    /// link markers no tick picked up.
    pub fn apply_stop(&mut self, v: &Value) {
        for k in ["status", "rate", "link"] {
            if let Some(x) = v.get(k).filter(|x| !x.is_null()) {
                match k {
                    "status" => self.status = x.clone(),
                    "rate" => self.rate = x.clone(),
                    _ => self.link = x.clone(),
                }
            }
        }
        let events = rows(v.get("events").unwrap_or(&Value::Null));
        self.markers_in(now_ms(), &events);
        if !events.is_empty() {
            self.markers = events;
        }
        if let Some(e) = v.get("ended").and_then(Value::as_str) {
            self.ended.get_or_insert_with(|| e.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn marker(state: &str, note: &str) -> Row {
        Row {
            param_id: "link".into(),
            name: "Link status".into(),
            note: Some(note.into()),
            text: Some(state.into()),
            ..Default::default()
        }
    }

    #[test]
    fn samples_read_from_the_engine_json_and_write_the_tauri_csv_row() {
        let v = json!([
            {"paramId": "rpm", "name": "Engine speed", "raw": 6904, "value": 1726.0, "units": "rpm", "note": null, "text": null},
            {"paramId": "vin", "name": "VIN", "raw": null, "value": null, "units": "", "note": "", "text": "WVW, \"x\""},
            {"paramId": "link", "name": "Link status", "raw": null, "value": null, "units": "", "note": "lost after 5 dead ticks", "text": "lost"},
            {"paramId": "map", "name": "MAP", "raw": 101, "value": 0.25, "units": "kPa", "note": "", "text": null}
        ]);
        let r = rows(&v);
        assert_eq!(r.len(), 4);
        assert_eq!(
            r[0].csv(1_700_000_000_000),
            [
                "1700000000000",
                "rpm",
                "Engine speed",
                "6904",
                "1726",
                "rpm",
                "",
                ""
            ]
            .map(String::from)
        );
        assert_eq!(r[1].csv(1)[7], "WVW, \"x\"");
        assert_eq!(r[1].csv(1)[4], "");
        assert!(r[2].is_marker() && !r[0].is_marker());
        assert_eq!(r[3].csv(1)[4], "0.25");
        assert_eq!(r[0].shown(), "1726");
        assert_eq!(r[1].shown(), "WVW, \"x\"");
        assert_eq!(fmt_num(12.34567), "12.346");
        assert_eq!(fmt_num(-0.5), "-0.5");
        assert_eq!(js_num(-0.0), "0");
        assert_eq!(
            CSV_HEAD.join(","),
            "ts,param_id,name,raw,value,units,note,text"
        );
    }

    #[test]
    fn the_written_csv_opens_in_the_log_viewer() {
        let mut csv = CSV_HEAD.join(",") + "\n";
        for (i, (rpm, ect)) in [(800.0, 80.0), (900.5, 81.0)].into_iter().enumerate() {
            let ts = 1_700_000_000_000 + i as u64 * 500;
            for r in [
                Row {
                    param_id: "rpm".into(),
                    name: "Engine speed".into(),
                    value: Some(rpm),
                    units: "rpm".into(),
                    ..Default::default()
                },
                Row {
                    param_id: "ect".into(),
                    name: "Coolant".into(),
                    value: Some(ect),
                    units: "C".into(),
                    ..Default::default()
                },
                marker("up", "ok"),
            ] {
                let cells = r.csv(ts);
                let line: Vec<_> = cells
                    .iter()
                    .map(|c| crate::widgets::csv::field(c))
                    .collect();
                csv.push_str(&line.join(","));
                csv.push('\n');
            }
        }
        let log =
            opentune_logview::parse(&csv, &opentune_logview::ParseOptions::default()).unwrap();
        assert_eq!(log.format, opentune_logview::LogFormat::OpentuneLong);
        assert_eq!(log.t, vec![0.0, 0.5]);
        assert_eq!(
            log.channel("Engine speed").unwrap().values,
            vec![800.0, 900.5]
        );
        assert_eq!(log.channel("Coolant").unwrap().unit, "C");
    }

    #[test]
    fn link_badges_follow_the_webview() {
        assert_eq!(link_badge(&json!({"state": "up"}), &[]), None);
        assert_eq!(link_badge(&Value::Null, &[]), None);
        let b = link_badge(
            &json!({"state": "up"}),
            &[marker("recovered", "back after 2 s")],
        )
        .unwrap();
        assert_eq!(
            (b.tone, b.text.as_str(), b.title.as_str()),
            (Tone::Ok, "reconnected", "back after 2 s")
        );
        let b = link_badge(
            &json!({"state": "silent", "dead_ticks": 2, "dead_after": 5}),
            &[],
        )
        .unwrap();
        assert_eq!(b.text, "no answer 2/5");
        assert_eq!(b.title, "nothing answered this tick");
        let lost = json!({"state": "lost", "reconnect": true, "error": "timeout"});
        let b = link_badge(&lost, &[]).unwrap();
        assert_eq!(
            (b.tone, b.text.as_str(), b.title.as_str()),
            (Tone::Crit, "link lost — reconnecting…", "timeout")
        );
        let b = link_badge(&json!({"state": "reconnecting", "attempt": 3, "retry_in_ms": 2000, "error": "no port"}), &[]).unwrap();
        assert_eq!(b.text, "reconnecting (attempt 3)");
        assert_eq!(b.title, "no port; next try in 2.0 s");
        assert_eq!(link_text(&lost, &[]), "link lost — reconnecting…: timeout");
        assert_eq!(link_text(&json!({"state": "reconnected"}), &[]), "");
        assert!(link_down(&lost) && !link_down(&json!({"state": "reconnected"})));
    }

    #[test]
    fn mode_badge_rate_target_and_ssm_lines() {
        let fast = json!({"mode": "fast-2c", "reason": null, "polled": ["vin"], "redefines": 1,
            "dynamic_dids": [{"did": 0xF200, "record_len": 4, "channels": ["rpm", "ect"]}]});
        let b = mode_badge(&fast).unwrap();
        assert_eq!(b.text, "fast-2c");
        assert_eq!(
            b.title,
            "Dynamic DIDs: 0xF200 (4 B, 2 ch); 1 channel(s) polled alongside; re-defined 1×"
        );
        assert_eq!(
            mode_why(&fast).unwrap(),
            "Polled alongside the dynamic DIDs: vin"
        );
        let poll = json!({"mode": "polling", "reason": "fast mode not requested"});
        assert_eq!(mode_badge(&poll).unwrap().tone, Tone::Muted);
        let refused = json!({"mode": "polling", "reason": "the ECU refused 0x2C (0x11)"});
        assert_eq!(mode_badge(&refused).unwrap().tone, Tone::Accent);
        assert_eq!(
            mode_why(&refused).unwrap(),
            "Polling: the ECU refused 0x2C (0x11)"
        );
        assert_eq!(mode_badge(&Value::Null), None);
        assert_eq!(fmt_rate(&json!({"ticks": 0})), "");
        assert_eq!(
            fmt_rate(&json!({"ticks": 10, "samples_per_s": 2.34, "requests_per_sample": 1.0})),
            "2.3 samples/s · 1.00 req/sample"
        );
        assert_eq!(
            target_text(&json!({"tx": "7E0", "rx": "7E8", "assumed": true})),
            "tx 7E0 / rx 7E8 · assumed engine (no module ids given)"
        );
        assert_eq!(
            target_text(
                &json!({"tx": "7E1", "rx": "7E9", "assumed": false, "ssm": "tcu", "wire": "can"})
            ),
            "SSM TCU over CAN (tx 7E1 / rx 7E9)"
        );
        let s = ssm_summary(&json!({"target": "ecu", "wire": "k-line", "ssm_id": "A1", "rom_id": "B2",
            "capability_bytes": 48, "logged": 3, "notes": ["P9: not supported"], "not_logged": ["P_ECT"]})).unwrap();
        assert_eq!(
            s.head,
            "ECU over K-line · SSM ID A1 · ROM ID B2 · 48 capability byte(s)"
        );
        assert_eq!(s.logged, "3 parameter(s) logged");
        assert_eq!(s.notes, vec!["P9: not supported"]);
        assert!(s.not_logged.ends_with(": P_ECT"));
        assert_eq!(ssm_summary(&Value::Null), None);
    }

    #[test]
    fn feed_frames_while_the_link_answers_errors_while_it_does_not() {
        let samples = rows(&json!([
            {"paramId": "rpm", "name": "RPM", "value": 800.0, "units": "rpm"},
            {"paramId": "vin", "name": "VIN", "text": "WVW"},
            {"paramId": "link", "name": "Link status", "text": "recovered", "note": "back"}
        ]));
        let gps = vec![Row {
            param_id: "gps_speed".into(),
            name: "GPS speed".into(),
            value: Some(50.0),
            units: "km/h".into(),
            ..Default::default()
        }];
        let up = json!({"ts": 42, "link": {"state": "up"}});
        match feed_event(&up, &samples, &gps) {
            FeedEvent::Frame {
                source,
                t_ms,
                values,
            } => {
                assert_eq!((source.as_str(), t_ms), (SOURCE, 42));
                let keys: Vec<_> = values.iter().map(|v| (v.key.as_str(), v.value)).collect();
                assert_eq!(
                    keys,
                    vec![
                        ("rpm", Some(800.0)),
                        ("vin", None),
                        ("gps_speed", Some(50.0))
                    ]
                );
            }
            e => panic!("{e:?}"),
        }
        let down = json!({"ts": 43, "link": {"state": "lost", "error": "timeout"}});
        assert_eq!(
            feed_event(&down, &[], &[]),
            FeedEvent::Error {
                source: SOURCE.into(),
                message: "link lost: timeout".into(),
                busy: false
            }
        );
        assert!(matches!(
            feed_error("adapter busy: x"),
            FeedEvent::Error { busy: true, .. }
        ));
        assert_eq!(vehicle_speed(&samples), None);
        assert_eq!(vehicle_speed(&gps), None, "GPS speed is not the vehicle's");
    }

    #[test]
    fn options_become_the_spec_fast_is_uds_only() {
        let mut o = Options::default();
        let s = o.spec("/d/logger.json", vec!["rpm".into()]).unwrap();
        assert!(!s.fast && s.dynamic_did.is_none() && s.max_record.is_none());
        assert_eq!(s.transport.as_deref(), Some("uds"));
        assert_eq!(s.timeout_ms, TIMEOUT_MS);
        o.fast = true;
        o.session = " 03 ".into();
        let s = o.spec("/d/logger.json", vec![]).unwrap();
        assert!(s.fast);
        assert_eq!(
            (
                s.dynamic_did.as_deref(),
                s.max_record,
                s.fast_session.as_deref()
            ),
            (Some("F200"), Some(32), Some("03"))
        );
        o.max_record = "x".into();
        assert!(o.spec("p", vec![]).unwrap_err().contains("max record"));
        // SSM: never fast, carries the module and the packet limit.
        o.transport = "ssm-can".into();
        o.ssm_target = "tcu".into();
        o.ssm_max_packet = "64".into();
        let s = o.spec("p", vec![]).unwrap();
        assert!(!s.fast && s.dynamic_did.is_none());
        assert_eq!(
            (
                s.transport.as_deref(),
                s.ssm_target.as_deref(),
                s.ssm_max_packet
            ),
            (Some("ssm-can"), Some("tcu"), Some(64))
        );
        assert_eq!(needs("ssm"), [Need::KLine]);
        assert_eq!(needs("ssm-can"), [Need::Diag]);
        assert_eq!(needs("uds"), [Need::Diag]);
    }

    #[test]
    fn the_board_follows_start_ticks_and_stop() {
        let def = vec![
            Param {
                id: "rpm".into(),
                name: "RPM".into(),
                units: "rpm".into(),
            },
            Param {
                id: "ect".into(),
                name: "ECT".into(),
                units: "C".into(),
            },
            Param {
                id: "map".into(),
                name: "MAP".into(),
                units: "kPa".into(),
            },
        ];
        let mut b = Board::starting(&def, &["rpm".into(), "map".into()]);
        assert_eq!(b.phase, Phase::Starting);
        assert_eq!(b.chans.len(), 2);
        let mut twice = def.clone();
        twice.push(def[0].clone());
        assert_eq!(
            Board::starting(&twice, &[]).chans.len(),
            3,
            "one row per id"
        );
        b.apply_start(&json!({
            "status": {"mode": "polling", "reason": "fast mode not requested"},
            "params": [{"id": "rpm", "name": "Engine speed", "units": "rpm"}],
            "target": {"tx": "7E0", "rx": "7E8", "assumed": false},
            "link": {"state": "up"}, "ssm": null,
        }));
        assert_eq!(b.phase, Phase::Running);
        assert_eq!(b.chans.len(), 1, "the session's own channel list wins");
        for (i, v) in [800.0, 900.0, 1000.0].into_iter().enumerate() {
            let tick = json!({"ts": 1000 + i as u64 * 500, "rate": {"ticks": i + 1}, "link": {"state": "up"},
                "status": {"mode": "polling"}, "running": true, "ended": null});
            let rows = rows(&json!([
                {"paramId": "rpm", "name": "Engine speed", "value": v, "units": "rpm"},
                {"paramId": "x", "name": "Extra", "value": 1.0, "units": ""}
            ]));
            b.apply_tick(&tick, &rows);
        }
        assert_eq!(b.ticks, 3);
        assert_eq!(b.samples, 6);
        let rpm = b.chan("rpm").unwrap();
        assert_eq!(
            rpm.hist.iter().copied().collect::<Vec<_>>(),
            vec![[0.0, 800.0], [0.5, 900.0], [1.0, 1000.0]]
        );
        assert_eq!(rpm.last.as_ref().unwrap().shown(), "1000");
        assert!(b.chan("x").is_some(), "unlisted channels are shown too");
        assert!(fmt_rate(&b.rate).ends_with("req/sample"));
        b.apply_tick(
            &json!({"ts": 3000, "link": {"state": "lost"}, "running": false, "ended": "connection lost"}),
            &rows(&json!([{"paramId": "link", "name": "Link status", "text": "lost", "note": "5 dead ticks"}])),
        );
        assert_eq!(b.ended.as_deref(), Some("connection lost"));
        assert_eq!(b.events.back().unwrap().1, "lost");
        assert_eq!(b.markers.len(), 1);
        b.apply_stop(
            &json!({"running": false, "status": {"mode": "polling"}, "rate": {"ticks": 4},
            "link": {"state": "gave-up"}, "ended": "other", "events": [
                {"paramId": "link", "name": "Link status", "text": "gave-up", "note": "gave up"}]}),
        );
        assert_eq!(
            b.ended.as_deref(),
            Some("connection lost"),
            "the first reason stays"
        );
        assert_eq!(b.events.len(), 2);
        assert_eq!(b.link["state"], "gave-up");
    }
}
