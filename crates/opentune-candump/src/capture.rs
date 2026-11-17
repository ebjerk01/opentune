//! The capture loop shared by the CLI and the desktop app.
use crate::stats::CaptureStats;
use crate::writer::FrameWriter;
use opentune_core::{CanMonitor, MonitorOpts, Result};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long one `next_frame` call may block, so the stop flag and the
/// duration are checked at least this often.
pub const POLL_MS: u64 = 100;

/// Parameters of one capture run.
#[derive(Debug, Clone, Default)]
pub struct CaptureParams {
    /// Passed to [`CanMonitor::start`]; its `filter_ids` are also applied in
    /// software.
    pub opts: MonitorOpts,
    /// Stop after this long (measured from the moment the monitor started).
    pub duration: Option<Duration>,
}

/// How a capture ended.
#[derive(Debug, Clone, PartialEq)]
pub struct CaptureOutcome {
    /// Frames written.
    pub frames: u64,
    /// Bytes written to the log file.
    pub bytes: u64,
    /// Capture duration, seconds.
    pub elapsed_s: f64,
    /// Adapter-reported dropped frames / overflow events.
    pub dropped: u64,
    /// True if the duration elapsed (false: stopped by the flag).
    pub timed_out: bool,
}

/// Run a capture: `monitor` must already be [started](CanMonitor::start).
/// Frames passing the filter are written to `writer` and accounted in `stats`
/// until `stop` is set or `params.duration` elapses. `on_tick` is called about
/// every [`POLL_MS`] with the bytes written so far (for live displays).
///
/// The monitor is always stopped and the writer finished before returning,
/// also on error; a monitor error ends the capture with that error after the
/// file has been finished.
pub fn run_capture<W: Write>(
    monitor: &mut dyn CanMonitor,
    writer: &mut FrameWriter<W>,
    stats: &Mutex<CaptureStats>,
    stop: &AtomicBool,
    params: &CaptureParams,
    on_tick: &mut dyn FnMut(u64),
) -> Result<CaptureOutcome> {
    let started = Instant::now();
    let mut timed_out = false;
    let mut last_tick = Instant::now();
    let res = (|| -> Result<()> {
        writer.start()?;
        loop {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            if let Some(d) = params.duration {
                if started.elapsed() >= d {
                    timed_out = true;
                    return Ok(());
                }
            }
            match monitor.next_frame(POLL_MS)? {
                Some(f) => {
                    if params.opts.accepts(f.id()) {
                        writer.write(&f)?;
                        if let Ok(mut s) = stats.lock() {
                            s.record(&f);
                        }
                    }
                }
                None => writer.tick(started.elapsed().as_micros() as u64)?,
            }
            if last_tick.elapsed() >= Duration::from_millis(POLL_MS) {
                last_tick = Instant::now();
                if let Ok(mut s) = stats.lock() {
                    s.set_dropped(monitor.dropped());
                }
                on_tick(writer.bytes_written());
            }
        }
    })();
    let stop_res = monitor.stop();
    let finish_res = writer.finish();
    let dropped = monitor.dropped();
    let frames = match stats.lock() {
        Ok(mut s) => {
            s.set_dropped(dropped);
            s.frames()
        }
        Err(_) => 0,
    };
    res?;
    finish_res?;
    stop_res?;
    on_tick(writer.bytes_written());
    Ok(CaptureOutcome {
        frames,
        bytes: writer.bytes_written(),
        elapsed_s: started.elapsed().as_secs_f64(),
        dropped,
        timed_out,
    })
}
