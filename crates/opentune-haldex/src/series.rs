//! Chart-ready series from a decoded Haldex CAN log: lock request / actual,
//! clutch value, Gen4 temperature and the front-rear wheel-speed delta, each
//! decimated for plotting ([`build_series`]); the panel payload of a decoded
//! log ([`decode_text`]); and the series as a Live-log CSV the Log viewer
//! opens ([`series_csv`]). Offline: nothing is transmitted.

use crate::canlog::{parse_log, summarize, LogFrame};
use crate::{Bus, HaldexFrame};
use serde_json::{json, Value};

/// Most points per chart series (longer logs are decimated evenly).
pub const MAX_POINTS: usize = 1500;
/// Most decoded frames listed in the panel's table.
pub const MAX_ROWS: usize = 200;

/// One chart series.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Series {
    pub key: &'static str,
    pub name: &'static str,
    pub units: &'static str,
    /// Seconds from the first timestamped frame.
    pub t: Vec<f64>,
    pub v: Vec<f64>,
}

impl Series {
    fn new(key: &'static str, name: &'static str, units: &'static str) -> Self {
        Series {
            key,
            name,
            units,
            t: Vec::new(),
            v: Vec::new(),
        }
    }

    fn push(&mut self, t: f64, v: f64) {
        if v.is_finite() {
            self.t.push(t);
            self.v.push(v);
        }
    }

    /// Keep at most `max` evenly spaced points (always the first and last).
    fn decimate(&mut self, max: usize) {
        let n = self.v.len();
        if max < 2 || n <= max {
            return;
        }
        let idx: Vec<usize> = (0..max).map(|i| i * (n - 1) / (max - 1)).collect();
        self.t = idx.iter().map(|&i| self.t[i]).collect();
        self.v = idx.iter().map(|&i| self.v[i]).collect();
    }
}

/// Chart-ready series from decoded frames: lock request / actual (%), clutch
/// value in the DBC scale, Gen4 temperature and the front-rear wheel-speed
/// delta. Empty series are dropped.
pub fn build_series(decoded: &[(LogFrame, HaldexFrame)], max_points: usize) -> Vec<Series> {
    let mut req = Series::new("lock_request", "Lock request", "%");
    let mut act = Series::new("lock_actual", "Lock actual", "%");
    let mut clutch = Series::new("clutch_actual", "Clutch actual (DBC scale)", "Nm/min");
    let mut temp = Series::new("gen4_temp", "Gen4 temperature", "°C");
    let mut delta = Series::new("wheel_delta", "Front - rear wheel speed", "km/h");
    let t0 = decoded.iter().find_map(|(f, _)| f.ts);
    let mut last_t = 0.0;
    for (f, d) in decoded {
        let t = match (f.ts, t0) {
            (Some(ts), Some(t0)) => ts - t0,
            _ => last_t,
        };
        last_t = t;
        match d {
            HaldexFrame::LockRequestPq { lock_pct, .. } => req.push(t, f64::from(*lock_pct)),
            HaldexFrame::StatusPq {
                engagement_pct,
                actual_nm_per_min,
                gen4_temp_c,
                ..
            } => {
                act.push(t, f64::from(*engagement_pct));
                clutch.push(t, f64::from(*actual_nm_per_min));
                if let Some(c) = gen4_temp_c {
                    temp.push(t, f64::from(*c));
                }
            }
            HaldexFrame::StatusMqb {
                engagement_pct: Some(p),
                ..
            } => act.push(t, f64::from(*p)),
            HaldexFrame::WheelSpeeds(w) => delta.push(t, f64::from(w.front_rear_delta())),
            _ => {}
        }
    }
    let mut out: Vec<Series> = [req, act, clutch, temp, delta]
        .into_iter()
        .filter(|s| !s.v.is_empty())
        .collect();
    for s in &mut out {
        s.decimate(max_points);
    }
    out
}

