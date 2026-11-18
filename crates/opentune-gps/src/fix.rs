//! Latest-fix state: merges RMC / GGA / VTG into one [`GpsFix`], counts every
//! accepted and rejected line in [`NmeaStats`], and answers "what do we know
//! right now, and how old is it" as a [`GpsSnapshot`].
//!
//! Time is passed in by the caller as monotonic milliseconds (`now_ms`): the
//! serial reader uses wall-clock time since it started, the file replay the
//! NMEA UTC timestamps. That keeps staleness testable without sleeping.

use crate::nmea::{parse_sentence, Body, NmeaError, Sentence, UtcDate, UtcTime};
use serde::{Deserialize, Serialize};

/// Default staleness limit: a 1 Hz receiver has missed a whole epoch.
pub const DEFAULT_STALE_AFTER_MS: u64 = 2000;

/// Longest line kept by [`LineAssembler`]. NMEA 0183 caps a sentence at 82
/// characters; anything far longer is noise or two lines run together.
pub const MAX_LINE: usize = 160;

/// Counters for every line seen. Rejections never stop a log; they are only
/// counted here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NmeaStats {
    /// Non-empty lines seen (accepted + rejected).
    pub lines: u64,
    /// Accepted `$--RMC` sentences.
    pub rmc: u64,
    /// Accepted `$--GGA` sentences.
    pub gga: u64,
    /// Accepted `$--VTG` sentences.
    pub vtg: u64,
    /// Checksum-valid sentences of other types (GSV, GSA, GLL, TXT, …).
    pub ignored: u64,
    /// Checksum mismatches.
    pub bad_checksum: u64,
    /// Lines without a `*hh` checksum (usually cut off mid-way).
    pub partial: u64,
    /// Supported sentences with an unparsable field, a bad address or a bad
    /// checksum field.
    pub malformed: u64,
    /// Lines not starting with `$` (noise, binary protocol bytes).
    pub noise: u64,
    /// Lines longer than [`MAX_LINE`], dropped by the assembler.
    pub overlong: u64,
}

impl NmeaStats {
    /// Accepted RMC + GGA + VTG sentences.
    pub fn accepted(&self) -> u64 {
        self.rmc + self.gga + self.vtg
    }

    /// Lines rejected for any reason (not counting ignored sentence types).
    pub fn rejected(&self) -> u64 {
        self.bad_checksum + self.partial + self.malformed + self.noise + self.overlong
    }

    fn count_error(&mut self, e: &NmeaError) {
        match e {
            NmeaError::Empty => {}
            NmeaError::NoStart => self.noise += 1,
            NmeaError::MissingChecksum => self.partial += 1,
            NmeaError::BadChecksum { .. } => self.bad_checksum += 1,
            NmeaError::Unsupported(_) => self.ignored += 1,
            NmeaError::BadChecksumField
            | NmeaError::BadAddress(_)
            | NmeaError::Malformed { .. } => self.malformed += 1,
        }
    }
}

impl std::fmt::Display for NmeaStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} line(s): {} RMC, {} GGA, {} VTG accepted; {} other ignored; rejected: \
             {} bad checksum, {} partial, {} malformed, {} noise, {} overlong",
            self.lines,
            self.rmc,
            self.gga,
            self.vtg,
            self.ignored,
            self.bad_checksum,
            self.partial,
            self.malformed,
            self.noise,
            self.overlong
        )
    }
}

/// The merged latest values. Fields keep their last reported value; whether
/// they are usable right now is [`GpsSnapshot::good`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GpsFix {
    /// Latitude, signed decimal degrees (north positive).
    pub lat: Option<f64>,
    /// Longitude, signed decimal degrees (east positive).
    pub lon: Option<f64>,
    /// Altitude above mean sea level, metres (GGA).
    pub alt_m: Option<f64>,
    /// Speed over ground, km/h (VTG km/h, else RMC/VTG knots converted).
    pub speed_kmh: Option<f64>,
    /// Course over ground (true), degrees.
    pub course_deg: Option<f64>,
    /// Satellites used (GGA).
    pub sats: Option<u8>,
    /// GGA fix quality (0 none, 1 GPS, 2 DGPS, 4 RTK fixed, 5 RTK float,
    /// 6 estimated, …).
    pub quality: Option<u8>,
    /// RMC status (`A` = true).
    pub rmc_valid: Option<bool>,
    /// Latest FAA mode indicator (RMC/VTG).
    pub mode: Option<char>,
    /// Horizontal dilution of precision (GGA).
    pub hdop: Option<f64>,
    /// Latest UTC time of day.
    pub time: Option<UtcTime>,
    /// Latest UTC date (RMC).
    pub date: Option<UtcDate>,
    /// Talker of the latest accepted sentence.
    pub talker: Option<String>,
}

