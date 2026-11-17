//! Streaming CAN log writers.
//!
//! All three formats are written frame by frame; [`FrameWriter`] flushes the
//! underlying writer at most every [`FLUSH_EVERY`] frames or
//! [`FLUSH_INTERVAL_US`] of capture time, and on [`FrameWriter::finish`].
//!
//! | Format | Example line |
//! |---|---|
//! | `candump` | `(1700000000.123456) can0 7E8#04410C1A` |
//! | `csv` | `1700000000.123456,7E8,0,4,04410C1A` |
//! | `asc` | `   0.012345 1  7E8             Rx   d 4 04 41 0C 1A` |
//!
//! Timestamps in candump and CSV are absolute Unix seconds; ASC uses seconds
//! since the start of measurement (`timestamps absolute` in Vector terms), with
//! the start date in the header.
use opentune_core::TimedFrame;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::str::FromStr;

/// Flush after this many frames…
pub const FLUSH_EVERY: u64 = 256;
/// …or this much capture time, whichever comes first.
pub const FLUSH_INTERVAL_US: u64 = 500_000;

/// Output log format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    /// Linux can-utils `candump -l` log format (replayable with `canplayer`).
    #[default]
    Candump,
    /// `ts,id,ext,dlc,data` with a header row.
    Csv,
    /// Vector ASCII log (`.asc`), readable by CANalyzer / python-can / SavvyCAN.
    Asc,
}

impl Format {
    /// Stable name: `candump`, `csv`, `asc`.
    pub fn as_str(self) -> &'static str {
        match self {
            Format::Candump => "candump",
            Format::Csv => "csv",
            Format::Asc => "asc",
        }
    }

    /// Conventional file extension.
    pub fn extension(self) -> &'static str {
        match self {
            Format::Candump => "log",
            Format::Csv => "csv",
            Format::Asc => "asc",
        }
    }
}

impl FromStr for Format {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "candump" | "log" => Ok(Format::Candump),
            "csv" => Ok(Format::Csv),
            "asc" | "vector" => Ok(Format::Asc),
            other => Err(format!(
                "unknown format '{other}' (expected candump, csv or asc)"
            )),
        }
    }
}

/// A streaming log writer for one capture.
pub struct FrameWriter<W: Write> {
    out: W,
    format: Format,
    channel: String,
    wall_base_us: u64,
    started: bool,
    finished: bool,
    bytes: u64,
    since_flush: u64,
    last_flush_us: u64,
    line: String,
}

impl<W: Write> FrameWriter<W> {
    /// A writer for `format`. `channel` is candump's interface column (e.g.
    /// `can0`); `wall_base_us` is the Unix time (µs) of `ts_us == 0`.
    pub fn new(out: W, format: Format, channel: &str, wall_base_us: u64) -> Self {
        Self {
            out,
            format,
            channel: if channel.trim().is_empty() {
                "can0".into()
            } else {
                channel.trim().to_string()
            },
            wall_base_us,
            started: false,
            finished: false,
            bytes: 0,
            since_flush: 0,
            last_flush_us: 0,
            line: String::with_capacity(128),
        }
    }

    /// The format being written.
    pub fn format(&self) -> Format {
        self.format
    }

    /// Bytes written to the output so far (header included).
    pub fn bytes_written(&self) -> u64 {
        self.bytes
    }

    fn emit(&mut self, s: &str) -> io::Result<()> {
        self.out.write_all(s.as_bytes())?;
        self.bytes += s.len() as u64;
        Ok(())
    }

    /// Write the format header, if any. Called automatically by the first
    /// [`write`](FrameWriter::write) / [`finish`](FrameWriter::finish).
    pub fn start(&mut self) -> io::Result<()> {
        if self.started {
            return Ok(());
        }
        self.started = true;
        let header = match self.format {
            Format::Candump => String::new(),
            Format::Csv => "ts,id,ext,dlc,data\n".to_string(),
            Format::Asc => {
                let date = asc_date(self.wall_base_us);
                format!(
                    "date {date}\nbase hex  timestamps absolute\ninternal events logged\n\
                     // version 9.0.0\nBegin Triggerblock {date}\n   0.000000 Start of measurement\n"
                )
            }
        };
        self.emit(&header)
    }

