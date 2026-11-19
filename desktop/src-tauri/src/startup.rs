//! Startup: timing trace, platform environment defaults and the data cache.
//!
//! Three small pieces that keep the window coming up fast and make a slow start
//! measurable on the user's own machine (see docs/TROUBLESHOOTING-STARTUP.md):
//!
//! * **Trace** — `OPENTUNE_TRACE_STARTUP=1` prints a timing line to stderr for
//!   every startup milestone: time the process spent before `main` (dynamic
//!   linking, AppImage mount), the setup plugin, data-root resolution, window /
//!   webview creation, page load, the first IPC calls (with how long each sync
//!   command held the UI thread), the data loads and the frontend's own marks
//!   (`startup_mark`). Off by default; when off every hook is a relaxed atomic
//!   load.
//! * **Platform environment** — [`apply_platform_env`] decides the WebKitGTK
//!   DMA-BUF renderer on Linux before GTK/WebKit initialise: GPU rendering
//!   stays on, and `WEBKIT_DISABLE_DMABUF_RENDERER=1` is set only on a
//!   detected known-bad setup (no usable render node, NVIDIA proprietary
//!   driver, virtual GPU, forced software GL, remote X) or after a start that
//!   never drew (self-heal). User overrides always win; see [`crate::dmabuf`].
//!   `WEBKIT_DISABLE_COMPOSITING_MODE` is never set.
//! * **Render environment** — [`render_env`] reports the whitelisted
//!   WebKit/GTK variables and the DMA-BUF decision for the Debug panel's
//!   Render check; [`render_note`] logs its results (`OPENTUNE_PERF=1`).
//! * **Data cache** — the ECU definitions (the MS4x defs alone carry ~2,900
//!   maps) and the vehicle catalog are parsed once, kept behind an `Arc`, and
//!   re-parsed only when a file in the directory changes (name, size or mtime),
//!   so edits still show up without a restart. [`warm`] fills the cache on a
//!   background thread at startup, so the first Maps/Identify call finds it
//!   ready instead of parsing on demand.

use opentune_core::{
    catalog::{self, CatalogEntry},
    ecudef::{self, EcuDef},
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Instant, SystemTime};

// ---------------------------------------------------------------------------
// Trace
// ---------------------------------------------------------------------------

/// Environment variable that turns the startup trace on (`1`/`true`/`yes`).
pub const TRACE_ENV: &str = "OPENTUNE_TRACE_STARTUP";

/// Opt-out: leave every WebKit/GTK environment variable exactly as inherited.
pub const KEEP_ENV: &str = "OPENTUNE_KEEP_WEBKIT_ENV";

static T0: OnceLock<Instant> = OnceLock::new();
static ENABLED: OnceLock<bool> = OnceLock::new();
/// IPC calls still traced (the first N; later calls are not startup).
static IPC_LEFT: AtomicU32 = AtomicU32::new(40);
static WINDOW_EVENT_SEEN: AtomicBool = AtomicBool::new(false);

fn truthy(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Is the startup trace on? Read once from [`TRACE_ENV`].
pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| {
        std::env::var(TRACE_ENV)
            .map(|v| truthy(&v))
            .unwrap_or(false)
    })
}

/// Start the startup clock (idempotent; the first caller wins). Called from
/// [`crate::run`] / [`crate::builder_with`] so an embedder gets it too.
pub fn begin() {
    let first = T0.set(Instant::now()).is_ok();
    if first && enabled() {
        match pre_main_ms() {
            Some(ms) => eprintln!(
                "[startup      0.0 ms] trace on (process was alive {ms:.0} ms before this point: \
                 dynamic linking, AppImage mount, static init)"
            ),
            None => eprintln!("[startup      0.0 ms] trace on"),
        }
    }
}

/// Milliseconds since [`begin`].
pub fn elapsed_ms() -> f64 {
    T0.get_or_init(Instant::now).elapsed().as_secs_f64() * 1000.0
}

/// One startup milestone: always recorded in the diagnostics log
/// ([`crate::diaglog`]), printed to stderr when the trace is on.
pub fn mark(label: &str) {
    let ms = elapsed_ms();
    crate::diaglog::record(
        crate::diaglog::Level::Info,
        crate::diaglog::Src::Startup,
        label,
        Some(serde_json::json!({ "ms": (ms * 10.0).round() / 10.0 })),
    );
    if enabled() {
        eprintln!("[startup {ms:8.1} ms] {label}");
    }
}

