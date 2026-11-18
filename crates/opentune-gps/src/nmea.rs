//! NMEA 0183 sentence parser for the three sentences a datalogger needs:
//! `$--RMC` (recommended minimum), `$--GGA` (fix data) and `$--VTG` (course
//! and ground speed), from any two-letter talker (`GP`, `GN`, `GL`, `GA`,
//! `GB`, `BD`, `GQ`, …).
//!
//! Every sentence must carry its `*hh` checksum (XOR of the bytes between `$`
//! and `*`); a missing or wrong one is rejected. Empty fields are `None`, never
//! an error; a non-empty field that does not parse is [`NmeaError::Malformed`].
//! Nothing here panics on any input.

use serde::{Deserialize, Serialize};

/// Knots to km/h (1 international nautical mile = 1852 m).
pub const KNOTS_TO_KMH: f64 = 1.852;

/// A UTC time of day as NMEA carries it (`hhmmss[.sss]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UtcTime {
    /// Hour, 0..=23.
    pub hour: u8,
    /// Minute, 0..=59.
    pub minute: u8,
    /// Second, 0..=60 (60 = leap second).
    pub second: u8,
    /// Milliseconds, 0..=999.
    pub millis: u16,
}

impl UtcTime {
    /// Milliseconds since midnight.
    pub fn ms_of_day(&self) -> u64 {
        ((u64::from(self.hour) * 60 + u64::from(self.minute)) * 60 + u64::from(self.second)) * 1000
            + u64::from(self.millis)
    }
}

impl std::fmt::Display for UtcTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:02}:{:02}:{:02}.{:03}",
            self.hour, self.minute, self.second, self.millis
        )
    }
}

/// A UTC date from `$--RMC` (`ddmmyy`; two-digit years map to 1980..=2079).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UtcDate {
    /// Full year.
    pub year: u16,
    /// Month, 1..=12.
    pub month: u8,
    /// Day of month, 1..=31.
    pub day: u8,
}

impl std::fmt::Display for UtcDate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

/// `$--RMC`: recommended minimum navigation data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rmc {
    /// UTC time of the fix.
    pub time: Option<UtcTime>,
    /// Status `A` (valid). `V` (warning / no fix) is `false`.
    pub valid: bool,
    /// Latitude, longitude in signed decimal degrees.
    pub position: Option<(f64, f64)>,
    /// Speed over ground, knots.
    pub speed_knots: Option<f64>,
    /// Course over ground (true), degrees.
    pub course_deg: Option<f64>,
    /// UTC date.
    pub date: Option<UtcDate>,
    /// FAA mode indicator (NMEA 2.3+): `A` autonomous, `D` differential,
    /// `E` estimated, `F` float RTK, `R` RTK, `M` manual, `S` simulator,
    /// `N` not valid.
    pub mode: Option<char>,
}

/// `$--GGA`: fix data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Gga {
    /// UTC time of the fix.
    pub time: Option<UtcTime>,
    /// Latitude, longitude in signed decimal degrees.
    pub position: Option<(f64, f64)>,
    /// Fix quality: 0 invalid, 1 GPS, 2 DGPS, 3 PPS, 4 RTK fixed, 5 RTK float,
    /// 6 estimated (dead reckoning), 7 manual, 8 simulation.
    pub quality: Option<u8>,
    /// Satellites used in the fix.
    pub sats: Option<u8>,
    /// Horizontal dilution of precision.
    pub hdop: Option<f64>,
    /// Altitude above mean sea level, metres.
    pub alt_m: Option<f64>,
    /// Geoid separation, metres.
    pub geoid_sep_m: Option<f64>,
}

/// `$--VTG`: course and speed over ground.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Vtg {
    /// Course over ground (true), degrees.
    pub course_deg: Option<f64>,
    /// Speed over ground, knots.
    pub speed_knots: Option<f64>,
    /// Speed over ground, km/h.
    pub speed_kmh: Option<f64>,
    /// FAA mode indicator (NMEA 2.3+), as in [`Rmc::mode`].
    pub mode: Option<char>,
}

