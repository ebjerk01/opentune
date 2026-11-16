//! Error type for CBF parsing.

/// Convenience alias for results produced while parsing a CBF file.
pub type Result<T> = std::result::Result<T, CbfError>;

/// Everything that can go wrong while reading a `.cbf` file.
#[derive(Debug, thiserror::Error)]
pub enum CbfError {
    /// A read ran past the end of the buffer (truncated or malformed file).
    #[error("unexpected end of data while reading CBF")]
    UnexpectedEof,

    /// The file does not start with the expected `CBF-TRANSLATOR-VERSION:04.xx`
    /// stub header, so it is not a supported CBF container.
    #[error("not a CBF 4.x file: {0}")]
    BadMagic(String),

    /// A structural value in the file was impossible (e.g. a pool whose size
    /// does not fit the file), indicating corruption or an unsupported variant.
    #[error("malformed CBF structure: {0}")]
    Malformed(String),

    /// Wrapper for `std::io` failures when reading a file from disk.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Serialising the parsed model to JSON failed.
    #[error("json serialisation error: {0}")]
    Json(#[from] serde_json::Error),
}
