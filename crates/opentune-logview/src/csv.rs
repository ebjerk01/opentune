//! CSV datalog parsing and format detection.
//!
//! Two layouts are understood:
//!
//! - **OpenTune Live log** (long format), as written by the desktop *Live log*
//!   panel: `ts,param_id,name,raw,value,units,note[,text]` with one row per
//!   parameter per tick and `ts` in Unix milliseconds. Rows sharing a `ts`
//!   become one sample; each `param_id` becomes a channel. The optional
//!   trailing `text` column carries text channels (identification DIDs, whose
//!   `value` is empty): they are listed in the notes, not plotted.
//! - **Wide CSV**: one column per channel, optionally a time column. Channel
//!   units come from a trailing `[unit]` / `(unit)` in the header name or from
//!   a units row directly under the header.
//!
//! Detection covers the delimiter (`,` `;` tab `|`), a header row, a units
//! row, a decimal comma (only when the delimiter is not a comma), preamble
//! lines before the header, `#` comment lines and the time unit. Anything that
//! was inferred rather than stated in the file is listed in [`Log::notes`] so
//! the UI can show it; the caller can override the time unit and column.

use crate::{LogError, Result};
use serde::{Deserialize, Serialize};

/// Largest log [`parse_file`] will read into memory.
pub const MAX_LOG_BYTES: u64 = 512 * 1024 * 1024;

/// The layout a log was read as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogFormat {
    /// OpenTune Live log export: one row per parameter per tick.
    OpentuneLong,
    /// One column per channel.
    Wide,
}

/// Unit of the time column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeUnit {
    /// Seconds.
    Seconds,
    /// Milliseconds.
    Milliseconds,
    /// Microseconds.
    Microseconds,
    /// Minutes.
    Minutes,
    /// Wall-clock `hh:mm:ss(.fff)` text.
    Clock,
    /// No time column: the sample index is used as time.
    Index,
}

impl TimeUnit {
    /// Parse a unit name as written in a header or chosen in the UI
    /// (`s`, `sec`, `ms`, `us`, `µs`, `min`, `clock`, `index`, …).
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        Some(match s.as_str() {
            "s" | "sec" | "secs" | "second" | "seconds" => Self::Seconds,
            "ms" | "msec" | "millis" | "milliseconds" => Self::Milliseconds,
            "us" | "µs" | "usec" | "microseconds" => Self::Microseconds,
            "min" | "mins" | "minute" | "minutes" => Self::Minutes,
            "clock" | "hh:mm:ss" => Self::Clock,
            "index" | "sample" | "samples" => Self::Index,
            _ => {
                // `µs` lower-cases to itself; handle the micro sign spelled as
                // a Greek mu too.
                if s == "μs" {
                    Self::Microseconds
                } else {
                    return None;
                }
            }
        })
    }

    /// Multiplier from this unit to seconds (`Clock` is already seconds once
    /// parsed; `Index` stays in samples).
    pub fn to_seconds(self) -> f64 {
        match self {
            Self::Seconds | Self::Clock | Self::Index => 1.0,
            Self::Milliseconds => 1e-3,
            Self::Microseconds => 1e-6,
            Self::Minutes => 60.0,
        }
    }
}

/// Where the time unit came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeSource {
    /// Stated in the time column's header (`Time (ms)`, `time_s`, …).
    Header,
    /// Fixed by the recognised log format.
    Format,
    /// Chosen by the caller.
    Override,
    /// Inferred from the values (see the notes).
    Inferred,
    /// No time column at all.
    None,
}

/// Parsing overrides.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ParseOptions {
    /// Force the time unit instead of detecting it.
    pub time_unit: Option<TimeUnit>,
    /// Force the time column (by header name) instead of detecting it.
    pub time_column: Option<String>,
}

/// One logged channel. Missing or unparseable samples are `NaN`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Channel {
    /// Channel name (header without the unit suffix).
    pub name: String,
    /// Unit as written in the log; empty when the log does not say.
    pub unit: String,
    /// One value per sample, aligned with [`Log::t`].
    pub values: Vec<f64>,
}

