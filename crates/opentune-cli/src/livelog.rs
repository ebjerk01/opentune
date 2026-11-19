//! Run-length and reconnect flags shared by `log` and `obd read`, mapped onto
//! [`opentune_core::resilient`] (the loop both commands run on).
use anyhow::{anyhow, Result};
use clap::Args;
use opentune_core::resilient::{ReconnectPolicy, RunLimits, RunReport};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// How long a live log runs and what it does when the link drops.
#[derive(Args, Debug, Clone)]
pub struct RunArgs {
    /// Number of rounds; 0 = until Ctrl-C or --duration.
    #[arg(long, default_value_t = 1)]
    pub rounds: u64,
    /// Sleep between rounds, milliseconds (0 = as fast as possible).
    #[arg(long, default_value_t = 0)]
    pub interval_ms: u64,
    /// Stop after this many seconds (with --rounds 0: the only limit).
    #[arg(long, value_name = "SECONDS")]
    pub duration: Option<f64>,
    /// When the link is lost (no answer for --dead-after rounds, or the
    /// adapter reset), close the driver and re-open it with the same
    /// settings, backing off, then resume (re-defining dynamic DIDs).
    /// Without it the log keeps polling the same link, backing off.
    #[arg(long)]
    pub reconnect: bool,
    /// Rounds in a row with no response at all before the link counts as lost.
    #[arg(long, default_value_t = 3)]
    pub dead_after: u32,
    /// First reconnect backoff, ms; doubles per attempt.
    #[arg(long, default_value_t = 500)]
    pub reconnect_backoff_ms: u64,
    /// Backoff ceiling, ms.
    #[arg(long, default_value_t = 10_000)]
    pub reconnect_max_backoff_ms: u64,
    /// Give up after this many failed re-opens (default: never).
    #[arg(long, value_name = "N")]
    pub reconnect_max_attempts: Option<u32>,
    /// Give up once the link has been down this many seconds (default: never).
    #[arg(long, value_name = "SECONDS")]
    pub reconnect_give_up: Option<f64>,
}

fn ms(s: f64, flag: &str) -> Result<u64> {
    if !(s.is_finite() && s >= 0.0) {
        return Err(anyhow!("{flag} must be a non-negative number of seconds"));
    }
    Ok((s * 1000.0).round() as u64)
}

impl RunArgs {
    /// Round / time limits.
    pub fn limits(&self) -> Result<RunLimits> {
        Ok(RunLimits {
            rounds: (self.rounds > 0).then_some(self.rounds),
            duration_ms: self.duration.map(|d| ms(d, "--duration")).transpose()?,
            interval_ms: self.interval_ms,
        })
    }

    /// The reconnect policy (validated).
    pub fn policy(&self) -> Result<ReconnectPolicy> {
        let p = ReconnectPolicy {
            reconnect: self.reconnect,
            dead_after: self.dead_after,
            initial_backoff_ms: self.reconnect_backoff_ms,
            max_backoff_ms: self.reconnect_max_backoff_ms,
            max_attempts: self.reconnect_max_attempts,
            give_up_after_ms: self
                .reconnect_give_up
                .map(|s| ms(s, "--reconnect-give-up"))
                .transpose()?,
        };
        p.validate().map_err(|e| anyhow!("reconnect policy: {e}"))?;
        Ok(p)
    }

    /// Runs until Ctrl-C (no round and no time limit).
    pub fn open_ended(&self) -> bool {
        self.rounds == 0 && self.duration.is_none()
    }
}

/// A stop flag set by Ctrl-C (best effort: if a handler is already
/// installed, the limits still end the run).
pub fn ctrlc_flag() -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let s = Arc::clone(&stop);
    let _ = ctrlc::set_handler(move || s.store(true, Ordering::Relaxed));
    stop
}

/// The stderr line about the link, if anything happened to it.
pub fn link_summary(r: &RunReport) -> Option<String> {
    if r.drops == 0 && r.dead_rounds == 0 {
        return None;
    }
    let mut s = format!(
        "link: {} dead round(s), {} drop(s), {} reconnect(s) in {} attempt(s), {} recovered \
         without reopen, {:.1} s down",
        r.dead_rounds,
        r.drops,
        r.reconnects,
        r.reopen_attempts,
        r.recoveries,
        r.downtime_ms as f64 / 1000.0
    );
    if r.ended == opentune_core::resilient::EndReason::GaveUp {
        s.push_str("; gave up reconnecting");
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct T {
        #[command(flatten)]
        run: RunArgs,
    }

    #[test]
    fn flags_map_onto_the_loop() {
        let t = T::parse_from([
            "t",
            "--rounds",
            "0",
            "--duration",
            "1.5",
            "--reconnect",
            "--reconnect-give-up",
            "30",
        ]);
        let l = t.run.limits().unwrap();
        assert_eq!((l.rounds, l.duration_ms), (None, Some(1500)));
        let p = t.run.policy().unwrap();
        assert!(p.reconnect && p.max_attempts.is_none());
        assert_eq!(p.give_up_after_ms, Some(30_000));
        assert!(!t.run.open_ended());
        let t = T::parse_from(["t", "--reconnect-backoff-ms", "0"]);
        assert!(t.run.policy().is_err());
        let t = T::parse_from(["t", "--duration=-1"]);
        assert!(t.run.limits().is_err());
    }
}
