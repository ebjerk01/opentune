//! C166 / C167 / ST10 bootstrap-loader (BSL) client — **read only**.
//!
//! The C167 family has a mask-ROM bootstrap loader: hold `P0L.4` low through
//! a hardware reset and the CPU, instead of running its program, waits on
//! serial port ASC0 for a zero byte, measures it to set its baud rate, answers
//! with an identification byte and then loads exactly 32 bytes into internal
//! RAM at `00'FA40h` and jumps there. Those 32 bytes are a *primary loader*
//! that pulls in a larger *monitor kernel*; the kernel then answers a small
//! command protocol (test, write word, read word, read block, block checksum)
//! over the same line. This crate drives that sequence to read memory:
//! internal ROM/flash, or the external flash once the bus/chip-select
//! registers are set for the board ([`presets`]).
//!
//! * [`protocol`] — the public facts: identification bytes, the 32-byte rule,
//!   the monitor command bytes and replies, the BSL autobaud calculation and
//!   decoders for the `SYSCON`/`BUSCONx`/`ADDRSELx` values the presets write.
//! * [`presets`] — EDC15 (VAG VM+/P+, Mercedes C6) and generic C167 memory
//!   regions with their register setup, as data.
//! * [`plan`] — builds every step and byte exchange of a read up front;
//!   [`plan::ReadPlan::render`] prints it (the CLI's `opentune bsl plan`).
//! * [`engine`] — executes exactly that plan over a [`port::Port`], with
//!   half-duplex (K-line) echo checking, per-step timeouts and the block
//!   checksum verified.
//! * [`sim`] — a simulated C167 BSL + monitor kernel with fault injection
//!   (wrong ID, echo corruption, silence, bad checksum, NAK).
//!
//! **No loader ships with OpenTune.** The loader/kernel pair publicly used for
//! this (Infineon MiniMon's K-line `LOADK.bin` + `MINIMONK.bin`, bundled by
//! the GPL-3 tools cited in `docs/PRIOR-ART.md`) is Infineon freeware without
//! source, so the user supplies it; [`protocol::KNOWN_PAYLOADS`] only
//! recognises it by SHA-256. The read path is **unverified on hardware**. See
//! `docs/C167-BSL.md`.
use thiserror::Error;

pub mod engine;
pub mod plan;
pub mod port;
pub mod presets;
pub mod protocol;
pub mod sim;

pub use engine::{run_plan, Exchange, LinkOptions, ReadOutcome};
pub use plan::{build_read_plan, Expect, Phase, PlanInput, ReadPlan, RenderOptions, Step, Tx};
pub use port::{Port, SerialPort};
pub use presets::{find_preset, Preset, RegWrite, Region, PRESETS};
pub use protocol::ChipFamily;
pub use sim::{Fault, SimC167, SimConfig};

/// Everything that can stop a plan or a read.
#[derive(Debug, Error)]
pub enum BslError {
    #[error("serial I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid plan: {0}")]
    Plan(String),
    #[error("step {step} ({label}): {what} not supplied (pass the file)")]
    MissingPayload {
        step: usize,
        label: String,
        what: String,
    },
    #[error(
        "step {step} ({label}): timeout: expected {expected} byte(s), got {} [{}]{hint}",
        got.len(),
        protocol::hex(got)
    )]
    Timeout {
        step: usize,
        label: String,
        expected: usize,
        got: Vec<u8>,
        hint: &'static str,
    },
    #[error(
        "step {step} ({label}): echo mismatch: sent [{}], line echoed [{}] — wrong baud, \
         a second talker on the line, or a full-duplex link (try --no-echo)",
        protocol::hex(sent),
        protocol::hex(got)
    )]
    EchoMismatch {
        step: usize,
        label: String,
        sent: Vec<u8>,
        got: Vec<u8>,
    },
    #[error(
        "identification byte {got:#04x} ({}) is not one of the accepted [{}]",
        protocol::ChipFamily::from_id(*got).map_or("unknown", |f| f.name()),
        protocol::hex(accepted)
    )]
    WrongId { got: u8, accepted: Vec<u8> },
    #[error(
        "step {step} ({label}): expected {expected}, got [{}]",
        protocol::hex(got)
    )]
    UnexpectedReply {
        step: usize,
        label: String,
        expected: String,
        got: Vec<u8>,
    },
    #[error(
        "block at {addr:#08x}: checksum mismatch: kernel reported {reported:#04x}, \
         received data XORs to {computed:#04x}"
    )]
    ChecksumMismatch {
        addr: u32,
        computed: u8,
        reported: u8,
    },
    #[error("register {reg} ({addr:#06x}): wrote {wrote:#06x}, read back {read:#06x}")]
    SetupReadback {
        reg: String,
        addr: u32,
        wrote: u16,
        read: u16,
    },
}

pub type Result<T> = std::result::Result<T, BslError>;