    /// Append one frame.
    pub fn write(&mut self, f: &TimedFrame) -> io::Result<()> {
        self.start()?;
        let mut line = std::mem::take(&mut self.line);
        line.clear();
        match self.format {
            Format::Candump => self.fmt_candump(&mut line, f),
            Format::Csv => self.fmt_csv(&mut line, f),
            Format::Asc => fmt_asc(&mut line, f),
        }
        let r = self.emit(&line);
        self.line = line;
        r?;
        self.since_flush += 1;
        if self.since_flush >= FLUSH_EVERY
            || f.ts_us.saturating_sub(self.last_flush_us) >= FLUSH_INTERVAL_US
        {
            self.flush_at(f.ts_us)?;
        }
        Ok(())
    }

    /// Flush if the flush interval has passed at capture time `now_us` (call
    /// on quiet intervals so a slow bus still reaches the disk).
    pub fn tick(&mut self, now_us: u64) -> io::Result<()> {
        if self.since_flush > 0 && now_us.saturating_sub(self.last_flush_us) >= FLUSH_INTERVAL_US {
            self.flush_at(now_us)?;
        }
        Ok(())
    }

    fn flush_at(&mut self, now_us: u64) -> io::Result<()> {
        self.out.flush()?;
        self.since_flush = 0;
        self.last_flush_us = now_us;
        Ok(())
    }

    /// Write the footer (ASC `End TriggerBlock`) and flush. Idempotent.
    pub fn finish(&mut self) -> io::Result<()> {
        self.start()?;
        if !self.finished {
            self.finished = true;
            if self.format == Format::Asc {
                self.emit("End TriggerBlock\n")?;
            }
        }
        self.out.flush()
    }

    /// Finish and return the underlying writer.
    pub fn into_inner(mut self) -> io::Result<W> {
        self.finish()?;
        Ok(self.out)
    }

    fn abs_ts(&self, ts_us: u64) -> String {
        let t = self.wall_base_us + ts_us;
        format!("{}.{:06}", t / 1_000_000, t % 1_000_000)
    }

    fn fmt_candump(&self, line: &mut String, f: &TimedFrame) {
        let _ = write!(line, "({}) {} ", self.abs_ts(f.ts_us), self.channel);
        push_id(line, f);
        if f.frame.fd {
            // can-utils CAN-FD: `<id>##<flags><data>`, flags nibble BRS=1.
            let flags = u8::from(f.frame.brs);
            let _ = write!(line, "##{flags:X}");
        } else if f.rtr {
            line.push_str("#R");
        } else {
            line.push('#');
        }
        if !f.rtr {
            push_hex(line, f.data(), "");
        }
        line.push('\n');
    }

    fn fmt_csv(&self, line: &mut String, f: &TimedFrame) {
        let _ = write!(line, "{},", self.abs_ts(f.ts_us));
        push_id(line, f);
        let _ = write!(line, ",{},{},", u8::from(f.extended), f.data().len());
        push_hex(line, f.data(), "");
        line.push('\n');
    }
}

/// `7E8` (3 digits) for standard ids, `18DAF110` (8 digits) for extended.
fn push_id(line: &mut String, f: &TimedFrame) {
    if f.extended {
        let _ = write!(line, "{:08X}", f.id() & 0x1FFF_FFFF);
    } else {
        let _ = write!(line, "{:03X}", f.id() & 0x7FF);
    }
}

fn push_hex(line: &mut String, data: &[u8], sep: &str) {
    for (i, b) in data.iter().enumerate() {
        if i > 0 {
            line.push_str(sep);
        }
        let _ = write!(line, "{b:02X}");
    }
}

/// Uppercase contiguous hex of `data` (used by stats/UI too).
pub fn hex(data: &[u8]) -> String {
    let mut s = String::with_capacity(data.len() * 2);
    push_hex(&mut s, data, "");
    s
}

