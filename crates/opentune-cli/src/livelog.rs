//! Run-length and reconnect flags shared by `log` and `obd read`, mapped onto
//! [`opentune_core::resilient`] (the loop both commands run on), and the
//! `--trigger` / `--split` output layer ([`opentune_logdef::trigger`]).
use anyhow::{anyhow, bail, Context, Result};
use clap::Args;
use opentune_core::resilient::{ReconnectPolicy, RunLimits, RunReport};
use opentune_logdef::{TriggerCsvWriter, TriggerOutput, TriggerReport, TriggerSet};
use std::cell::RefCell;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// How long a live log runs and what it does when the link drops.
#[derive(Args, Debug, Clone)]
pub struct RunArgs {
    /// Number of rounds; 0 = until Ctrl-C or --duration.
    #[arg(long, default_value_t = 1)]
    pub rounds: u64,
    /// Sleep between rounds, milliseconds (0 = as fast as possible).
    #[arg(long, default_value_t = 0)]
    pub interval_ms: u64,
    /// Stop after this many seconds (with --rounds 0: the only limit).
    #[arg(long, value_name = "SECONDS")]
    pub duration: Option<f64>,
    /// When the link is lost (no answer for --dead-after rounds, or the
    /// adapter reset), close the driver and re-open it with the same
    /// settings, backing off, then resume (re-defining dynamic DIDs).
    /// Without it the log keeps polling the same link, backing off.
    #[arg(long)]
    pub reconnect: bool,
    /// Rounds in a row with no response at all before the link counts as lost.
    #[arg(long, default_value_t = 3)]
    pub dead_after: u32,
    /// First reconnect backoff, ms; doubles per attempt.
    #[arg(long, default_value_t = 500)]
    pub reconnect_backoff_ms: u64,
    /// Backoff ceiling, ms.
    #[arg(long, default_value_t = 10_000)]
    pub reconnect_max_backoff_ms: u64,
    /// Give up after this many failed re-opens (default: never).
    #[arg(long, value_name = "N")]
    pub reconnect_max_attempts: Option<u32>,
    /// Give up once the link has been down this many seconds (default: never).
    #[arg(long, value_name = "SECONDS")]
    pub reconnect_give_up: Option<f64>,
}

fn ms(s: f64, flag: &str) -> Result<u64> {
    if !(s.is_finite() && s >= 0.0) {
        return Err(anyhow!("{flag} must be a non-negative number of seconds"));
    }
    Ok((s * 1000.0).round() as u64)
}

impl RunArgs {
    /// Round / time limits.
    pub fn limits(&self) -> Result<RunLimits> {
        Ok(RunLimits {
            rounds: (self.rounds > 0).then_some(self.rounds),
            duration_ms: self.duration.map(|d| ms(d, "--duration")).transpose()?,
            interval_ms: self.interval_ms,
        })
    }

    /// The reconnect policy (validated).
    pub fn policy(&self) -> Result<ReconnectPolicy> {
        let p = ReconnectPolicy {
            reconnect: self.reconnect,
            dead_after: self.dead_after,
            initial_backoff_ms: self.reconnect_backoff_ms,
            max_backoff_ms: self.reconnect_max_backoff_ms,
            max_attempts: self.reconnect_max_attempts,
            give_up_after_ms: self
                .reconnect_give_up
                .map(|s| ms(s, "--reconnect-give-up"))
                .transpose()?,
        };
        p.validate().map_err(|e| anyhow!("reconnect policy: {e}"))?;
        Ok(p)
    }

    /// Runs until Ctrl-C (no round and no time limit).
    pub fn open_ended(&self) -> bool {
        self.rounds == 0 && self.duration.is_none()
    }
}

/// A stop flag set by Ctrl-C (best effort: if a handler is already
/// installed, the limits still end the run).
pub fn ctrlc_flag() -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let s = Arc::clone(&stop);
    let _ = ctrlc::set_handler(move || s.store(true, Ordering::Relaxed));
    stop
}

