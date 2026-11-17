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
//! * **Platform environment** — [`apply_platform_env`] sets WebKitGTK's
//!   `WEBKIT_DISABLE_DMABUF_RENDERER=1` on Linux before GTK/WebKit initialise
//!   (unless the user set it, or opted out with `OPENTUNE_KEEP_WEBKIT_ENV=1`).
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

/// Print one trace line (no-op unless the trace is on).
pub fn mark(label: &str) {
    if enabled() {
        eprintln!("[startup {:8.1} ms] {label}", elapsed_ms());
    }
}

/// Time `f` and print `label (n ms)` when the trace is on.
pub fn timed<T>(label: &str, f: impl FnOnce() -> T) -> T {
    if !enabled() {
        return f();
    }
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

/// Frontend milestone (`invoke("startup_mark", { label })`). A no-op unless
/// the trace is on; async so it never queues behind the UI thread.
#[tauri::command]
pub async fn startup_mark(label: String) {
    mark(&format!("ui: {label}"));
}

// ---------------------------------------------------------------------------
// Platform environment
// ---------------------------------------------------------------------------

/// Apply safe WebKitGTK defaults before GTK/WebKit initialise. Must run before
/// the Tauri app is built (no webview exists yet, and no other thread is
/// reading the environment). Returns what was set, for the trace.
///
/// `WEBKIT_DISABLE_DMABUF_RENDERER=1`: WebKitGTK ≥ 2.42 renders through
/// DMA-BUF/GBM buffers by default. With some GPU drivers (and in an AppImage,
/// where a bundled WebKit meets the host's Mesa/libgbm) that path stalls,
/// draws a blank window or falls back slowly. The shared-memory path is
/// plenty fast for this UI. A value the user already set is never touched,
/// and `OPENTUNE_KEEP_WEBKIT_ENV=1` skips this entirely.
pub fn apply_platform_env() -> Vec<String> {
    let mut set = Vec::new();
    if !cfg!(target_os = "linux") {
        return set;
    }
    if std::env::var(KEEP_ENV).map(|v| truthy(&v)).unwrap_or(false) {
        mark(&format!(
            "{KEEP_ENV}=1: WebKit environment left as inherited"
        ));
        return set;
    }
    const DMABUF: &str = "WEBKIT_DISABLE_DMABUF_RENDERER";
    if std::env::var_os(DMABUF).is_none() {
        std::env::set_var(DMABUF, "1");
        set.push(format!("{DMABUF}=1"));
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
        mark(&format!(
            "platform env: set [{}]; now {shown}",
            set.join(", ")
        ));
    }
    set
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