/// One ASC message line (channel 1, direction Rx), python-can/Vector layout.
fn fmt_asc(line: &mut String, f: &TimedFrame) {
    let secs = f.ts_us as f64 / 1e6;
    let mut id = if f.extended {
        format!("{:X}x", f.id() & 0x1FFF_FFFF)
    } else {
        format!("{:X}", f.id() & 0x7FF)
    };
    if f.frame.fd {
        // CANFD <ch> <dir> <id> <symbolic name> <brs> <esi> <dlc> <len> <data>
        // <duration> <msg length> <flags> <crc> <bit timing ×4>
        let len = f.data().len();
        let dlc = f.frame.dlc().unwrap_or(0);
        let flags: u32 = (1 << 12) | if f.frame.brs { 1 << 13 } else { 0 };
        let _ = write!(
            line,
            "{secs:>11.6} CANFD {ch:>3} {dir:<4} {id:>8}  {name:>32} {brs} {esi} {dlc:x} {len:>2} ",
            ch = 1,
            dir = "Rx",
            name = "",
            brs = u8::from(f.frame.brs),
            esi = 0,
        );
        push_hex(line, f.data(), " ");
        let _ = write!(
            line,
            " {:>8} {:>4} {flags:>8X} {:>8} {:>8} {:>8} {:>8} {:>8}",
            0, 0, 0, 0, 0, 0, 0
        );
        line.push('\n');
        return;
    }
    if id.len() < 15 {
        id = format!("{id:<15}");
    }
    if f.rtr {
        let _ = writeln!(line, "{secs:>11.6} 1  {id} Rx   r");
        return;
    }
    let _ = write!(line, "{secs:>11.6} 1  {id} Rx   d {:x}", f.data().len());
    if !f.data().is_empty() {
        line.push(' ');
        push_hex(line, f.data(), " ");
    }
    line.push('\n');
}

/// Vector ASC header date (UTC), e.g. `Tue Nov 14 10:13:20.123 pm 2023`.
pub fn asc_date(unix_us: u64) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let secs = unix_us / 1_000_000;
    let ms = (unix_us % 1_000_000) / 1000;
    let days = (secs / 86_400) as i64;
    let sod = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    let wd = DAYS[((days + 4).rem_euclid(7)) as usize];
    let (h, min, s) = (sod / 3600, (sod / 60) % 60, sod % 60);
    let (h12, ampm) = match h {
        0 => (12, "am"),
        1..=11 => (h, "am"),
        12 => (12, "pm"),
        _ => (h - 12, "pm"),
    };
    format!(
        "{wd} {} {d:02} {h12:02}:{min:02}:{s:02}.{ms:03} {ampm} {y}",
        MONTHS[(m - 1) as usize]
    )
}

