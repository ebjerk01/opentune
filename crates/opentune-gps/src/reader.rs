//! Background reader: a thread pulls bytes from a serial port (or any
//! [`Read`]), splits lines, parses them and keeps the latest fix behind a
//! mutex. [`GpsReader::snapshot`] never blocks on I/O.

use crate::fix::{FixTracker, GpsSnapshot, LineAssembler, LineItem, DEFAULT_STALE_AFTER_MS};
use crate::GpsFeed;
use std::io::{ErrorKind, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Debug)]
struct Shared {
    tracker: FixTracker,
    ended: Option<String>,
}

fn lock(m: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Latest-fix state fed by a background thread.
///
/// The thread exits when the stream ends, on a read error, or when the
/// reader is stopped/dropped. The source must not block forever in `read`
/// (serial ports are opened with a short timeout) or stopping waits for it.
pub struct GpsReader {
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    t0: Instant,
    label: String,
}

impl GpsReader {
    /// Read NMEA from `src` on a background thread.
    pub fn spawn<R: Read + Send + 'static>(label: impl Into<String>, mut src: R) -> Self {
        let shared = Arc::new(Mutex::new(Shared {
            tracker: FixTracker::new(DEFAULT_STALE_AFTER_MS),
            ended: None,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let t0 = Instant::now();
        let (sh, st) = (Arc::clone(&shared), Arc::clone(&stop));
        let handle = std::thread::Builder::new()
            .name("opentune-gps".into())
            .spawn(move || {
                let mut asm = LineAssembler::new();
                let mut buf = [0u8; 512];
                let ended = loop {
                    if st.load(Ordering::Relaxed) {
                        break "stopped".to_string();
                    }
                    match src.read(&mut buf) {
                        Ok(0) => {
                            let rest = asm.finish();
                            apply(&sh, t0, rest);
                            break "end of stream".to_string();
                        }
                        Ok(n) => {
                            let items = asm.push(&buf[..n]);
                            apply(&sh, t0, items);
                        }
                        Err(e)
                            if matches!(
                                e.kind(),
                                ErrorKind::TimedOut
                                    | ErrorKind::WouldBlock
                                    | ErrorKind::Interrupted
                            ) => {}
                        Err(e) => break format!("read error: {e}"),
                    }
                };
                lock(&sh).ended = Some(ended);
            });
        let handle = match handle {
            Ok(h) => Some(h),
            Err(e) => {
                lock(&shared).ended = Some(format!("could not start reader thread: {e}"));
                None
            }
        };
        Self {
            shared,
            stop,
            handle,
            t0,
            label: label.into(),
        }
    }

    /// Open a serial GNSS receiver (u-blox style CDC-ACM: `/dev/ttyACM0`,
    /// `COM5`, …). USB CDC-ACM ignores `baud`; a UART receiver needs its
    /// configured rate (9600 is the common NMEA default).
    #[cfg(feature = "serial")]
    pub fn open_serial(port: &str, baud: u32) -> Result<Self, String> {
        let mut p = serialport::new(port, baud)
            .timeout(Duration::from_millis(200))
            .open()
            .map_err(|e| format!("open GPS port '{port}': {e}"))?;
        // Some CDC-ACM receivers only stream once DTR is asserted.
        let _ = p.write_data_terminal_ready(true);
        Ok(Self::spawn(format!("serial {port}"), p))
    }

    /// Staleness limit in milliseconds (default
    /// [`DEFAULT_STALE_AFTER_MS`]).
    pub fn with_stale_after(self, ms: u64) -> Self {
        lock(&self.shared).tracker.stale_after_ms = ms;
        self
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.t0.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// What is known right now.
    pub fn current(&self) -> GpsSnapshot {
        let now = self.now_ms();
        let g = lock(&self.shared);
        let mut s = g.tracker.snapshot(now);
        s.ended.clone_from(&g.ended);
        // Say why a source that went quiet stopped (end of stream, a read
        // error after a USB unplug), not only that it is stale.
        if let Some(e) = &g.ended {
            if !s.good && (s.silent_ms.is_none() || s.stale) {
                s.note = format!("{} (source ended: {e})", s.note);
            }
        }
        s
    }

    /// Poll until `pred` holds or `timeout` passes; returns the last snapshot.
    pub fn wait_until(
        &self,
        timeout: Duration,
        pred: impl Fn(&GpsSnapshot) -> bool,
    ) -> GpsSnapshot {
        let until = Instant::now() + timeout;
        loop {
            let s = self.current();
            if pred(&s) || Instant::now() >= until || (s.ended.is_some() && !self.running()) {
                return s;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The reader thread is still running.
    pub fn running(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// Stop the thread and wait for it.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn apply(sh: &Mutex<Shared>, t0: Instant, items: Vec<LineItem>) {
    if items.is_empty() {
        return;
    }
    let now = u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX);
    let mut g = lock(sh);
    for item in items {
        match item {
            LineItem::Line(l) => {
                g.tracker.ingest_line(&l, now);
            }
            LineItem::Overlong => g.tracker.count_overlong(),
        }
    }
}

impl Drop for GpsReader {
    fn drop(&mut self) {
        self.stop();
    }
}

impl GpsFeed for GpsReader {
    fn snapshot(&mut self) -> GpsSnapshot {
        self.current()
    }

    fn describe(&self) -> String {
        self.label.clone()
    }

    fn wait_ready(&mut self, timeout: Duration) {
        self.wait_until(timeout, |s| s.silent_ms.is_some());
    }
}
