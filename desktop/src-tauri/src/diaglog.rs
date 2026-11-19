//! Diagnostics log: a small local record of what went wrong on this machine.
//!
//! `<app data dir>/logs/opentune.log`, one JSON object per line:
//! `{"ts", "level", "src", "msg", "ctx"?}` with `src` one of `rust` (panics,
//! the session log), `js` (uncaught frontend errors), `cmd` (a backend command
//! that returned an error) or `startup` (startup milestones and the session
//! header). Each file is capped at [`MAX_FILE_BYTES`]; a full file rotates to
//! `opentune.log.1` … `.5` ([`ROTATED_FILES`]), so the log never grows past
//! 6 MiB. It is local only: nothing here sends anything anywhere.
//!
//! * **One write path.** Every record goes through [`record`] /
//!   [`write_global`] → [`DiagLog::write`], which bounds and masks it
//!   ([`sanitize`], [`mask`]) before it reaches the file. Records made before
//!   the app data dir is known (the panic hook and the startup marks run
//!   before Tauri's setup) wait in a small in-memory buffer and follow the
//!   session header when [`init`] opens the file.
//! * **Never in the way.** One `write` per record behind a mutex (no
//!   background thread, so a panic record is on disk before the process
//!   dies); a write error turns logging off for the rest of the session and
//!   says so once in the session log ([`DebugLog`]); nothing here panics.
//! * **Masked at write time.** SecurityAccess (service 0x27) seeds and keys in
//!   exchange hex, and hex payloads longer than 64 bytes (cut to their length
//!   and first 16 bytes); see [`mask`]. File contents are never recorded:
//!   frontend records carry a message and a stack, command records the
//!   command name and its error text, never arguments.
//! * **Captured:** Rust panics ([`install_panic_hook`], chained to the
//!   previous hook), uncaught JS errors and command errors ([`diag_log`],
//!   rate-limited; the frontend half is `ui/src/diaglog.js`), every session
//!   log line ([`DebugLog::push`]) and every startup mark
//!   ([`crate::startup::mark`]).
//!
//! Why command errors are logged in the frontend: Tauri 2's generated
//! dispatch (`generate_handler!` → each command's own `__cmd__*` macro)
//! resolves a command's `Result` inside that macro and hands the response to
//! a responder that is crate-private (`InvokeResolver::new` is `pub(crate)`,
//! and `Builder` has no invoke-response hook), so a wrapper around the
//! registered handler only ever sees "handled: true". The one place every
//! command's `Err` does pass is the frontend's `window.__TAURI__.core.invoke`
//! promise, which `diaglog.js` wraps once for every panel and plugin.
//!
//! Reading back (Debug panel card, and a later upload step): [`read_tail`],
//! [`read_since_session`], [`read_all`], [`sessions`] and [`log_dir`].

use crate::state::{BuildChannel, DebugLog};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::cell::Cell;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, Once, OnceLock, TryLockError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The live log file's name.
pub const FILE_NAME: &str = "opentune.log";
/// The log directory under the app data dir.
pub const DIR_NAME: &str = "logs";
/// Size cap of one log file.
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Rotated files kept next to the live one (`opentune.log.1` … `.5`).
pub const ROTATED_FILES: usize = 5;
/// `msg` of the session header record (`src: startup`).
pub const SESSION_MSG: &str = "session start";
/// A record's `msg` is cut to this many characters.
pub const MAX_MSG_CHARS: usize = 2000;
/// Each string inside `ctx` (a stack, a backtrace) is cut to this many characters.
pub const MAX_CTX_STR_CHARS: usize = 4000;
/// A `ctx` larger than this (as JSON) is replaced by `{"truncated": true}`.
const MAX_CTX_BYTES: usize = 12 * 1024;
/// Records kept in memory before the log file is open.
const PENDING_CAP: usize = 256;
/// Hex payloads longer than this many bytes are cut ...
pub const HEX_MAX_BYTES: usize = opentune_core::redact::HEX_MAX_BYTES;
/// ... to their length and this many leading bytes.
pub const HEX_KEEP_BYTES: usize = opentune_core::redact::HEX_KEEP_BYTES;
/// Frontend records (`js` + `cmd`) accepted per second.
pub const FRONTEND_PER_SEC: u32 = 5;

// ---------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------

/// Severity, lowest first (`debug` < `info` < `warn` < `error`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    /// `debug` / `info` / `warn` / `error` (and a few common aliases).
    pub fn parse(s: &str) -> Option<Level> {
        match s.trim().to_ascii_lowercase().as_str() {
            "debug" | "trace" => Some(Level::Debug),
            "info" | "log" => Some(Level::Info),
            "warn" | "warning" => Some(Level::Warn),
            "error" | "fatal" | "panic" => Some(Level::Error),
            _ => None,
        }
    }
}

/// Where a record came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Src {
    /// The Rust backend: panics, session-log lines.
    Rust,
    /// The frontend: uncaught errors and unhandled promise rejections.
    Js,
    /// A backend command that returned an error.
    Cmd,
    /// Startup milestones and the session header.
    Startup,
}

/// One line of the log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// UTC, RFC 3339 with milliseconds (`2026-11-19T15:04:05.123Z`).
    pub ts: String,
    pub level: Level,
    pub src: Src,
    pub msg: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx: Option<Value>,
}

impl Record {
    /// A record stamped with the current time.
    pub fn new(level: Level, src: Src, msg: impl Into<String>, ctx: Option<Value>) -> Record {
        Record {
            ts: ts_from(SystemTime::now()),
            level,
            src,
            msg: msg.into(),
            ctx,
        }
    }

    /// The first record of a session (see [`session_header`]).
    pub fn is_session_header(&self) -> bool {
        self.src == Src::Startup && self.msg == SESSION_MSG
    }
}

