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

/// Build [`MonitorOpts`](opentune_core::MonitorOpts) from the usual connection
/// options. Blank strings are ignored; `bitrate` `None`/`0` derives it from
/// `bus` (`ms` = 125 kbit/s, else 500 kbit/s).
pub fn monitor_opts(
    port: Option<&str>,
    baud: Option<&str>,
    bus: Option<&str>,
    bitrate: Option<u32>,
    filter_ids: Vec<u32>,
) -> opentune_core::MonitorOpts {
    let mut o = opentune_core::MonitorOpts {
        bitrate: bitrate.unwrap_or(0),
        filter_ids,
        ..Default::default()
    };
    for (k, v) in [("port", port), ("baud", baud), ("bus", bus)] {
        if let Some(v) = v.map(str::trim).filter(|v| !v.is_empty()) {
            o.options.insert(k.into(), v.to_string());
        }
    }
    o
}

/// Pick a format from a file name's extension (`.csv`, `.asc`, else candump).
pub fn format_for_path(path: &std::path::Path) -> Format {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("csv") => Format::Csv,
        Some("asc") => Format::Asc,
        _ => Format::Candump,
    }
}

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

    #[test]
    fn opts_and_format_helpers() {
        let o = monitor_opts(
            Some("/dev/ttyUSB0"),
            Some(" "),
            Some("ms"),
            None,
            vec![0x7E8],
        );
        assert_eq!(o.option("port"), Some("/dev/ttyUSB0"));
        assert!(!o.options.contains_key("baud"));
        assert_eq!(o.resolved_bitrate(), 125_000);
        assert_eq!(o.filter_ids, vec![0x7E8]);
        assert_eq!(format_for_path("a/b.CSV".as_ref()), Format::Csv);
        assert_eq!(format_for_path("x.asc".as_ref()), Format::Asc);
        assert_eq!(format_for_path("x.log".as_ref()), Format::Candump);
    }
}