/// A parsed datalog: a shared time base and numeric channels.
#[derive(Debug, Clone, Serialize)]
pub struct Log {
    /// Layout the log was read as.
    pub format: LogFormat,
    /// Field delimiter.
    pub delimiter: char,
    /// Numbers used a decimal comma (`12,5`).
    pub decimal_comma: bool,
    /// The first row held column names.
    pub header: bool,
    /// A units row followed the header.
    pub units_row: bool,
    /// Time column name, `None` when the sample index is used.
    pub time_column: Option<String>,
    /// Unit of the time column as found in the file.
    pub time_unit: TimeUnit,
    /// How [`Log::time_unit`] was decided.
    pub time_source: TimeSource,
    /// When the time column was absolute (Unix epoch), its first value in
    /// seconds since the epoch; [`Log::t`] is then relative to it.
    pub epoch_start: Option<f64>,
    /// Sample times in seconds (samples for [`TimeUnit::Index`]), sorted.
    pub t: Vec<f64>,
    /// Numeric channels.
    pub channels: Vec<Channel>,
    /// Everything that was inferred, dropped or repaired while reading.
    pub notes: Vec<String>,
}

impl Log {
    /// Number of samples.
    pub fn len(&self) -> usize {
        self.t.len()
    }

    /// `true` when the log has no samples.
    pub fn is_empty(&self) -> bool {
        self.t.is_empty()
    }

    /// Look a channel up by name (exact, then case-insensitive).
    pub fn channel(&self, name: &str) -> Option<&Channel> {
        self.channels.iter().find(|c| c.name == name).or_else(|| {
            self.channels
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(name))
        })
    }

    /// Like [`Log::channel`] but an error naming the missing channel.
    pub fn require(&self, name: &str) -> Result<&Channel> {
        self.channel(name)
            .ok_or_else(|| LogError::Input(format!("the log has no channel '{name}'")))
    }

    /// Time span covered, `t[last] - t[0]`.
    pub fn duration(&self) -> f64 {
        match (self.t.first(), self.t.last()) {
            (Some(a), Some(b)) => b - a,
            _ => 0.0,
        }
    }

    /// Median spacing between samples (0 for fewer than two samples).
    pub fn median_dt(&self) -> f64 {
        median_step(&self.t)
    }
}

/// Read and parse the log at `path`.
pub fn parse_file(path: &std::path::Path, opts: &ParseOptions) -> Result<Log> {
    let meta = std::fs::metadata(path)?;
    if meta.len() > MAX_LOG_BYTES {
        return Err(LogError::Format(format!(
            "log is {} MiB; the viewer reads logs up to {} MiB",
            meta.len() / (1024 * 1024),
            MAX_LOG_BYTES / (1024 * 1024)
        )));
    }
    let bytes = std::fs::read(path)?;
    let text = match String::from_utf8(bytes) {
        Ok(s) => s,
        // Logs from Windows tools are often Latin-1 (`°C`); map bytes 1:1.
        Err(e) => e.into_bytes().iter().map(|&b| b as char).collect(),
    };
    parse(&text, opts)
}

/// Parse CSV log text.
pub fn parse(text: &str, opts: &ParseOptions) -> Result<Log> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let lines: Vec<&str> = text
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with('#') && !t.starts_with("//")
        })
        .collect();
    if lines.is_empty() {
        return Err(LogError::Format("the file has no data lines".into()));
    }

    let delimiter = detect_delimiter(&lines)?;
    let rows: Vec<Vec<String>> = lines.iter().map(|l| split_line(l, delimiter)).collect();
    let width = modal_width(&rows);
    let mut notes = Vec::new();

    // Preamble: free-text lines before the first row of the table's width.
    let start = rows.iter().position(|r| r.len() == width).unwrap_or(0);
    if start > 0 {
        notes.push(format!("skipped {start} preamble line(s) before the table"));
    }
    let rows = &rows[start..];
    let decimal_comma = delimiter != ',';

    let first = &rows[0];
    let header = numeric_share(first, decimal_comma) < 0.5;
    let (names, mut data_start) = if header {
        (first.clone(), 1)
    } else {
        notes.push("no header row; columns are named col1, col2, …".into());
        ((1..=width).map(|i| format!("col{i}")).collect(), 0)
    };

    // The OpenTune Live log export has a fixed header and no units row.
    let lower: Vec<String> = names
        .iter()
        .map(|c| c.trim().to_ascii_lowercase())
        .collect();
    let has = |n: &str| lower.iter().position(|c| c == n);
    let long = match (has("ts"), has("param_id"), has("value")) {
        (Some(ts), Some(pid), Some(val)) if header => Some((ts, pid, val)),
        _ => None,
    };

    // Units row: a mostly-text row right under the header, followed by data.
    let mut units_row = None;
    if header && long.is_none() && rows.len() > 2 {
        let cand = &rows[1];
        let filled = cand.iter().filter(|f| !f.is_empty()).count();
        if filled > 0
            && numeric_share(cand, decimal_comma) < 0.5
            && numeric_share(&rows[2], decimal_comma) >= 0.5
        {
            units_row = Some(cand.clone());
            data_start = 2;
        }
    }

    let data = &rows[data_start..];
    if data.is_empty() {
        return Err(LogError::Format(
            "the file has a header but no data rows".into(),
        ));
    }
    let ragged = data.iter().filter(|r| r.len() != names.len()).count();
    if ragged > 0 {
        notes.push(format!(
            "{ragged} row(s) had a different number of fields than the header; missing fields are empty"
        ));
    }

    let cols: Vec<Column> = names
        .iter()
        .enumerate()
        .map(|(i, raw)| {
            let (name, mut unit) = split_name_unit(raw);
            if let Some(u) = units_row.as_ref().and_then(|u| u.get(i)) {
                if !u.is_empty() {
                    unit = strip_brackets(u).to_string();
                }
            }
            Column {
                raw_name: raw.trim().to_string(),
                name,
                unit,
                cells: data
                    .iter()
                    .map(|r| r.get(i).cloned().unwrap_or_default())
                    .collect(),
            }
        })
        .collect();

    let mut log = if let Some((ts, pid, val)) = long {
        pivot_long(
            &cols,
            ts,
            pid,
            has("name"),
            val,
            has("units"),
            has("text"),
            opts,
            &mut notes,
        )?
    } else {
        wide(&cols, opts, &mut notes)?
    };

    log.delimiter = delimiter;
    log.header = header;
    log.units_row = units_row.is_some();
    log.decimal_comma = decimal_comma
        && cols
            .iter()
            .any(|c| c.cells.iter().take(200).any(|s| is_decimal_comma(s)));
    log.notes = notes;
    finish(&mut log);
    Ok(log)
}

