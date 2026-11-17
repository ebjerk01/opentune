//! The SA2 instruction set: opcode byte constants and the typed [`Op`] form.
//!
//! The operation *classes* here (load/store, add/sub, bitwise, rotate/shift,
//! byte/nibble swaps, subroutine/loop, output-key) follow the publicly
//! documented SA2 computational model as described by the `jglim/UnlockECU`
//! project (MIT). This is a clean-room reimplementation of that **model**: the
//! opcode byte assignments below are OpenTune's own, self-consistent encoding,
//! chosen to be readable and auditable. No OEM opcode table, bytecode, key, or
//! fixed algorithm is reproduced — a per-ECU SA2 program is always supplied by
//! the caller (from the vehicle's own A2L/ODX description or by the user).
//!
//! # Encoding
//!
//! Every instruction is one opcode byte, optionally followed by operand bytes:
//!
//! * `imm32` — a big-endian 4-byte immediate.
//! * `reg`   — a single register-index byte (`0..`[`REG_COUNT`]).
//! * `n`     — a single count byte (shift/rotate amount, or key-byte count).
//! * `addr`  — a big-endian 2-byte **byte offset** into the program, used as a
//!   branch target. A target must land exactly on an instruction boundary.

/// Number of general-purpose 32-bit registers the VM exposes.
pub const REG_COUNT: u8 = 8;

// --- Opcode byte constants -------------------------------------------------

/// Do nothing.
pub const OP_NOP: u8 = 0x00;

/// `acc = imm32`.
pub const OP_LOAD_IMM: u8 = 0x10;
/// `acc = regs[reg]`.
pub const OP_LOAD_REG: u8 = 0x11;
/// `regs[reg] = acc`.
pub const OP_STORE_REG: u8 = 0x12;
/// `acc = seed_word[n]` (0 if that word index is past the seed).
pub const OP_LOAD_SEED: u8 = 0x13;

/// `acc = acc.wrapping_add(imm32)`.
pub const OP_ADD_IMM: u8 = 0x20;
/// `acc = acc.wrapping_add(regs[reg])`.
pub const OP_ADD_REG: u8 = 0x21;
/// `acc = acc.wrapping_sub(imm32)`.
pub const OP_SUB_IMM: u8 = 0x22;
/// `acc = acc.wrapping_sub(regs[reg])`.
pub const OP_SUB_REG: u8 = 0x23;

/// `acc &= imm32`.
pub const OP_AND_IMM: u8 = 0x30;
/// `acc &= regs[reg]`.
pub const OP_AND_REG: u8 = 0x31;
/// `acc |= imm32`.
pub const OP_OR_IMM: u8 = 0x32;
/// `acc |= regs[reg]`.
pub const OP_OR_REG: u8 = 0x33;
/// `acc ^= imm32`.
pub const OP_XOR_IMM: u8 = 0x34;
/// `acc ^= regs[reg]`.
pub const OP_XOR_REG: u8 = 0x35;

/// `acc = acc.rotate_left(n & 31)`.
pub const OP_ROL_IMM: u8 = 0x40;
/// `acc = acc.rotate_right(n & 31)`.
pub const OP_ROR_IMM: u8 = 0x41;
/// `acc = acc.rotate_left(regs[reg] & 31)`.
pub const OP_ROL_REG: u8 = 0x42;
/// `acc = acc.rotate_right(regs[reg] & 31)`.
pub const OP_ROR_REG: u8 = 0x43;
/// `acc <<= (n & 31)` (logical, zero-fill).
pub const OP_SHL_IMM: u8 = 0x44;
/// `acc >>= (n & 31)` (logical, zero-fill).
pub const OP_SHR_IMM: u8 = 0x45;

/// Swap the two 16-bit halves of `acc`.
pub const OP_SWAP_WORDS: u8 = 0x50;
/// Reverse the four bytes of `acc` (endianness flip).
pub const OP_SWAP_BYTES: u8 = 0x51;
/// Swap the high and low nibble of every byte of `acc`.
pub const OP_SWAP_NIBBLES: u8 = 0x52;

/// Push the next instruction's index and jump to `addr`.
pub const OP_CALL: u8 = 0x60;
/// Pop a return index and jump to it.
pub const OP_RET: u8 = 0x61;
/// `regs[reg] -= 1`; if it is still non-zero, jump to `addr` (count-down loop).
pub const OP_LOOP: u8 = 0x62;
/// Unconditional jump to `addr`.
pub const OP_JMP: u8 = 0x63;

/// Append the low `n` bytes of `acc` (big-endian order) to the key output.
pub const OP_OUTKEY: u8 = 0x70;
/// Stop execution.
pub const OP_HALT: u8 = 0xFF;