/// Time `f` and [`mark`] `label (n ms)`.
pub fn timed<T>(label: &str, f: impl FnOnce() -> T) -> T {
    let t = Instant::now();
    let out = f();
    mark(&format!(
        "{label} ({:.1} ms)",
        t.elapsed().as_secs_f64() * 1000.0
    ));
    out
}

/// How long the process ran before [`begin`], from `/proc` (Linux only; the
/// kernel reports process start in clock ticks, normally 100 Hz → 10 ms
/// resolution).
fn pre_main_ms() -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
        // Field 22 (starttime). Skip past the parenthesised command name,
        // which may itself contain spaces.
        let rest = &stat[stat.rfind(')')? + 2..];
        let start_ticks: f64 = rest.split_whitespace().nth(19)?.parse().ok()?;
        let uptime: f64 = std::fs::read_to_string("/proc/uptime")
            .ok()?
            .split_whitespace()
            .next()?
            .parse()
            .ok()?;
        let ms = uptime * 1000.0 - start_ticks * 10.0;
        (ms >= 0.0).then_some(ms)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Trace an IPC call (the first 40 calls only). `run` dispatches it; for a
/// sync command that is the whole command, executed on the UI thread, so its
/// duration is time the window could not paint.
pub fn ipc<T>(cmd: &str, run: impl FnOnce() -> T) -> T {
    if !enabled() || IPC_LEFT.load(Ordering::Relaxed) == 0 {
        return run();
    }
    IPC_LEFT.fetch_sub(1, Ordering::Relaxed);
    let t = Instant::now();
    let out = run();
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    mark(&format!(
        "ipc {cmd} (dispatch held the UI thread {ms:.1} ms)"
    ));
    out
}

/// Trace the first window event (the window exists and the event loop is
/// delivering events to it).
pub fn window_event(what: &str) {
    if enabled() && !WINDOW_EVENT_SEEN.swap(true, Ordering::Relaxed) {
        mark(&format!("first window event: {what}"));
    }
}

/// Frontend milestone (`invoke("startup_mark", { label })`). Printed only
/// when the trace is on; async so it never queues behind the UI thread. The
/// "first frame painted" mark also clears the DMA-BUF self-heal marker (the
/// start drew, see [`crate::dmabuf`]).
#[tauri::command]
pub async fn startup_mark(label: String) {
    if label.starts_with("first frame painted") {
        frame_painted();
    }
    mark(&format!("ui: {label}"));
}

static PAINTED: AtomicBool = AtomicBool::new(false);