/// `t` as UTC RFC 3339 with milliseconds.
pub fn ts_from(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let (y, m, day) = civil_from_days(secs.div_euclid(86_400));
    let sod = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        sod / 3600,
        sod % 3600 / 60,
        sod % 60,
        d.subsec_millis()
    )
}

/// Days since 1970-01-01 → (year, month, day), proleptic Gregorian.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, day)
}

/// What the session header records about this start.
#[derive(Debug, Clone, Default)]
pub struct SessionInfo {
    /// The app's product name.
    pub app: String,
    /// The app's version (an embedder's own version in a plugin build).
    pub version: String,
    /// The build channel name.
    pub channel: String,
    /// WebKitGTK / WebView2 / WKWebView version, when known.
    pub webkit: Option<String>,
    /// The render-environment decision (DMA-BUF on/off and why, what
    /// OpenTune set); see [`crate::startup::render_summary`].
    pub render: Value,
}

/// The session header: the first record of every start.
pub fn session_header(info: &SessionInfo) -> Record {
    Record::new(
        Level::Info,
        Src::Startup,
        SESSION_MSG,
        Some(json!({
            "session": true,
            "app": info.app,
            "version": info.version,
            "core": env!("CARGO_PKG_VERSION"),
            "channel": info.channel,
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "os_release": os_release(),
            "webkit": info.webkit,
            "render": info.render,
        })),
    )
}

/// The distribution's name (`PRETTY_NAME` from `/etc/os-release`).
fn os_release() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let s = fs::read_to_string("/etc/os-release").ok()?;
        s.lines()
            .find_map(|l| l.strip_prefix("PRETTY_NAME="))
            .map(|v| clip(v.trim().trim_matches('"'), 80))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

// ---------------------------------------------------------------------------
// Bounding and masking (applied to every record at write time)
// ---------------------------------------------------------------------------

/// `s` cut to `max` characters (with an ellipsis when cut).
fn clip(s: &str, max: usize) -> String {
    if s.len() <= max || s.chars().count() <= max {
        return s.to_string();
    }
    let mut o: String = s.chars().take(max).collect();
    o.push('…');
    o
}

/// Bound and mask one record: [`mask`] on the message and every string in
/// `ctx`, then the size caps. Called by [`DiagLog::write`] for every record.
pub fn sanitize(mut rec: Record) -> Record {
    rec.msg = clip(&mask(&rec.msg), MAX_MSG_CHARS);
    if let Some(c) = rec.ctx.take() {
        let c = sanitize_value(c, 0);
        let big = serde_json::to_string(&c).map_or(true, |s| s.len() > MAX_CTX_BYTES);
        rec.ctx = Some(if big { json!({ "truncated": true }) } else { c });
    }
    rec
}

fn sanitize_value(v: Value, depth: usize) -> Value {
    const MAX_ITEMS: usize = 32;
    if depth > 4 {
        return Value::String("…".into());
    }
    match v {
        Value::String(s) => Value::String(clip(&mask(&s), MAX_CTX_STR_CHARS)),
        Value::Array(a) => Value::Array(
            a.into_iter()
                .take(MAX_ITEMS)
                .map(|x| sanitize_value(x, depth + 1))
                .collect(),
        ),
        Value::Object(o) => Value::Object(
            o.into_iter()
                .take(MAX_ITEMS)
                .map(|(k, x)| (clip(&k, 64), sanitize_value(x, depth + 1)))
                .collect(),
        ),
        other => other,
    }
}

/// The write-time masker for one text (a message or a `ctx` string):
///
/// * **SecurityAccess** — in hex that is a 0x27 request or a 0x67 response
///   (also behind an ISO-TP PCI or a KWP2000 header), everything after the
///   sub-function (the seed or key) becomes `<masked: N bytes>`;
/// * a hex value right after a `seed` / `key` label is masked the same way;
/// * **long payloads** — hex longer than [`HEX_MAX_BYTES`] bytes keeps its
///   first [`HEX_KEEP_BYTES`] bytes and its length.
///
/// Everything else is left exactly as it was. The diagnostics upload
/// ([`crate::diagupload`]) redacts much more on top of this
/// ([`opentune_core::redact::redact`]: VINs, part numbers, paths, …).
pub fn mask(s: &str) -> String {
    opentune_core::redact::mask_secrets(s)
}

fn encode(rec: &Record) -> String {
    serde_json::to_string(rec).unwrap_or_else(|e| {
        format!(
            "{{\"ts\":\"{}\",\"level\":\"error\",\"src\":\"rust\",\"msg\":\"unencodable record: {}\"}}",
            rec.ts,
            e.to_string().replace(['"', '\\'], "'")
        )
    })
}

// ---------------------------------------------------------------------------
// The writer
// ---------------------------------------------------------------------------

thread_local! {
    /// This thread is inside a write (a panic there must not wait for the lock).
    static BUSY: Cell<bool> = const { Cell::new(false) };
}

struct Sink {
    dir: PathBuf,
    file: Option<File>,
    size: u64,
    /// The encoded session header, rewritten after a clear.
    header: String,
}

enum State {
    /// Not opened yet: records wait here (bounded).
    Pending {
        lines: Vec<String>,
        dropped: usize,
    },
    Open(Sink),
    /// Off for this session (no data dir, or a write failed).
    Off,
}

/// A size-capped, rotating JSON-lines log. The app uses one process-wide
/// instance ([`record`]); tests make their own with small limits.
pub struct DiagLog {
    max_bytes: u64,
    keep: usize,
    state: Mutex<State>,
}

impl Default for DiagLog {
    fn default() -> Self {
        Self::new()
    }
}

