//! Diagnostic logging to stderr, **off by default**.
//!
//! Set `OPENTUNE_LOG` (or `RUST_LOG`) to turn it on, e.g. `trace`, or
//! per target: `openport2=trace,warn`. A bare level applies to every target;
//! `target=level` applies to targets that start with `target`. Drivers log
//! through the `log` facade (the OpenPort 2.0 driver logs every command line
//! it sends and every record it receives at `trace`, target `openport2`).
//! Hand-written so the CLI needs no logger dependency.

use log::{LevelFilter, Log, Metadata, Record};

/// A parsed `OPENTUNE_LOG` / `RUST_LOG` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Spec {
    default: LevelFilter,
    targets: Vec<(String, LevelFilter)>,
}

fn level(s: &str) -> Option<LevelFilter> {
    match s.trim().to_ascii_lowercase().as_str() {
        "off" => Some(LevelFilter::Off),
        "error" => Some(LevelFilter::Error),
        "warn" | "warning" => Some(LevelFilter::Warn),
        "info" => Some(LevelFilter::Info),
        "debug" => Some(LevelFilter::Debug),
        "trace" => Some(LevelFilter::Trace),
        _ => None,
    }
}

impl Spec {
    /// Parse `level` / `target=level` items separated by commas. Unknown
    /// items are ignored (a typo must not break the command).
    pub(crate) fn parse(s: &str) -> Spec {
        let mut spec = Spec {
            default: LevelFilter::Off,
            targets: Vec::new(),
        };
        for item in s.split(',').map(str::trim).filter(|i| !i.is_empty()) {
            match item.split_once('=') {
                Some((t, l)) => {
                    if let Some(l) = level(l) {
                        spec.targets.push((t.trim().replace('-', "_"), l));
                    }
                }
                None => match level(item) {
                    Some(l) => spec.default = l,
                    // A bare target name means "everything from it".
                    None => spec
                        .targets
                        .push((item.replace('-', "_"), LevelFilter::Trace)),
                },
            }
        }
        // Longest prefix first, so the most specific item wins.
        spec.targets
            .sort_by_key(|(t, _)| std::cmp::Reverse(t.len()));
        spec
    }

    pub(crate) fn level_for(&self, target: &str) -> LevelFilter {
        let target = target.replace('-', "_");
        self.targets
            .iter()
            .find(|(t, _)| target.starts_with(t.as_str()))
            .map(|(_, l)| *l)
            .unwrap_or(self.default)
    }

    fn max(&self) -> LevelFilter {
        self.targets
            .iter()
            .map(|(_, l)| *l)
            .chain([self.default])
            .max()
            .unwrap_or(LevelFilter::Off)
    }
}

struct StderrLog(Spec);

impl Log for StderrLog {
    fn enabled(&self, m: &Metadata) -> bool {
        m.level() <= self.0.level_for(m.target())
    }
    fn log(&self, r: &Record) {
        if self.enabled(r.metadata()) {
            eprintln!("[{} {}] {}", r.level(), r.target(), r.args());
        }
    }
    fn flush(&self) {}
}

/// Install the stderr logger when `OPENTUNE_LOG` / `RUST_LOG` asks for it.
/// A no-op otherwise, and when an embedder already installed a logger.
pub(crate) fn init() {
    let Some(s) = std::env::var("OPENTUNE_LOG")
        .ok()
        .or_else(|| std::env::var("RUST_LOG").ok())
        .filter(|s| !s.trim().is_empty())
    else {
        return;
    };
    let spec = Spec::parse(&s);
    let max = spec.max();
    if max == LevelFilter::Off {
        return;
    }
    if log::set_boxed_logger(Box::new(StderrLog(spec))).is_ok() {
        log::set_max_level(max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_levels_and_targets() {
        let s = Spec::parse("trace");
        assert_eq!(s.level_for("openport2"), LevelFilter::Trace);
        let s = Spec::parse("openport2=trace,warn");
        assert_eq!(s.level_for("openport2"), LevelFilter::Trace);
        assert_eq!(s.level_for("opentune_core::x"), LevelFilter::Warn);
        let s = Spec::parse("opentune-driver-openport2=debug");
        assert_eq!(
            s.level_for("opentune_driver_openport2::kline"),
            LevelFilter::Debug
        );
        assert_eq!(s.level_for("other"), LevelFilter::Off);
        // Bare target = everything from it; typos are ignored.
        let s = Spec::parse("openport2, nonsense=loud");
        assert_eq!(s.level_for("openport2"), LevelFilter::Trace);
        assert_eq!(s.level_for("nonsense"), LevelFilter::Off);
        // Most specific wins.
        let s = Spec::parse("a=warn,a::b=trace");
        assert_eq!(s.level_for("a::b::c"), LevelFilter::Trace);
        assert_eq!(s.level_for("a::x"), LevelFilter::Warn);
        assert_eq!(Spec::parse("").max(), LevelFilter::Off);
    }
}
