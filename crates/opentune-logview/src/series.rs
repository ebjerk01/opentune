//! Channel statistics and min/max decimation for plotting.
//!
//! [`series`] returns at most two points per pixel bucket per channel: the
//! bucket's minimum and maximum, in time order. A line drawn through them
//! shows every spike of the full-resolution log, so a 100 000-sample log
//! plots as fast as a 1 000-sample one without hiding peaks.

use crate::{Log, LogError, Result};
use serde::Serialize;

/// Summary statistics of one channel over a time range.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChannelStats {
    /// Channel name.
    pub name: String,
    /// Unit as logged.
    pub unit: String,
    /// Number of valid (finite) samples.
    pub count: usize,
    /// Samples that were missing or not numeric.
    pub invalid: usize,
    /// Smallest value.
    pub min: Option<f64>,
    /// Largest value.
    pub max: Option<f64>,
    /// Mean.
    pub mean: Option<f64>,
    /// Population standard deviation.
    pub std: Option<f64>,
    /// Time of the smallest value.
    pub t_min: Option<f64>,
    /// Time of the largest value.
    pub t_max: Option<f64>,
}

/// Index range `[lo, hi)` of samples with `t0 <= t <= t1`.
pub fn index_range(log: &Log, t0: Option<f64>, t1: Option<f64>) -> (usize, usize) {
    let lo = t0.map_or(0, |a| log.t.partition_point(|&t| t < a));
    let hi = t1.map_or(log.t.len(), |b| log.t.partition_point(|&t| t <= b));
    (lo, hi.max(lo))
}

/// Statistics of every channel, optionally limited to `t0..=t1`.
pub fn stats(log: &Log, t0: Option<f64>, t1: Option<f64>) -> Vec<ChannelStats> {
    let (lo, hi) = index_range(log, t0, t1);
    log.channels
        .iter()
        .map(|c| {
            let mut n = 0usize;
            let mut sum = 0.0;
            let mut sq = 0.0;
            let mut min: Option<(f64, usize)> = None;
            let mut max: Option<(f64, usize)> = None;
            for (i, &v) in c.values.iter().enumerate().take(hi).skip(lo) {
                if !v.is_finite() {
                    continue;
                }
                n += 1;
                sum += v;
                sq += v * v;
                if min.is_none_or_gt(v) {
                    min = Some((v, i));
                }
                if max.is_none_or_lt(v) {
                    max = Some((v, i));
                }
            }
            let mean = (n > 0).then(|| sum / n as f64);
            let std = mean.map(|m| (sq / n as f64 - m * m).max(0.0).sqrt());
            ChannelStats {
                name: c.name.clone(),
                unit: c.unit.clone(),
                count: n,
                invalid: (hi - lo) - n,
                min: min.map(|x| x.0),
                max: max.map(|x| x.0),
                mean,
                std,
                t_min: min.map(|x| log.t[x.1]),
                t_max: max.map(|x| log.t[x.1]),
            }
        })
        .collect()
}

trait Extreme {
    fn is_none_or_gt(&self, v: f64) -> bool;
    fn is_none_or_lt(&self, v: f64) -> bool;
}
impl Extreme for Option<(f64, usize)> {
    fn is_none_or_gt(&self, v: f64) -> bool {
        match self {
            None => true,
            Some((m, _)) => v < *m,
        }
    }
    fn is_none_or_lt(&self, v: f64) -> bool {
        match self {
            None => true,
            Some((m, _)) => v > *m,
        }
    }
}

/// One plotted channel. `v[i]` is `None` for a gap (no valid sample).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Series {
    /// Channel name.
    pub name: String,
    /// Unit as logged.
    pub unit: String,
    /// Point times.
    pub t: Vec<f64>,
    /// Point values; `None` breaks the line.
    pub v: Vec<Option<f64>>,
}

/// The result of [`series`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SeriesSet {
    /// Start of the requested range.
    pub t0: f64,
    /// End of the requested range.
    pub t1: f64,
    /// Samples of the log inside the range.
    pub points_in_range: usize,
    /// `true` when min/max buckets were used instead of raw samples.
    pub decimated: bool,
    /// Bucket count used.
    pub buckets: usize,
    /// One entry per requested channel, in request order.
    pub series: Vec<Series>,
}

/// Most buckets a caller may ask for.
pub const MAX_BUCKETS: usize = 20_000;

