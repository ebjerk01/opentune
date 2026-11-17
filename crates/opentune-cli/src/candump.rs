//! `opentune candump`: passive full-bus capture to a log file.
use anyhow::{anyhow, Context, Result};
use opentune_candump::{
    format_for_path, monitor_opts, parse_id_list, run_capture, CaptureParams, CaptureStats, Format,
    FrameWriter,
};
use opentune_core::Registry;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Arguments of `opentune candump`.
#[derive(clap::Args)]
pub struct CandumpArgs {
    /// Driver id (see `drivers`): mock, socketcan, gsusb, slcan, elm327obd.
    #[arg(long)]
    driver: String,
    /// Serial device (slcan, elm327obd) or CAN interface (socketcan, gsusb).
    #[arg(long)]
    port: Option<String>,
    /// Serial baud (slcan default 115200, ELM327 default 38400).
    #[arg(long)]
    baud: Option<String>,
    /// Bus: `hs` (500 kbit/s, default) or `ms` (125 kbit/s; a switched ELM327
    /// also needs its HS/MS switch on MS).
    #[arg(long)]
    bus: Option<String>,
    /// Explicit CAN bitrate in bit/s (overrides `--bus`).
    #[arg(long)]
    bitrate: Option<u32>,
    /// Output file.
    #[arg(long)]
    out: PathBuf,
    /// Log format: candump (default; can-utils compatible), csv or asc.
    /// Default: from the file extension (.csv / .asc), else candump.
    #[arg(long)]
    format: Option<String>,
    /// Stop after this many seconds (default: until Ctrl-C).
    #[arg(long)]
    duration: Option<f64>,
    /// Only capture these ids (hex, comma separated), e.g. `7E8,7E0`.
    #[arg(long)]
    filter: Option<String>,
    /// Extra driver option `KEY=VALUE` (repeatable), e.g. `listen_only=require`.
    #[arg(long = "opt", value_name = "KEY=VALUE")]
    opts: Vec<String>,
    /// No live status line.
    #[arg(long)]
    quiet: bool,
}

/// Run a capture until the duration or Ctrl-C.
pub fn run(reg: &Registry, a: CandumpArgs) -> Result<()> {
    let format: Format = match &a.format {
        Some(f) => f.parse().map_err(|e: String| anyhow!(e))?,
        None => format_for_path(&a.out),
    };
    let filter = match &a.filter {
        Some(f) => parse_id_list(f).map_err(|e| anyhow!(e))?,
        None => Vec::new(),
    };
    let duration = match a.duration {
        Some(d) if d.is_finite() && d > 0.0 => Some(Duration::from_secs_f64(d)),
        Some(_) => return Err(anyhow!("--duration must be a positive number of seconds")),
        None => None,
    };
    let mut opts = monitor_opts(
        a.port.as_deref(),
        a.baud.as_deref(),
        a.bus.as_deref(),
        a.bitrate,
        filter,
    );
    for kv in &a.opts {
        let (k, v) = kv
            .split_once('=')
            .ok_or_else(|| anyhow!("--opt expects KEY=VALUE, got '{kv}'"))?;
        opts.options.insert(k.trim().into(), v.trim().into());
    }

    let mut drv = reg
        .make(&a.driver)
        .ok_or_else(|| anyhow!("unknown driver '{}' (see `opentune drivers`)", a.driver))?;
    let mon = drv.can_monitor().ok_or_else(|| {
        anyhow!(
            "driver '{}' cannot capture raw CAN traffic (supported: mock, socketcan, gsusb, slcan, elm327obd)",
            a.driver
        )
    })?;

    let file =
        std::fs::File::create(&a.out).with_context(|| format!("create {}", a.out.display()))?;
    if let Err(e) = mon.start(&opts) {
        drop(file);
        let _ = std::fs::remove_file(&a.out);
        return Err(anyhow!("start capture on {}: {e}", a.driver));
    }
    eprintln!(
        "capturing on {} — listen-only: {} — {} → {}{}",
        if mon.describe().is_empty() {
            a.driver.clone()
        } else {
            mon.describe()
        },
        mon.listen_only().as_str(),
        format.as_str(),
        a.out.display(),
        if duration.is_none() {
            " (Ctrl-C to stop)"
        } else {
            ""
        }
    );

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        // Best effort: if a handler is already installed, the duration still ends it.
        let _ = ctrlc::set_handler(move || stop.store(true, Ordering::Relaxed));
    }
    let mut writer = FrameWriter::new(
        BufWriter::new(file),
        format,
        &mon.channel(),
        mon.wall_base_us(),
    );
    let stats = Mutex::new(CaptureStats::new());
    let params = CaptureParams { opts, duration };
    let started = Instant::now();
    let mut last_line = Instant::now();
    let quiet = a.quiet;
    let mut tick = |bytes: u64| {
        if quiet || last_line.elapsed() < Duration::from_millis(500) {
            return;
        }
        last_line = Instant::now();
        if let Ok(s) = stats.lock() {
            let snap = s.snapshot(started.elapsed().as_micros() as u64, 0);
            eprint!(
                "\r{:>7.1}s  frames {:>9}  {:>6.0} fps  ids {:>4}  dropped {:>5}  {:>9} B ",
                snap.elapsed_s, snap.frames, snap.fps, snap.unique_ids, snap.dropped, bytes
            );
            let _ = std::io::stderr().flush();
        }
    };
    let outcome = run_capture(mon, &mut writer, &stats, &stop, &params, &mut tick);
    if !quiet {
        eprintln!();
    }
    let outcome = outcome.map_err(|e| anyhow!("capture failed: {e}"))?;
    eprintln!(
        "{} frames, {} bytes in {:.1} s{} → {}",
        outcome.frames,
        outcome.bytes,
        outcome.elapsed_s,
        if outcome.dropped > 0 {
            format!(", {} dropped by the adapter", outcome.dropped)
        } else {
            String::new()
        },
        a.out.display()
    );
    Ok(())
}