// ---------------------------------------------------------------------------
// Layouts
// ---------------------------------------------------------------------------

struct Column {
    raw_name: String,
    name: String,
    unit: String,
    cells: Vec<String>,
}

fn empty_log(format: LogFormat) -> Log {
    Log {
        format,
        delimiter: ',',
        decimal_comma: false,
        header: true,
        units_row: false,
        time_column: None,
        time_unit: TimeUnit::Index,
        time_source: TimeSource::None,
        epoch_start: None,
        t: Vec::new(),
        channels: Vec::new(),
        notes: Vec::new(),
    }
}

/// OpenTune Live log: rows sharing `ts` are one sample.
#[allow(clippy::too_many_arguments)]
fn pivot_long(
    cols: &[Column],
    ts: usize,
    pid: usize,
    name: Option<usize>,
    val: usize,
    units: Option<usize>,
    text: Option<usize>,
    opts: &ParseOptions,
    notes: &mut Vec<String>,
) -> Result<Log> {
    let n = cols[ts].cells.len();
    // Per id: did any row carry a number, and the last text (text channels).
    let mut numeric: Vec<bool> = Vec::new();
    let mut texts: Vec<Option<String>> = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    let mut id_index = std::collections::HashMap::new();
    let mut labels: Vec<(String, String)> = Vec::new(); // (name, unit) per id
    let mut times: Vec<f64> = Vec::new();
    let mut rows: Vec<Vec<f64>> = Vec::new();
    let mut last_ts: Option<String> = None;
    let mut bad_time = 0usize;
    for i in 0..n {
        let ts_s = &cols[ts].cells[i];
        let Some(tv) = num(ts_s, false) else {
            bad_time += 1;
            continue;
        };
        let id = cols[pid].cells[i].clone();
        let k = *id_index.entry(id.clone()).or_insert_with(|| {
            ids.push(id.clone());
            let nm = name
                .map(|c| cols[c].cells[i].clone())
                .filter(|s| !s.is_empty());
            let un = units.map(|c| cols[c].cells[i].clone()).unwrap_or_default();
            labels.push((nm.unwrap_or_else(|| id.clone()), un));
            for r in rows.iter_mut() {
                r.push(f64::NAN);
            }
            numeric.push(false);
            texts.push(None);
            ids.len() - 1
        });
        if labels[k].1.is_empty() {
            if let Some(u) = units.map(|c| &cols[c].cells[i]).filter(|s| !s.is_empty()) {
                labels[k].1 = u.clone();
            }
        }
        if last_ts.as_deref() != Some(ts_s.as_str()) {
            times.push(tv);
            rows.push(vec![f64::NAN; ids.len()]);
            last_ts = Some(ts_s.clone());
        }
        if let Some(v) = num(&cols[val].cells[i], false) {
            numeric[k] = true;
            if let Some(row) = rows.last_mut() {
                row[k] = v;
            }
        } else if let Some(t) = text.map(|c| &cols[c].cells[i]).filter(|s| !s.is_empty()) {
            texts[k] = Some(t.clone());
        }
    }
    if bad_time > 0 {
        notes.push(format!("dropped {bad_time} row(s) without a usable ts"));
    }
    if times.is_empty() {
        return Err(LogError::Format(
            "the Live log has no rows with a timestamp".into(),
        ));
    }

    let mut log = empty_log(LogFormat::OpentuneLong);
    let unit = opts.time_unit.unwrap_or(TimeUnit::Milliseconds);
    log.time_source = if opts.time_unit.is_some() {
        TimeSource::Override
    } else {
        TimeSource::Format
    };
    log.time_unit = unit;
    log.time_column = Some(cols[ts].raw_name.clone());
    log.t = times.iter().map(|v| v * unit.to_seconds()).collect();

    // Channel names: the parameter name, disambiguated by id when two share one.
    let mut channels = Vec::with_capacity(ids.len());
    for (k, id) in ids.iter().enumerate() {
        let (nm, un) = &labels[k];
        let dup = labels.iter().filter(|(n2, _)| n2 == nm).count() > 1;
        channels.push(Channel {
            name: if dup {
                format!("{nm} ({id})")
            } else {
                nm.clone()
            },
            unit: un.clone(),
            values: rows
                .iter()
                .map(|r| r.get(k).copied().unwrap_or(f64::NAN))
                .collect(),
        });
    }
    // Text channels (identification DIDs) have nothing to plot: list them with
    // their last text instead of showing an empty trace.
    let mut shown_text = Vec::new();
    let mut kept = Vec::with_capacity(channels.len());
    for (k, ch) in channels.into_iter().enumerate() {
        match &texts[k] {
            Some(t) if !numeric[k] => shown_text.push(format!("{} = {t}", ch.name)),
            _ => kept.push(ch),
        }
    }
    if !shown_text.is_empty() {
        notes.push(format!(
            "{} text channel(s) not plotted (last value): {}",
            shown_text.len(),
            shown_text.join("; ")
        ));
    }
    log.channels = kept;
    Ok(log)
}

