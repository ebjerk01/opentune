//! CAN log readers and an offline Haldex summary.
//!
//! Reads four text formats, auto-detected per line:
//!
//! * can-utils **candump** log: `(1700000000.123456) can0 2C0#1880000054C0FE73`
//!   (also `##<flags>` CAN-FD and `#R` remote frames) — what `opentune candump`
//!   writes by default;
//! * **OpenTune CSV**: `ts,id,ext,dlc,data` (`opentune candump --format csv`);
//! * **SavvyCAN / GVRET CSV**: `Time Stamp,ID,Extended,Dir,Bus,LEN,D1,...,D8`
//!   (SavvyCAN exports, OpenHaldex's own captures);
//! * **Vector ASC**: `   0.012345 1  7E8             Rx   d 4 04 41 0C 1A`,
//!   extended ids with an `x` suffix, `CANFD` lines, `base dec` ids
//!   (`opentune candump --format asc`, CANalyzer, python-can). Timestamps are
//!   relative to the start of the measurement; event lines (`Start of
//!   measurement`, error frames, statistics) are skipped like headers.
//!
//! Header and comment lines are skipped; malformed lines are counted, not
//! fatal.
use crate::can::{decode_frame, Bus, HaldexFrame};
use crate::openhaldex::Mode;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// One frame read from a log.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LogFrame {
    /// Timestamp in seconds as written in the log (format-specific epoch).
    pub ts: Option<f64>,
    /// Interface / bus column, when the format has one.
    pub channel: Option<String>,
    pub id: u32,
    pub extended: bool,
    pub data: Vec<u8>,
}

/// Result of [`parse_log`].
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ParsedLog {
    pub frames: Vec<LogFrame>,
    /// Non-empty, non-header lines that could not be parsed.
    pub bad_lines: usize,
}

fn hex_bytes(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

fn parse_id(s: &str) -> Option<(u32, bool)> {
    let t = s.trim();
    let t = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    let id = u32::from_str_radix(t, 16).ok()?;
    if id > 0x1FFF_FFFF {
        return None;
    }
    Some((id, t.len() > 3 && id > 0x7FF))
}

fn parse_candump(line: &str) -> Option<LogFrame> {
    // (ts) iface ID#DATA
    let rest = line.strip_prefix('(')?;
    let (ts, rest) = rest.split_once(')')?;
    let mut parts = rest.split_whitespace();
    let channel = parts.next()?.to_string();
    let frame = parts.next()?;
    let (id_s, payload) = frame.split_once('#')?;
    // can-utils writes 3 hex digits for an 11-bit id and 8 for a 29-bit one.
    if id_s.is_empty() || id_s.len() > 8 || !id_s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let extended = id_s.len() > 3;
    let id = u32::from_str_radix(id_s, 16).ok()?;
    if id > if extended { 0x1FFF_FFFF } else { 0x7FF } {
        return None;
    }
    let data = if let Some(fd) = payload.strip_prefix('#') {
        // `##<flags nibble><data>`
        hex_bytes(fd.get(1..)?).filter(|d| d.len() <= 64)?
    } else if payload.starts_with('R') {
        Vec::new()
    } else {
        hex_bytes(payload).filter(|d| d.len() <= 8)?
    };
    Some(LogFrame {
        ts: ts.trim().parse().ok(),
        channel: Some(channel),
        id,
        extended,
        data,
    })
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" => Some(true),
        "0" | "false" => Some(false),
        _ => None,
    }
}

fn parse_csv(line: &str) -> Option<LogFrame> {
    let cols: Vec<&str> = line.split(',').map(str::trim).collect();
    // OpenTune: ts,id,ext,dlc,data
    if cols.len() == 5 {
        let (id, _) = parse_id(cols[1])?;
        let extended = parse_bool(cols[2])?;
        let dlc: usize = cols[3].parse().ok()?;
        let data = hex_bytes(cols[4])?;
        if data.len() != dlc || dlc > 64 || (!extended && id > 0x7FF) {
            return None;
        }
        return Some(LogFrame {
            ts: cols[0].parse().ok(),
            channel: None,
            id,
            extended,
            data,
        });
    }
    // SavvyCAN: Time Stamp,ID,Extended,Dir,Bus,LEN,D1..Dn[,]
    if cols.len() >= 6 {
        let (id, _) = parse_id(cols[1])?;
        let extended = parse_bool(cols[2])?;
        let len: usize = cols[5].parse().ok().filter(|&n| n <= 64)?;
        if !extended && id > 0x7FF {
            return None;
        }
        let bytes: Vec<&str> = cols[6..]
            .iter()
            .copied()
            .filter(|c| !c.is_empty())
            .collect();
        if bytes.len() < len {
            return None;
        }
        let data = bytes[..len]
            .iter()
            .map(|b| u8::from_str_radix(b, 16).ok())
            .collect::<Option<Vec<u8>>>()?;
        return Some(LogFrame {
            // SavvyCAN writes microseconds.
            ts: cols[0].parse::<f64>().ok().map(|us| us / 1e6),
            channel: Some(format!("bus{}", cols[4])),
            id,
            extended,
            data,
        });
    }
    None
}

