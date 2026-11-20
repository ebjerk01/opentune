//! CSV recording for the live panels (Dashboard, Live log, Haldex): a
//! streaming writer plus a Record / Stop control.
//!
//! The recorder only ever creates NEW files: it refuses an existing path
//! (nothing is replaced, so it needs no confirm + backup), and a row with the
//! wrong number of cells is refused rather than written misaligned.

use crate::host::Host;
use crate::icons;
use crate::theme::{self, font, MONO_MEDIUM};
use eframe::egui::{RichText, Ui};
use std::borrow::Cow;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

/// One CSV field, quoted when it holds a separator, a quote or a line break
/// (RFC 4180).
pub fn field(s: &str) -> Cow<'_, str> {
    if s.contains([',', '"', '\n', '\r']) {
        Cow::Owned(format!("\"{}\"", s.replace('"', "\"\"")))
    } else {
        Cow::Borrowed(s)
    }
}

/// A CSV file being written row by row.
pub struct CsvRecorder {
    path: PathBuf,
    out: BufWriter<File>,
    columns: usize,
    rows: u64,
}

impl CsvRecorder {
    /// Create `path` (which must not exist yet) and write the header.
    pub fn create(path: &Path, header: &[&str]) -> Result<Self, String> {
        if header.is_empty() {
            return Err("a CSV needs at least one column".into());
        }
        let f = File::options()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::AlreadyExists => format!(
                    "{} already exists; the recorder never replaces a file, pick a new name",
                    path.display()
                ),
                _ => format!("create {}: {e}", path.display()),
            })?;
        let mut r = Self {
            path: path.to_path_buf(),
            out: BufWriter::new(f),
            columns: header.len(),
            rows: 0,
        };
        r.line(header.iter().copied())?;
        Ok(r)
    }

    fn line<'s>(&mut self, cells: impl Iterator<Item = &'s str>) -> Result<(), String> {
        let line: Vec<Cow<str>> = cells.map(field).collect();
        writeln!(self.out, "{}", line.join(","))
            .map_err(|e| format!("write {}: {e}", self.path.display()))
    }

    /// Append one row (as many cells as the header has columns).
    pub fn row<S: AsRef<str>>(&mut self, cells: &[S]) -> Result<(), String> {
        if cells.len() != self.columns {
            return Err(format!(
                "row has {} cells, the header {} columns",
                cells.len(),
                self.columns
            ));
        }
        self.line(cells.iter().map(AsRef::as_ref))?;
        self.rows += 1;
        Ok(())
    }

    pub fn rows(&self) -> u64 {
        self.rows
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Flush and close; returns the file written.
    pub fn finish(mut self) -> Result<PathBuf, String> {
        self.out
            .flush()
            .map_err(|e| format!("write {}: {e}", self.path.display()))?;
        Ok(self.path)
    }
}

/// A Record / Stop button with the row count, owning the recorder.
#[derive(Default)]
pub struct CsvRecordControl {
    rec: Option<CsvRecorder>,
    note: Option<String>,
}

impl CsvRecordControl {
    pub fn is_recording(&self) -> bool {
        self.rec.is_some()
    }

    /// Write a row while recording (a no-op otherwise). A write error stops
    /// the recording and is shown next to the button.
    pub fn push<S: AsRef<str>>(&mut self, cells: &[S]) {
        if let Some(r) = &mut self.rec {
            if let Err(e) = r.row(cells) {
                self.note = Some(e);
                self.rec = None;
            }
        }
    }

    /// The control. `default_name` seeds the save dialog; `header` is used
    /// when a recording starts.
    pub fn ui(&mut self, ui: &mut Ui, host: &Host<'_>, default_name: &str, header: &[&str]) {
        let t = *host.tokens();
        ui.horizontal(|ui| match self.rec.take() {
            Some(r) => {
                if theme::button(ui, &t, Some(icons::STOP), "Stop recording").clicked() {
                    self.note = Some(match r.finish() {
                        Ok(p) => format!("saved {}", p.display()),
                        Err(e) => e,
                    });
                } else {
                    ui.label(
                        RichText::new(format!("{} rows", r.rows()))
                            .font(font(MONO_MEDIUM, 12.0))
                            .color(t.crit),
                    );
                    self.rec = Some(r);
                }
            }
            None => {
                if theme::button(ui, &t, Some(icons::RECORD), "Record CSV").clicked() {
                    if let Some(p) = host.pick_save("Record to CSV", default_name) {
                        match CsvRecorder::create(&p, header) {
                            Ok(r) => {
                                self.rec = Some(r);
                                self.note = None;
                            }
                            Err(e) => self.note = Some(e),
                        }
                    }
                }
                if let Some(n) = &self.note {
                    ui.label(RichText::new(n).color(t.fg_dim));
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_quoted_rows_and_never_replaces_a_file() {
        let d = std::env::temp_dir().join(format!("ot-csv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("log.csv");
        let mut r = CsvRecorder::create(&p, &["t", "name"]).unwrap();
        r.row(&["0.1", "rpm, engine"]).unwrap();
        r.row(&["0.2".to_string(), "say \"hi\"".to_string()])
            .unwrap();
        assert!(r.row(&["only one"]).is_err());
        assert_eq!(r.rows(), 2);
        r.finish().unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "t,name\n0.1,\"rpm, engine\"\n0.2,\"say \"\"hi\"\"\"\n"
        );
        let e = CsvRecorder::create(&p, &["x"]).err().unwrap();
        assert!(e.contains("never replaces"), "{e}");
        assert_eq!(std::fs::read_to_string(&p).unwrap().lines().count(), 3);
        let _ = std::fs::remove_dir_all(&d);
    }
}