impl Vtg {
    /// Speed in km/h: the km/h field, else the knots field converted.
    pub fn kmh(&self) -> Option<f64> {
        self.speed_kmh
            .or_else(|| self.speed_knots.map(|k| k * KNOTS_TO_KMH))
    }
}

/// The decoded payload of a supported sentence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Body {
    /// `$--RMC`.
    Rmc(Rmc),
    /// `$--GGA`.
    Gga(Gga),
    /// `$--VTG`.
    Vtg(Vtg),
}

/// One parsed sentence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sentence {
    /// Two-letter talker id (`GP`, `GN`, `GL`, `GA`, `GB`, `BD`, …).
    pub talker: String,
    /// The decoded sentence.
    pub body: Body,
}

impl Sentence {
    /// The UTC time this sentence carries (RMC and GGA do, VTG does not).
    pub fn time(&self) -> Option<UtcTime> {
        match &self.body {
            Body::Rmc(r) => r.time,
            Body::Gga(g) => g.time,
            Body::Vtg(_) => None,
        }
    }
}

/// Why a line was rejected. The tracker counts these; none stops a log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NmeaError {
    /// Blank line.
    Empty,
    /// The line does not start with `$` (noise, a partial line, binary UBX).
    NoStart,
    /// No `*hh` checksum (often a line cut off mid-way).
    MissingChecksum,
    /// The checksum field is not two hex digits.
    BadChecksumField,
    /// The checksum does not match the sentence.
    BadChecksum {
        /// Checksum computed over the sentence.
        computed: u8,
        /// Checksum the sentence claims.
        stated: u8,
    },
    /// The address field is not a two-letter talker + three-letter type
    /// (proprietary `$P…` sentences land here too).
    BadAddress(String),
    /// A well-formed sentence of a type this parser does not decode
    /// (`GSV`, `GSA`, `GLL`, `TXT`, …).
    Unsupported(String),
    /// A supported sentence with a field that does not parse.
    Malformed {
        /// Sentence type (`RMC`, `GGA`, `VTG`).
        sentence: &'static str,
        /// The offending field.
        field: &'static str,
    },
}

impl std::fmt::Display for NmeaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NmeaError::Empty => write!(f, "empty line"),
            NmeaError::NoStart => write!(f, "line does not start with '$'"),
            NmeaError::MissingChecksum => write!(f, "no *hh checksum"),
            NmeaError::BadChecksumField => write!(f, "checksum is not two hex digits"),
            NmeaError::BadChecksum { computed, stated } => {
                write!(
                    f,
                    "checksum mismatch: computed {computed:02X}, stated {stated:02X}"
                )
            }
            NmeaError::BadAddress(a) => write!(f, "unrecognised address field '{a}'"),
            NmeaError::Unsupported(t) => write!(f, "unsupported sentence {t}"),
            NmeaError::Malformed { sentence, field } => {
                write!(f, "malformed {sentence} field '{field}'")
            }
        }
    }
}
impl std::error::Error for NmeaError {}

/// XOR checksum of `body` (the bytes between `$` and `*`).
pub fn checksum(body: &str) -> u8 {
    body.bytes().fold(0u8, |a, b| a ^ b)
}

