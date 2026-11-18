//! The VAG SA2 instruction set: opcode bytes, operand widths and the typed
//! [`Op`] form.
//!
//! An SA2 script is a flat byte string of variable-length instructions: one
//! opcode byte followed by zero, one or four operand bytes. There are ten
//! opcodes. Four-byte operands are big-endian. Branch operands are an unsigned
//! *forward* skip: a taken branch at offset `o` continues at `o + 2 + d`.
//!
//! | byte | mnemonic | operands | effect |
//! |------|----------|----------|--------|
//! | `0x81` | `rsl` | — | rotate the register left by one bit; carry := the bit rotated out (old bit 31) |
//! | `0x82` | `rsr` | — | rotate the register right by one bit; carry := the bit rotated out (old bit 0) |
//! | `0x93` | `add` | imm32 | register += imm (mod 2³²); carry := unsigned overflow |
//! | `0x84` | `sub` | imm32 | register −= imm (mod 2³²); carry := borrow (imm > register) |
//! | `0x87` | `eor` | imm32 | register ^= imm; carry unchanged |
//! | `0x68` | `loop` | n (u8) | open a loop: the bytes after this instruction up to the matching `next` run `n` times (`n` ≥ 1) |
//! | `0x49` | `next` | — | close the innermost loop: jump back to its body while iterations remain, else drop it and fall through |
//! | `0x4A` | `bcc` | d (u8) | branch forward `d` bytes past the end of this instruction if carry is **clear** |
//! | `0x6B` | `bra` | d (u8) | branch forward `d` bytes past the end of this instruction, unconditionally |
//! | `0x4C` | `finish` | — | end of script: the register is the key |
//!
//! The encoding and semantics are taken from public descriptions of the SA2
//! VM (see the crate documentation for sources and licences); this module is
//! written independently from those descriptions.

use crate::error::{Sa2Error, Sa2Result};
use core::fmt;

/// `rsl`: rotate left one bit through the carry output.
pub const OP_RSL: u8 = 0x81;
/// `rsr`: rotate right one bit through the carry output.
pub const OP_RSR: u8 = 0x82;
/// `add imm32`.
pub const OP_ADD: u8 = 0x93;
/// `sub imm32`.
pub const OP_SUB: u8 = 0x84;
/// `eor imm32` (exclusive or).
pub const OP_EOR: u8 = 0x87;
/// `loop n`: start of a counted loop.
pub const OP_LOOP: u8 = 0x68;
/// `next`: end of the innermost loop.
pub const OP_NEXT: u8 = 0x49;
/// `bcc d`: forward branch when carry is clear.
pub const OP_BCC: u8 = 0x4A;
/// `bra d`: unconditional forward branch.
pub const OP_BRA: u8 = 0x6B;
/// `finish`: end of script.
pub const OP_FINISH: u8 = 0x4C;

/// Every opcode byte, in table order.
pub const OPCODES: [u8; 10] = [
    OP_RSL, OP_RSR, OP_ADD, OP_SUB, OP_EOR, OP_LOOP, OP_NEXT, OP_BCC, OP_BRA, OP_FINISH,
];

/// Number of operand bytes that follow `opcode`, or `None` if it is not an
/// SA2 opcode.
pub const fn operand_len(opcode: u8) -> Option<usize> {
    match opcode {
        OP_RSL | OP_RSR | OP_NEXT | OP_FINISH => Some(0),
        OP_LOOP | OP_BCC | OP_BRA => Some(1),
        OP_ADD | OP_SUB | OP_EOR => Some(4),
        _ => None,
    }
}

/// One decoded SA2 instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Op {
    /// Rotate left by one; carry := old bit 31.
    Rsl,
    /// Rotate right by one; carry := old bit 0.
    Rsr,
    /// Wrapping add; carry := overflow.
    Add(u32),
    /// Wrapping subtract; carry := borrow.
    Sub(u32),
    /// Exclusive or; carry unchanged.
    Eor(u32),
    /// Open a loop whose body runs this many times.
    Loop(u8),
    /// Close the innermost loop.
    Next,
    /// Branch forward this many bytes (past the instruction) if carry is clear.
    Bcc(u8),
    /// Branch forward this many bytes (past the instruction).
    Bra(u8),
    /// End of script.
    Finish,
}