/// Decode a log text: `{ summary, series, rows, bad_lines, bus }`.
pub fn decode_text(text: &str, bus: Bus, max_points: usize) -> Value {
    let log = parse_log(text);
    let (summary, decoded) = summarize(&log, bus);
    let t0 = decoded.iter().find_map(|(f, _)| f.ts);
    let rows: Vec<Value> = decoded
        .iter()
        .take(MAX_ROWS)
        .map(|(f, d)| {
            json!({
                "t": f.ts.zip(t0).map(|(a, b)| a - b),
                "id": format!("{:03X}", f.id),
                "decoded": d,
            })
        })
        .collect();
    json!({
        "summary": summary,
        "series": build_series(&decoded, max_points),
        "rows": rows,
        "rows_total": decoded.len(),
        "bad_lines": log.bad_lines,
        "bus": bus,
    })
}

/// The series as a Live-log CSV (`ts,param_id,name,raw,value,units,note`,
/// `ts` in ms from the start of the capture) that the Log viewer opens.
pub fn series_csv(series: &[Series]) -> String {
    let mut rows: Vec<(u64, usize, f64)> = Vec::new();
    for (si, s) in series.iter().enumerate() {
        for (t, v) in s.t.iter().zip(&s.v) {
            rows.push(((t * 1000.0).round().max(0.0) as u64, si, *v));
        }
    }
    rows.sort_by_key(|r| (r.0, r.1));
    let mut out = String::from("ts,param_id,name,raw,value,units,note\n");
    for (ts, si, v) in rows {
        let s = &series[si];
        out.push_str(&format!("{ts},{},{},,{v},{},\n", s.key, s.name, s.units));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = "\
(1.0) can0 2A0#7F0000
(1.1) can0 2A0#FE0000
(1.2) can0 2C0#1880000054C0FE73
(1.3) can0 2C0#01BF0080560000F3
(1.4) can0 4A0#88130000A00F0000
(1.5) can0 6B0#0000C0280000020A
(1.6) can0 7E8#0102
";

    #[test]
    fn log_decode_gives_summary_series_and_rows() {
        let v = decode_text(LOG, Bus::Auto, MAX_POINTS);
        assert_eq!(v["summary"]["frames"], 7);
        assert_eq!(v["rows_total"], 6);
        assert_eq!(v["rows"][0]["id"], "2A0");
        let series = v["series"].as_array().unwrap();
        let get = |k: &str| series.iter().find(|s| s["key"] == k).cloned();
        let req = get("lock_request").unwrap();
        assert_eq!(req["v"], json!([0.0, 100.0]));
        assert!((req["t"][1].as_f64().unwrap() - 0.1).abs() < 1e-9);
        let temp = get("gen4_temp").unwrap();
        assert_eq!(temp["v"], json!([24.0, 26.0]));
        assert!(get("lock_actual").is_some() && get("clutch_actual").is_some());
        assert!(get("wheel_delta").is_some());
    }

    #[test]
    fn series_decimate_and_export_as_live_log_csv() {
        let mut s = Series::new("x", "X", "u");
        for i in 0..1000 {
            s.push(i as f64 * 0.01, i as f64);
        }
        s.push(10.0, f64::NAN);
        assert_eq!(s.v.len(), 1000, "non-finite values are skipped");
        s.decimate(11);
        assert_eq!(s.v.len(), 11);
        assert_eq!((s.v[0], s.v[10]), (0.0, 999.0));
        let log = parse_log(LOG);
        let (_, decoded) = summarize(&log, Bus::Auto);
        let csv = series_csv(&build_series(&decoded, usize::MAX));
        let mut lines = csv.lines();
        assert_eq!(lines.next(), Some("ts,param_id,name,raw,value,units,note"));
        assert_eq!(lines.next(), Some("0,lock_request,Lock request,,0,%,"));
        assert!(csv.contains("200,gen4_temp,Gen4 temperature,,24,°C,"));
    }
}