/// Parse one line (with or without the trailing CR/LF).
pub fn parse_sentence(line: &str) -> Result<Sentence, NmeaError> {
    let line = line.trim_matches(|c: char| c.is_ascii_whitespace() || c == '\0');
    if line.is_empty() {
        return Err(NmeaError::Empty);
    }
    let rest = line.strip_prefix('$').ok_or(NmeaError::NoStart)?;
    let star = rest.rfind('*').ok_or(NmeaError::MissingChecksum)?;
    let body = &rest[..star];
    let cs = &rest[star + 1..];
    if cs.len() != 2 || !cs.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(NmeaError::BadChecksumField);
    }
    let stated = u8::from_str_radix(cs, 16).map_err(|_| NmeaError::BadChecksumField)?;
    let computed = checksum(body);
    if stated != computed {
        return Err(NmeaError::BadChecksum { computed, stated });
    }

    let fields: Vec<&str> = body.split(',').collect();
    let addr = fields.first().copied().unwrap_or("");
    let ok_addr = addr.len() == 5
        && addr
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        && addr.as_bytes()[0].is_ascii_uppercase()
        && !addr.starts_with('P');
    if !ok_addr {
        return Err(NmeaError::BadAddress(addr.chars().take(16).collect()));
    }
    let talker = addr[..2].to_string();
    let kind = &addr[2..];
    let f = &fields[1..];
    let body = match kind {
        "RMC" => Body::Rmc(parse_rmc(f)?),
        "GGA" => Body::Gga(parse_gga(f)?),
        "VTG" => Body::Vtg(parse_vtg(f)?),
        other => return Err(NmeaError::Unsupported(other.to_string())),
    };
    Ok(Sentence { talker, body })
}

fn get<'a>(f: &[&'a str], i: usize) -> &'a str {
    f.get(i).copied().unwrap_or("")
}

/// A finite decimal number made only of digits, one optional leading sign and
/// at most one dot (rejects `inf`, `NaN`, exponents and hex).
fn num(s: &str) -> Option<f64> {
    let digits = s.strip_prefix(['-', '+']).unwrap_or(s);
    if digits.is_empty()
        || digits == "."
        || !digits.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        || digits.bytes().filter(|&b| b == b'.').count() > 1
    {
        return None;
    }
    s.parse::<f64>().ok().filter(|v| v.is_finite())
}

/// Optional number field: empty is `Ok(None)`, unparsable is an error.
fn opt_num(s: &str, sentence: &'static str, field: &'static str) -> Result<Option<f64>, NmeaError> {
    if s.is_empty() {
        return Ok(None);
    }
    num(s)
        .map(Some)
        .ok_or(NmeaError::Malformed { sentence, field })
}

/// Optional non-negative number field.
fn opt_unsigned(
    s: &str,
    sentence: &'static str,
    field: &'static str,
) -> Result<Option<f64>, NmeaError> {
    match opt_num(s, sentence, field)? {
        Some(v) if v < 0.0 => Err(NmeaError::Malformed { sentence, field }),
        v => Ok(v),
    }
}

fn opt_time(s: &str, sentence: &'static str) -> Result<Option<UtcTime>, NmeaError> {
    if s.is_empty() {
        return Ok(None);
    }
    let bad = NmeaError::Malformed {
        sentence,
        field: "time",
    };
    let (hms, frac) = match s.split_once('.') {
        Some((a, b)) => (a, b),
        None => (s, ""),
    };
    if hms.len() != 6
        || !hms.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(bad);
    }
    let d = |i: usize| -> u8 { hms.as_bytes()[i] - b'0' };
    let hour = d(0) * 10 + d(1);
    let minute = d(2) * 10 + d(3);
    let second = d(4) * 10 + d(5);
    if hour > 23 || minute > 59 || second > 60 {
        return Err(bad);
    }
    // Fraction of a second to milliseconds (first three digits).
    let mut millis = 0u16;
    for i in 0..3 {
        let digit = frac.as_bytes().get(i).map_or(0, |b| u16::from(b - b'0'));
        millis = millis * 10 + digit;
    }
    Ok(Some(UtcTime {
        hour,
        minute,
        second,
        millis,
    }))
}

fn opt_date(s: &str, sentence: &'static str) -> Result<Option<UtcDate>, NmeaError> {
    if s.is_empty() {
        return Ok(None);
    }
    let bad = NmeaError::Malformed {
        sentence,
        field: "date",
    };
    if s.len() != 6 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad);
    }
    let d = |i: usize| -> u8 { s.as_bytes()[i] - b'0' };
    let day = d(0) * 10 + d(1);
    let month = d(2) * 10 + d(3);
    let yy = u16::from(d(4) * 10 + d(5));
    if !(1..=31).contains(&day) || !(1..=12).contains(&month) {
        return Err(bad);
    }
    let year = if yy < 80 { 2000 + yy } else { 1900 + yy };
    Ok(Some(UtcDate { year, month, day }))
}

