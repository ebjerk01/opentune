//! The trigger engine as a [`Write`] layer over a Live-log CSV stream.
//!
//! The logging loops ([`LiveSession::run_csv_resilient`], `obd read --out`)
//! write their CSV through a [`TriggerCsvWriter`] instead of the file: it
//! reads the header to find the `ts`, `param_id`, `value` and `units`
//! columns, groups the rows into rounds (consecutive rows with the same
//! `round` and `ts`; a link-event row is a round of its own), feeds each
//! round to the [`TriggerEngine`] and writes what it emits. Both Live-log
//! layouts work: `round,param_id,name,raw,value,units,note,text,ts`
//! (`opentune log`) and `ts,param_id,name,raw,value,units,note,text`
//! (`obd read`, the desktop Dashboard).
//!
//! A round is evaluated when the next one starts (or at
//! [`finish`](TriggerCsvWriter::finish)), so the last round of a log is only
//! written by `finish`, which must be called.
//!
//! [`LiveSession::run_csv_resilient`]: crate::LiveSession::run_csv_resilient

use super::eval::{Emit, MarkerKind, MarkerRow, TriggerEngine, TriggerReport};
use super::TriggerSet;
use crate::live::Sample;
use crate::resilient::LINK_PARAM_ID;
use std::io::{self, Write};

/// `param_id` of the trigger marker rows (window start/end, `mark`).
pub const MARKER_PARAM_ID: &str = "trigger";

/// A factory for per-window outputs: called with the 1-based window number.
pub type WindowFactory<'a> = Box<dyn FnMut(u32) -> io::Result<Box<dyn Write + 'a>> + 'a>;

/// Where a [`TriggerCsvWriter`] writes.
pub enum TriggerOutput<'a> {
    /// One stream: the header, then every window (or, without a gating
    /// trigger, every row plus `mark` rows).
    Single(Box<dyn Write + 'a>),
    /// One stream per recording window (header first in each), opened when
    /// the window opens and dropped when it closes.
    PerWindow(WindowFactory<'a>),
}

/// One buffered CSV line.
struct Line {
    text: String,
    round: String,
}

/// Column positions found in the header.
struct Cols {
    n: usize,
    ts: usize,
    param_id: usize,
    value: usize,
    units: Option<usize>,
    name: Option<usize>,
    note: Option<usize>,
    text: Option<usize>,
    round: Option<usize>,
}

impl Cols {
    fn from_header(h: &str) -> io::Result<Self> {
        let names: Vec<String> = split_csv(h)
            .into_iter()
            .map(|s| s.trim().trim_start_matches('\u{feff}').to_ascii_lowercase())
            .collect();
        let find = |n: &str| names.iter().position(|c| c == n);
        let (Some(ts), Some(param_id), Some(value)) = (find("ts"), find("param_id"), find("value"))
        else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("trigger: the CSV header has no ts/param_id/value columns: {h}"),
            ));
        };
        Ok(Cols {
            n: names.len(),
            ts,
            param_id,
            value,
            units: find("units"),
            name: find("name"),
            note: find("note"),
            text: find("text"),
            round: find("round"),
        })
    }
}

/// Runs a [`TriggerEngine`] over the Live-log CSV written into it. See the
/// [module docs](self).
pub struct TriggerCsvWriter<'a> {
    engine: TriggerEngine<Line>,
    out: TriggerOutput<'a>,
    current: Option<Box<dyn Write + 'a>>,
    header: Option<String>,
    cols: Option<Cols>,
    pending: Vec<u8>,
    group: Vec<(Line, Sample)>,
    group_key: Option<(String, u64)>,
    group_ts: u64,
    pending_start: Option<MarkerRow>,
    last_round: String,
    files: u32,
    bad_rows: u64,
}

impl<'a> TriggerCsvWriter<'a> {
    /// A writer applying `set` (validated here) and writing to `out`. A
    /// per-window output needs a gating (`record` / `split`) trigger.
    pub fn new(set: TriggerSet, out: TriggerOutput<'a>) -> Result<Self, String> {
        set.check()?;
        if matches!(out, TriggerOutput::PerWindow(_)) && !set.gates() {
            return Err(
                "one file per window needs a record or split trigger (only mark triggers given)"
                    .into(),
            );
        }
        Ok(Self {
            engine: TriggerEngine::new(set),
            out,
            current: None,
            header: None,
            cols: None,
            pending: Vec::new(),
            group: Vec::new(),
            group_key: None,
            group_ts: 0,
            pending_start: None,
            last_round: String::new(),
            files: 0,
            bad_rows: 0,
        })
    }