/// The stderr line about the link, if anything happened to it.
pub fn link_summary(r: &RunReport) -> Option<String> {
    if r.drops == 0 && r.dead_rounds == 0 {
        return None;
    }
    let mut s = format!(
        "link: {} dead round(s), {} drop(s), {} reconnect(s) in {} attempt(s), {} recovered \
         without reopen, {:.1} s down",
        r.dead_rounds,
        r.drops,
        r.reconnects,
        r.reopen_attempts,
        r.recoveries,
        r.downtime_ms as f64 / 1000.0
    );
    if r.ended == opentune_core::resilient::EndReason::GaveUp {
        s.push_str("; gave up reconnecting");
    }
    Some(s)
}

/// Datalogging triggers on the recorded CSV.
#[derive(Args, Debug, Clone, Default)]
pub struct TriggerArgs {
    /// Datalogging triggers: an `opentune-triggers` JSON file (e.g. a pack's
    /// `triggers/*.json`). `record`/`split` triggers keep only the windows
    /// around their condition (pre/post history, start and end marker rows);
    /// `mark` triggers add a marker row when they fire.
    #[arg(long, value_name = "FILE")]
    pub trigger: Option<PathBuf>,
    /// With --trigger and --out: one CSV per recording window,
    /// `<out>-001.csv`, `<out>-002.csv`, … (as a `split` trigger does).
    #[arg(long, requires = "trigger")]
    pub split: bool,
}

impl TriggerArgs {
    /// Read and validate the trigger file, if one was given.
    pub fn load(&self) -> Result<Option<TriggerSet>> {
        let Some(path) = &self.trigger else {
            return Ok(None);
        };
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read trigger file {}", path.display()))?;
        let set = TriggerSet::load_json(&text)
            .map_err(|e| anyhow!("trigger file {}: {e}", path.display()))?;
        Ok(Some(set))
    }
}

/// Warn (stderr) about trigger comparisons whose channel is not logged.
pub fn warn_missing_channels(set: &TriggerSet, logged: &[&str]) {
    for c in set.missing_channels(logged) {
        eprintln!("trigger: warning: '{c}' is not among the logged channels; that comparison stays unknown");
    }
}

/// `<dir>/<stem>-NNN.<ext>` for window `n` of a split log written to `base`.
pub fn window_path(base: &Path, n: u32) -> PathBuf {
    let stem = base
        .file_stem()
        .map_or_else(|| "log".into(), |s| s.to_string_lossy().into_owned());
    let ext = base
        .extension()
        .map_or_else(|| "csv".into(), |s| s.to_string_lossy().into_owned());
    base.with_file_name(format!("{stem}-{n:03}.{ext}"))
}

/// What the triggers did, with the files a split log wrote.
pub struct TriggerOutcome {
    /// The engine's report.
    pub report: TriggerReport,
    /// Per-window files, in window order (empty unless split).
    pub files: Vec<PathBuf>,
}

/// The CSV destination of a live log: the file / stdout, or the trigger
/// layer in front of it (one file, or one file per window).
pub enum CsvOut {
    /// Everything as logged.
    Plain(Box<dyn Write>),
    /// Through the triggers.
    Triggered {
        /// The trigger layer.
        w: Box<TriggerCsvWriter<'static>>,
        /// Files opened for windows (split).
        files: Rc<RefCell<Vec<PathBuf>>>,
    },
}