/// `ddmm.mmmm` / `dddmm.mmmm` + hemisphere to signed decimal degrees. Both
/// fields empty is `None`; one without the other is malformed.
fn opt_coord(
    value: &str,
    hemi: &str,
    lat: bool,
    sentence: &'static str,
) -> Result<Option<f64>, NmeaError> {
    let field = if lat { "latitude" } else { "longitude" };
    let bad = NmeaError::Malformed { sentence, field };
    if value.is_empty() && hemi.is_empty() {
        return Ok(None);
    }
    let Some(v) = num(value).filter(|v| *v >= 0.0) else {
        return Err(bad);
    };
    let deg = (v / 100.0).floor();
    let min = v - deg * 100.0;
    let max_deg = if lat { 90.0 } else { 180.0 };
    if min >= 60.0 {
        return Err(bad);
    }
    let dec = deg + min / 60.0;
    if dec > max_deg {
        return Err(bad);
    }
    let sign = match (lat, hemi) {
        (true, "N") | (false, "E") => 1.0,
        (true, "S") | (false, "W") => -1.0,
        _ => return Err(bad),
    };
    Ok(Some(sign * dec))
}

fn opt_position(
    f: &[&str],
    at: usize,
    sentence: &'static str,
) -> Result<Option<(f64, f64)>, NmeaError> {
    let lat = opt_coord(get(f, at), get(f, at + 1), true, sentence)?;
    let lon = opt_coord(get(f, at + 2), get(f, at + 3), false, sentence)?;
    match (lat, lon) {
        (Some(a), Some(o)) => Ok(Some((a, o))),
        (None, None) => Ok(None),
        _ => Err(NmeaError::Malformed {
            sentence,
            field: "position",
        }),
    }
}

fn opt_mode(s: &str, sentence: &'static str) -> Result<Option<char>, NmeaError> {
    let mut c = s.chars();
    match (c.next(), c.next()) {
        (None, _) => Ok(None),
        (Some(m), None) if "ADEFMNPRS".contains(m) => Ok(Some(m)),
        _ => Err(NmeaError::Malformed {
            sentence,
            field: "mode",
        }),
    }
}

fn need(f: &[&str], n: usize, sentence: &'static str) -> Result<(), NmeaError> {
    if f.len() < n {
        return Err(NmeaError::Malformed {
            sentence,
            field: "field count",
        });
    }
    Ok(())
}

// RMC: time,status,lat,N,lon,E,sog,cog,date,magvar,E[,mode[,navstatus]]
fn parse_rmc(f: &[&str]) -> Result<Rmc, NmeaError> {
    const S: &str = "RMC";
    need(f, 9, S)?;
    let valid = match get(f, 1) {
        "A" => true,
        "V" | "" => false,
        _ => {
            return Err(NmeaError::Malformed {
                sentence: S,
                field: "status",
            })
        }
    };
    Ok(Rmc {
        time: opt_time(get(f, 0), S)?,
        valid,
        position: opt_position(f, 2, S)?,
        speed_knots: opt_unsigned(get(f, 6), S, "speed")?,
        course_deg: opt_num(get(f, 7), S, "course")?,
        date: opt_date(get(f, 8), S)?,
        mode: opt_mode(get(f, 11), S)?,
    })
}