/// Days since 1970-01-01 → (year, month 1..=12, day 1..=31). H. Hinnant's
/// civil-from-days algorithm (proleptic Gregorian).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentune_core::protocol::CanFrame;

    /// 2023-11-14 22:13:20 UTC.
    const BASE: u64 = 1_700_000_000_000_000;

    fn frames() -> Vec<TimedFrame> {
        vec![
            TimedFrame::classic(123_456, 0x7E8, false, vec![0x04, 0x41, 0x0C, 0x1A]),
            TimedFrame::classic(
                1_000_001,
                0x18DA_F110,
                true,
                vec![0x03, 0x41, 0x0D, 0x00, 0xAA, 0xAA, 0xAA, 0xAA],
            ),
            TimedFrame {
                ts_us: 2_000_000,
                extended: false,
                rtr: true,
                frame: CanFrame::classic(0x123, vec![]),
            },
            TimedFrame {
                ts_us: 2_500_000,
                extended: false,
                rtr: false,
                frame: CanFrame::fd(0x456, (0..12).collect(), true),
            },
        ]
    }

    fn render(format: Format) -> String {
        let mut w = FrameWriter::new(Vec::new(), format, "can0", BASE);
        for f in frames() {
            w.write(&f).unwrap();
        }
        let n = w.bytes_written();
        let out = String::from_utf8(w.into_inner().unwrap()).unwrap();
        // bytes_written covers everything but the ASC footer.
        assert!(out.len() as u64 >= n);
        out
    }

    #[test]
    fn candump_golden() {
        assert_eq!(
            render(Format::Candump),
            "(1700000000.123456) can0 7E8#04410C1A\n\
             (1700000001.000001) can0 18DAF110#03410D00AAAAAAAA\n\
             (1700000002.000000) can0 123#R\n\
             (1700000002.500000) can0 456##1000102030405060708090A0B\n"
        );
    }

    #[test]
    fn csv_golden() {
        assert_eq!(
            render(Format::Csv),
            "ts,id,ext,dlc,data\n\
             1700000000.123456,7E8,0,4,04410C1A\n\
             1700000001.000001,18DAF110,1,8,03410D00AAAAAAAA\n\
             1700000002.000000,123,0,0,\n\
             1700000002.500000,456,0,12,000102030405060708090A0B\n"
        );
    }

    #[test]
    fn asc_golden() {
        let out = render(Format::Asc);
        let expected = "date Tue Nov 14 10:13:20.000 pm 2023\n\
             base hex  timestamps absolute\n\
             internal events logged\n\
             // version 9.0.0\n\
             Begin Triggerblock Tue Nov 14 10:13:20.000 pm 2023\n   \
             0.000000 Start of measurement\n   \
             0.123456 1  7E8             Rx   d 4 04 41 0C 1A\n   \
             1.000001 1  18DAF110x       Rx   d 8 03 41 0D 00 AA AA AA AA\n   \
             2.000000 1  123             Rx   r\n   \
             2.500000 CANFD   1 Rx        456                                   1 0 9 12 \
             00 01 02 03 04 05 06 07 08 09 0A 0B        0    0     3000        0        0        0        0        0\n\
             End TriggerBlock\n";
        assert_eq!(out, expected);
    }

    #[test]
    fn empty_capture_still_has_header_and_footer() {
        let w = FrameWriter::new(Vec::new(), Format::Asc, "can0", BASE);
        let out = String::from_utf8(w.into_inner().unwrap()).unwrap();
        assert!(out.starts_with("date "));
        assert!(out.ends_with("End TriggerBlock\n"));
        let w = FrameWriter::new(Vec::new(), Format::Candump, "", BASE);
        assert!(w.into_inner().unwrap().is_empty());
    }

    #[test]
    fn format_parse() {
        assert_eq!("CSV".parse::<Format>().unwrap(), Format::Csv);
        assert_eq!("".parse::<Format>().unwrap(), Format::Candump);
        assert_eq!("asc".parse::<Format>().unwrap().extension(), "asc");
        assert!("pcap".parse::<Format>().is_err());
    }

    #[test]
    fn asc_dates() {
        assert_eq!(asc_date(0), "Thu Jan 01 12:00:00.000 am 1970");
        // 2024-02-29 12:30:05.250 UTC (leap day, noon → pm)
        assert_eq!(
            asc_date(1_709_209_805_250_000),
            "Thu Feb 29 12:30:05.250 pm 2024"
        );
    }

    /// A writer that counts flushes.
    struct Counting {
        flushes: usize,
    }
    impl Write for Counting {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    #[test]
    fn flushes_periodically() {
        let mut w = FrameWriter::new(Counting { flushes: 0 }, Format::Candump, "can0", 0);
        // 1000 frames 1 ms apart: flush by count (every 256) and by time (500 ms).
        for i in 0..1000u64 {
            w.write(&TimedFrame::classic(i * 1000, 0x100, false, vec![1]))
                .unwrap();
        }
        let n = w.out.flushes;
        assert!((3..=6).contains(&n), "flushes = {n}");
        w.tick(5_000_000).unwrap();
        assert!(w.out.flushes >= n);
    }
}
