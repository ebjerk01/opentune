//! Typed errors for the SA2 seed/key VM, its decoder and its assembler.
//!
//! Every failure mode is explicit; the decoder, the VM and the assembler never
//! panic on malformed input, out-of-range operands or runaway programs.
use thiserror::Error;

/// An error raised while decoding, executing or assembling an SA2 script.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum Sa2Error {
    /// The seed is not exactly four bytes (SA2 works on one 32-bit register).
    #[error("seed length {0} not supported (SA2 seeds are exactly 4 bytes)")]
    SeedLength(usize),

    /// The script ended in the middle of an instruction's operand bytes.
    #[error(
        "script truncated: opcode 0x{opcode:02X} at offset {offset} needs {needed} operand byte(s)"
    )]
    Truncated {
        /// Offset of the opcode whose operands ran past the end of the script.
        offset: usize,
        /// The opcode byte that was being decoded.
        opcode: u8,
        /// How many operand bytes the opcode takes.
        needed: usize,
    },

    /// A byte that is not one of the ten SA2 opcodes.
    #[error("unknown opcode 0x{opcode:02X} at offset {offset}")]
    UnknownOpcode {
        /// Offset of the offending byte.
        offset: usize,
        /// The undefined opcode byte.
        opcode: u8,
    },

    /// A loop (`0x68`) with an iteration count of zero. The public reference
    /// implementations disagree on what it means (run once vs. 256 times) and
    /// no known script uses it, so it is refused rather than guessed.
    #[error("loop at offset {offset} has iteration count 0 (ambiguous; refused)")]
    ZeroLoopCount {
        /// Offset of the loop opcode.
        offset: usize,
    },

    /// A loop end (`0x49`) executed with no open loop.
    #[error("loop end (0x49) at offset {offset} without an open loop")]
    NextWithoutLoop {
        /// Offset of the loop-end opcode.
        offset: usize,
    },

    /// More loops were open at once than the VM allows.
    #[error("loop nesting deeper than {0}")]
    LoopStackOverflow(usize),

    /// The script ran longer than the step budget.
    #[error("execution exceeded the step budget of {0} instructions")]
    StepBudgetExceeded(u64),

    /// The text assembler rejected a line.
    #[error("assembler line {line}: {msg}")]
    Asm {
        /// 1-based source line number.
        line: usize,
        /// What was wrong.
        msg: String,
    },

    /// A hex string could not be decoded.
    #[error("bad hex: {0}")]
    BadHex(String),
}

/// Result type for SA2 operations.
pub type Sa2Result<T> = core::result::Result<T, Sa2Error>;