/// Header names that mean "time", compared after lower-casing and dropping
/// everything but letters and digits.
const TIME_NAMES: &[&str] = &[
    "time",
    "t",
    "timestamp",
    "ts",
    "zeit",
    "elapsed",
    "elapsedtime",
    "logtime",
    "sec",
    "secs",
    "seconds",
    "timesec",
    "timesecs",
    "timeseconds",
    "times",
    "timems",
    "timemsec",
    "tms",
    "ms",
    "millis",
    "timeus",
    "timemin",
    "timestampms",
    "tsms",
];

/// Generic wide CSV.
fn wide(cols: &[Column], opts: &ParseOptions, notes: &mut Vec<String>) -> Result<Log> {
    let mut log = empty_log(LogFormat::Wide);
    let key = |s: &str| -> String {
        s.to_ascii_lowercase()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect()
    };

    let time_idx = match &opts.time_column {
        Some(want) => Some(
            cols.iter()
                .position(|c| c.raw_name == *want || c.name == *want)
                .ok_or_else(|| LogError::Input(format!("the log has no column '{want}'")))?,
        ),
        None => cols
            .iter()
            .position(|c| TIME_NAMES.contains(&key(&c.name).as_str()))
            .or_else(|| {
                // A first column of clock times counts as time too.
                let c = cols.first()?;
                (c.cells
                    .iter()
                    .take(20)
                    .filter(|s| parse_clock(s).is_some())
                    .count()
                    >= 2)
                    .then_some(0)
            }),
    };

    let n = cols.first().map(|c| c.cells.len()).unwrap_or(0);
    let mut keep: Vec<bool> = vec![true; n];
    if let Some(ti) = time_idx {
        let tc = &cols[ti];
        let is_clock = opts.time_unit == Some(TimeUnit::Clock)
            || (opts.time_unit.is_none()
                && tc.cells.iter().take(50).any(|s| parse_clock(s).is_some()));
        let raw: Vec<Option<f64>> = tc
            .cells
            .iter()
            .map(|s| {
                if is_clock {
                    parse_clock(s)
                } else {
                    num(s, true)
                }
            })
            .collect();
        let bad = raw.iter().filter(|v| v.is_none()).count();
        if bad > 0 {
            notes.push(format!("dropped {bad} row(s) without a usable time value"));
        }
        for (k, v) in keep.iter_mut().zip(&raw) {
            *k = v.is_some();
        }
        let vals: Vec<f64> = raw.iter().flatten().copied().collect();
        if vals.is_empty() {
            return Err(LogError::Format(format!(
                "the time column '{}' has no numeric values",
                tc.raw_name
            )));
        }

        let (unit, source) = if let Some(u) = opts.time_unit {
            (u, TimeSource::Override)
        } else if is_clock {
            (TimeUnit::Clock, TimeSource::Header)
        } else if let Some(u) = time_unit_from_header(&tc.raw_name, &tc.unit) {
            (u, TimeSource::Header)
        } else {
            let (u, why) = infer_time_unit(&vals);
            notes.push(format!(
                "time unit of '{}' inferred as {} ({why}); choose the unit if this is wrong",
                tc.raw_name,
                unit_label(u)
            ));
            (u, TimeSource::Inferred)
        };
        log.time_unit = unit;
        log.time_source = source;
        log.time_column = Some(tc.raw_name.clone());
        log.t = vals.iter().map(|v| v * unit.to_seconds()).collect();
    } else {
        notes.push("no time column found; the sample index is used as time".into());
        log.time_unit = opts.time_unit.unwrap_or(TimeUnit::Index);
        log.time_source = if opts.time_unit.is_some() {
            TimeSource::Override
        } else {
            TimeSource::None
        };
        log.t = (0..n).map(|i| i as f64).collect();
    }

    let mut used: Vec<String> = Vec::new();
    for (i, c) in cols.iter().enumerate() {
        if Some(i) == time_idx {
            continue;
        }
        let values: Vec<f64> = c
            .cells
            .iter()
            .zip(&keep)
            .filter(|(_, k)| **k)
            .map(|(s, _)| num(s, true).unwrap_or(f64::NAN))
            .collect();
        let filled = c.cells.iter().filter(|s| !s.is_empty()).count();
        if values.iter().all(|v| v.is_nan()) {
            if filled > 0 {
                notes.push(format!(
                    "column '{}' is not numeric and was left out",
                    c.raw_name
                ));
            }
            continue;
        }
        let mut name = if c.name.is_empty() {
            format!("col{}", i + 1)
        } else {
            c.name.clone()
        };
        if used.contains(&name) {
            let mut k = 2;
            while used.contains(&format!("{name} ({k})")) {
                k += 1;
            }
            name = format!("{name} ({k})");
        }
        used.push(name.clone());
        log.channels.push(Channel {
            name,
            unit: c.unit.clone(),
            values,
        });
    }
    if log.channels.is_empty() {
        return Err(LogError::Format("the log has no numeric channels".into()));
    }
    Ok(log)
}