/// A decoded SA2 instruction.
///
/// `imm32` operands are kept as `u32`; `reg`/`n` as `u8`; branch targets as the
/// raw program **byte offset** (resolved to an instruction index at run time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(missing_docs)] // variant names mirror the opcode constants documented above
pub enum Op {
    Nop,

    LoadImm(u32),
    LoadReg(u8),
    StoreReg(u8),
    LoadSeed(u8),

    AddImm(u32),
    AddReg(u8),
    SubImm(u32),
    SubReg(u8),

    AndImm(u32),
    AndReg(u8),
    OrImm(u32),
    OrReg(u8),
    XorImm(u32),
    XorReg(u8),

    RolImm(u8),
    RorImm(u8),
    RolReg(u8),
    RorReg(u8),
    ShlImm(u8),
    ShrImm(u8),

    SwapWords,
    SwapBytes,
    SwapNibbles,

    /// Call the subroutine at the given program byte offset.
    Call(u16),
    Ret,
    /// Decrement `regs[reg]`; branch to the byte offset while it stays non-zero.
    Loop {
        reg: u8,
        target: u16,
    },
    /// Jump to the given program byte offset.
    Jmp(u16),

    /// Emit the low `n` bytes of `acc` as key output.
    OutKey(u8),
    Halt,
}

impl Op {
    /// Append this instruction's opcode and operand bytes to `out`.
    ///
    /// The inverse of parsing: `Program::parse(&assemble(ops))` yields `ops`
    /// back (when branch targets are valid instruction boundaries).
    pub fn encode(&self, out: &mut Vec<u8>) {
        match *self {
            Op::Nop => out.push(OP_NOP),

            Op::LoadImm(v) => push_u32(out, OP_LOAD_IMM, v),
            Op::LoadReg(r) => out.extend_from_slice(&[OP_LOAD_REG, r]),
            Op::StoreReg(r) => out.extend_from_slice(&[OP_STORE_REG, r]),
            Op::LoadSeed(n) => out.extend_from_slice(&[OP_LOAD_SEED, n]),

            Op::AddImm(v) => push_u32(out, OP_ADD_IMM, v),
            Op::AddReg(r) => out.extend_from_slice(&[OP_ADD_REG, r]),
            Op::SubImm(v) => push_u32(out, OP_SUB_IMM, v),
            Op::SubReg(r) => out.extend_from_slice(&[OP_SUB_REG, r]),

            Op::AndImm(v) => push_u32(out, OP_AND_IMM, v),
            Op::AndReg(r) => out.extend_from_slice(&[OP_AND_REG, r]),
            Op::OrImm(v) => push_u32(out, OP_OR_IMM, v),
            Op::OrReg(r) => out.extend_from_slice(&[OP_OR_REG, r]),
            Op::XorImm(v) => push_u32(out, OP_XOR_IMM, v),
            Op::XorReg(r) => out.extend_from_slice(&[OP_XOR_REG, r]),

            Op::RolImm(n) => out.extend_from_slice(&[OP_ROL_IMM, n]),
            Op::RorImm(n) => out.extend_from_slice(&[OP_ROR_IMM, n]),
            Op::RolReg(r) => out.extend_from_slice(&[OP_ROL_REG, r]),
            Op::RorReg(r) => out.extend_from_slice(&[OP_ROR_REG, r]),
            Op::ShlImm(n) => out.extend_from_slice(&[OP_SHL_IMM, n]),
            Op::ShrImm(n) => out.extend_from_slice(&[OP_SHR_IMM, n]),

            Op::SwapWords => out.push(OP_SWAP_WORDS),
            Op::SwapBytes => out.push(OP_SWAP_BYTES),
            Op::SwapNibbles => out.push(OP_SWAP_NIBBLES),

            Op::Call(t) => push_u16(out, OP_CALL, t),
            Op::Ret => out.push(OP_RET),
            Op::Loop { reg, target } => {
                out.push(OP_LOOP);
                out.push(reg);
                out.extend_from_slice(&target.to_be_bytes());
            }
            Op::Jmp(t) => push_u16(out, OP_JMP, t),

            Op::OutKey(n) => out.extend_from_slice(&[OP_OUTKEY, n]),
            Op::Halt => out.push(OP_HALT),
        }
    }
}

/// Assemble a slice of [`Op`]s into a raw SA2 byte program.
pub fn assemble(ops: &[Op]) -> Vec<u8> {
    let mut out = Vec::new();
    for op in ops {
        op.encode(&mut out);
    }
    out
}

fn push_u16(out: &mut Vec<u8>, opcode: u8, v: u16) {
    out.push(opcode);
    out.extend_from_slice(&v.to_be_bytes());
}

fn push_u32(out: &mut Vec<u8>, opcode: u8, v: u32) {
    out.push(opcode);
    out.extend_from_slice(&v.to_be_bytes());
}