fn is_header(line: &str) -> bool {
    let l = line.trim_start().to_ascii_lowercase();
    l.starts_with('#')
        || l.starts_with("//")
        || l.starts_with("ts,")
        || l.starts_with("time stamp")
        || l.starts_with("time,")
        // Vector ASC header / trigger-block lines
        || l.starts_with("date ")
        || l.starts_with("base ")
        || l.starts_with("internal events")
        || l.starts_with("no internal events")
        || l.starts_with("begin triggerblock")
        || l.starts_with("end triggerblock")
}

/// An ASC line starts with a bare relative timestamp (no commas: that is CSV).
fn is_asc(line: &str) -> bool {
    !line.contains(',')
        && line
            .split_whitespace()
            .next()
            .is_some_and(|t| t.parse::<f64>().is_ok())
}

/// One Vector ASC line. `Ok(None)` is an event line (no CAN frame: start of
/// measurement, error frame, statistics), `Err(())` a frame line that does not
/// parse. `dec` = the header said `base dec`.
fn parse_asc(line: &str, dec: bool) -> Result<Option<LogFrame>, ()> {
    let t: Vec<&str> = line.split_whitespace().collect();
    let ts: f64 = t[0].parse().map_err(|_| ())?;
    let id_of = |s: &str| -> Option<(u32, bool)> {
        let (body, ext) = match s.strip_suffix(['x', 'X']) {
            Some(b) => (b, true),
            None => (s, false),
        };
        let id = if dec {
            body.parse().ok()?
        } else {
            u32::from_str_radix(body, 16).ok()?
        };
        (id <= if ext { 0x1FFF_FFFF } else { 0x7FF }).then_some((id, ext))
    };
    let byte = |s: &str| -> Option<u8> {
        if dec {
            s.parse().ok()
        } else {
            u8::from_str_radix(s, 16).ok()
        }
    };
    let take = |from: usize, n: usize| -> Option<Vec<u8>> {
        t.get(from..from.checked_add(n)?)?
            .iter()
            .map(|b| byte(b))
            .collect()
    };
    // CANFD <ch> <dir> <id> [<name>] <brs> <esi> <dlc> <len> <data…> …
    if t.get(1).is_some_and(|s| s.eq_ignore_ascii_case("CANFD")) {
        if t.len() < 5 {
            return Err(());
        }
        let (id, extended) = id_of(t[4]).ok_or(())?;
        let bit = |s: &str| s == "0" || s == "1";
        // A symbolic name, when present, sits between the id and BRS/ESI.
        let at = if t.len() > 7 && bit(t[5]) && bit(t[6]) {
            5
        } else {
            6
        };
        // A CAN FD payload is at most 64 bytes.
        let len: usize = t
            .get(at + 3)
            .and_then(|s| s.parse().ok())
            .filter(|&n| n <= 64)
            .ok_or(())?;
        let data = take(at + 4, len).ok_or(())?;
        return Ok(Some(LogFrame {
            ts: Some(ts),
            channel: Some(t[2].to_string()),
            id,
            extended,
            data,
        }));
    }
    // <ch> <id> <Rx|Tx> <d|r> [<dlc> <data…>] …
    let frame_line = t.len() >= 5
        && t[1].bytes().all(|b| b.is_ascii_digit())
        && (t[4].eq_ignore_ascii_case("d") || t[4].eq_ignore_ascii_case("r"));
    if !frame_line {
        return Ok(None);
    }
    let (id, extended) = id_of(t[2]).ok_or(())?;
    let data = if t[4].eq_ignore_ascii_case("r") {
        Vec::new()
    } else {
        let dlc = t
            .get(5)
            .and_then(|s| usize::from_str_radix(s, 16).ok())
            .ok_or(())?;
        take(6, dlc.min(8)).ok_or(())?
    };
    Ok(Some(LogFrame {
        ts: Some(ts),
        channel: Some(t[1].to_string()),
        id,
        extended,
        data,
    }))
}