/// Sort by time if needed, make absolute (epoch) time relative.
fn finish(log: &mut Log) {
    let backwards = log.t.windows(2).filter(|w| w[1] < w[0]).count();
    if backwards > 0 {
        let mut order: Vec<usize> = (0..log.t.len()).collect();
        order.sort_by(|&a, &b| log.t[a].total_cmp(&log.t[b]));
        log.t = order.iter().map(|&i| log.t[i]).collect();
        for c in &mut log.channels {
            c.values = order.iter().map(|&i| c.values[i]).collect();
        }
        log.notes.push(format!(
            "time went backwards {backwards} time(s); samples were sorted by time"
        ));
    }
    // Seconds since 2001 or later: an absolute Unix timestamp.
    if log.time_unit != TimeUnit::Index {
        if let Some(&t0) = log.t.first() {
            if t0 >= 1.0e9 {
                log.epoch_start = Some(t0);
                for t in &mut log.t {
                    *t -= t0;
                }
            }
        }
    }
}

fn unit_label(u: TimeUnit) -> &'static str {
    match u {
        TimeUnit::Seconds => "seconds",
        TimeUnit::Milliseconds => "milliseconds",
        TimeUnit::Microseconds => "microseconds",
        TimeUnit::Minutes => "minutes",
        TimeUnit::Clock => "clock time",
        TimeUnit::Index => "sample index",
    }
}

