//! Frame timing for the proof-of-approach: a scripted scroll, per-frame
//! intervals and CPU time, written as CSV, summarised like the webview's
//! Render check (`desktop/ui/src/render-check.js`: fps, p50/p95/max frame
//! interval, frames over 50 ms) plus frames over one 60 Hz period (16.7 ms).

use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

/// What the scripted run scrolls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scene {
    Hex,
    Maps,
}

impl Scene {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "hex" => Some(Scene::Hex),
            "maps" => Some(Scene::Maps),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Scene::Hex => "hex",
            Scene::Maps => "maps",
        }
    }
}

/// Settings of a scripted run.
#[derive(Clone, Debug)]
pub struct BenchCfg {
    pub scene: Scene,
    /// Frames to record (after the warm-up).
    pub frames: usize,
    /// Scroll distance per frame, in points.
    pub px_per_frame: f32,
    pub csv: Option<PathBuf>,
}

/// One recorded frame.
#[derive(Clone, Copy, Debug)]
pub struct Sample {
    /// Time since the previous frame started (ms): what the user sees.
    pub interval_ms: f64,
    /// Time spent building the UI in `update` (ms): our CPU cost.
    pub cpu_ms: f64,
}

/// Frames ignored at the start (window mapping, font atlas, first upload).
pub const WARMUP: usize = 30;
pub const LATE_MS: f64 = 1000.0 / 60.0;
/// One and a half 60 Hz periods: a frame that really missed a vsync. With
/// vsync on, intervals sit on the 16.7 ms period and jitter puts about half of
/// them just over `LATE_MS`, so `late` alone over-counts.
pub const MISSED_MS: f64 = 1.5 * LATE_MS;
/// The webview's Render check calls a frame this long a visible stutter.
pub const LONG_MS: f64 = 50.0;

pub struct Bench {
    pub cfg: BenchCfg,
    pub frame: usize,
    last: Option<Instant>,
    pub samples: Vec<Sample>,
    pub done: bool,
}

impl Bench {
    pub fn new(cfg: BenchCfg) -> Self {
        Self {
            cfg,
            frame: 0,
            last: None,
            samples: Vec::new(),
            done: false,
        }
    }

    /// Scroll offset for this frame.
    pub fn offset(&self) -> f32 {
        self.frame as f32 * self.cfg.px_per_frame
    }

    /// Record a frame that started at `start` and spent `cpu_ms` in `update`.
    /// Returns true once the run is complete.
    pub fn record(&mut self, start: Instant, cpu_ms: f64) -> bool {
        if let Some(prev) = self.last {
            if self.frame > WARMUP {
                self.samples.push(Sample {
                    interval_ms: start.duration_since(prev).as_secs_f64() * 1000.0,
                    cpu_ms,
                });
            }
        }
        self.last = Some(start);
        self.frame += 1;
        if self.samples.len() >= self.cfg.frames {
            self.done = true;
        }
        self.done
    }
}

/// Summary statistics over the recorded intervals.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stats {
    pub frames: usize,
    pub fps: f64,
    pub p50: f64,
    pub p95: f64,
    pub max: f64,
    pub late: usize,
    pub missed: usize,
    pub long: usize,
    pub cpu_p50: f64,
    pub cpu_p95: f64,
    pub cpu_max: f64,
}

/// Nearest-rank percentile, as in render-check.js.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let k = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[k.saturating_sub(1).min(sorted.len() - 1)]
}

fn sorted(v: impl Iterator<Item = f64>) -> Vec<f64> {
    let mut s: Vec<f64> = v.filter(|x| x.is_finite() && *x > 0.0).collect();
    s.sort_by(|a, b| a.total_cmp(b));
    s
}

pub fn stats(samples: &[Sample]) -> Stats {
    let iv = sorted(samples.iter().map(|s| s.interval_ms));
    let cpu = sorted(samples.iter().map(|s| s.cpu_ms));
    if iv.is_empty() {
        return Stats::default();
    }
    let total: f64 = iv.iter().sum();
    Stats {
        frames: iv.len(),
        fps: iv.len() as f64 * 1000.0 / total,
        p50: percentile(&iv, 50.0),
        p95: percentile(&iv, 95.0),
        max: *iv.last().unwrap(),
        late: iv.iter().filter(|&&x| x > LATE_MS).count(),
        missed: iv.iter().filter(|&&x| x > MISSED_MS).count(),
        long: iv.iter().filter(|&&x| x > LONG_MS).count(),
        cpu_p50: percentile(&cpu, 50.0),
        cpu_p95: percentile(&cpu, 95.0),
        cpu_max: cpu.last().copied().unwrap_or(0.0),
    }
}

/// The same verdict words as the webview's Render check.
pub fn verdict(s: &Stats) -> &'static str {
    if s.frames == 0 {
        "no frames"
    } else if s.p50 > 25.0 || s.fps < 40.0 {
        "slow"
    } else if s.p95 > 25.0 || s.long > 0 {
        "uneven"
    } else {
        "smooth"
    }
}

pub fn summary_line(scene: Scene, s: &Stats, env: &str) -> String {
    format!(
        "scroll {}: {} frames, {:.1} fps, p50 {:.1} ms, p95 {:.1} ms, max {:.1} ms, \
         {} late (>16.7 ms), {} missed (>25 ms), {} long (>50 ms), ui cpu p50 {:.2} ms p95 {:.2} ms max {:.2} ms — {} [{}]",
        scene.name(),
        s.frames,
        s.fps,
        s.p50,
        s.p95,
        s.max,
        s.late,
        s.missed,
        s.long,
        s.cpu_p50,
        s.cpu_p95,
        s.cpu_max,
        verdict(s),
        env
    )
}

/// Write every sample plus the summary as comment lines.
pub fn write_csv(path: &PathBuf, samples: &[Sample], summary: &str) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    writeln!(f, "# {summary}")?;
    writeln!(f, "frame,interval_ms,ui_cpu_ms")?;
    for (i, s) in samples.iter().enumerate() {
        writeln!(f, "{i},{:.3},{:.3}", s.interval_ms, s.cpu_ms)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(iv: f64) -> Sample {
        Sample {
            interval_ms: iv,
            cpu_ms: 1.0,
        }
    }

    #[test]
    fn stats_match_the_webview_definitions() {
        let mut v: Vec<Sample> = (0..98).map(|_| s(16.0)).collect();
        v.push(s(20.0));
        v.push(s(60.0));
        let st = stats(&v);
        assert_eq!(st.frames, 100);
        assert_eq!(st.p50, 16.0);
        assert_eq!(st.p95, 16.0);
        assert_eq!(st.max, 60.0);
        assert_eq!(st.late, 2);
        assert_eq!(st.missed, 1);
        assert_eq!(st.long, 1);
        assert_eq!(verdict(&st), "uneven");
        assert_eq!(verdict(&stats(&v[..98])), "smooth");
        assert_eq!(verdict(&Stats::default()), "no frames");
    }

    #[test]
    fn warmup_is_skipped_and_run_ends() {
        let mut b = Bench::new(BenchCfg {
            scene: Scene::Hex,
            frames: 5,
            px_per_frame: 10.0,
            csv: None,
        });
        let t0 = Instant::now();
        let mut done = false;
        for i in 0..(WARMUP + 10) {
            done = b.record(t0 + std::time::Duration::from_millis(16 * i as u64), 0.5);
            if done {
                break;
            }
        }
        assert!(done);
        assert_eq!(b.samples.len(), 5);
        assert!(b
            .samples
            .iter()
            .all(|x| (x.interval_ms - 16.0).abs() < 0.01));
    }
}