impl GpsFix {
    /// The receiver reports a usable 2D/3D fix: a position, a fix quality of
    /// 1..=5 (not estimated/manual/simulated), RMC status not `V` and no
    /// `N`/`E`/`M`/`S` mode indicator. Says nothing about age.
    pub fn valid(&self) -> bool {
        let has_status = self.quality.is_some() || self.rmc_valid.is_some();
        let quality_ok = !matches!(self.quality, Some(q) if !(1..=5).contains(&q));
        let status_ok = self.rmc_valid != Some(false);
        let mode_ok = !matches!(self.mode, Some('N' | 'E' | 'M' | 'S'));
        self.lat.is_some() && self.lon.is_some() && has_status && quality_ok && status_ok && mode_ok
    }

    /// Numeric fix code for the `gps_fix` channel: the GGA quality when known,
    /// else 1 for an RMC `A` status and 0 otherwise.
    pub fn fix_code(&self) -> u8 {
        match (self.quality, self.rmc_valid) {
            (Some(q), _) => q,
            (None, Some(true)) => 1,
            _ => 0,
        }
    }

    /// Why [`valid`](Self::valid) is false (empty when it is true).
    pub fn why_invalid(&self) -> String {
        if self.valid() {
            return String::new();
        }
        let mut why = Vec::new();
        match self.quality {
            Some(0) => why.push("GGA quality 0".to_string()),
            Some(q) if q > 5 => why.push(format!("GGA quality {q} (not a measured fix)")),
            _ => {}
        }
        if self.rmc_valid == Some(false) {
            why.push("RMC status V".into());
        }
        if let Some(m @ ('N' | 'E' | 'M' | 'S')) = self.mode {
            why.push(format!("mode {m}"));
        }
        if self.lat.is_none() || self.lon.is_none() {
            why.push("no position yet".into());
        }
        if why.is_empty() {
            why.push("no RMC/GGA status yet".into());
        }
        format!("no fix ({})", why.join(", "))
    }
}

/// What a GPS source knows at one instant.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GpsSnapshot {
    /// The merged latest values.
    pub fix: GpsFix,
    /// Usable right now: not stale, a valid fix, position recently updated.
    pub good: bool,
    /// Nothing accepted for longer than the staleness limit (or ever).
    pub stale: bool,
    /// Milliseconds since the last valid position, if any.
    pub age_ms: Option<u64>,
    /// Milliseconds since the last accepted sentence, if any.
    pub silent_ms: Option<u64>,
    /// Why the snapshot is not `good` (empty when it is).
    pub note: String,
    /// Line counters.
    pub stats: NmeaStats,
    /// Set once the source has ended (end of file, port error), with why.
    pub ended: Option<String>,
}

/// Merges sentences into the latest fix and counts lines.
#[derive(Debug, Clone)]
pub struct FixTracker {
    fix: GpsFix,
    stats: NmeaStats,
    last_pos_ms: Option<u64>,
    last_sentence_ms: Option<u64>,
    /// Staleness limit in milliseconds.
    pub stale_after_ms: u64,
}

impl Default for FixTracker {
    fn default() -> Self {
        Self::new(DEFAULT_STALE_AFTER_MS)
    }
}

impl FixTracker {
    /// An empty tracker with the given staleness limit.
    pub fn new(stale_after_ms: u64) -> Self {
        Self {
            fix: GpsFix::default(),
            stats: NmeaStats::default(),
            last_pos_ms: None,
            last_sentence_ms: None,
            stale_after_ms,
        }
    }

    /// The counters so far.
    pub fn stats(&self) -> &NmeaStats {
        &self.stats
    }

    /// The merged fix so far.
    pub fn fix(&self) -> &GpsFix {
        &self.fix
    }

    /// Parse and apply one line, counting it. Returns the sentence if accepted.
    pub fn ingest_line(&mut self, line: &str, now_ms: u64) -> Option<Sentence> {
        match parse_sentence(line) {
            Ok(s) => {
                self.stats.lines += 1;
                self.ingest(&s, now_ms);
                Some(s)
            }
            Err(NmeaError::Empty) => None,
            Err(e) => {
                self.stats.lines += 1;
                self.stats.count_error(&e);
                None
            }
        }
    }

    /// Count a line the assembler dropped as too long.
    pub fn count_overlong(&mut self) {
        self.stats.lines += 1;
        self.stats.overlong += 1;
    }