/// A unit stated by the time column's header: `Time (ms)`, `time[s]`,
/// `time_ms`, `timestamp_ms`, `seconds`, …
fn time_unit_from_header(raw: &str, bracket_unit: &str) -> Option<TimeUnit> {
    if let Some(u) = TimeUnit::parse(bracket_unit) {
        return Some(u);
    }
    let l = raw.trim().to_ascii_lowercase();
    let base = l.split_once(['[', '(']).map(|x| x.0).unwrap_or(&l).trim();
    for sep in ['_', '-', ' ', '.'] {
        if let Some((_, suffix)) = base.rsplit_once(sep) {
            if let Some(u) = TimeUnit::parse(suffix) {
                return Some(u);
            }
        }
    }
    match base {
        "sec" | "secs" | "seconds" => Some(TimeUnit::Seconds),
        "ms" | "millis" => Some(TimeUnit::Milliseconds),
        _ => None,
    }
}

/// Guess a time unit from the values alone. Always reported as inferred.
fn infer_time_unit(vals: &[f64]) -> (TimeUnit, String) {
    let first = vals.first().copied().unwrap_or(0.0).abs();
    if first >= 1.0e14 {
        return (
            TimeUnit::Microseconds,
            "Unix-epoch-sized microsecond values".into(),
        );
    }
    if first >= 1.0e11 {
        return (
            TimeUnit::Milliseconds,
            "Unix-epoch-sized millisecond values".into(),
        );
    }
    if first >= 1.0e9 {
        return (TimeUnit::Seconds, "Unix-epoch-sized second values".into());
    }
    let step = median_step(vals);
    let integers = vals.iter().take(1000).all(|v| v.fract() == 0.0);
    if integers && step >= 5.0 {
        (
            TimeUnit::Milliseconds,
            format!("whole-number steps of {step}"),
        )
    } else {
        (TimeUnit::Seconds, format!("median step {step}"))
    }
}

fn median_step(v: &[f64]) -> f64 {
    let mut d: Vec<f64> = v
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|d| d.is_finite() && *d > 0.0)
        .collect();
    if d.is_empty() {
        return 0.0;
    }
    let mid = d.len() / 2;
    d.select_nth_unstable_by(mid, f64::total_cmp);
    d[mid]
}

// ---------------------------------------------------------------------------
// Low-level CSV helpers
// ---------------------------------------------------------------------------

const DELIMS: [char; 4] = ['\t', ';', '|', ','];

/// Pick the delimiter that gives the most rows of one consistent width (>1).
/// Ties go to the earlier entry of [`DELIMS`], so `;` wins over `,` for a
/// decimal-comma log whose numbers also contain commas.
fn detect_delimiter(lines: &[&str]) -> Result<char> {
    let sample: Vec<&str> = lines.iter().take(60).copied().collect();
    let mut best: Option<(usize, usize, char)> = None;
    for &d in &DELIMS {
        let widths: Vec<usize> = sample.iter().map(|l| split_line(l, d).len()).collect();
        let w = mode(&widths);
        if w < 2 {
            continue;
        }
        let agree = widths.iter().filter(|&&x| x == w).count();
        let better = match best {
            None => true,
            Some((a, _, _)) => agree > a,
        };
        if better {
            best = Some((agree, w, d));
        }
    }
    match best {
        Some((_, _, d)) => Ok(d),
        None if lines.len() > 1 => Ok(','), // a single-column log
        None => Err(LogError::Format(
            "could not find a field delimiter (tried , ; tab |)".into(),
        )),
    }
}

fn mode(v: &[usize]) -> usize {
    let mut counts = std::collections::BTreeMap::new();
    for &x in v {
        *counts.entry(x).or_insert(0usize) += 1;
    }
    // Highest count; on a tie the wider row (the table, not the preamble).
    counts
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)))
        .map(|(w, _)| w)
        .unwrap_or(0)
}

fn modal_width(rows: &[Vec<String>]) -> usize {
    let w: Vec<usize> = rows.iter().take(200).map(|r| r.len()).collect();
    mode(&w)
}

/// Split one line on `d`, honouring `"quoted, fields"` with `""` escapes.
/// Fields are trimmed.
pub fn split_line(line: &str, d: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    cur.push('"');
                    chars.next();
                } else {
                    quoted = false;
                }
            } else {
                cur.push(c);
            }
        } else if c == '"' && cur.trim().is_empty() {
            cur.clear();
            quoted = true;
        } else if c == d {
            out.push(cur.trim().to_string());
            cur.clear();
        } else {
            cur.push(c);
        }
    }
    out.push(cur.trim().to_string());
    out
}

