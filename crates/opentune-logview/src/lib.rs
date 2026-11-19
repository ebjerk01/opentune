//! # Offline datalog viewer
//!
//! Closes the tune loop: *log → see which map cells were used → edit → compare*.
//!
//! - [`csv`] — open a CSV datalog. Recognises the OpenTune **Live log** export
//!   (long format: one row per parameter per tick) and **generic wide CSV**
//!   (one column per channel plus a time column). The delimiter, header row,
//!   optional units row, decimal comma and time unit are detected; every
//!   detection that was *inferred* rather than read from the file is reported
//!   in [`Log::notes`].
//! - [`series`] — per-channel statistics and min/max **decimation** so a long
//!   log plots in a fixed number of pixel buckets without losing peaks, and
//!   [`series::replay`] frames for playing a log back through a live view.
//! - [`overlay`] — bin the log over a calibration map's breakpoints (nearest
//!   cell or bilinear weights) into per-cell hit counts, time-in-cell and the
//!   value range of a chosen channel.
//! - [`units`] — unit comparison between a log channel and a map axis. Units
//!   are only ever *compared* (with a small table of exact identities such as
//!   `1/min` = `rpm`); nothing is converted.
//!
//! The crate does no ECU-definition or image decoding itself: the caller hands
//! [`overlay::map_hits`] the physical axis breakpoints.

pub mod csv;
pub mod overlay;
pub mod series;
pub mod units;

pub use csv::{parse, parse_file, Channel, Log, LogFormat, ParseOptions, TimeSource, TimeUnit};
pub use overlay::{map_hits, BinMode, MapHits, OverlayInput};
pub use series::{replay, sample_at, series, stats, ChannelStats, Replay, Series, SeriesSet};
pub use units::{check as check_units, UnitCheck};

/// Errors from opening or querying a log.
#[derive(Debug, thiserror::Error)]
pub enum LogError {
    /// The log file could not be read.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// The file is not a usable CSV log.
    #[error("{0}")]
    Format(String),
    /// A request referred to something the log or the map does not have.
    #[error("{0}")]
    Input(String),
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, LogError>;