/// Plot data for `channels` over `t0..=t1` (whole log when `None`).
///
/// With at most `2 * buckets` samples in range the raw samples are returned.
/// Otherwise the range is cut into `buckets` equal-time buckets and each
/// channel contributes its bucket minimum and maximum (one point when they
/// coincide, a `None` gap when the bucket has no valid sample).
pub fn series(
    log: &Log,
    channels: &[&str],
    t0: Option<f64>,
    t1: Option<f64>,
    buckets: usize,
) -> Result<SeriesSet> {
    let chans = channels
        .iter()
        .map(|n| log.require(n))
        .collect::<Result<Vec<_>>>()?;
    let first = log.t.first().copied().unwrap_or(0.0);
    let last = log.t.last().copied().unwrap_or(0.0);
    let mut a = t0.unwrap_or(first);
    let mut b = t1.unwrap_or(last);
    if !a.is_finite() || !b.is_finite() {
        return Err(LogError::Input("time range must be finite".into()));
    }
    if a > b {
        std::mem::swap(&mut a, &mut b);
    }
    let buckets = buckets.clamp(1, MAX_BUCKETS);
    let (lo, hi) = index_range(log, Some(a), Some(b));
    let n = hi - lo;
    let decimated = n > 2 * buckets && b > a;

    let mut out = Vec::with_capacity(chans.len());
    for c in chans {
        let mut s = Series {
            name: c.name.clone(),
            unit: c.unit.clone(),
            t: Vec::new(),
            v: Vec::new(),
        };
        if !decimated {
            for i in lo..hi {
                s.t.push(log.t[i]);
                let v = c.values[i];
                s.v.push(v.is_finite().then_some(v));
            }
        } else {
            let w = (b - a) / buckets as f64;
            let mut i = lo;
            for k in 0..buckets {
                let end_t = a + w * (k + 1) as f64;
                let start = i;
                while i < hi && (log.t[i] < end_t || k == buckets - 1) {
                    i += 1;
                }
                if start == i {
                    continue; // no samples in this bucket at all
                }
                let mut mn: Option<(f64, usize)> = None;
                let mut mx: Option<(f64, usize)> = None;
                for (j, &v) in c.values.iter().enumerate().take(i).skip(start) {
                    if !v.is_finite() {
                        continue;
                    }
                    if mn.is_none_or_gt(v) {
                        mn = Some((v, j));
                    }
                    if mx.is_none_or_lt(v) {
                        mx = Some((v, j));
                    }
                }
                match (mn, mx) {
                    (Some((vmin, jmin)), Some((vmax, jmax))) => {
                        let (p, q) = if jmin <= jmax {
                            ((jmin, vmin), (jmax, vmax))
                        } else {
                            ((jmax, vmax), (jmin, vmin))
                        };
                        s.t.push(log.t[p.0]);
                        s.v.push(Some(p.1));
                        if q.0 != p.0 {
                            s.t.push(log.t[q.0]);
                            s.v.push(Some(q.1));
                        }
                    }
                    _ => {
                        s.t.push(log.t[start]);
                        s.v.push(None);
                    }
                }
            }
        }
        out.push(s);
    }
    Ok(SeriesSet {
        t0: a,
        t1: b,
        points_in_range: n,
        decimated,
        buckets,
        series: out,
    })
}