// GGA: time,lat,N,lon,E,quality,sats,hdop,alt,M,sep,M,age,station
fn parse_gga(f: &[&str]) -> Result<Gga, NmeaError> {
    const S: &str = "GGA";
    need(f, 9, S)?;
    let quality = match get(f, 5) {
        "" => None,
        q => match q.parse::<u8>() {
            Ok(v) if v <= 8 && q.len() == 1 => Some(v),
            _ => {
                return Err(NmeaError::Malformed {
                    sentence: S,
                    field: "quality",
                })
            }
        },
    };
    let sats = match get(f, 6) {
        "" => None,
        n => match n.parse::<u8>() {
            Ok(v) if n.len() <= 2 => Some(v),
            _ => {
                return Err(NmeaError::Malformed {
                    sentence: S,
                    field: "satellites",
                })
            }
        },
    };
    for (i, field) in [(9usize, "altitude unit"), (11, "separation unit")] {
        if !matches!(get(f, i), "" | "M") {
            return Err(NmeaError::Malformed { sentence: S, field });
        }
    }
    Ok(Gga {
        time: opt_time(get(f, 0), S)?,
        position: opt_position(f, 1, S)?,
        quality,
        sats,
        hdop: opt_unsigned(get(f, 7), S, "hdop")?,
        alt_m: opt_num(get(f, 8), S, "altitude")?,
        geoid_sep_m: opt_num(get(f, 10), S, "separation")?,
    })
}