/// The path of rotated file `i` (0 = the live file).
pub fn file_path(dir: &Path, i: usize) -> PathBuf {
    if i == 0 {
        dir.join(FILE_NAME)
    } else {
        dir.join(format!("{FILE_NAME}.{i}"))
    }
}

fn rename_if_exists(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

impl DiagLog {
    /// A log with the default caps (1 MiB × (1 + 5 rotated)).
    pub const fn new() -> Self {
        Self::with_limits(MAX_FILE_BYTES, ROTATED_FILES)
    }

    /// A log with its own caps: `max_bytes` per file and `keep` rotated files.
    pub const fn with_limits(max_bytes: u64, keep: usize) -> Self {
        DiagLog {
            max_bytes,
            keep,
            state: Mutex::new(State::Pending {
                lines: Vec::new(),
                dropped: 0,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Open `dir/opentune.log` (creating `dir`), write the session `header`,
    /// then the records made before. On failure the log is off for the
    /// session and the error says why.
    pub fn open(&self, dir: &Path, header: Record) -> Result<PathBuf, String> {
        let header = encode(&sanitize(header));
        let mut st = self.lock();
        let (pending, dropped) = match std::mem::replace(&mut *st, State::Off) {
            State::Pending { lines, dropped } => (lines, dropped),
            _ => (Vec::new(), 0),
        };
        let path = file_path(dir, 0);
        let opened = fs::create_dir_all(dir)
            .and_then(|_| open_append(&path))
            .and_then(|f| Ok((f.metadata()?.len(), f)));
        let (size, file) = opened
            .map_err(|e| format!("diagnostics log off: cannot open {}: {e}", path.display()))?;
        let mut sink = Sink {
            dir: dir.to_path_buf(),
            file: Some(file),
            size,
            header: header.clone(),
        };
        let mut lines = vec![header];
        lines.extend(pending);
        if dropped > 0 {
            lines.push(encode(&Record::new(
                Level::Warn,
                Src::Rust,
                format!("{dropped} early record(s) dropped before the log file was open"),
                None,
            )));
        }
        for l in &lines {
            self.put(&mut sink, l).map_err(|e| {
                format!("diagnostics log off: cannot write {}: {e}", path.display())
            })?;
        }
        *st = State::Open(sink);
        Ok(path)
    }

    /// Turn the log off for this session (pending records are dropped).
    pub fn disable(&self) {
        *self.lock() = State::Off;
    }

    /// Append one line to the sink, rotating first when it would overflow.
    fn put(&self, s: &mut Sink, line: &str) -> io::Result<()> {
        let len = line.len() as u64 + 1;
        if s.size > 0 && s.size + len > self.max_bytes {
            self.rotate(s)?;
        }
        if s.file.is_none() {
            s.file = Some(open_append(&file_path(&s.dir, 0))?);
        }
        let Some(f) = s.file.as_mut() else {
            return Err(io::Error::other("log file closed"));
        };
        let mut buf = Vec::with_capacity(line.len() + 1);
        buf.extend_from_slice(line.as_bytes());
        buf.push(b'\n');
        f.write_all(&buf)?;
        s.size += len;
        Ok(())
    }

    /// opentune.log → .1 → … → .keep (the oldest is deleted).
    fn rotate(&self, s: &mut Sink) -> io::Result<()> {
        s.file = None; // closed before the rename (Windows)
        let live = file_path(&s.dir, 0);
        if self.keep == 0 {
            File::create(&live)?;
        } else {
            match fs::remove_file(file_path(&s.dir, self.keep)) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
            for i in (1..self.keep).rev() {
                rename_if_exists(&file_path(&s.dir, i), &file_path(&s.dir, i + 1))?;
            }
            rename_if_exists(&live, &file_path(&s.dir, 1))?;
        }
        s.file = Some(open_append(&live)?);
        s.size = 0;
        Ok(())
    }

    fn write_locked(&self, st: &mut State, line: String) -> Result<(), String> {
        match st {
            State::Pending { lines, dropped } => {
                if lines.len() < PENDING_CAP {
                    lines.push(line);
                } else {
                    *dropped += 1;
                }
                Ok(())
            }
            State::Off => Ok(()),
            State::Open(sink) => match self.put(sink, &line) {
                Ok(()) => Ok(()),
                Err(e) => {
                    let msg = format!(
                        "diagnostics log stopped for this session: cannot write to {}: {e}",
                        sink.dir.display()
                    );
                    *st = State::Off;
                    Err(msg)
                }
            },
        }
    }

    /// Write one record (masked and bounded by [`sanitize`]). `Err` once, on
    /// the write that failed: the log is off from then on.
    pub fn write(&self, rec: Record) -> Result<(), String> {
        let line = encode(&sanitize(rec));
        BUSY.with(|b| b.set(true));
        let r = {
            let mut st = self.lock();
            self.write_locked(&mut st, line)
        };
        BUSY.with(|b| b.set(false));
        r
    }

    /// The panic path: never waits long for the lock (another thread may be
    /// mid-write) and never waits on itself.
    fn write_from_panic(&self, rec: Record) {
        if BUSY.with(Cell::get) {
            return;
        }
        let line = encode(&sanitize(rec));
        for _ in 0..50 {
            let mut st = match self.state.try_lock() {
                Ok(g) => g,
                Err(TryLockError::Poisoned(p)) => p.into_inner(),
                Err(TryLockError::WouldBlock) => {
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
            };
            let _ = self.write_locked(&mut st, line);
            return;
        }
    }

    /// Empty the log: the rotated files are deleted, the live file starts
    /// over with this session's header.
    pub fn clear(&self) -> Result<(), String> {
        let mut st = self.lock();
        match &mut *st {
            State::Open(s) => {
                s.file = None;
                for i in 1..=self.keep.max(ROTATED_FILES) {
                    let _ = fs::remove_file(file_path(&s.dir, i));
                }
                let live = file_path(&s.dir, 0);
                File::create(&live).map_err(|e| format!("cannot clear {}: {e}", live.display()))?;
                s.size = 0;
                let header = s.header.clone();
                let note = encode(&Record::new(Level::Info, Src::Rust, "log cleared", None));
                for l in [header, note] {
                    self.write_locked(&mut st, l)?;
                }
                Ok(())
            }
            State::Pending { lines, dropped } => {
                lines.clear();
                *dropped = 0;
                Ok(())
            }
            State::Off => Ok(()),
        }
    }

    /// The directory being written to (`None` before [`DiagLog::open`] and
    /// when the log is off).
    pub fn dir(&self) -> Option<PathBuf> {
        match &*self.lock() {
            State::Open(s) => Some(s.dir.clone()),
            _ => None,
        }
    }

    /// `pending`, `on` or `off`.
    pub fn state_name(&self) -> &'static str {
        match &*self.lock() {
            State::Pending { .. } => "pending",
            State::Open(_) => "on",
            State::Off => "off",
        }
    }
}

// ---------------------------------------------------------------------------
// The process-wide log
// ---------------------------------------------------------------------------

static GLOBAL: DiagLog = DiagLog::new();
/// Where a write failure is reported (once): the app's session log.
static NOTE: OnceLock<DebugLog> = OnceLock::new();

/// The process-wide log.
pub fn global() -> &'static DiagLog {
    &GLOBAL
}

/// Write one record to the process-wide log. Every capture (panics,
/// frontend errors, command errors, session-log lines, startup marks) goes
/// through here; an embedder may add its own records the same way.
pub fn record(level: Level, src: Src, msg: impl Into<String>, ctx: Option<Value>) {
    write_global(Record::new(level, src, msg, ctx));
}

/// [`record`] for an already built [`Record`].
pub fn write_global(rec: Record) {
    if BUSY.with(Cell::get) {
        return; // a record made while writing one (never recursive)
    }
    if let Err(e) = GLOBAL.write(rec) {
        match NOTE.get() {
            Some(log) => log.push(e),
            None => eprintln!("{e}"),
        }
    }
}

/// The directory of the process-wide log (`<app data dir>/logs`), once open.
pub fn log_dir() -> Option<PathBuf> {
    GLOBAL.dir()
}

/// Open the process-wide log under the app data dir and write the session
/// header (called from the setup plugin). Without a data dir, or when the
/// file cannot be opened, logging is off for the session and the session
/// log says why.
pub(crate) fn init<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    channel: &BuildChannel,
    debug: &DebugLog,
) {
    use tauri::Manager;
    let _ = NOTE.set(debug.clone());
    let info = SessionInfo {
        app: app.package_info().name.clone(),
        version: app.package_info().version.to_string(),
        channel: channel.as_str().to_string(),
        webkit: tauri::webview_version().ok(),
        render: crate::startup::render_summary(),
    };
    let res = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("diagnostics log off: no app data directory ({e})"))
        .and_then(|d| GLOBAL.open(&d.join(DIR_NAME), session_header(&info)));
    if let Err(e) = res {
        GLOBAL.disable();
        debug.push(e);
    }
}

// ---------------------------------------------------------------------------
// Panic hook
// ---------------------------------------------------------------------------

type PanicHook = Box<dyn Fn(&PanicHookInfo<'_>) + Sync + Send + 'static>;

fn backtrace_wanted() -> bool {
    std::env::var("RUST_BACKTRACE").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// The record for one panic: the payload, where it happened, the thread, and
/// a backtrace when `backtrace` (`RUST_BACKTRACE` is set).
pub fn panic_record(info: &PanicHookInfo<'_>, backtrace: bool) -> Record {
    let p = info.payload();
    let what = p
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "(non-text panic payload)".into());
    let mut ctx = Map::new();
    if let Some(l) = info.location() {
        ctx.insert(
            "location".into(),
            json!(format!("{}:{}:{}", l.file(), l.line(), l.column())),
        );
    }
    ctx.insert(
        "thread".into(),
        json!(std::thread::current().name().unwrap_or("unnamed")),
    );
    if backtrace {
        ctx.insert(
            "backtrace".into(),
            json!(std::backtrace::Backtrace::force_capture().to_string()),
        );
    }
    Record::new(
        Level::Error,
        Src::Rust,
        format!("panic: {what}"),
        Some(Value::Object(ctx)),
    )
}

/// A panic hook that writes the panic to `log`, then runs `prev` (the hook
/// that was installed before, normally the default one printing to stderr).
pub fn chain_panic_hook(log: &'static DiagLog, prev: PanicHook) -> PanicHook {
    Box::new(move |info: &PanicHookInfo<'_>| {
        log.write_from_panic(panic_record(info, backtrace_wanted()));
        prev(info);
    })
}

/// Install the panic hook for the process-wide log (once; keeps the
/// previous hook).
pub fn install_panic_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(chain_panic_hook(&GLOBAL, prev));
    });
}