/// The first frame was painted: record a clean start for the self-heal (once).
pub fn frame_painted() {
    if PAINTED.swap(true, Ordering::Relaxed) {
        return;
    }
    if let Some(dir) = HEAL_DIR.get() {
        if let Err(e) = crate::dmabuf::painted(dir) {
            mark(&format!(
                "dmabuf self-heal: could not clear the marker: {e}"
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// Platform environment
// ---------------------------------------------------------------------------

/// The DMA-BUF decision of this process (for [`render_env`] and the trace).
static DECISION: OnceLock<crate::dmabuf::Decision> = OnceLock::new();
/// The self-heal directory, when this start uses it (auto mode).
static HEAL_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Decide the WebKitGTK environment before GTK/WebKit initialise. Must run
/// before the Tauri app is built (no webview exists yet, and no other thread
/// is reading the environment). Returns what was set, for the trace.
///
/// WebKitGTK ≥ 2.42 renders through DMA-BUF/GBM buffers by default; that is
/// the fast path and stays on. On setups where it is known to stall or draw a
/// blank window (see [`crate::dmabuf`] for the rules and the self-heal),
/// `WEBKIT_DISABLE_DMABUF_RENDERER=1` selects the shared-memory path. A value
/// the user set is never touched, `OPENTUNE_DMABUF=on|off` forces it, and
/// `OPENTUNE_KEEP_WEBKIT_ENV=1` skips this entirely.
pub fn apply_platform_env() -> Vec<String> {
    use crate::dmabuf;
    let mut set = Vec::new();
    if !cfg!(target_os = "linux") {
        let _ = DECISION.set(dmabuf::Decision::not_linux());
        return set;
    }
    let get = |k: &str| std::env::var(k).ok();
    let o = dmabuf::Overrides::from_env(&get);
    let probe = dmabuf::probe_system(&get);
    let auto = !o.keep_env && o.user_var.is_none() && o.knob == dmabuf::Knob::Auto;
    let dir = if auto { dmabuf::state_dir(&get) } else { None };
    let (persisted, marker) = match dir.as_deref() {
        Some(d) => (dmabuf::read_state(d), dmabuf::read_marker(d)),
        None => (dmabuf::Persist::default(), None),
    };
    let plan = dmabuf::decide(&o, &probe, persisted, marker);
    if plan.decision.set_env {
        std::env::set_var(dmabuf::DMABUF_ENV, "1");
        std::env::set_var(dmabuf::SET_BY_US_ENV, "1");
        set.push(format!("{}=1", dmabuf::DMABUF_ENV));
    } else if !o.keep_env && o.user_var.is_none() {
        // Inherited from an OpenTune parent that set it: this start decided on.
        std::env::remove_var(dmabuf::DMABUF_ENV);
        std::env::remove_var(dmabuf::SET_BY_US_ENV);
    }
    if let (Some(d), Some(st), Some(m)) = (dir, plan.persist, plan.marker) {
        match dmabuf::write_start(&d, st, m) {
            Ok(()) => {
                let _ = HEAL_DIR.set(d);
            }
            Err(e) => mark(&format!(
                "dmabuf self-heal off: cannot write {}: {e}",
                d.display()
            )),
        }
    }
    let _ = SET_BY_US.set(set.clone());
    let d = &plan.decision;
    if let Some(n) = &d.notice {
        mark(&format!("dmabuf self-heal: {n}"));
    }
    if let Some(k) = &o.knob_unknown {
        mark(&format!(
            "{}={k} not understood (auto|on|off): using auto",
            dmabuf::KNOB_ENV
        ));
    }
    if enabled() {
        let shown = [
            "WEBKIT_DISABLE_DMABUF_RENDERER",
            "WEBKIT_DISABLE_COMPOSITING_MODE",
            "GDK_BACKEND",
            "APPIMAGE",
            "XDG_SESSION_TYPE",
        ]
        .iter()
        .map(|k| format!("{k}={}", std::env::var(k).unwrap_or_else(|_| "-".into())))
        .collect::<Vec<_>>()
        .join(" ");
        let drivers = if d.drivers.is_empty() {
            "none".to_string()
        } else {
            d.drivers.join(", ")
        };
        mark(&format!(
            "platform env: DMA-BUF {} — {} [{:?}]; render-node driver(s): {drivers}; set [{}]; now {shown}",
            if d.enabled { "on" } else { "off" },
            d.reason,
            d.rule,
            set.join(", ")
        ));
    }
    let _ = DECISION.set(plan.decision);
    set
}

// ---------------------------------------------------------------------------
// Render environment (Debug panel "Render check", OPENTUNE_PERF)
// ---------------------------------------------------------------------------

/// `OPENTUNE_PERF=1`: the frontend logs the render environment at startup and
/// measures scrolling after each panel switch (see `render-check.js`); the
/// lines go to the session log and to stderr.
pub const PERF_ENV: &str = "OPENTUNE_PERF";

/// What [`apply_platform_env`] set (for [`render_env`]).
static SET_BY_US: OnceLock<Vec<String>> = OnceLock::new();
static PERF: OnceLock<bool> = OnceLock::new();

/// Is `OPENTUNE_PERF` on? Read once.
pub fn perf_enabled() -> bool {
    *PERF.get_or_init(|| std::env::var(PERF_ENV).map(|v| truthy(&v)).unwrap_or(false))
}

/// The only variables whose values [`render_env`] reports.
pub const RENDER_VALUE_KEYS: [&str; 5] = [
    "WEBKIT_DISABLE_COMPOSITING_MODE",
    "WEBKIT_DISABLE_DMABUF_RENDERER",
    "GDK_BACKEND",
    "XDG_SESSION_TYPE",
    KEEP_ENV,
];

/// Variables reported as set / unset only (their values are socket names or
/// paths that can carry a user name).
pub const RENDER_PRESENCE_KEYS: [&str; 3] = ["WAYLAND_DISPLAY", "DISPLAY", "APPIMAGE"];

/// The WebKit/GTK-relevant environment as this process sees it. A fixed
/// whitelist: nothing else from the environment is read or returned.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct RenderEnv {
    /// `std::env::consts::OS`.
    pub os: &'static str,
    /// [`RENDER_VALUE_KEYS`] → value (`None` = unset). Values are cut to 64
    /// characters, control characters dropped.
    pub vars: std::collections::BTreeMap<&'static str, Option<String>>,
    /// [`RENDER_PRESENCE_KEYS`] → set?
    pub present: std::collections::BTreeMap<&'static str, bool>,
    /// `/dev/dri/renderD*` node names (empty off Linux or without a GPU node).
    pub dri_render_nodes: Vec<String>,
    /// What OpenTune itself set before WebKit started (e.g.
    /// `WEBKIT_DISABLE_DMABUF_RENDERER=1`).
    pub set_by_opentune: Vec<String>,
    /// `OPENTUNE_PERF` is on.
    pub perf: bool,
    /// The DMA-BUF renderer decision: on/off, the rule that fired, the
    /// render-node driver(s) and a one-shot self-heal notice.
    pub dmabuf: Option<crate::dmabuf::Decision>,
}

fn clean_value(v: &str) -> String {
    v.chars().filter(|c| !c.is_control()).take(64).collect()
}

/// [`RenderEnv`] from an injected environment lookup and `/dev/dri` path, so
/// the whitelist can be tested without touching the real environment.
pub fn render_env_from(
    get: impl Fn(&str) -> Option<String>,
    dri_dir: &Path,
    set_by_opentune: Vec<String>,
    dmabuf: Option<crate::dmabuf::Decision>,
) -> RenderEnv {
    let vars = RENDER_VALUE_KEYS
        .iter()
        .map(|&k| (k, get(k).map(|v| clean_value(&v))))
        .collect();
    let present = RENDER_PRESENCE_KEYS
        .iter()
        .map(|&k| (k, get(k).is_some()))
        .collect();
    let mut dri_render_nodes: Vec<String> = std::fs::read_dir(dri_dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("renderD"))
                .collect()
        })
        .unwrap_or_default();
    dri_render_nodes.sort();
    RenderEnv {
        os: std::env::consts::OS,
        vars,
        present,
        dri_render_nodes,
        set_by_opentune,
        perf: get(PERF_ENV).map(|v| truthy(&v)).unwrap_or(false),
        dmabuf,
    }
}

/// The render decision of this start, for the diagnostics log's session
/// header: the DMA-BUF decision and what OpenTune set before WebKit started.
pub fn render_summary() -> serde_json::Value {
    serde_json::json!({
        "dmabuf": DECISION.get(),
        "set_by_opentune": SET_BY_US.get().cloned().unwrap_or_default(),
    })
}

/// The render environment of this process (Debug panel → Render check, and
/// the compositing warning). Available on every channel: it reads only the
/// whitelisted keys.
#[tauri::command]
pub async fn render_env() -> RenderEnv {
    render_env_from(
        |k| std::env::var(k).ok(),
        Path::new("/dev/dri"),
        SET_BY_US.get().cloned().unwrap_or_default(),
        DECISION.get().cloned(),
    )
}

/// One Render check result line from the frontend: appended to the session
/// log, and printed to stderr when `OPENTUNE_PERF` or the startup trace is on.
#[tauri::command]
pub async fn render_note(
    line: String,
    log: tauri::State<'_, crate::state::DebugLog>,
) -> Result<(), String> {
    let line: String = line.chars().filter(|c| !c.is_control()).take(400).collect();
    log.push(format!("render: {line}"));
    if enabled() {
        mark(&format!("ui render: {line}"));
    } else if perf_enabled() {
        eprintln!("[perf {:8.1} ms] {line}", elapsed_ms());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Data cache
// ---------------------------------------------------------------------------

/// Directory fingerprint: every relevant file's (path, size, mtime). Cheap
/// (a read_dir + stat per file) next to a full JSON parse.
type Sig = Vec<(PathBuf, u64, Option<SystemTime>)>;

fn signature(dir: &Path, recursive: bool, out: &mut Sig) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        let Ok(md) = e.metadata() else { continue };
        if md.is_dir() {
            if recursive {
                signature(&p, true, out);
            }
        } else if p.extension().and_then(|s| s.to_str()) == Some("json") {
            out.push((p, md.len(), md.modified().ok()));
        }
    }
}

fn dir_sig(dir: &Path, recursive: bool) -> Sig {
    let mut s = Vec::new();
    signature(dir, recursive, &mut s);
    s.sort_by(|a, b| a.0.cmp(&b.0));
    s
}

/// A cached load: the directory, its fingerprint and the parsed entries.
type Cached<T> = (PathBuf, Sig, Arc<Vec<T>>);

/// One cached directory load. The mutex is held while loading, so a command
/// arriving during the background warm-up waits for it instead of parsing the
/// same files a second time.
struct Slot<T> {
    inner: Mutex<Option<Cached<T>>>,
}

impl<T> Slot<T> {
    const fn new() -> Self {
        Slot {
            inner: Mutex::new(None),
        }
    }

    fn get(
        &self,
        what: &str,
        dir: &Option<PathBuf>,
        recursive: bool,
        load: impl FnOnce(&Path) -> Vec<T>,
    ) -> Arc<Vec<T>> {
        let Some(dir) = dir.as_deref() else {
            return Arc::new(Vec::new());
        };
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let sig = dir_sig(dir, recursive);
        if let Some((d, s, v)) = g.as_ref() {
            if d == dir && *s == sig {
                return v.clone();
            }
        }
        let t = Instant::now();
        let v = Arc::new(load(dir));
        mark(&format!(
            "data: parsed {what} — {} entries from {} files ({:.1} ms)",
            v.len(),
            sig.len(),
            t.elapsed().as_secs_f64() * 1000.0
        ));
        *g = Some((dir.to_path_buf(), sig, v.clone()));
        v
    }
}

static DEFS: Slot<EcuDef> = Slot::new();
static CATALOG: Slot<CatalogEntry> = Slot::new();

/// The ECU definitions in `dir`, parsed once and re-parsed only when a file
/// changes. Empty when the directory is missing or fails to load.
pub fn defs(dir: &Option<PathBuf>) -> Arc<Vec<EcuDef>> {
    DEFS.get("ecudefs", dir, false, |d| {
        ecudef::load_dir(d).unwrap_or_default()
    })
}

/// The vehicle catalog in `dir` (recursive), cached like [`defs`].
pub fn catalog(dir: &Option<PathBuf>) -> Arc<Vec<CatalogEntry>> {
    CATALOG.get("catalog", dir, true, |d| {
        catalog::load_catalog(d).unwrap_or_default()
    })
}

/// Parse the definitions and the catalog on a background thread so the first
/// command that needs them finds them ready. Never blocks the caller.
pub fn warm(ecudefs: Option<PathBuf>, catalog_dir: Option<PathBuf>) {
    let spawned = std::thread::Builder::new()
        .name("opentune-warm".into())
        .spawn(move || {
            mark("data: background warm-up started");
            let _ = defs(&ecudefs);
            let _ = catalog(&catalog_dir);
            mark("data: background warm-up done");
        });
    if spawned.is_err() {
        mark("data: could not start the warm-up thread (loads on first use)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(name: &str) -> Option<PathBuf> {
        Some(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join(name),
        )
    }

    #[test]
    fn render_env_reports_only_whitelisted_keys() {
        let dri = std::env::temp_dir().join(format!("ot-render-dri-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dri);
        std::fs::create_dir_all(&dri).unwrap();
        for n in ["renderD129", "renderD128", "card0", "by-path"] {
            std::fs::write(dri.join(n), "").unwrap();
        }
        // An environment that answers every key, with secrets among them.
        let get = |k: &str| -> Option<String> {
            Some(match k {
                "WAYLAND_DISPLAY" => "wayland-secret-socket".into(),
                "DISPLAY" => ":0".into(),
                "APPIMAGE" => "/home/someuser/Apps/OpenTune.AppImage".into(),
                "WEBKIT_DISABLE_COMPOSITING_MODE" => "1\u{7}".into(),
                "GDK_BACKEND" => "x".repeat(200),
                "OPENTUNE_PERF" => "yes".into(),
                other => format!("value-of-{other}"),
            })
        };
        let env = render_env_from(
            get,
            &dri,
            vec!["WEBKIT_DISABLE_DMABUF_RENDERER=1".into()],
            Some(crate::dmabuf::Decision::not_linux()),
        );
        let keys: Vec<&str> = env.vars.keys().copied().collect();
        let mut want = RENDER_VALUE_KEYS.to_vec();
        want.sort_unstable();
        assert_eq!(keys, want);
        assert_eq!(
            env.vars["WEBKIT_DISABLE_COMPOSITING_MODE"].as_deref(),
            Some("1")
        );
        assert_eq!(env.vars["GDK_BACKEND"].as_ref().unwrap().len(), 64);
        assert!(env.present.values().all(|&p| p));
        assert_eq!(env.present.len(), RENDER_PRESENCE_KEYS.len());
        assert_eq!(env.dri_render_nodes, vec!["renderD128", "renderD129"]);
        assert!(env.perf);
        let json = serde_json::to_string(&env).unwrap();
        for leak in [
            "HOME",
            "PATH",
            "someuser",
            "wayland-secret-socket",
            "value-of-HOME",
            "card0",
        ] {
            assert!(!json.contains(leak), "{leak} leaked into {json}");
        }
        let _ = std::fs::remove_dir_all(&dri);

        let none = render_env_from(|_| None, Path::new("/nonexistent/ot-dri"), Vec::new(), None);
        assert!(none.vars.values().all(Option::is_none));
        assert!(none.present.values().all(|&p| !p));
        assert!(none.dri_render_nodes.is_empty() && !none.perf);
    }

    #[test]
    fn truthy_values() {
        assert!(truthy("1") && truthy("TRUE") && truthy(" yes ") && truthy("on"));
        assert!(!truthy("0") && !truthy("") && !truthy("no"));
    }

    // The tests use their own slots: the global ones are shared with every
    // other test in the crate, which may run in parallel.
    #[test]
    fn defs_are_cached_and_shared() {
        let slot: Slot<EcuDef> = Slot::new();
        let dir = repo("ecudefs");
        let load = |d: &Path| ecudef::load_dir(d).unwrap_or_default();
        let a = slot.get("ecudefs", &dir, false, load);
        let b = slot.get("ecudefs", &dir, false, load);
        assert!(!a.is_empty(), "bundled ecudefs load");
        assert!(Arc::ptr_eq(&a, &b), "second call reuses the parsed defs");
    }

    #[test]
    fn missing_dir_is_empty() {
        assert!(defs(&None).is_empty());
        let slot: Slot<CatalogEntry> = Slot::new();
        let nowhere = Some(PathBuf::from("/nonexistent/opentune-startup-test"));
        let got = slot.get("catalog", &nowhere, true, |d| {
            catalog::load_catalog(d).unwrap_or_default()
        });
        assert!(got.is_empty());
    }

    #[test]
    fn cache_reloads_when_a_file_changes() {
        let tmp = std::env::temp_dir().join(format!("ot-startup-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let slot: Slot<String> = Slot::new();
        let dir = Some(tmp.clone());
        let load = |d: &Path| -> Vec<String> {
            let mut v: Vec<String> = std::fs::read_dir(d)
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            v.sort();
            v
        };
        std::fs::write(tmp.join("a.json"), "{}").unwrap();
        let first = slot.get("t", &dir, false, load);
        assert_eq!(first.len(), 1);
        assert!(Arc::ptr_eq(&first, &slot.get("t", &dir, false, load)));
        std::fs::write(tmp.join("b.json"), "{}").unwrap();
        let second = slot.get("t", &dir, false, load);
        assert_eq!(second.len(), 2, "a new file invalidates the cache");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Timing of the startup-relevant loads, cold vs. cached. Run with
    /// `cargo test --release startup_load_timings -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn startup_load_timings() {
        let (d, c) = (repo("ecudefs"), repo("catalog"));
        let t = Instant::now();
        let n: usize = defs(&d).iter().map(|x| x.maps.len()).sum();
        let cold = t.elapsed();
        let t = Instant::now();
        let _ = defs(&d);
        let warm_hit = t.elapsed();
        let t = Instant::now();
        let cat = catalog(&c).len();
        let cat_cold = t.elapsed();
        eprintln!(
            "ecudefs: {n} maps cold {cold:?}, cached {warm_hit:?}; catalog {cat} entries cold {cat_cold:?}"
        );
    }
}