impl Op {
    /// The opcode byte.
    pub const fn opcode(&self) -> u8 {
        match self {
            Op::Rsl => OP_RSL,
            Op::Rsr => OP_RSR,
            Op::Add(_) => OP_ADD,
            Op::Sub(_) => OP_SUB,
            Op::Eor(_) => OP_EOR,
            Op::Loop(_) => OP_LOOP,
            Op::Next => OP_NEXT,
            Op::Bcc(_) => OP_BCC,
            Op::Bra(_) => OP_BRA,
            Op::Finish => OP_FINISH,
        }
    }

    /// The lower-case mnemonic used by the assembler and disassembler.
    pub const fn mnemonic(&self) -> &'static str {
        match self {
            Op::Rsl => "rsl",
            Op::Rsr => "rsr",
            Op::Add(_) => "add",
            Op::Sub(_) => "sub",
            Op::Eor(_) => "eor",
            Op::Loop(_) => "loop",
            Op::Next => "next",
            Op::Bcc(_) => "bcc",
            Op::Bra(_) => "bra",
            Op::Finish => "finish",
        }
    }

    /// Encoded length in bytes (opcode plus operands).
    pub const fn len(&self) -> usize {
        match operand_len(self.opcode()) {
            Some(n) => 1 + n,
            None => 1, // unreachable: every Op has a defined opcode
        }
    }

    /// Always false: every instruction is at least one byte. Present so the
    /// `len` method has its conventional companion.
    pub const fn is_empty(&self) -> bool {
        false
    }

    /// For a branch, the absolute script offset a taken branch continues at,
    /// given the offset this instruction starts at.
    pub const fn branch_target(&self, at: usize) -> Option<usize> {
        match self {
            Op::Bcc(d) | Op::Bra(d) => Some(at + 2 + *d as usize),
            _ => None,
        }
    }

    /// Decode the instruction that starts at `offset` in `script`.
    ///
    /// Fails with [`Sa2Error::UnknownOpcode`] or [`Sa2Error::Truncated`]; an
    /// `offset` at or past the end is reported as truncated opcode `0x00`.
    pub fn decode(script: &[u8], offset: usize) -> Sa2Result<Op> {
        let Some(&opcode) = script.get(offset) else {
            return Err(Sa2Error::Truncated {
                offset,
                opcode: 0,
                needed: 1,
            });
        };
        let needed = operand_len(opcode).ok_or(Sa2Error::UnknownOpcode { offset, opcode })?;
        let operands = script
            .get(offset + 1..offset + 1 + needed)
            .ok_or(Sa2Error::Truncated {
                offset,
                opcode,
                needed,
            })?;
        let imm = || u32::from_be_bytes([operands[0], operands[1], operands[2], operands[3]]);
        Ok(match opcode {
            OP_RSL => Op::Rsl,
            OP_RSR => Op::Rsr,
            OP_ADD => Op::Add(imm()),
            OP_SUB => Op::Sub(imm()),
            OP_EOR => Op::Eor(imm()),
            OP_LOOP => Op::Loop(operands[0]),
            OP_NEXT => Op::Next,
            OP_BCC => Op::Bcc(operands[0]),
            OP_BRA => Op::Bra(operands[0]),
            _ => Op::Finish, // OP_FINISH: operand_len() admitted only the ten opcodes
        })
    }

    /// Append this instruction's bytes to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.push(self.opcode());
        match *self {
            Op::Add(v) | Op::Sub(v) | Op::Eor(v) => out.extend_from_slice(&v.to_be_bytes()),
            Op::Loop(n) | Op::Bcc(n) | Op::Bra(n) => out.push(n),
            Op::Rsl | Op::Rsr | Op::Next | Op::Finish => {}
        }
    }
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Op::Add(v) | Op::Sub(v) | Op::Eor(v) => write!(f, "{} 0x{v:08X}", self.mnemonic()),
            Op::Loop(n) => write!(f, "loop {n}"),
            Op::Bcc(d) | Op::Bra(d) => write!(f, "{} +{d}", self.mnemonic()),
            _ => f.write_str(self.mnemonic()),
        }
    }
}

/// Encode a sequence of instructions into a script.
pub fn assemble(ops: &[Op]) -> Vec<u8> {
    let mut out = Vec::new();
    for op in ops {
        op.encode(&mut out);
    }
    out
}