// ---------------------------------------------------------------------------
// Frontend records (rate-limited)
// ---------------------------------------------------------------------------

/// At most `per_sec` records per one-second window; the rest are counted and
/// reported with the next accepted record.
#[derive(Debug)]
pub struct RateLimiter {
    per_sec: u32,
    window: Option<Instant>,
    count: u32,
    dropped: u32,
}

impl RateLimiter {
    pub const fn new(per_sec: u32) -> Self {
        RateLimiter {
            per_sec,
            window: None,
            count: 0,
            dropped: 0,
        }
    }

    /// `Some(dropped since the last accepted record)` when a record may be
    /// written at `now`, `None` when it is over the limit.
    pub fn check(&mut self, now: Instant) -> Option<u32> {
        let fresh = self
            .window
            .is_none_or(|w| now.saturating_duration_since(w) >= Duration::from_secs(1));
        if fresh {
            self.window = Some(now);
            self.count = 0;
        }
        if self.count < self.per_sec {
            self.count += 1;
            Some(std::mem::take(&mut self.dropped))
        } else {
            self.dropped += 1;
            None
        }
    }
}

static FRONTEND_LIMIT: Mutex<RateLimiter> = Mutex::new(RateLimiter::new(FRONTEND_PER_SEC));

/// The records for one frontend report: `src` is `js` or `cmd` (anything
/// else counts as `js`; the frontend cannot write `rust` / `startup`
/// records), `level` defaults to `error`. Empty when over the rate limit;
/// a "dropped" note comes first when earlier reports were dropped.
pub fn accept_frontend(
    limiter: &Mutex<RateLimiter>,
    now: Instant,
    level: Option<&str>,
    src: Option<&str>,
    msg: &str,
    ctx: Option<Value>,
) -> Vec<Record> {
    let dropped = {
        let mut l = limiter.lock().unwrap_or_else(|p| p.into_inner());
        match l.check(now) {
            Some(d) => d,
            None => return Vec::new(),
        }
    };
    let mut out = Vec::new();
    if dropped > 0 {
        out.push(Record::new(
            Level::Warn,
            Src::Js,
            format!("{dropped} frontend record(s) dropped (over {FRONTEND_PER_SEC} per second)"),
            None,
        ));
    }
    let src = match src {
        Some("cmd") => Src::Cmd,
        _ => Src::Js,
    };
    let level = level.and_then(Level::parse).unwrap_or(Level::Error);
    out.push(Record::new(level, src, clip(msg, MAX_MSG_CHARS), ctx));
    out
}

