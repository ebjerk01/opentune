//! Error type shared by the sparse image model and the HEX/SREC codecs.
//!
//! Every failure mode is explicit and carries enough context to point at the
//! offending record. The codecs never silently drop or truncate data: a bad
//! checksum, a malformed record, a premature end of input or an overlapping
//! write is always surfaced as one of these variants.

use core::fmt;

/// Something went wrong while parsing, building or serialising an image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageError {
    /// A record's trailing checksum byte did not match the computed value.
    Checksum {
        /// 1-based input line the record came from.
        line: usize,
        /// Checksum the record claimed.
        expected: u8,
        /// Checksum recomputed from the record body.
        found: u8,
    },
    /// A record was structurally invalid (bad length, stray byte, odd nibble…).
    Record {
        /// 1-based input line the record came from.
        line: usize,
        /// Human-readable reason.
        reason: String,
    },
    /// Input ended before the mandatory terminator record.
    ///
    /// Intel HEX requires a type-01 end-of-file record; Motorola SREC requires
    /// an S7/S8/S9 termination record. Their absence means the file was
    /// truncated and is rejected rather than accepted partially.
    Truncated {
        /// Human-readable reason.
        reason: String,
    },
    /// An insert overlapped existing data under [`Overlap::Error`].
    ///
    /// [`Overlap::Error`]: crate::Overlap::Error
    Overlap {
        /// First overlapping address (inclusive).
        start: u64,
        /// One past the last overlapping address (exclusive).
        end: u64,
    },
    /// An address could not be represented in the selected output record width.
    AddressWidth {
        /// The address that did not fit.
        address: u64,
        /// Number of address bits the chosen record type provides.
        max_bits: u8,
    },
    /// A write was requested for an image that carries no data.
    Empty,
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageError::Checksum {
                line,
                expected,
                found,
            } => write!(
                f,
                "line {line}: checksum mismatch (record says {expected:#04x}, computed {found:#04x})"
            ),
            ImageError::Record { line, reason } => write!(f, "line {line}: malformed record: {reason}"),
            ImageError::Truncated { reason } => write!(f, "truncated input: {reason}"),
            ImageError::Overlap { start, end } => write!(
                f,
                "overlapping write at [{start:#x}, {end:#x}) rejected by overlap policy"
            ),
            ImageError::AddressWidth { address, max_bits } => write!(
                f,
                "address {address:#x} does not fit a {max_bits}-bit record"
            ),
            ImageError::Empty => write!(f, "cannot serialise an empty image"),
        }
    }
}

impl std::error::Error for ImageError {}

/// Convenience alias for results produced by this crate.
pub type Result<T> = core::result::Result<T, ImageError>;