/// Parse a whole log (any of the supported formats, mixed per line).
pub fn parse_log(text: &str) -> ParsedLog {
    let mut out = ParsedLog::default();
    let mut asc_dec = false;
    for line in text.lines() {
        let line = line.trim();
        if line.to_ascii_lowercase().starts_with("base ") {
            asc_dec = line.split_whitespace().nth(1) == Some("dec");
        }
        if line.is_empty() || is_header(line) {
            continue;
        }
        if is_asc(line) {
            match parse_asc(line, asc_dec) {
                Ok(Some(f)) => out.frames.push(f),
                Ok(None) => {}
                Err(()) => out.bad_lines += 1,
            }
            continue;
        }
        let f = if line.starts_with('(') {
            parse_candump(line)
        } else {
            parse_csv(line)
        };
        match f {
            Some(f) => out.frames.push(f),
            None => out.bad_lines += 1,
        }
    }
    out
}

/// Running min / max / last of a series.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Range {
    pub min: f32,
    pub max: f32,
    pub last: f32,
    pub samples: u64,
}

impl Range {
    fn push(slot: &mut Option<Range>, v: f32) {
        match slot {
            Some(r) => {
                r.min = r.min.min(v);
                r.max = r.max.max(v);
                r.last = v;
                r.samples += 1;
            }
            None => {
                *slot = Some(Range {
                    min: v,
                    max: v,
                    last: v,
                    samples: 1,
                })
            }
        }
    }
}

/// Offline summary of a capture's Haldex traffic.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct HaldexSummary {
    /// Frames in the log.
    pub frames: u64,
    /// Frames decoded as Haldex-related.
    pub decoded: u64,
    /// Count per decoded frame kind (`lock_request_pq`, `status_pq`, ...).
    pub per_kind: BTreeMap<String, u64>,
    /// Lock request, percent (PQ `Bremse_4` lever scale).
    pub lock_request_pct: Option<Range>,
    /// Lock actual, percent (PQ `Allrad_1` / MQB `Allrad_03`).
    pub lock_actual_pct: Option<Range>,
    /// Gen4 unit temperature from `Allrad_1` byte 4, °C (community decode).
    pub gen4_temp_c: Option<Range>,
    /// Largest mean-front minus mean-rear wheel-speed difference seen, km/h.
    pub max_front_rear_delta_kmh: Option<f32>,
    /// Highest wheel speed seen, km/h.
    pub max_wheel_speed_kmh: Option<f32>,
    /// Every status flag seen set at least once.
    pub flags_seen: BTreeSet<String>,
    /// MQB fault-reaction status (`Allrad_03` status 2) seen.
    pub mqb_fault_reaction: bool,
    /// Gen4 "fault stored" bit seen.
    pub fault_stored_seen: bool,
    /// OpenHaldex modes seen in its broadcast.
    pub openhaldex_modes: BTreeSet<String>,
    /// OpenHaldex mode-change commands seen on 0x6A0.
    pub openhaldex_commands: Vec<String>,
}

