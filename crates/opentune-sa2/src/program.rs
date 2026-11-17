//! Parsing a raw SA2 byte stream into a typed, executable [`Program`].
use crate::error::{Sa2Error, Sa2Result};
use crate::op::*;

/// A parsed SA2 program: the decoded instructions plus the byte offset each one
/// started at, so that branch targets (expressed as byte offsets) can be
/// resolved to instruction indices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    ops: Vec<Op>,
    /// `offsets[i]` is the program byte offset where `ops[i]` began; kept sorted
    /// and strictly increasing. A final entry equal to the program length marks
    /// the end, which is a valid (terminal) branch target.
    offsets: Vec<usize>,
}

impl Program {
    /// Decode a raw SA2 byte program.
    ///
    /// Returns [`Sa2Error::UnknownOpcode`] for an undefined opcode,
    /// [`Sa2Error::Truncated`] if operand bytes run past the end, and
    /// [`Sa2Error::BadRegister`] for an out-of-range register index.
    pub fn parse(bytes: &[u8]) -> Sa2Result<Self> {
        let mut ops = Vec::new();
        let mut offsets = Vec::new();
        let mut pc = 0usize;

        while pc < bytes.len() {
            let start = pc;
            let opcode = bytes[pc];
            pc += 1;

            let op = match opcode {
                OP_NOP => Op::Nop,
                OP_LOAD_IMM => Op::LoadImm(read_u32(bytes, &mut pc, start, opcode)?),
                OP_LOAD_REG => Op::LoadReg(read_reg(bytes, &mut pc, start, opcode)?),
                OP_STORE_REG => Op::StoreReg(read_reg(bytes, &mut pc, start, opcode)?),
                OP_LOAD_SEED => Op::LoadSeed(read_u8(bytes, &mut pc, start, opcode)?),

                OP_ADD_IMM => Op::AddImm(read_u32(bytes, &mut pc, start, opcode)?),
                OP_ADD_REG => Op::AddReg(read_reg(bytes, &mut pc, start, opcode)?),
                OP_SUB_IMM => Op::SubImm(read_u32(bytes, &mut pc, start, opcode)?),
                OP_SUB_REG => Op::SubReg(read_reg(bytes, &mut pc, start, opcode)?),

                OP_AND_IMM => Op::AndImm(read_u32(bytes, &mut pc, start, opcode)?),
                OP_AND_REG => Op::AndReg(read_reg(bytes, &mut pc, start, opcode)?),
                OP_OR_IMM => Op::OrImm(read_u32(bytes, &mut pc, start, opcode)?),
                OP_OR_REG => Op::OrReg(read_reg(bytes, &mut pc, start, opcode)?),
                OP_XOR_IMM => Op::XorImm(read_u32(bytes, &mut pc, start, opcode)?),
                OP_XOR_REG => Op::XorReg(read_reg(bytes, &mut pc, start, opcode)?),

                OP_ROL_IMM => Op::RolImm(read_u8(bytes, &mut pc, start, opcode)?),
                OP_ROR_IMM => Op::RorImm(read_u8(bytes, &mut pc, start, opcode)?),
                OP_ROL_REG => Op::RolReg(read_reg(bytes, &mut pc, start, opcode)?),
                OP_ROR_REG => Op::RorReg(read_reg(bytes, &mut pc, start, opcode)?),
                OP_SHL_IMM => Op::ShlImm(read_u8(bytes, &mut pc, start, opcode)?),
                OP_SHR_IMM => Op::ShrImm(read_u8(bytes, &mut pc, start, opcode)?),

                OP_SWAP_WORDS => Op::SwapWords,
                OP_SWAP_BYTES => Op::SwapBytes,
                OP_SWAP_NIBBLES => Op::SwapNibbles,

                OP_CALL => Op::Call(read_u16(bytes, &mut pc, start, opcode)?),
                OP_RET => Op::Ret,
                OP_LOOP => {
                    let reg = read_reg(bytes, &mut pc, start, opcode)?;
                    let target = read_u16(bytes, &mut pc, start, opcode)?;
                    Op::Loop { reg, target }
                }
                OP_JMP => Op::Jmp(read_u16(bytes, &mut pc, start, opcode)?),

                OP_OUTKEY => {
                    let n = read_u8(bytes, &mut pc, start, opcode)?;
                    if !(1..=4).contains(&n) {
                        return Err(Sa2Error::BadOperand {
                            opcode,
                            detail: "OUTKEY byte count must be 1..=4",
                        });
                    }
                    Op::OutKey(n)
                }
                OP_HALT => Op::Halt,

                other => {
                    return Err(Sa2Error::UnknownOpcode {
                        offset: start,
                        opcode: other,
                    })
                }
            };

            ops.push(op);
            offsets.push(start);
        }

        offsets.push(bytes.len());
        Ok(Program { ops, offsets })
    }

    /// The decoded instructions, in program order.
    pub fn ops(&self) -> &[Op] {
        &self.ops
    }

    /// Number of instructions.
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// Whether the program has no instructions.
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Resolve a branch-target byte offset to the instruction index at that
    /// offset. An offset equal to the program length resolves to `len()`
    /// (the terminal position), which stops execution cleanly.
    pub(crate) fn resolve_target(&self, target: u16) -> Sa2Result<usize> {
        let target = target as usize;
        match self.offsets.binary_search(&target) {
            Ok(index) => Ok(index),
            Err(_) => Err(Sa2Error::BadTarget { target }),
        }
    }
}

// --- operand readers -------------------------------------------------------

fn read_u8(bytes: &[u8], pc: &mut usize, start: usize, opcode: u8) -> Sa2Result<u8> {
    let b = *bytes.get(*pc).ok_or(Sa2Error::Truncated {
        offset: start,
        opcode,
    })?;
    *pc += 1;
    Ok(b)
}

fn read_reg(bytes: &[u8], pc: &mut usize, start: usize, opcode: u8) -> Sa2Result<u8> {
    let index = read_u8(bytes, pc, start, opcode)?;
    if index >= REG_COUNT {
        return Err(Sa2Error::BadRegister {
            index,
            count: REG_COUNT,
        });
    }
    Ok(index)
}

fn read_u16(bytes: &[u8], pc: &mut usize, start: usize, opcode: u8) -> Sa2Result<u16> {
    let hi = read_u8(bytes, pc, start, opcode)? as u16;
    let lo = read_u8(bytes, pc, start, opcode)? as u16;
    Ok((hi << 8) | lo)
}

fn read_u32(bytes: &[u8], pc: &mut usize, start: usize, opcode: u8) -> Sa2Result<u32> {
    let mut v = 0u32;
    for _ in 0..4 {
        v = (v << 8) | read_u8(bytes, pc, start, opcode)? as u32;
    }
    Ok(v)
}
