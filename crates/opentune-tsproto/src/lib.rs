//! Serial protocol for TunerStudio-compatible aftermarket ECUs.
//!
//! The command strings come from the ECU's INI ([`opentune_tsini::Ini`]):
//! `queryCommand`, `pageReadCommand`, `pageChunkWrite`/`pageValueWrite`,
//! `burnCommand`, `crc32CheckCommand`, `ochGetCommand`, sized by `pageSize`,
//! `blockingFactor` and `ochBlockSize`. They are framed one of two ways
//! ([`Framing`]):
//!
//! - **msEnvelope** (Speeduino new comms, rusEFI, MS3): `[u16 BE length]
//!   [payload] [CRC32 BE]`, responses start with a status byte
//!   ([`framing`]);
//! - **plain** (MS1/MS2 legacy, Speeduino compatibility mode): raw bytes, the
//!   response length known from the request.
//!
//! Safety model: [`Client`] reads freely, but every method that changes the
//! ECU takes a [`WriteAuth`], which a front end creates only after the user
//! explicitly confirmed the write. [`write_tune`] writes only the bytes that
//! differ, reads every written page back and compares it with the target,
//! and burns only after every page verified; a verify failure burns nothing.
//!
//! [`sim::SimEcu`] is an in-memory ECU that answers the same INI-defined
//! commands, for tests and the CLI's `--port sim`.
use thiserror::Error;

pub mod client;
pub mod crc;
pub mod framing;
pub mod ops;
pub mod port;
pub mod sim;

pub use client::{Client, ClientOptions, Framing, SignatureCheck, Stats};
pub use ops::{
    plan_write, write_tune, PageChange, WriteOptions, WritePlan, WriteReport, WriteStage,
};
pub use port::{Port, SerialPort};
pub use sim::{Fault, SimEcu, SimEvent, SimOptions};

/// Permission to change the ECU. Create it only after the user explicitly
/// confirmed the write (e.g. a `--confirm` flag or a confirmation dialog).
#[derive(Debug)]
pub struct WriteAuth {
    _private: (),
}

impl WriteAuth {
    /// The caller asserts that the user explicitly confirmed this write.
    pub fn user_confirmed() -> Self {
        Self { _private: () }
    }
}

/// Protocol and transfer errors.
#[derive(Debug, Error)]
pub enum ProtoError {
    #[error("serial I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("timeout waiting for {what}: got {got} of {expected} bytes")]
    Timeout {
        what: String,
        got: usize,
        expected: usize,
    },
    #[error("response CRC mismatch (frame says {expected:#010x}, computed {actual:#010x})")]
    Crc { expected: u32, actual: u32 },
    #[error("bad response: {0}")]
    Frame(String),
    #[error("ECU answered {name} (0x{code:02X}) to {command}")]
    Ecu {
        code: u8,
        name: &'static str,
        command: String,
    },
    #[error("INI: {0}")]
    Definition(String),
    #[error(
        "ECU signature {ecu:?} does not match the INI signature {ini:?}; load the INI for this firmware"
    )]
    SignatureMismatch { ecu: String, ini: String },
    #[error(
        "read-back verify failed on page {page} at offset {offset}: wrote 0x{wrote:02X}, read 0x{read:02X}; nothing was burned"
    )]
    Verify {
        page: usize,
        offset: usize,
        wrote: u8,
        read: u8,
    },
    #[error("page {page}: ECU CRC32 0x{ecu:08X} after burn, expected 0x{local:08X}")]
    CrcCheck { page: usize, ecu: u32, local: u32 },
    #[error(transparent)]
    Tune(#[from] opentune_tsini::Error),
}

pub type Result<T> = std::result::Result<T, ProtoError>;
