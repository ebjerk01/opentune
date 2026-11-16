//! Error type for container decode/repack.

use std::fmt;

/// Errors produced while parsing or transforming a VAG flash container.
#[derive(Debug)]
pub enum Error {
    /// The input is shorter than a field the format requires.
    Truncated {
        /// What was being read (e.g. `"SGO header"`).
        what: &'static str,
        /// Bytes required at the offset.
        need: usize,
        /// Byte offset the read started at.
        at: usize,
        /// Total length of the input.
        len: usize,
    },
    /// A magic number / signature did not match the expected format.
    BadMagic(&'static str),
    /// The container referenced a feature we do not implement.
    Unsupported(String),
    /// A caller-supplied key/table was required but not provided, or the wrong size.
    MissingKey(&'static str),
    /// Malformed XML in an ODX / ODX-F document.
    Xml(String),
    /// A decoded block did not match the size declared in its descriptor.
    SizeMismatch {
        /// What the descriptor said.
        expected: usize,
        /// What decoding actually produced.
        got: usize,
    },
    /// A ZIP member inside an FRF could not be read.
    Zip(String),
    /// Hex field contained a non-hex byte or odd length.
    BadHex(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Truncated {
                what,
                need,
                at,
                len,
            } => write!(
                f,
                "truncated {what}: need {need:#x} bytes at {at:#x}, input is {len:#x} bytes"
            ),
            Error::BadMagic(what) => write!(f, "bad magic for {what}"),
            Error::Unsupported(s) => write!(f, "unsupported: {s}"),
            Error::MissingKey(s) => write!(f, "missing or invalid caller-supplied key: {s}"),
            Error::Xml(s) => write!(f, "XML error: {s}"),
            Error::SizeMismatch { expected, got } => {
                write!(
                    f,
                    "decoded size mismatch: expected {expected:#x}, got {got:#x}"
                )
            }
            Error::Zip(s) => write!(f, "zip error: {s}"),
            Error::BadHex(s) => write!(f, "bad hex data: {s}"),
        }
    }
}

impl std::error::Error for Error {}

/// Crate result alias.
pub type Result<T> = std::result::Result<T, Error>;
