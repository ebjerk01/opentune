//! CAN log readers and an offline Haldex summary.
//!
//! Reads three text formats, auto-detected per line:
//!
//! * can-utils **candump** log: `(1700000000.123456) can0 2C0#1880000054C0FE73`
//!   (also `##<flags>` CAN-FD and `#R` remote frames) — what `opentune candump`
//!   writes by default;
//! * **OpenTune CSV**: `ts,id,ext,dlc,data` (`opentune candump --format csv`);
//! * **SavvyCAN / GVRET CSV**: `Time Stamp,ID,Extended,Dir,Bus,LEN,D1,...,D8`
//!   (SavvyCAN exports, OpenHaldex's own captures).
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
    let extended = id_s.len() > 3;
    let id = u32::from_str_radix(id_s, 16).ok()?;
    let data = if let Some(fd) = payload.strip_prefix('#') {
        // `##<flags nibble><data>`
        hex_bytes(fd.get(1..)?)?
    } else if payload.starts_with('R') {
        Vec::new()
    } else {
        hex_bytes(payload)?
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
        if data.len() != dlc {
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
        let len: usize = cols[5].parse().ok()?;
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
}

/// Parse a whole log (any of the supported formats, mixed per line).
pub fn parse_log(text: &str) -> ParsedLog {
    let mut out = ParsedLog::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || is_header(line) {
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