    /// Apply an already-parsed sentence (does not touch `lines`).
    pub fn ingest(&mut self, s: &Sentence, now_ms: u64) {
        self.last_sentence_ms = Some(now_ms);
        self.fix.talker = Some(s.talker.clone());
        match &s.body {
            Body::Rmc(r) => {
                self.stats.rmc += 1;
                self.fix.time = r.time.or(self.fix.time);
                self.fix.date = r.date.or(self.fix.date);
                self.fix.rmc_valid = Some(r.valid);
                if r.mode.is_some() {
                    self.fix.mode = r.mode;
                }
                let usable = r.valid && !matches!(r.mode, Some('N' | 'E' | 'M' | 'S'));
                if usable {
                    if let Some((lat, lon)) = r.position {
                        self.fix.lat = Some(lat);
                        self.fix.lon = Some(lon);
                        self.last_pos_ms = Some(now_ms);
                    }
                    if let Some(k) = r.speed_knots {
                        self.fix.speed_kmh = Some(k * crate::nmea::KNOTS_TO_KMH);
                    }
                    if r.course_deg.is_some() {
                        self.fix.course_deg = r.course_deg;
                    }
                }
            }
            Body::Gga(g) => {
                self.stats.gga += 1;
                self.fix.time = g.time.or(self.fix.time);
                self.fix.quality = g.quality;
                self.fix.sats = g.sats;
                self.fix.hdop = g.hdop;
                let q = g.quality.unwrap_or(0);
                if q > 0 {
                    if let Some((lat, lon)) = g.position {
                        self.fix.lat = Some(lat);
                        self.fix.lon = Some(lon);
                        if q <= 5 {
                            self.last_pos_ms = Some(now_ms);
                        }
                    }
                    if g.alt_m.is_some() {
                        self.fix.alt_m = g.alt_m;
                    }
                }
            }
            Body::Vtg(v) => {
                self.stats.vtg += 1;
                if v.mode.is_some() {
                    self.fix.mode = v.mode;
                }
                if !matches!(v.mode, Some('N')) {
                    if let Some(kmh) = v.kmh() {
                        self.fix.speed_kmh = Some(kmh);
                    }
                    if v.course_deg.is_some() {
                        self.fix.course_deg = v.course_deg;
                    }
                }
            }
        }
    }

    /// What is known at `now_ms`.
    pub fn snapshot(&self, now_ms: u64) -> GpsSnapshot {
        let since = |t: Option<u64>| t.map(|t| now_ms.saturating_sub(t));
        let silent_ms = since(self.last_sentence_ms);
        let age_ms = since(self.last_pos_ms);
        let limit = self.stale_after_ms;
        let stale = silent_ms.map_or(true, |s| s > limit);
        let fresh_pos = age_ms.is_some_and(|a| a <= limit);
        let good = !stale && self.fix.valid() && fresh_pos;
        let note = if good {
            String::new()
        } else if silent_ms.is_none() {
            let rejected = self.stats.rejected();
            if rejected > 0 {
                format!("no valid NMEA yet ({rejected} line(s) rejected)")
            } else {
                "no NMEA received yet".into()
            }
        } else if stale {
            format!(
                "stale: no NMEA for {:.1} s",
                silent_ms.unwrap_or(0) as f64 / 1000.0
            )
        } else if !self.fix.valid() {
            self.fix.why_invalid()
        } else {
            format!(
                "position not updated for {:.1} s",
                age_ms.unwrap_or(0) as f64 / 1000.0
            )
        };
        GpsSnapshot {
            fix: self.fix.clone(),
            good,
            stale,
            age_ms,
            silent_ms,
            note,
            stats: self.stats.clone(),
            ended: None,
        }
    }
}

/// One item out of the [`LineAssembler`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineItem {
    /// A complete (or cut-off, see below) line, without CR/LF.
    Line(String),
    /// A line longer than [`MAX_LINE`] was dropped.
    Overlong,
}

/// Splits a byte stream into lines. Reads from a serial port arrive in
/// arbitrary chunks, so a sentence can span several reads. A `$` in the
/// middle of a line starts a new one and hands the cut-off part on as its
/// own line (it then fails as `partial`). Invalid UTF-8 is replaced, never
/// fatal.
#[derive(Debug, Default, Clone)]
pub struct LineAssembler {
    buf: Vec<u8>,
    overflow: bool,
}

