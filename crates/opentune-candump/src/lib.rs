//! Full CAN bus dump to file.
//!
//! Sits on top of the driver-side [`CanMonitor`](opentune_core::CanMonitor)
//! (passive, listen-only raw capture) and provides:
//!
//! - [`writer`] — streaming log writers: Linux can-utils **candump** log
//!   (`(1700000000.123456) can0 7E8#04410C1A`, the default), **CSV**
//!   (`ts,id,ext,dlc,data`) and **Vector ASC**.
//! - [`stats`] — a live aggregator: totals, fps, per-id count / Hz / last data,
//!   dropped frames.
//! - [`capture`] — the capture loop shared by the CLI and the desktop app:
//!   monitor → filter → writer + stats, until a stop flag or a duration.
pub mod capture;
pub mod stats;
pub mod writer;

pub use capture::{run_capture, CaptureOutcome, CaptureParams};
pub use stats::{CaptureStats, IdSnapshot, StatsSnapshot};
pub use writer::{Format, FrameWriter};

/// Parse a comma/space separated list of CAN ids (hex, optional `0x`), e.g.
/// `7E8,7E0` or `0x280 0x1A0`.
pub fn parse_id_list(s: &str) -> Result<Vec<u32>, String> {
    s.split(|c: char| c == ',' || c.is_whitespace())
        .filter(|t| !t.is_empty())
        .map(|t| {
            let h = t
                .strip_prefix("0x")
                .or_else(|| t.strip_prefix("0X"))
                .unwrap_or(t);
            u32::from_str_radix(h, 16)
                .ok()
                .filter(|&v| v <= 0x1FFF_FFFF)
                .ok_or_else(|| format!("invalid CAN id '{t}' (expected hex, e.g. 7E8)"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_list_parses() {
        assert_eq!(parse_id_list("7E8,7E0").unwrap(), vec![0x7E8, 0x7E0]);
        assert_eq!(
            parse_id_list(" 0x280 1a0 ,18DAF110").unwrap(),
            vec![0x280, 0x1A0, 0x18DA_F110]
        );
        assert!(parse_id_list("").unwrap().is_empty());
        assert!(parse_id_list("zz").is_err());
        assert!(parse_id_list("FFFFFFFF").is_err());
    }
}