// ---------------------------------------------------------------------------
// Reading back
// ---------------------------------------------------------------------------

/// The log files in `dir`, oldest first (`.N` … `.1`, then the live file).
pub fn log_files(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = (1..=99)
        .map(|i| file_path(dir, i))
        .take_while(|p| p.exists())
        .collect();
    out.reverse();
    let live = file_path(dir, 0);
    if live.exists() {
        out.push(live);
    }
    out
}

/// The records of one file (lines that do not parse are skipped).
fn read_file(path: &Path) -> Vec<Record> {
    let Ok(f) = File::open(path) else {
        return Vec::new();
    };
    BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect()
}

/// Every record in `dir`, oldest first (at most 6 MiB of lines).
pub fn read_all(dir: &Path) -> Vec<Record> {
    log_files(dir).iter().flat_map(|p| read_file(p)).collect()
}

/// The last `n` records at `min` level or above, oldest first.
pub fn read_tail(dir: &Path, n: usize, min: Level) -> Vec<Record> {
    let mut out: Vec<Record> = Vec::new();
    for p in log_files(dir).iter().rev() {
        let recs: Vec<Record> = read_file(p)
            .into_iter()
            .filter(|r| r.level >= min)
            .collect();
        let need = n.saturating_sub(out.len());
        let skip = recs.len().saturating_sub(need);
        out.splice(0..0, recs.into_iter().skip(skip));
        if out.len() >= n {
            break;
        }
    }
    out
}

/// The records of the latest session: from the last session header to the
/// end (everything, when no header is left after rotation).
pub fn read_since_session(dir: &Path) -> Vec<Record> {
    let mut out: Vec<Record> = Vec::new();
    for p in log_files(dir).iter().rev() {
        let recs = read_file(p);
        if let Some(i) = recs.iter().rposition(Record::is_session_header) {
            out.splice(0..0, recs.into_iter().skip(i));
            return out;
        }
        out.splice(0..0, recs);
    }
    out
}

/// `records` split into sessions, each starting at its header (records
/// before the first header form a session of their own).
pub fn sessions(records: &[Record]) -> Vec<&[Record]> {
    let mut starts: Vec<usize> = records
        .iter()
        .enumerate()
        .filter(|(_, r)| r.is_session_header())
        .map(|(i, _)| i)
        .collect();
    if starts.first() != Some(&0) {
        starts.insert(0, 0);
    }
    starts
        .iter()
        .enumerate()
        .map(|(k, &s)| &records[s..starts.get(k + 1).copied().unwrap_or(records.len())])
        .filter(|s| !s.is_empty())
        .collect()
}

// ---------------------------------------------------------------------------
// Commands (every channel: the log is local and public)
// ---------------------------------------------------------------------------

/// One frontend report (`ui/src/diaglog.js`): an uncaught error, an
/// unhandled rejection or a command that failed. Rate-limited to
/// [`FRONTEND_PER_SEC`] per second.
#[tauri::command]
pub async fn diag_log(level: Option<String>, src: Option<String>, msg: String, ctx: Option<Value>) {
    for r in accept_frontend(
        &FRONTEND_LIMIT,
        Instant::now(),
        level.as_deref(),
        src.as_deref(),
        &msg,
        ctx,
    ) {
        write_global(r);
    }
}

/// The last `limit` records (default 100, at most 2000) at `level` or
/// above (default: all), oldest first.
#[tauri::command]
pub async fn diag_log_tail(limit: Option<usize>, level: Option<String>) -> Vec<Record> {
    let Some(dir) = log_dir() else {
        return Vec::new();
    };
    let min = level
        .as_deref()
        .and_then(Level::parse)
        .unwrap_or(Level::Debug);
    read_tail(&dir, limit.unwrap_or(100).min(2000), min)
}