/// Share of non-empty fields that are numbers.
fn numeric_share(row: &[String], decimal_comma: bool) -> f64 {
    let filled: Vec<&String> = row.iter().filter(|s| !s.is_empty()).collect();
    if filled.is_empty() {
        return 0.0;
    }
    let n = filled
        .iter()
        .filter(|s| num(s, decimal_comma).is_some() || parse_clock(s).is_some())
        .count();
    n as f64 / filled.len() as f64
}

fn is_decimal_comma(s: &str) -> bool {
    let s = s.trim();
    s.contains(',') && !s.contains('.') && num(s, true).is_some()
}

/// Parse a finite number; with `decimal_comma`, `12,5` reads as 12.5.
pub fn num(s: &str, decimal_comma: bool) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let v = if decimal_comma && s.contains(',') && !s.contains('.') && s.matches(',').count() == 1 {
        s.replace(',', ".").parse::<f64>().ok()
    } else {
        s.parse::<f64>().ok()
    }?;
    v.is_finite().then_some(v)
}

/// `hh:mm:ss(.fff)` or `mm:ss(.fff)` to seconds.
fn parse_clock(s: &str) -> Option<f64> {
    let s = s.trim();
    let parts: Vec<&str> = s.split(':').collect();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }
    let mut secs = 0.0;
    for (i, p) in parts.iter().enumerate() {
        let last = i == parts.len() - 1;
        let v: f64 = if last {
            p.replace(',', ".").parse().ok()?
        } else {
            if p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            p.parse().ok()?
        };
        if !v.is_finite() || v < 0.0 {
            return None;
        }
        secs = secs * 60.0 + v;
    }
    Some(secs)
}

/// `"Engine speed [rpm]"` → (`"Engine speed"`, `"rpm"`); also `(unit)`.
fn split_name_unit(raw: &str) -> (String, String) {
    let s = raw.trim();
    for (open, close) in [('[', ']'), ('(', ')')] {
        if s.ends_with(close) {
            if let Some(i) = s.rfind(open) {
                let name = s[..i].trim();
                if !name.is_empty() {
                    return (name.to_string(), s[i + 1..s.len() - 1].trim().to_string());
                }
            }
        }
    }
    (s.to_string(), String::new())
}

