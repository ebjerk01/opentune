//! Typed errors for the SA2 bytecode VM.
//!
//! Every failure mode is explicit; the parser and executor never panic on
//! malformed input, out-of-range operands, or runaway programs.
use thiserror::Error;

/// An error raised while parsing or executing an SA2 program.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum Sa2Error {
    /// The seed length is outside the supported 2..=8 byte range.
    #[error("seed length {0} out of range (expected 2..=8 bytes)")]
    SeedLength(usize),

    /// The program byte stream ended in the middle of an instruction's operands.
    #[error(
        "program truncated: opcode 0x{opcode:02X} at offset {offset} needs more operand bytes"
    )]
    Truncated {
        /// Offset of the opcode whose operands ran past the end of the program.
        offset: usize,
        /// The opcode byte that was being decoded.
        opcode: u8,
    },

    /// An opcode byte that the instruction set does not define.
    #[error("unknown opcode 0x{opcode:02X} at offset {offset}")]
    UnknownOpcode {
        /// Offset of the offending byte.
        offset: usize,
        /// The undefined opcode byte.
        opcode: u8,
    },

    /// A register index operand referenced a register that does not exist.
    #[error("register index {index} out of range (0..{count})")]
    BadRegister {
        /// The requested register index.
        index: u8,
        /// Number of registers the VM provides.
        count: u8,
    },

    /// A branch/call/loop target did not land on an instruction boundary.
    #[error("branch target offset {target} is not a valid instruction boundary")]
    BadTarget {
        /// The byte offset the branch tried to jump to.
        target: usize,
    },

    /// An operand value was outside the range an opcode accepts.
    #[error("invalid operand for opcode 0x{opcode:02X}: {detail}")]
    BadOperand {
        /// The opcode whose operand was rejected.
        opcode: u8,
        /// Human-readable reason.
        detail: &'static str,
    },

    /// `RET` executed with no matching `CALL` on the stack.
    #[error("RET with empty call stack")]
    CallStackUnderflow,

    /// The call stack grew past its depth limit.
    #[error("call stack overflow (depth limit {0})")]
    CallStackOverflow(usize),

    /// The program ran longer than the step budget (likely a non-terminating loop).
    #[error("execution exceeded the step budget of {0} instructions")]
    StepBudgetExceeded(u64),
}

/// Result type for SA2 VM operations.
pub type Sa2Result<T> = core::result::Result<T, Sa2Error>;