/// Where the log lives and how big it is.
#[tauri::command]
pub async fn diag_log_info() -> Value {
    let dir = log_dir();
    let files = dir.as_deref().map(log_files).unwrap_or_default();
    let bytes: u64 = files
        .iter()
        .filter_map(|p| fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    json!({
        "state": GLOBAL.state_name(),
        "dir": dir.as_ref().map(|d| d.display().to_string()),
        "file": dir.as_ref().map(|d| file_path(d, 0).display().to_string()),
        "files": files.len(),
        "bytes": bytes,
        "max_file_bytes": MAX_FILE_BYTES,
        "rotated_files": ROTATED_FILES,
    })
}

/// Open the log folder in the system file manager; returns its path.
#[tauri::command]
pub async fn diag_log_open_folder() -> Result<String, String> {
    let dir = log_dir().ok_or("the diagnostics log is off for this session")?;
    #[cfg(target_os = "windows")]
    let mut c = std::process::Command::new("explorer");
    #[cfg(target_os = "macos")]
    let mut c = std::process::Command::new("open");
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let mut c = std::process::Command::new("xdg-open");
    use std::process::Stdio;
    let mut child = c
        .arg(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot open {}: {e}", dir.display()))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(dir.display().to_string())
}

/// Delete the log's contents (the live file restarts with this session's
/// header).
#[tauri::command]
pub async fn diag_log_clear() -> Result<(), String> {
    GLOBAL.clear()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::hexs;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn tmp(tag: &str) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("ot-diaglog-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn header() -> Record {
        session_header(&SessionInfo {
            app: "OpenTune".into(),
            version: "0.0.0-test".into(),
            channel: "alpha".into(),
            webkit: Some("2.46.0".into()),
            render: json!({ "dmabuf": { "enabled": true } }),
        })
    }

    fn info(msg: &str) -> Record {
        Record::new(Level::Info, Src::Rust, msg, None)
    }

    #[test]
    fn timestamps_are_utc_rfc3339() {
        assert_eq!(ts_from(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        let t = UNIX_EPOCH + Duration::from_millis(1_700_000_000_123);
        assert_eq!(ts_from(t), "2023-11-14T22:13:20.123Z");
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400); // 2000-02-29
        assert!(ts_from(leap).starts_with("2000-02-29T00:00:00"));
    }

    #[test]
    fn levels_order_and_parse() {
        assert!(Level::Debug < Level::Info && Level::Warn < Level::Error);
        assert_eq!(Level::parse(" WARNING "), Some(Level::Warn));
        assert_eq!(Level::parse("nope"), None);
        let r = info("x");
        let line = encode(&r);
        assert!(line.contains(r#""level":"info""#) && line.contains(r#""src":"rust""#));
        assert!(!line.contains("ctx"), "no ctx key when there is none");
    }

    #[test]
    fn open_writes_header_then_pending_records() {
        let d = tmp("open");
        let log = DiagLog::new();
        log.write(info("before open")).unwrap();
        assert_eq!(log.state_name(), "pending");
        let path = log.open(&d, header()).unwrap();
        assert_eq!(path, d.join(FILE_NAME));
        log.write(info("after open")).unwrap();
        let recs = read_all(&d);
        assert!(recs[0].is_session_header());
        let ctx = recs[0].ctx.as_ref().unwrap();
        assert_eq!(ctx["channel"], "alpha");
        assert_eq!(ctx["os"], std::env::consts::OS);
        assert_eq!(ctx["webkit"], "2.46.0");
        assert_eq!(ctx["render"]["dmabuf"]["enabled"], true);
        let msgs: Vec<&str> = recs.iter().map(|r| r.msg.as_str()).collect();
        assert_eq!(msgs[1..], ["before open", "after open"]);
        assert_eq!(log.dir(), Some(d.clone()));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn pending_buffer_is_bounded() {
        let d = tmp("pending");
        let log = DiagLog::new();
        for i in 0..PENDING_CAP + 10 {
            log.write(info(&format!("early {i}"))).unwrap();
        }
        log.open(&d, header()).unwrap();
        let recs = read_all(&d);
        assert_eq!(recs.len(), 1 + PENDING_CAP + 1);
        assert!(recs
            .last()
            .unwrap()
            .msg
            .starts_with("10 early record(s) dropped"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn rotation_caps_file_size_and_count() {
        let d = tmp("rotate");
        let log = DiagLog::with_limits(1024, 2);
        log.open(&d, header()).unwrap();
        for i in 0..200 {
            log.write(info(&format!(
                "record number {i:04} with some padding text"
            )))
            .unwrap();
        }
        let files = log_files(&d);
        assert_eq!(files.len(), 3, "live + 2 rotated: {files:?}");
        assert!(!file_path(&d, 3).exists(), "the oldest is deleted");
        for f in &files {
            let n = fs::metadata(f).unwrap().len();
            assert!(n <= 1024, "{} is {n} bytes", f.display());
        }
        // Oldest first across the files, ending with the newest record.
        let all = read_all(&d);
        let nums: Vec<usize> = all
            .iter()
            .filter_map(|r| r.msg.strip_prefix("record number "))
            .map(|s| s[..4].parse().unwrap())
            .collect();
        assert!(
            nums.windows(2).all(|w| w[0] + 1 == w[1]),
            "contiguous: {nums:?}"
        );
        assert_eq!(*nums.last().unwrap(), 199);
        // Tail across a rotation boundary.
        assert!(
            all.len() > 15 && all.len() < 30,
            "{} records kept",
            all.len()
        );
        let tail = read_tail(&d, 15, Level::Debug);
        assert_eq!(tail.len(), 15);
        assert_eq!(tail, all[all.len() - 15..]);
        assert_eq!(read_tail(&d, 1000, Level::Debug).len(), all.len());
        assert!(tail.last().unwrap().msg.starts_with("record number 0199"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn default_caps_are_one_mib_times_six() {
        assert_eq!(MAX_FILE_BYTES, 1 << 20);
        assert_eq!(ROTATED_FILES, 5);
        let d = tmp("bigline");
        let log = DiagLog::new();
        log.open(&d, header()).unwrap();
        // A huge message is cut before it is written.
        log.write(info(&"x".repeat(100_000))).unwrap();
        let rec = read_all(&d).pop().unwrap();
        assert_eq!(rec.msg.chars().count(), MAX_MSG_CHARS + 1);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn tail_filters_by_level_and_sessions_split() {
        let d = tmp("tail");
        let log = DiagLog::new();
        log.open(&d, header()).unwrap();
        log.write(Record::new(Level::Error, Src::Cmd, "old error", None))
            .unwrap();
        log.write(info("noise")).unwrap();
        // A second start: a new header in the same file.
        log.write(header()).unwrap();
        log.write(Record::new(Level::Warn, Src::Js, "new warning", None))
            .unwrap();
        log.write(Record::new(Level::Debug, Src::Rust, "detail", None))
            .unwrap();
        let errs = read_tail(&d, 10, Level::Warn);
        let m: Vec<&str> = errs.iter().map(|r| r.msg.as_str()).collect();
        assert_eq!(m, ["old error", "new warning"]);
        let since = read_since_session(&d);
        assert!(since[0].is_session_header());
        assert_eq!(since.len(), 3);
        let all = read_all(&d);
        let s = sessions(&all);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].len(), 3);
        assert!(s[1][0].is_session_header());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn clear_keeps_the_session_header() {
        let d = tmp("clear");
        let log = DiagLog::with_limits(512, 2);
        log.open(&d, header()).unwrap();
        for i in 0..50 {
            log.write(info(&format!("line {i} ........................")))
                .unwrap();
        }
        assert!(log_files(&d).len() > 1);
        log.clear().unwrap();
        assert_eq!(log_files(&d).len(), 1);
        let recs = read_all(&d);
        assert_eq!(recs.len(), 2);
        assert!(recs[0].is_session_header());
        assert_eq!(recs[1].msg, "log cleared");
        log.write(info("after clear")).unwrap();
        assert_eq!(read_all(&d).len(), 3);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_write_failure_stops_logging_once() {
        let d = tmp("fail");
        let log = DiagLog::with_limits(256, 1);
        log.open(&d, header()).unwrap();
        fs::remove_dir_all(&d).unwrap();
        // The next rotation cannot reopen the file: one error, then silence.
        let mut errs = 0;
        for i in 0..20 {
            if log
                .write(info(&format!("{i} ......................................")))
                .is_err()
            {
                errs += 1;
            }
        }
        assert_eq!(errs, 1);
        assert_eq!(log.state_name(), "off");
        assert!(log.dir().is_none());
    }

    #[test]
    fn open_failure_reports_why() {
        let d = tmp("nodir");
        fs::create_dir_all(d.parent().unwrap()).unwrap();
        fs::write(&d, b"a file, not a directory").unwrap();
        let log = DiagLog::new();
        let e = log.open(&d.join("logs"), header()).unwrap_err();
        assert!(e.contains("diagnostics log off"), "{e}");
        assert_eq!(log.state_name(), "off");
        let _ = fs::remove_file(&d);
    }

    // ---- maskers ------------------------------------------------------------

    #[test]
    fn mask_security_access_seed_and_key() {
        // Seed response and key request, as the session log writes them.
        assert_eq!(
            mask("[7E0] -> 27 01  <- 67 01 1A 2B 3C 4D  (12 ms)"),
            "[7E0] -> 27 01  <- 67 01 <masked: 4 bytes>  (12 ms)"
        );
        assert_eq!(
            mask("sim -> 27 02 11 22 33 44  <- 67 02"),
            "sim -> 27 02 <masked: 4 bytes>  <- 67 02"
        );
        // Lower case and contiguous hex too.
        assert_eq!(mask("tx 670112345678"), "tx 67 01 <masked: 4 bytes>");
        // Behind an ISO-TP single-frame PCI and a KWP2000 header.
        assert_eq!(mask("06 27 02 aa bb cc dd"), "06 27 02 <masked: 4 bytes>");
        assert_eq!(
            mask("80 10 F1 06 27 02 AA BB CC DD"),
            "80 10 F1 06 27 02 <masked: 4 bytes>"
        );
        // A labelled seed / key in a message.
        assert_eq!(mask("got seed: 0x1A2B3C4D"), "got seed: <masked: 4 bytes>");
        assert_eq!(mask("key=DEADBEEF ok"), "key=<masked: 4 bytes> ok");
        // Nothing secret: left exactly as is.
        for s in [
            "27 01",
            "-> 7F 27 35",
            "[7E0] -> 22 F1 90  <- 62 F1 90 57 56 57",
            "took 27 ms",
            "decade of data",
        ] {
            assert_eq!(mask(s), s);
        }
    }

    #[test]
    fn mask_truncates_long_payloads() {
        let long: Vec<u8> = (0..100u8).collect();
        let s = format!("<- {}  (40 ms)", hexs(&long));
        assert_eq!(
            mask(&s),
            "<- 00 01 02 03 04 05 06 07 08 09 0A 0B 0C 0D 0E 0F … <100 bytes>  (40 ms)"
        );
        let exactly: Vec<u8> = vec![0x36; HEX_MAX_BYTES];
        let s = hexs(&exactly);
        assert_eq!(mask(&s), s, "64 bytes stay");
        // Contiguous hex as well.
        let blob = "AB".repeat(200);
        assert_eq!(mask(&blob), format!("{} … <200 bytes>", hexs(&[0xAB; 16])));
        // Non-ASCII text around a run is kept intact.
        assert_eq!(mask("ø -> 67 01 AA BB ø"), "ø -> 67 01 <masked: 2 bytes> ø");
    }

    #[test]
    fn records_are_masked_at_write_time() {
        let d = tmp("mask");
        let log = DiagLog::new();
        log.open(&d, header()).unwrap();
        log.write(Record::new(
            Level::Error,
            Src::Cmd,
            "coding_write failed: -> 27 02 11 22 33 44",
            Some(json!({ "stack": "<- 67 01 99 88 77 66", "n": 3 })),
        ))
        .unwrap();
        let text = fs::read_to_string(d.join(FILE_NAME)).unwrap();
        for secret in ["11 22 33 44", "99 88 77 66"] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
        let r = read_all(&d).pop().unwrap();
        assert_eq!(r.ctx.unwrap()["n"], 3);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn oversized_ctx_is_bounded() {
        let big: Vec<Value> = (0..100)
            .map(|i| json!(format!("{i}{}", "y".repeat(100))))
            .collect();
        let r = sanitize(Record::new(
            Level::Error,
            Src::Js,
            "m",
            Some(json!({ "a": big })),
        ));
        assert_eq!(r.ctx.unwrap()["a"].as_array().unwrap().len(), 32);
        let huge = json!({ "stack": "z".repeat(50_000) });
        let r = sanitize(Record::new(Level::Error, Src::Js, "m", Some(huge)));
        let s = r.ctx.unwrap()["stack"].as_str().unwrap().chars().count();
        assert_eq!(s, MAX_CTX_STR_CHARS + 1);
        let deep = json!({"a":{"b":{"c":{"d":{"e":{"f":"g"}}}}}});
        let r = sanitize(Record::new(Level::Info, Src::Js, "m", Some(deep)));
        assert_eq!(r.ctx.unwrap()["a"]["b"]["c"]["d"]["e"], "…");
        let wide: Map<String, Value> = (0..32)
            .map(|i| (format!("k{i}"), json!("v".repeat(1000))))
            .collect();
        let r = sanitize(Record::new(
            Level::Info,
            Src::Js,
            "m",
            Some(Value::Object(wide)),
        ));
        assert_eq!(r.ctx.unwrap(), json!({ "truncated": true }));
    }

    // ---- frontend records -----------------------------------------------------

    #[test]
    fn rate_limiter_caps_per_second_and_counts_drops() {
        let mut l = RateLimiter::new(3);
        let t0 = Instant::now();
        assert_eq!(l.check(t0), Some(0));
        assert_eq!(l.check(t0), Some(0));
        assert_eq!(l.check(t0 + Duration::from_millis(500)), Some(0));
        assert_eq!(l.check(t0 + Duration::from_millis(600)), None);
        assert_eq!(l.check(t0 + Duration::from_millis(900)), None);
        // A new window: accepted, and it reports the two dropped.
        assert_eq!(l.check(t0 + Duration::from_millis(1000)), Some(2));
        assert_eq!(l.check(t0 + Duration::from_millis(1001)), Some(0));
    }

    #[test]
    fn frontend_records_are_limited_and_cannot_claim_rust() {
        let lim = Mutex::new(RateLimiter::new(FRONTEND_PER_SEC));
        let t0 = Instant::now();
        let mut written = 0;
        for i in 0..20 {
            written +=
                accept_frontend(&lim, t0, Some("error"), Some("js"), &format!("e{i}"), None).len();
        }
        assert_eq!(written, FRONTEND_PER_SEC as usize);
        let next = accept_frontend(
            &lim,
            t0 + Duration::from_secs(1),
            None,
            Some("rust"),
            &"m".repeat(5000),
            Some(json!({ "stack": "at x" })),
        );
        assert_eq!(next.len(), 2);
        assert!(next[0].msg.starts_with("15 frontend record(s) dropped"));
        assert_eq!(next[1].src, Src::Js, "a frontend report is never `rust`");
        assert_eq!(next[1].level, Level::Error);
        assert_eq!(next[1].msg.chars().count(), MAX_MSG_CHARS + 1);
        let cmd = accept_frontend(
            &lim,
            t0 + Duration::from_secs(2),
            Some("warn"),
            Some("cmd"),
            "image_info failed: no such file",
            Some(json!({ "cmd": "image_info" })),
        );
        assert_eq!(cmd[0].src, Src::Cmd);
        assert_eq!(cmd[0].level, Level::Warn);
    }

    // ---- panic hook -------------------------------------------------------------

    #[test]
    fn panic_hook_writes_a_record_and_chains() {
        static LOG: DiagLog = DiagLog::new();
        static CHAINED: AtomicBool = AtomicBool::new(false);
        let d = tmp("panic");
        LOG.open(&d, header()).unwrap();
        let prev = std::panic::take_hook();
        std::panic::set_hook(chain_panic_hook(
            &LOG,
            Box::new(|_: &PanicHookInfo<'_>| CHAINED.store(true, Ordering::SeqCst)),
        ));
        let r = std::panic::catch_unwind(|| panic!("diaglog test boom {}", 42));
        std::panic::set_hook(prev);
        assert!(r.is_err());
        assert!(CHAINED.load(Ordering::SeqCst), "the previous hook ran");
        let rec = read_all(&d)
            .into_iter()
            .find(|r| r.msg == "panic: diaglog test boom 42")
            .expect("panic record");
        assert_eq!(rec.level, Level::Error);
        assert_eq!(rec.src, Src::Rust);
        let ctx = rec.ctx.unwrap();
        assert!(
            ctx["location"].as_str().unwrap().contains("diaglog.rs"),
            "{ctx}"
        );
        assert!(ctx.get("thread").is_some());
        let _ = fs::remove_dir_all(&d);
    }
}