// VTG (NMEA 2+): cogT,T,cogM,M,sogN,N,sogK,K[,mode]
// VTG (NMEA 1.x, no unit letters): cogT,cogM,sogN,sogK
fn parse_vtg(f: &[&str]) -> Result<Vtg, NmeaError> {
    const S: &str = "VTG";
    if f.len() == 4 {
        return Ok(Vtg {
            course_deg: opt_num(get(f, 0), S, "course")?,
            speed_knots: opt_unsigned(get(f, 2), S, "speed knots")?,
            speed_kmh: opt_unsigned(get(f, 3), S, "speed km/h")?,
            mode: None,
        });
    }
    need(f, 8, S)?;
    for (i, unit) in [(1usize, "T"), (3, "M"), (5, "N"), (7, "K")] {
        let u = get(f, i);
        if !u.is_empty() && u != unit {
            return Err(NmeaError::Malformed {
                sentence: S,
                field: "unit letter",
            });
        }
    }
    Ok(Vtg {
        course_deg: opt_num(get(f, 0), S, "course")?,
        speed_knots: opt_unsigned(get(f, 4), S, "speed knots")?,
        speed_kmh: opt_unsigned(get(f, 6), S, "speed km/h")?,
        mode: opt_mode(get(f, 8), S)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a sentence with a correct checksum.
    fn s(body: &str) -> String {
        format!("${body}*{:02X}", checksum(body))
    }

    #[test]
    fn checksum_of_a_reference_sentence() {
        // The classic GGA example from public NMEA documentation.
        let line = "$GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*47";
        let p = parse_sentence(line).unwrap();
        let Body::Gga(g) = p.body else { panic!() };
        assert_eq!(p.talker, "GP");
        assert_eq!(g.quality, Some(1));
        assert_eq!(g.sats, Some(8));
        assert_eq!(g.hdop, Some(0.9));
        assert_eq!(g.alt_m, Some(545.4));
        let (lat, lon) = g.position.unwrap();
        assert!((lat - (48.0 + 7.038 / 60.0)).abs() < 1e-9);
        assert!((lon - (11.0 + 31.0 / 60.0)).abs() < 1e-9);
        assert_eq!(g.time.unwrap().to_string(), "12:35:19.000");
    }

    #[test]
    fn rmc_with_mode_and_south_west() {
        let p = parse_sentence(&s(
            "GNRMC,235959.50,A,3330.000,S,07030.000,W,10.0,270.5,311299,,,D,V",
        ))
        .unwrap();
        let Body::Rmc(r) = p.body else { panic!() };
        assert!(r.valid);
        assert_eq!(r.mode, Some('D'));
        assert_eq!(r.position, Some((-33.5, -70.5)));
        assert_eq!(r.date.unwrap().to_string(), "1999-12-31");
        assert_eq!(r.time.unwrap().millis, 500);
        assert_eq!(r.speed_knots, Some(10.0));
    }

    #[test]
    fn vtg_both_forms() {
        let p = parse_sentence(&s("GPVTG,054.7,T,034.4,M,005.5,N,010.2,K,A")).unwrap();
        let Body::Vtg(v) = p.body else { panic!() };
        assert_eq!(v.kmh(), Some(10.2));
        assert_eq!(v.mode, Some('A'));
        let p = parse_sentence(&s("GPVTG,054.7,034.4,005.5,010.2")).unwrap();
        let Body::Vtg(v) = p.body else { panic!() };
        assert_eq!(v.course_deg, Some(54.7));
        assert_eq!(v.kmh(), Some(10.2));
        // A receiver with no fix sends every field empty.
        let p = parse_sentence(&s("GPVTG,,,,,,,,,N")).unwrap();
        let Body::Vtg(v) = p.body else { panic!() };
        assert_eq!(v.kmh(), None);
        assert_eq!(v.mode, Some('N'));
    }

    #[test]
    fn rejections() {
        assert_eq!(parse_sentence(""), Err(NmeaError::Empty));
        assert_eq!(parse_sentence("GPRMC,1*00"), Err(NmeaError::NoStart));
        assert_eq!(
            parse_sentence("$GPRMC,123519,A,4807"),
            Err(NmeaError::MissingChecksum)
        );
        assert_eq!(
            parse_sentence("$GPRMC,1*G1"),
            Err(NmeaError::BadChecksumField)
        );
        assert!(matches!(
            parse_sentence("$GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*48"),
            Err(NmeaError::BadChecksum {
                computed: 0x47,
                stated: 0x48
            })
        ));
        assert_eq!(
            parse_sentence(&s("GPGSV,1,1,00")),
            Err(NmeaError::Unsupported("GSV".into()))
        );
        assert!(matches!(
            parse_sentence(&s("PUBX,00,1")),
            Err(NmeaError::BadAddress(_))
        ));
        // Field-level garbage.
        for body in [
            "GPRMC,1235,A,4807.038,N,01131.000,E,0,0,010120",
            "GPRMC,123519,X,4807.038,N,01131.000,E,0,0,010120",
            "GPRMC,123519,A,4860.000,N,01131.000,E,0,0,010120",
            "GPRMC,123519,A,4807.038,Q,01131.000,E,0,0,010120",
            "GPRMC,123519,A,4807.038,N,,,0,0,010120",
            "GPRMC,123519,A,4807.038,N,01131.000,E,inf,0,010120",
            "GPRMC,123519,A,4807.038,N,01131.000,E,-1,0,010120",
            "GPRMC,123519,A,4807.038,N,01131.000,E,0,0,320120",
            "GPRMC,123519,A",
            "GPGGA,123519,4807.038,N,01131.000,E,9,08,0.9,545.4,M,46.9,M,,",
            "GPGGA,123519,4807.038,N,01131.000,E,1,x8,0.9,545.4,M,46.9,M,,",
            "GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,F,46.9,M,,",
            "GPGGA,123519,9100.000,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,",
            "GPVTG,054.7,T,034.4,M,005.5,N,010.2,X,A",
            "GPVTG,054.7,T,034.4,M,005.5,N,010.2,K,ZZ",
            "GPVTG,1e3,T,034.4,M,005.5,N,010.2,K,A",
        ] {
            assert!(
                matches!(parse_sentence(&s(body)), Err(NmeaError::Malformed { .. })),
                "{body}"
            );
        }
    }

    #[test]
    fn no_panic_on_hostile_input() {
        let inputs = [
            "$",
            "$*",
            "$*00",
            "$,*2C",
            "$GP*17",
            "$\u{e9}\u{e9}RMC*00",
            "$GPRMC,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,*6B",
            "$GPGGA,999999.999999999,,,,,,,,,,,,,,*00",
        ];
        for i in inputs {
            let _ = parse_sentence(i);
        }
        // Multi-byte characters just before the checksum.
        let body = "GPRMC,\u{2603}";
        let _ = parse_sentence(&s(body));
    }
}
