//! Error and Result types used across OpenTune.
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("not supported by this interface: {0}")]
    Unsupported(String),
    #[error("not connected to an ECU")]
    NotConnected,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("timed out after {0} ms")]
    Timeout(u64),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("negative response to service 0x{service:02X}: 0x{code:02X} ({name})")]
    NegativeResponse {
        service: u8,
        code: u8,
        name: &'static str,
    },
    #[error("security access denied")]
    SecurityDenied,
    #[error("verification failed: {0}")]
    Verify(String),
    #[error("safety check failed: {0}")]
    Safety(String),
    #[error("configuration error: {0}")]
    Config(String),
    /// The interface adapter reset or lost its configuration mid-session (e.g.
    /// an ELM327 printed its banner or started echoing again). The session
    /// state is gone; the caller must re-open the interface.
    #[error("interface adapter reset: {0}")]
    AdapterReset(String),
}

pub type Result<T> = std::result::Result<T, Error>;