    /// The report so far (windows closed so far).
    pub fn report(&self) -> TriggerReport {
        self.engine.report()
    }

    /// Process what is left (a last line without a newline, the last
    /// round), close an open window, flush, and report.
    pub fn finish(mut self) -> io::Result<TriggerReport> {
        if !self.pending.is_empty() {
            let rest = std::mem::take(&mut self.pending);
            let line = String::from_utf8_lossy(&rest).into_owned();
            self.line(line.trim_end_matches(['\r', '\n']))?;
        }
        self.flush_group()?;
        let emits = self.engine.finish();
        self.apply(emits)?;
        self.flush_outputs()?;
        let mut r = self.engine.report();
        if self.bad_rows > 0 {
            r.warnings.push(format!(
                "{} row(s) without a usable ts kept the previous round's time",
                self.bad_rows
            ));
        }
        Ok(r)
    }

    /// Files (per-window outputs) opened so far.
    pub fn files_opened(&self) -> u32 {
        self.files
    }

    // ---- internals ----

    fn line(&mut self, line: &str) -> io::Result<()> {
        if self.cols.is_none() {
            if line.trim().is_empty() {
                return Ok(());
            }
            self.cols = Some(Cols::from_header(line)?);
            self.header = Some(line.to_string());
            if let TriggerOutput::Single(w) = &mut self.out {
                writeln!(w, "{line}")?;
            }
            return Ok(());
        }
        if line.is_empty() {
            return Ok(());
        }
        let cols = self.cols.as_ref().expect("header seen");
        let f = split_csv(line);
        let get = |i: usize| f.get(i).map(String::as_str).unwrap_or("");
        let opt = |i: Option<usize>| i.map(get).unwrap_or("");
        let ts = match get(cols.ts).trim().parse::<u64>() {
            Ok(t) => t,
            Err(_) => {
                self.bad_rows += 1;
                self.group_ts
            }
        };
        let round = opt(cols.round).to_string();
        let param_id = get(cols.param_id).to_string();
        let value = get(cols.value).trim().parse::<f64>().ok();
        let text = Some(opt(cols.text))
            .filter(|s| !s.is_empty())
            .map(String::from);
        let sample = Sample {
            name: opt(cols.name).to_string(),
            raw: None,
            value,
            units: opt(cols.units).to_string(),
            note: opt(cols.note).to_string(),
            text,
            param_id,
        };
        let line = Line {
            text: line.to_string(),
            round: round.clone(),
        };
        let link = sample.param_id == LINK_PARAM_ID;
        let key = (round, ts);
        if link || self.group_key.as_ref() != Some(&key) {
            self.flush_group()?;
        }
        self.group_ts = ts;
        self.group.push((line, sample));
        if link {
            self.flush_group()?;
        } else {
            self.group_key = Some(key);
        }
        Ok(())
    }

    fn flush_group(&mut self) -> io::Result<()> {
        self.group_key = None;
        if self.group.is_empty() {
            return Ok(());
        }
        let (lines, samples): (Vec<Line>, Vec<Sample>) =
            std::mem::take(&mut self.group).into_iter().unzip();
        let emits = self.engine.push(self.group_ts, &samples, lines);
        self.apply(emits)
    }

    fn apply(&mut self, emits: Vec<Emit<Line>>) -> io::Result<()> {
        for e in emits {
            match e {
                Emit::Open { window } => {
                    if let TriggerOutput::PerWindow(make) = &mut self.out {
                        if let Some(mut w) = self.current.take() {
                            w.flush()?;
                        }
                        let mut w = make(window)?;
                        self.files += 1;
                        if let Some(h) = &self.header {
                            writeln!(w, "{h}")?;
                        }
                        self.current = Some(w);
                    }
                }
                Emit::Marker(m) if m.kind == MarkerKind::Start => self.pending_start = Some(m),
                Emit::Marker(m) => {
                    self.write_pending_start(None)?;
                    let round = self.last_round.clone();
                    self.write_marker(&m, &round)?;
                }
                Emit::Row(l) => {
                    self.write_pending_start(Some(&l.round))?;
                    self.write_line(&l.text)?;
                    self.last_round = l.round;
                }
                Emit::Close { .. } => {
                    self.write_pending_start(None)?;
                    if let Some(mut w) = self.current.take() {
                        w.flush()?;
                    }
                }
            }
        }
        Ok(())
    }