fn strip_brackets(s: &str) -> &str {
    let s = s.trim();
    for (open, close) in [('[', ']'), ('(', ')')] {
        if let Some(inner) = s.strip_prefix(open).and_then(|x| x.strip_suffix(close)) {
            return inner.trim();
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(text: &str) -> Log {
        parse(text, &ParseOptions::default()).unwrap()
    }

    #[test]
    fn wide_comma_with_units_in_header() {
        let log = p(
            "Time (s),Engine speed [rpm],Boost (hPa)\n0.0,800,1000\n0.1,900,1010\n0.2,1000,1020\n",
        );
        assert_eq!(log.format, LogFormat::Wide);
        assert_eq!(log.delimiter, ',');
        assert_eq!(log.time_unit, TimeUnit::Seconds);
        assert_eq!(log.time_source, TimeSource::Header);
        assert_eq!(log.channels.len(), 2);
        assert_eq!(log.channels[0].name, "Engine speed");
        assert_eq!(log.channels[0].unit, "rpm");
        assert_eq!(log.channels[1].unit, "hPa");
        assert_eq!(log.t, vec![0.0, 0.1, 0.2]);
    }

    #[test]
    fn semicolon_decimal_comma_and_units_row() {
        let log = p(
            "Zeit;Drehzahl;Luftmasse\ns;1/min;mg/Hub\n0,0;800;12,5\n0,1;820;13,0\n0,2;840;13,5\n",
        );
        assert_eq!(log.delimiter, ';');
        assert!(log.decimal_comma);
        assert!(log.units_row);
        assert_eq!(log.time_unit, TimeUnit::Seconds);
        assert_eq!(log.channels[1].name, "Luftmasse");
        assert_eq!(log.channels[1].unit, "mg/Hub");
        assert_eq!(log.channels[1].values, vec![12.5, 13.0, 13.5]);
    }

    #[test]
    fn tab_delimited_ms_inferred_and_reported() {
        let log = p("t\trpm\n0\t800\n100\t810\n200\t820\n");
        assert_eq!(log.delimiter, '\t');
        assert_eq!(log.time_unit, TimeUnit::Milliseconds);
        assert_eq!(log.time_source, TimeSource::Inferred);
        assert!(log.notes.iter().any(|n| n.contains("inferred")));
        assert!((log.t[2] - 0.2).abs() < 1e-12);
    }

    #[test]
    fn override_time_unit() {
        let opts = ParseOptions {
            time_unit: Some(TimeUnit::Seconds),
            time_column: None,
        };
        let log = parse("t,rpm\n0,800\n100,810\n", &opts).unwrap();
        assert_eq!(log.time_source, TimeSource::Override);
        assert_eq!(log.t, vec![0.0, 100.0]);
    }

    #[test]
    fn time_suffix_in_name() {
        let log = p("time_ms,rpm\n0,800\n20,810\n");
        assert_eq!(log.time_unit, TimeUnit::Milliseconds);
        assert_eq!(log.time_source, TimeSource::Header);
    }

    #[test]
    fn no_header_no_time() {
        let log = p("1,2\n3,4\n5,6\n");
        assert!(!log.header);
        assert_eq!(log.time_unit, TimeUnit::Index);
        assert_eq!(log.channels[0].name, "col1");
        assert_eq!(log.t, vec![0.0, 1.0, 2.0]);
        assert!(log.notes.iter().any(|n| n.contains("no header")));
    }

    #[test]
    fn opentune_live_log_long_format_pivots() {
        let csv = "ts,param_id,name,raw,value,units,note\n\
            1700000000000,P1,Engine speed,3200,800,rpm,\n\
            1700000000000,P2,Coolant,90,50,C,\n\
            1700000000500,P1,Engine speed,3600,900,rpm,\n\
            1700000000500,P2,Coolant,,,C,no response\n";
        let log = p(csv);
        assert_eq!(log.format, LogFormat::OpentuneLong);
        assert_eq!(log.time_source, TimeSource::Format);
        assert_eq!(log.epoch_start, Some(1_700_000_000.0));
        assert_eq!(log.t, vec![0.0, 0.5]);
        let rpm = log.channel("Engine speed").unwrap();
        assert_eq!(rpm.unit, "rpm");
        assert_eq!(rpm.values, vec![800.0, 900.0]);
        let c = log.channel("Coolant").unwrap();
        assert_eq!(c.values[0], 50.0);
        assert!(c.values[1].is_nan());
    }

    #[test]
    fn live_log_text_channels_are_listed_not_plotted() {
        // The `text` column is optional and last: older 7-column logs still parse.
        let csv = "ts,param_id,name,raw,value,units,note,text\n\
            1700000000000,P1,Engine speed,3200,800,rpm,,\n\
            1700000000000,F190,VIN,,,text,,WVWZZZ00000000000\n\
            1700000000500,P1,Engine speed,3600,900,rpm,,\n\
            1700000000500,F190,VIN,,,text,,WVWZZZ00000000000\n";
        let log = p(csv);
        assert_eq!(log.format, LogFormat::OpentuneLong);
        assert_eq!(log.channels.len(), 1);
        assert_eq!(log.channels[0].values, vec![800.0, 900.0]);
        assert!(
            log.notes
                .iter()
                .any(|n| n.contains("VIN = WVWZZZ00000000000")),
            "{:?}",
            log.notes
        );
    }

    #[test]
    fn quoted_fields_preamble_comments_and_text_columns() {
        let csv = "Exported by some tool\n# comment\n\"time [s]\",\"rpm, engine\",gear\n0,800,N\n0.5,900,1\n1.0,1000,2\n";
        let log = p(csv);
        assert_eq!(log.channels[0].name, "rpm, engine");
        assert!(log.notes.iter().any(|n| n.contains("preamble")));
        // gear has one text value: it stays numeric with a NaN.
        assert!(log.channel("gear").unwrap().values[0].is_nan());
    }

    #[test]
    fn backwards_time_is_sorted() {
        let log = p("time [s],a\n0,1\n2,3\n1,2\n");
        assert_eq!(log.t, vec![0.0, 1.0, 2.0]);
        assert_eq!(log.channels[0].values, vec![1.0, 2.0, 3.0]);
        assert!(log.notes.iter().any(|n| n.contains("sorted")));
    }

    #[test]
    fn clock_time_column() {
        let log = p("Time,rpm\n12:00:00.0,800\n12:00:00.5,900\n12:00:01.0,950\n");
        assert_eq!(log.time_unit, TimeUnit::Clock);
        assert_eq!(log.t, vec![43200.0, 43200.5, 43201.0]);
    }

    #[test]
    fn rejects_empty_and_textual() {
        assert!(parse("\n\n", &ParseOptions::default()).is_err());
        assert!(parse("a,b\nx,y\nz,w\n", &ParseOptions::default()).is_err());
    }
}
