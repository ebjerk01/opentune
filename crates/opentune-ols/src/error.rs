//! Error type for `.ols` parsing.

use std::fmt;

/// Something that prevented a `.ols` container from being read at all.
///
/// A recoverable, partial-fidelity condition (an unreadable map record, a
/// 64-bit cell type OpenTune cannot store, a missing axis) is **not** an error:
/// it is reported as a [`crate::OlsProject::warnings`] string and the rest of
/// the project is still returned. An `Error` means the bytes are not a WinOLS
/// project this crate can open, so nothing trustworthy can be produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The file is too short, or its magic / `"WinOLS File"` signature is
    /// absent. Either it is not a `.ols` file, or it is an encrypted /
    /// "protected" WinOLS project whose header is obfuscated (see the crate
    /// docs): in both cases the plaintext container layout is not present.
    NotContainer,
    /// The header is valid but no version directory could be located, so the
    /// embedded ROM image cannot be sliced out safely. Map records may still be
    /// present; callers that only need metadata can inspect what was returned
    /// before this error was raised, but this crate returns the error because an
    /// `.ols` with no extractable ROM is not usefully "imported".
    NoVersionDirectory,
    /// A version index was requested that the container does not contain.
    VersionOutOfRange {
        /// The index that was asked for.
        requested: u32,
        /// How many versions the container actually holds.
        available: u32,
    },
    /// The version directory points past the end of the file (truncated or
    /// corrupt container).
    Truncated,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotContainer => write!(
                f,
                "not a WinOLS .ols container (bad magic/signature, or an encrypted project)"
            ),
            Error::NoVersionDirectory => {
                write!(f, "no version directory found; cannot locate the ROM image")
            }
            Error::VersionOutOfRange {
                requested,
                available,
            } => write!(
                f,
                "version index {requested} out of range (container has {available} version(s))"
            ),
            Error::Truncated => write!(f, "version data extends past the end of the file"),
        }
    }
}

impl std::error::Error for Error {}

/// Convenience alias for this crate's fallible operations.
pub type Result<T> = std::result::Result<T, Error>;