impl CsvOut {
    /// Open the destination: `out` (or stdout), through `set` when given.
    /// `split` (or a `split` trigger) writes one file per window next to
    /// `out`, which it then requires.
    pub fn open(out: Option<&Path>, set: Option<TriggerSet>, split: bool) -> Result<Self> {
        let plain = |out: Option<&Path>| -> Result<Box<dyn Write>> {
            Ok(match out {
                Some(p) => {
                    let f = std::fs::File::create(p)
                        .map_err(|e| anyhow!("create {}: {e}", p.display()))?;
                    Box::new(std::io::BufWriter::new(f))
                }
                None => Box::new(std::io::stdout().lock()),
            })
        };
        let Some(set) = set else {
            return Ok(CsvOut::Plain(plain(out)?));
        };
        let files: Rc<RefCell<Vec<PathBuf>>> = Rc::default();
        let output = if split || set.splits() {
            let Some(base) = out else {
                bail!(
                    "--split (or a split trigger) needs --out: the windows go to <out>-001.csv, …"
                );
            };
            let base = base.to_path_buf();
            let f2 = Rc::clone(&files);
            TriggerOutput::PerWindow(Box::new(move |n: u32| -> std::io::Result<Box<dyn Write>> {
                let p = window_path(&base, n);
                let f = std::fs::File::create(&p).map_err(|e| {
                    std::io::Error::new(e.kind(), format!("create {}: {e}", p.display()))
                })?;
                f2.borrow_mut().push(p);
                Ok(Box::new(std::io::BufWriter::new(f)))
            }))
        } else {
            TriggerOutput::Single(plain(out)?)
        };
        let w = TriggerCsvWriter::new(set, output).map_err(|e| anyhow!("--trigger: {e}"))?;
        Ok(CsvOut::Triggered {
            w: Box::new(w),
            files,
        })
    }

    /// Flush; through triggers, evaluate the last round, close an open
    /// window and report.
    pub fn finish(self) -> Result<Option<TriggerOutcome>> {
        match self {
            CsvOut::Plain(mut w) => {
                w.flush()?;
                Ok(None)
            }
            CsvOut::Triggered { w, files } => {
                let report = w.finish()?;
                let files = files.borrow().clone();
                Ok(Some(TriggerOutcome { report, files }))
            }
        }
    }
}

impl Write for CsvOut {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            CsvOut::Plain(w) => w.write(buf),
            CsvOut::Triggered { w, .. } => w.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            CsvOut::Plain(w) => w.flush(),
            CsvOut::Triggered { w, .. } => w.flush(),
        }
    }
}

/// The stderr lines about the triggers: fires, windows (with durations and
/// files), marks, dropped rows and warnings.
pub fn trigger_summary(o: &TriggerOutcome) -> Vec<String> {
    let r = &o.report;
    let per: Vec<String> = r.fires.iter().map(|(n, k)| format!("{n} {k}")).collect();
    let mut lines = vec![format!(
        "trigger: {} fire(s) ({}); {} window(s); {} mark(s); {} row(s) outside windows dropped",
        r.total_fires(),
        per.join(", "),
        r.windows.len(),
        r.marks,
        r.dropped_rows
    )];
    for (i, w) in r.windows.iter().enumerate() {
        let mut l = format!(
            "  window {}: {:.3} s, {} row(s), {}",
            w.index,
            w.duration_ms() as f64 / 1000.0,
            w.rows,
            w.triggers.join("+")
        );
        if w.cut_by_end {
            l.push_str(", cut by the end of the log");
        }
        if let Some(f) = o.files.get(i) {
            l.push_str(&format!(" -> {}", f.display()));
        }
        lines.push(l);
    }
    for w in &r.warnings {
        lines.push(format!("trigger: warning: {w}"));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct T {
        #[command(flatten)]
        run: RunArgs,
    }

    #[test]
    fn window_files_are_numbered_next_to_out() {
        assert_eq!(
            window_path(Path::new("/tmp/x/pulls.csv"), 7),
            PathBuf::from("/tmp/x/pulls-007.csv")
        );
        assert_eq!(
            window_path(Path::new("run"), 12),
            PathBuf::from("run-012.csv")
        );
    }

    #[test]
    fn flags_map_onto_the_loop() {
        let t = T::parse_from([
            "t",
            "--rounds",
            "0",
            "--duration",
            "1.5",
            "--reconnect",
            "--reconnect-give-up",
            "30",
        ]);
        let l = t.run.limits().unwrap();
        assert_eq!((l.rounds, l.duration_ms), (None, Some(1500)));
        let p = t.run.policy().unwrap();
        assert!(p.reconnect && p.max_attempts.is_none());
        assert_eq!(p.give_up_after_ms, Some(30_000));
        assert!(!t.run.open_ended());
        let t = T::parse_from(["t", "--reconnect-backoff-ms", "0"]);
        assert!(t.run.policy().is_err());
        let t = T::parse_from(["t", "--duration=-1"]);
        assert!(t.run.limits().is_err());
    }
}