impl LineAssembler {
    /// A new, empty assembler.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes; complete lines come out in order.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<LineItem> {
        let mut out = Vec::new();
        for &b in bytes {
            match b {
                b'\n' | b'\r' => self.flush(&mut out),
                b'$' if !self.buf.is_empty() || self.overflow => {
                    self.flush(&mut out);
                    self.buf.push(b);
                }
                _ if self.overflow => {}
                _ => {
                    self.buf.push(b);
                    if self.buf.len() > MAX_LINE {
                        self.buf.clear();
                        self.overflow = true;
                    }
                }
            }
        }
        out
    }

    /// Whatever is left at end of stream (a final line with no newline).
    pub fn finish(&mut self) -> Vec<LineItem> {
        let mut out = Vec::new();
        self.flush(&mut out);
        out
    }

    fn flush(&mut self, out: &mut Vec<LineItem>) {
        if self.overflow {
            self.overflow = false;
            out.push(LineItem::Overlong);
        } else if !self.buf.is_empty() {
            out.push(LineItem::Line(
                String::from_utf8_lossy(&self.buf).into_owned(),
            ));
        }
        self.buf.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nmea::checksum;

    fn s(body: &str) -> String {
        format!("${body}*{:02X}", checksum(body))
    }

    #[test]
    fn assembler_handles_split_and_cut_lines() {
        let mut a = LineAssembler::new();
        assert!(a.push(b"$GPGGA,1").is_empty());
        let out = a.push(b"23*00\r\n$GPRMC,12$GPVTG\r\n\r\n");
        assert_eq!(
            out,
            vec![
                LineItem::Line("$GPGGA,123*00".into()),
                LineItem::Line("$GPRMC,12".into()),
                LineItem::Line("$GPVTG".into()),
            ]
        );
        let long = vec![b'x'; MAX_LINE * 2];
        assert!(a.push(&long).is_empty());
        assert_eq!(a.push(b"\n"), vec![LineItem::Overlong]);
        assert_eq!(a.push(b"$tail"), vec![]);
        assert_eq!(a.finish(), vec![LineItem::Line("$tail".into())]);
    }

    #[test]
    fn staleness_and_validity() {
        let mut t = FixTracker::new(2000);
        let snap = t.snapshot(0);
        assert!(snap.stale && !snap.good);
        assert_eq!(snap.note, "no NMEA received yet");

        t.ingest_line(
            &s("GNGGA,120000.00,5000.000,N,00100.000,W,1,09,0.8,12.0,M,45.0,M,,"),
            1000,
        );
        t.ingest_line(
            &s("GNRMC,120000.00,A,5000.000,N,00100.000,W,54.0,90.0,010126,,,A"),
            1000,
        );
        let snap = t.snapshot(1500);
        assert!(snap.good, "{}", snap.note);
        assert_eq!(snap.fix.fix_code(), 1);
        assert!((snap.fix.speed_kmh.unwrap() - 100.008).abs() < 1e-9);
        assert_eq!(snap.fix.lon, Some(-1.0));
        assert_eq!(snap.age_ms, Some(500));

        // Silence past the limit: stale.
        let snap = t.snapshot(3001);
        assert!(snap.stale && !snap.good);
        assert!(
            snap.note.starts_with("stale: no NMEA for 2.0"),
            "{}",
            snap.note
        );

        // The receiver keeps talking but loses the fix.
        t.ingest_line(&s("GNGGA,120005.00,,,,,0,03,99.9,,,,,,"), 6000);
        t.ingest_line(&s("GNRMC,120005.00,V,,,,,,,010126,,,N"), 6000);
        let snap = t.snapshot(6000);
        assert!(!snap.stale && !snap.good);
        assert_eq!(snap.fix.fix_code(), 0);
        assert_eq!(snap.fix.sats, Some(3));
        assert!(snap.note.contains("GGA quality 0"), "{}", snap.note);
        // The last known position is kept, flagged by `good`.
        assert_eq!(snap.fix.lat, Some(50.0));

        // Estimated (dead reckoning) is not a good fix.
        t.ingest_line(
            &s("GNGGA,120006.00,5000.000,N,00100.000,W,6,00,,12.0,M,45.0,M,,"),
            7000,
        );
        t.ingest_line(
            &s("GNRMC,120006.00,A,5000.000,N,00100.000,W,0.0,90.0,010126,,,E"),
            7000,
        );
        let snap = t.snapshot(7000);
        assert!(!snap.good);
        assert_eq!(snap.fix.fix_code(), 6);
    }

    #[test]
    fn rejected_lines_are_counted_not_fatal() {
        let mut t = FixTracker::default();
        t.ingest_line("garbage", 0);
        t.ingest_line("$GPRMC,123519,A,48", 0);
        t.ingest_line(
            "$GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*00",
            0,
        );
        t.ingest_line(&s("GPGSV,1,1,00"), 0);
        t.ingest_line(&s("GPRMC,123519,Z,,,,,,,"), 0);
        t.ingest_line("", 0);
        t.count_overlong();
        let st = t.stats();
        assert_eq!(
            (
                st.lines,
                st.noise,
                st.partial,
                st.bad_checksum,
                st.ignored,
                st.malformed,
                st.overlong
            ),
            (6, 1, 1, 1, 1, 1, 1)
        );
        assert_eq!(st.rejected(), 5);
        assert_eq!(t.snapshot(0).note, "no valid NMEA yet (5 line(s) rejected)");
    }
}