    fn write_pending_start(&mut self, round: Option<&str>) -> io::Result<()> {
        if let Some(m) = self.pending_start.take() {
            let round = round.map_or_else(|| self.last_round.clone(), str::to_string);
            self.write_marker(&m, &round)?;
        }
        Ok(())
    }

    fn write_marker(&mut self, m: &MarkerRow, round: &str) -> io::Result<()> {
        let line = {
            let c = self.cols.as_ref().expect("markers follow rows");
            let mut f = vec![String::new(); c.n];
            f[c.ts] = m.ts.to_string();
            f[c.param_id] = MARKER_PARAM_ID.into();
            if let Some(i) = c.round {
                f[i] = round.to_string();
            }
            if let Some(i) = c.name {
                f[i] = clean(&m.trigger);
            }
            if let Some(i) = c.note {
                f[i] = clean(&m.note);
            }
            if let Some(i) = c.text {
                f[i] = m.kind.as_str().into();
            }
            f.join(",")
        };
        self.write_line(&line)
    }

    fn write_line(&mut self, line: &str) -> io::Result<()> {
        let w: &mut dyn Write = match (&mut self.out, &mut self.current) {
            (TriggerOutput::Single(w), _) => w.as_mut(),
            (TriggerOutput::PerWindow(_), Some(w)) => w.as_mut(),
            // Per-window output with no window open: nothing is recorded.
            (TriggerOutput::PerWindow(_), None) => return Ok(()),
        };
        writeln!(w, "{line}")
    }

    fn flush_outputs(&mut self) -> io::Result<()> {
        if let TriggerOutput::Single(w) = &mut self.out {
            w.flush()?;
        }
        if let Some(w) = self.current.as_mut() {
            w.flush()?;
        }
        Ok(())
    }
}

impl Write for TriggerCsvWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.pending.extend_from_slice(buf);
        let mut start = 0;
        let mut quotes = 0usize;
        let mut i = 0;
        while i < self.pending.len() {
            match self.pending[i] {
                b'"' => quotes += 1,
                // A line break inside a quoted field is part of the field.
                b'\n' if quotes % 2 == 0 => {
                    let raw = self.pending[start..i].to_vec();
                    let line = String::from_utf8_lossy(&raw);
                    self.line(line.trim_end_matches('\r'))?;
                    start = i + 1;
                    quotes = 0;
                }
                _ => {}
            }
            i += 1;
        }
        self.pending.drain(..start);
        Ok(buf.len())
    }

    /// Flushes the outputs. A round still being collected is not evaluated
    /// (its end is not known yet); [`finish`](TriggerCsvWriter::finish) does
    /// that.
    fn flush(&mut self) -> io::Result<()> {
        self.flush_outputs()
    }
}

/// A marker field: single-line and comma-free, like the logger's own rows.
fn clean(s: &str) -> String {
    s.replace(['\n', '\r', ','], " ").replace('"', "'")
}

