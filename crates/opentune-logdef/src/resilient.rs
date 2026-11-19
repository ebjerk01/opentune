//! [`LiveSession`] over a link that may drop out: the CSV writer as a
//! [`LogJob`] for [`opentune_core::resilient::run_resilient`].
//!
//! [`LiveSession::run_csv_resilient`] writes the same CSV as
//! [`LiveSession::run_csv`] and survives the transport dropping out: dead
//! rounds keep their rows (empty values, the transport error in `note`),
//! every link event becomes a **marker row** with `param_id`
//! [`LINK_PARAM_ID`] (empty `value`, the event in `note`, its state in
//! `text`), and after a reopen [`LiveSession::start`] runs again so the
//! dynamic DIDs are re-defined before the log resumes. No value is ever
//! carried over or invented for the time the link was down.

use crate::fast::LiveSession;
use crate::gps::GpsLogger;
use crate::live::{write_csv_row, Sample, CSV_HEADER};
use opentune_core::resilient::{run_resilient, LinkEvent, LogJob, RunOptions, RunReport};
use opentune_core::RawDiag;
use std::io::Write;

/// `param_id` of the link-event marker rows.
pub const LINK_PARAM_ID: &str = "link";

/// The marker-row sample for a link event: no `raw`/`value`, the event in
/// `note`, its [state](LinkEvent::state) in `text`.
pub fn link_event_sample(ev: &LinkEvent) -> Sample {
    Sample {
        param_id: LINK_PARAM_ID.into(),
        name: "Link status".into(),
        raw: None,
        value: None,
        units: String::new(),
        note: ev.describe(),
        text: Some(ev.state().into()),
    }
}

struct CsvJob<'a, W: Write> {
    session: &'a mut LiveSession,
    gps: Option<&'a mut GpsLogger>,
    out: &'a mut W,
}

impl<W: Write> LogJob for CsvJob<'_, W> {
    fn round(&mut self, diag: &mut dyn RawDiag, round: u64, ts: u64) -> std::io::Result<()> {
        let r = usize::try_from(round).unwrap_or(usize::MAX);
        let samples = self.session.tick(diag);
        for s in &samples {
            write_csv_row(&mut *self.out, r, ts, s)?;
        }
        if let Some(g) = self.gps.as_deref_mut() {
            for s in g.samples(&samples) {
                write_csv_row(&mut *self.out, r, ts, &s)?;
            }
        }
        Ok(())
    }

    fn event(&mut self, ev: &LinkEvent, round: u64, ts: u64) -> std::io::Result<()> {
        let r = usize::try_from(round).unwrap_or(usize::MAX);
        write_csv_row(&mut *self.out, r, ts, &link_event_sample(ev))?;
        self.out.flush()
    }

    fn reopened(&mut self, diag: &mut dyn RawDiag) {
        self.session.start(diag);
    }

    fn stop(&mut self, diag: &mut dyn RawDiag) {
        self.session.stop(diag);
    }
}

impl LiveSession {
    /// Log to CSV (header + one row per channel per round, plus GPS rows) over
    /// `link` until `opts` ends the run, surviving transport drops: see the
    /// [module docs](crate::resilient) and
    /// [`run_resilient`](opentune_core::resilient::run_resilient).
    ///
    /// Call [`start`](Self::start) on `link` first. `open` re-opens the
    /// transport after a drop (used only when `opts.policy.reconnect`); the
    /// session is then started again on the new link. A clean end stops the
    /// session (clearing its dynamic DIDs) while the link is alive.
    pub fn run_csv_resilient<'l, W: Write>(
        &mut self,
        link: Box<dyn RawDiag + 'l>,
        open: &mut dyn FnMut() -> Result<Box<dyn RawDiag + 'l>, String>,
        opts: &RunOptions<'_>,
        gps: Option<&mut GpsLogger>,
        out: &mut W,
    ) -> std::io::Result<RunReport> {
        writeln!(out, "{CSV_HEADER}")?;
        let mut job = CsvJob {
            session: self,
            gps,
            out,
        };
        let report = run_resilient(&mut job, link, open, opts)?;
        job.out.flush()?;
        Ok(report)
    }
}