/// The sample nearest to time `t`: its index, its time and every channel's
/// value there (`None` when missing). `None` for an empty log.
pub fn sample_at(log: &Log, t: f64) -> Option<(usize, f64, Vec<Option<f64>>)> {
    if log.t.is_empty() || !t.is_finite() {
        return None;
    }
    let i = log.t.partition_point(|&x| x < t);
    let idx = if i == 0 {
        0
    } else if i >= log.t.len() {
        log.t.len() - 1
    } else if (log.t[i] - t) < (t - log.t[i - 1]) {
        i
    } else {
        i - 1
    };
    let vals = log
        .channels
        .iter()
        .map(|c| {
            let v = c.values[idx];
            v.is_finite().then_some(v)
        })
        .collect();
    Some((idx, log.t[idx], vals))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Channel, LogFormat, TimeSource, TimeUnit};

    fn synth(n: usize) -> Log {
        // A slow sine with sharp single-sample spikes that a naive
        // every-Nth-sample decimator would miss.
        let t: Vec<f64> = (0..n).map(|i| i as f64 * 0.01).collect();
        let v: Vec<f64> = (0..n)
            .map(|i| {
                let base = (i as f64 * 0.001).sin() * 10.0;
                if i % 997 == 13 {
                    base + 500.0
                } else if i % 1499 == 7 {
                    base - 400.0
                } else {
                    base
                }
            })
            .collect();
        let mut w = v.clone();
        w[n / 2] = f64::NAN;
        Log {
            format: LogFormat::Wide,
            delimiter: ',',
            decimal_comma: false,
            header: true,
            units_row: false,
            time_column: Some("t".into()),
            time_unit: TimeUnit::Seconds,
            time_source: TimeSource::Header,
            epoch_start: None,
            t,
            channels: vec![
                Channel {
                    name: "a".into(),
                    unit: "".into(),
                    values: v,
                },
                Channel {
                    name: "b".into(),
                    unit: "".into(),
                    values: w,
                },
            ],
            notes: vec![],
        }
    }

    #[test]
    fn decimation_keeps_global_and_per_bucket_extremes() {
        let log = synth(100_000);
        let buckets = 300;
        let s = series(&log, &["a"], None, None, buckets).unwrap();
        assert!(s.decimated);
        let a = &s.series[0];
        assert!(a.t.len() <= 2 * buckets);
        let vals: Vec<f64> = a.v.iter().flatten().copied().collect();
        let st = &stats(&log, None, None)[0];
        let out_max = vals.iter().cloned().fold(f64::MIN, f64::max);
        let out_min = vals.iter().cloned().fold(f64::MAX, f64::min);
        assert_eq!(out_max, st.max.unwrap());
        assert_eq!(out_min, st.min.unwrap());
        // Every spike survives.
        let spikes = (0..100_000).filter(|i| i % 997 == 13).count();
        let kept = vals.iter().filter(|&&v| v > 400.0).count();
        assert_eq!(kept, spikes);
        // Points stay in time order.
        assert!(a.t.windows(2).all(|w| w[0] <= w[1]));
        // Each bucket's min and max equal the raw data's.
        let w = (s.t1 - s.t0) / buckets as f64;
        for k in [0usize, 17, 150, buckets - 1] {
            let lo = s.t0 + w * k as f64;
            let hi = s.t0 + w * (k + 1) as f64;
            let raw: Vec<f64> = log
                .t
                .iter()
                .zip(&log.channels[0].values)
                .filter(|(t, _)| **t >= lo && (**t < hi || k == buckets - 1))
                .map(|(_, v)| *v)
                .collect();
            let dec: Vec<f64> =
                a.t.iter()
                    .zip(&a.v)
                    .filter(|(t, _)| **t >= lo && (**t < hi || k == buckets - 1))
                    .filter_map(|(_, v)| *v)
                    .collect();
            let f = |v: &[f64], max: bool| {
                v.iter().cloned().fold(
                    if max { f64::MIN } else { f64::MAX },
                    if max { f64::max } else { f64::min },
                )
            };
            assert_eq!(f(&raw, true), f(&dec, true), "bucket {k} max");
            assert_eq!(f(&raw, false), f(&dec, false), "bucket {k} min");
        }
    }

    #[test]
    fn small_range_returns_raw_samples() {
        let log = synth(10_000);
        let s = series(&log, &["a", "b"], Some(1.0), Some(2.0), 400).unwrap();
        assert!(!s.decimated);
        assert_eq!(s.series[0].t.len(), 101);
        assert_eq!(s.series[0].t[0], 1.0);
    }

    #[test]
    fn nan_becomes_gap_and_is_counted() {
        let log = synth(1000);
        let s = series(&log, &["b"], None, None, 1000).unwrap();
        assert_eq!(s.series[0].v[500], None);
        let st = stats(&log, None, None);
        assert_eq!(st[1].invalid, 1);
        assert_eq!(st[1].count, 999);
    }

    #[test]
    fn unknown_channel_is_an_error() {
        let log = synth(10);
        assert!(series(&log, &["nope"], None, None, 10).is_err());
    }

    #[test]
    fn nearest_sample() {
        let log = synth(10);
        assert_eq!(sample_at(&log, 0.034).unwrap().0, 3);
        assert_eq!(sample_at(&log, 0.036).unwrap().0, 4);
        assert_eq!(sample_at(&log, -5.0).unwrap().0, 0);
        assert_eq!(sample_at(&log, 99.0).unwrap().0, 9);
    }
}