/// Decode every frame of `log` and aggregate. Returns the summary and the
/// decoded frames in log order.
pub fn summarize(log: &ParsedLog, bus: Bus) -> (HaldexSummary, Vec<(LogFrame, HaldexFrame)>) {
    let mut s = HaldexSummary {
        frames: log.frames.len() as u64,
        ..Default::default()
    };
    let mut decoded = Vec::new();
    for f in &log.frames {
        let Some(d) = decode_frame(f.id, f.extended, &f.data, bus) else {
            continue;
        };
        s.decoded += 1;
        let kind = match &d {
            HaldexFrame::LockRequestPq { lock_pct, .. } => {
                Range::push(&mut s.lock_request_pct, *lock_pct);
                "lock_request_pq"
            }
            HaldexFrame::StatusPq {
                flags,
                engagement_pct,
                fault_stored,
                gen4_temp_c,
                ..
            } => {
                Range::push(&mut s.lock_actual_pct, *engagement_pct);
                if let Some(t) = gen4_temp_c {
                    Range::push(&mut s.gen4_temp_c, *t as f32);
                }
                s.fault_stored_seen |= fault_stored.unwrap_or(false);
                s.flags_seen
                    .extend(flags.names().into_iter().map(String::from));
                "status_pq"
            }
            HaldexFrame::StatusMqb {
                status,
                engagement_pct,
                ..
            } => {
                if let Some(p) = engagement_pct {
                    Range::push(&mut s.lock_actual_pct, *p);
                }
                if *status == 2 {
                    s.mqb_fault_reaction = true;
                    s.flags_seen.insert("fault-reaction".into());
                }
                "status_mqb"
            }
            HaldexFrame::WheelSpeeds(w) => {
                let delta = w.front_rear_delta();
                let m = s.max_front_rear_delta_kmh.get_or_insert(delta);
                if delta.abs() > m.abs() {
                    *m = delta;
                }
                let mx = s.max_wheel_speed_kmh.get_or_insert(0.0);
                *mx = mx.max(w.max());
                "wheel_speeds"
            }
            HaldexFrame::EnginePq { .. } => "engine_pq",
            HaldexFrame::OpenHaldexBroadcast(b) => {
                let name = b
                    .mode
                    .map(|m| m.to_string())
                    .unwrap_or_else(|| format!("unknown({})", b.mode_index));
                s.openhaldex_modes.insert(name);
                "openhaldex_broadcast"
            }
            HaldexFrame::OpenHaldexCommand { value, mode } => {
                s.openhaldex_commands.push(
                    mode.map(|m: Mode| m.to_string())
                        .unwrap_or_else(|| format!("unknown({value})")),
                );
                "openhaldex_command"
            }
        };
        *s.per_kind.entry(kind.into()).or_default() += 1;
        decoded.push((f.clone(), d));
    }
    (s, decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_all_three_formats() {
        let text = "\
# comment
(1700000000.000100) can0 2C0#1880000054C0FE73
(1700000000.000200) can0 18DAF110#0102
(1700000000.000300) can0 123##1AABB
(1700000000.000400) can0 2A0#R
ts,id,ext,dlc,data
1700000000.000500,2A0,0,3,FE0000
Time Stamp,ID,Extended,Dir,Bus,LEN,D1,D2,D3,D4,D5,D6,D7,D8
4551584025,000002C0,false,Rx,1,8,18,80,00,00,54,C0,FE,73,
garbage line
";
        let p = parse_log(text);
        assert_eq!(p.bad_lines, 1);
        assert_eq!(p.frames.len(), 6);
        assert_eq!(p.frames[0].id, 0x2C0);
        assert!(!p.frames[0].extended);
        assert_eq!(p.frames[0].data.len(), 8);
        assert!(p.frames[1].extended && p.frames[1].id == 0x18DA_F110);
        assert_eq!(p.frames[2].data, vec![0xAA, 0xBB]);
        assert!(p.frames[3].data.is_empty());
        assert_eq!(p.frames[4].data, vec![0xFE, 0, 0]);
        assert_eq!(p.frames[5].channel.as_deref(), Some("bus1"));
        assert_eq!(p.frames[5].data[1], 0x80);
        assert!((p.frames[5].ts.unwrap() - 4551.584025).abs() < 1e-6);
    }

    #[test]
    fn reads_vector_asc() {
        // The layout `opentune candump --format asc` writes (see
        // opentune-candump's asc golden test), plus a dec-base block.
        let text = "\
date Tue Nov 14 10:13:20.000 pm 2023
base hex  timestamps absolute
internal events logged
// version 9.0.0
Begin Triggerblock Tue Nov 14 10:13:20.000 pm 2023
   0.000000 Start of measurement
   0.123456 1  7E8             Rx   d 4 04 41 0C 1A
   1.000001 1  18DAF110x       Rx   d 8 03 41 0D 00 AA AA AA AA
   2.000000 1  123             Rx   r
   2.500000 CANFD   1 Rx        456                                   1 0 9 12 00 01 02 03 04 05 06 07 08 09 0A 0B        0    0     3000        0        0        0        0        0
   2.600000 CANFD   1 Rx        457  Some_Name  1 0 2 2 0A 0B        0    0     3000        0        0        0        0        0
   2.700000 1  ErrorFrame
   2.800000 1  2C0             Rx   d 8 18 80 00
End TriggerBlock
base dec  timestamps absolute
   3.000000 2  704             Rx   d 2 255 16
";
        let p = parse_log(text);
        assert_eq!(p.frames.len(), 6, "{:?}", p.frames);
        assert_eq!(p.bad_lines, 1, "the truncated 0x2C0 line");
        let f = &p.frames;
        assert_eq!((f[0].id, f[0].extended), (0x7E8, false));
        assert_eq!(f[0].data, vec![0x04, 0x41, 0x0C, 0x1A]);
        assert!((f[0].ts.unwrap() - 0.123456).abs() < 1e-9);
        assert_eq!(f[0].channel.as_deref(), Some("1"));
        assert_eq!((f[1].id, f[1].extended), (0x18DA_F110, true));
        assert!(f[2].data.is_empty());
        assert_eq!(f[3].id, 0x456);
        assert_eq!(f[3].data.len(), 12);
        assert_eq!(f[3].data[11], 0x0B);
        assert_eq!((f[4].id, f[4].data.clone()), (0x457, vec![0x0A, 0x0B]));
        assert_eq!((f[5].id, f[5].data.clone()), (704, vec![255, 16]));
        assert_eq!(f[5].channel.as_deref(), Some("2"));
    }

    #[test]
    fn ids_and_payloads_must_fit_the_frame_type() {
        // Regression: an 11-bit id above 0x7FF, a 29-bit id above 0x1FFFFFFF
        // (or a 32-bit one), a signed id, a classic payload over 8 bytes and
        // a CSV frame whose id contradicts its 11-bit flag were all read as
        // frames.
        let text = "\
(1.0) can0 FFF#00
(1.0) can0 FFFFFFFF#00
(1.0) can0 +7E8#00
(1.0) can0 7E8#000102030405060708
(1.0) can0 7E8##
1.0,800,0,1,00
1,00000800,false,Rx,1,1,00
";
        let p = parse_log(text);
        assert_eq!(p.frames, vec![]);
        assert_eq!(p.bad_lines, 7);
        // The edges are kept.
        let p =
            parse_log("(1.0) can0 7FF#0001020304050607\n(1.0) can0 1FFFFFFF#00\n1.0,800,1,1,00\n");
        assert_eq!(p.bad_lines, 0);
        assert_eq!(
            p.frames
                .iter()
                .map(|f| (f.id, f.extended))
                .collect::<Vec<_>>(),
            vec![(0x7FF, false), (0x1FFF_FFFF, true), (0x800, true)]
        );
    }

    #[test]
    fn asc_canfd_length_is_bounded() {
        // Regression: the data length is a free number; `from + len` with a
        // huge one overflowed (a panic in debug builds).
        for len in ["18446744073709551615", "18446744073709551606", "65"] {
            let line = format!("   2.5 CANFD 1 Rx 456 1 0 8 {len} 00 01");
            let p = parse_log(&line);
            assert_eq!((p.frames.len(), p.bad_lines), (0, 1), "{len}");
        }
        let p = parse_log("   2.5 CANFD 1 Rx 456 1 0 8 2 00 01");
        assert_eq!(p.frames[0].data, vec![0, 1]);
    }

    #[test]
    fn summary_aggregates_lock_flags_and_modes() {
        let text = "\
(1.0) can0 2A0#7F0000
(1.1) can0 2A0#FE0000
(1.2) can0 2C0#1880000054C0FE73
(1.3) can0 2C0#01BF0080560000F3
(1.4) can0 4A0#88130000A00F0000
(1.5) can0 6B0#0000C0280000020A
(1.6) can0 6A0#0300000000000000
(1.7) can0 7E8#0102
";
        let (s, frames) = summarize(&parse_log(text), Bus::Auto);
        assert_eq!(s.frames, 8);
        assert_eq!(s.decoded, 7);
        assert_eq!(frames.len(), 7);
        let req = s.lock_request_pct.unwrap();
        assert_eq!((req.min, req.max, req.samples), (0.0, 100.0, 2));
        let act = s.lock_actual_pct.unwrap();
        assert_eq!(act.min, 0.0);
        assert!((act.max - 50.4).abs() < 0.01);
        let t = s.gen4_temp_c.unwrap();
        assert_eq!((t.min, t.max), (24.0, 26.0));
        assert!(s.flags_seen.contains("coupling-open"));
        assert!(s.flags_seen.contains("clutch-fault"));
        assert!(s.fault_stored_seen);
        // Bremse_3: VL = 0x1388>>1 *0.01 = 25 km/h, HL = 0x0FA0>>1 *0.01 = 20 km/h.
        assert!((s.max_wheel_speed_kmh.unwrap() - 25.0).abs() < 0.01);
        assert!((s.max_front_rear_delta_kmh.unwrap() - 2.5).abs() < 0.01);
        assert!(s.openhaldex_modes.contains("5050"));
        assert_eq!(s.openhaldex_commands, vec!["5050".to_string()]);
        assert_eq!(s.per_kind["status_pq"], 2);
    }
}