/// Split one CSV line (RFC 4180 quoting: `"a,b"`, `""` for a quote).
fn split_csv(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('"', true) if chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            ('"', true) => quoted = false,
            ('"', false) if cur.is_empty() => quoted = true,
            (',', false) => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Clone, Default)]
    struct Shared(Rc<RefCell<Vec<u8>>>);
    impl Write for Shared {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl Shared {
        fn text(&self) -> String {
            String::from_utf8(self.0.borrow().clone()).unwrap()
        }
    }

    const SET: &str = r#"{"triggers":[
      {"name":"wot","action":"record","when":{"channel":["P0C","rpm"],"op":">","value":3000,"units":"rpm"},"post_ms":100},
      {"name":"hot","action":"mark","when":{"channel":"ect","op":">","value":105,"units":"°C"}}
    ]}"#;

    #[test]
    fn splits_quoted_fields() {
        assert_eq!(
            split_csv(r#"1,"a,b","c ""q""",,x"#),
            ["1", "a,b", "c \"q\"", "", "x"]
        );
    }

    #[test]
    fn gates_a_logger_csv_and_keeps_the_layout() {
        let sink = Shared::default();
        let set = TriggerSet::load_json(SET).unwrap();
        let mut w =
            TriggerCsvWriter::new(set, TriggerOutput::Single(Box::new(sink.clone()))).unwrap();
        let mut csv = String::from("round,param_id,name,raw,value,units,note,text,ts\n");
        for (r, rpm) in [1000, 4000, 1000, 1000, 1000].iter().enumerate() {
            let ts = 1000 + r * 100;
            csv += &format!("{r},P0C,Engine speed,,{rpm},rpm,,,{ts}\n");
            csv += &format!(
                "{r},ect,Coolant,,{},°C,,,{ts}\n",
                if r == 2 { 110 } else { 90 }
            );
        }
        csv += "5,link,Link status,,,,link lost after 3 dead round(s),lost,1500\n";
        // Written in odd-sized chunks, as a BufWriter would.
        for chunk in csv.as_bytes().chunks(7) {
            w.write_all(chunk).unwrap();
        }
        let r = w.finish().unwrap();
        let text = sink.text();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[..9],
            [
                "round,param_id,name,raw,value,units,note,text,ts",
                "1,trigger,wot,,,,window 1 start: wot fired at +0 ms: P0C > 3000 rpm,start,1100",
                "1,P0C,Engine speed,,4000,rpm,,,1100",
                "1,ect,Coolant,,90,°C,,,1100",
                "2,P0C,Engine speed,,1000,rpm,,,1200",
                "2,ect,Coolant,,110,°C,,,1200",
                "2,trigger,hot,,,,hot fired: ect > 105 °C,mark,1200",
                "3,P0C,Engine speed,,1000,rpm,,,1300",
                "3,ect,Coolant,,90,°C,,,1300",
            ]
        );
        assert!(
            lines[9].starts_with("3,trigger,wot,,,,window 1 end: 0.200 s"),
            "{text}"
        );
        assert!(lines[9].ends_with(",end,1300"), "{text}");
        assert_eq!(lines.len(), 10, "{text}");
        assert_eq!(r.total_fires(), 2);
        assert_eq!(r.marks, 1);
        assert_eq!(r.link_rounds, 1);
        assert_eq!(r.windows.len(), 1);
    }

    #[test]
    fn one_file_per_window_for_the_obd_layout() {
        let files: Rc<RefCell<Vec<Shared>>> = Rc::default();
        let f2 = Rc::clone(&files);
        let set = TriggerSet::load_json(SET).unwrap();
        let mut w = TriggerCsvWriter::new(
            set,
            TriggerOutput::PerWindow(Box::new(move |n: u32| -> io::Result<Box<dyn Write>> {
                assert_eq!(n as usize, f2.borrow().len() + 1);
                let s = Shared::default();
                f2.borrow_mut().push(s.clone());
                Ok(Box::new(s))
            })),
        )
        .unwrap();
        writeln!(w, "ts,param_id,name,raw,value,units,note,text").unwrap();
        for (i, rpm) in [4000, 1000, 1000, 4000, 4000, 1000, 1000]
            .iter()
            .enumerate()
        {
            let ts = 1000 + i * 100;
            writeln!(w, "{ts},rpm,\"Engine speed, crank\",,{rpm},rpm,,").unwrap();
        }
        let r = w.finish().unwrap();
        let files = files.borrow();
        assert_eq!(files.len(), 2);
        assert_eq!(r.windows.len(), 2);
        for f in files.iter() {
            let t = f.text();
            let lines: Vec<&str> = t.lines().collect();
            assert_eq!(lines[0], "ts,param_id,name,raw,value,units,note,text");
            assert!(
                lines[1].contains(",trigger,wot,") && lines[1].ends_with(",start"),
                "{t}"
            );
            assert!(lines.last().unwrap().ends_with(",end"), "{t}");
            assert!(lines[2].contains("\"Engine speed, crank\""), "{t}");
        }
        assert_eq!(files[0].text().lines().count(), 1 + 1 + 3 + 1);
        assert_eq!(files[1].text().lines().count(), 1 + 1 + 4 + 1);
    }

    #[test]
    fn rejects_csv_without_the_needed_columns_and_mark_only_split() {
        let set = TriggerSet::load_json(SET).unwrap();
        let mut w =
            TriggerCsvWriter::new(set, TriggerOutput::Single(Box::new(Vec::new()))).unwrap();
        assert!(writeln!(w, "time,rpm").is_err());
        let marks = TriggerSet::load_json(
            r#"{"triggers":[{"name":"m","action":"mark","when":{"channel":"c","op":">","value":1}}]}"#,
        )
        .unwrap();
        let per = TriggerOutput::PerWindow(Box::new(|_: u32| -> io::Result<Box<dyn Write>> {
            Ok(Box::new(Vec::new()))
        }));
        assert!(TriggerCsvWriter::new(marks, per).is_err());
    }
}
