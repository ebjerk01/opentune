//! The SA2 virtual machine: executes a parsed [`Program`] over a seed.
use crate::error::{Sa2Error, Sa2Result};
use crate::op::{Op, REG_COUNT};
use crate::program::Program;

/// Maximum number of instructions a single run may execute before the VM aborts
/// with [`Sa2Error::StepBudgetExceeded`]. Bounds non-terminating loops.
pub const MAX_STEPS: u64 = 100_000;

/// Maximum nesting depth of `CALL` before [`Sa2Error::CallStackOverflow`].
pub const MAX_CALL_DEPTH: usize = 64;

/// Split a 2..=8 byte seed into big-endian 32-bit words.
///
/// Each chunk of up to four bytes becomes its plain big-endian integer value
/// (a trailing partial word is right-aligned), so a 4-byte seed `AA BB CC DD`
/// yields `0xAABBCCDD` and a 2-byte seed `AA BB` yields `0x0000AABB`.
fn seed_to_words(seed: &[u8]) -> Vec<u32> {
    seed.chunks(4)
        .map(|chunk| chunk.iter().fold(0u32, |acc, &b| (acc << 8) | b as u32))
        .collect()
}

/// Run `program` over `seed`, returning the computed key bytes.
///
/// The seed must be 2..=8 bytes. The accumulator is seeded with the first seed
/// word and registers `0..` with the remaining words (zero-filled beyond the
/// seed). Every `OUTKEY` appends to the key; if a program emits none, the key
/// defaults to the final accumulator as four big-endian bytes.
pub fn execute(seed: &[u8], program: &[u8]) -> Sa2Result<Vec<u8>> {
    let program = Program::parse(program)?;
    run(seed, &program)
}

/// Run an already-parsed [`Program`] over `seed`.
pub fn run(seed: &[u8], program: &Program) -> Sa2Result<Vec<u8>> {
    if !(2..=8).contains(&seed.len()) {
        return Err(Sa2Error::SeedLength(seed.len()));
    }
    let words = seed_to_words(seed);

    let mut acc: u32 = words.first().copied().unwrap_or(0);
    let mut regs = [0u32; REG_COUNT as usize];
    for (reg, &word) in regs.iter_mut().zip(words.iter()) {
        *reg = word;
    }

    let mut call_stack: Vec<usize> = Vec::new();
    let mut out: Vec<u8> = Vec::new();
    let ops = program.ops();
    let mut pc = 0usize;
    let mut steps = 0u64;

    while pc < ops.len() {
        steps += 1;
        if steps > MAX_STEPS {
            return Err(Sa2Error::StepBudgetExceeded(MAX_STEPS));
        }

        let mut next = pc + 1;
        match ops[pc] {
            Op::Nop => {}

            Op::LoadImm(v) => acc = v,
            Op::LoadReg(r) => acc = regs[r as usize],
            Op::StoreReg(r) => regs[r as usize] = acc,
            Op::LoadSeed(n) => acc = words.get(n as usize).copied().unwrap_or(0),

            Op::AddImm(v) => acc = acc.wrapping_add(v),
            Op::AddReg(r) => acc = acc.wrapping_add(regs[r as usize]),
            Op::SubImm(v) => acc = acc.wrapping_sub(v),
            Op::SubReg(r) => acc = acc.wrapping_sub(regs[r as usize]),

            Op::AndImm(v) => acc &= v,
            Op::AndReg(r) => acc &= regs[r as usize],
            Op::OrImm(v) => acc |= v,
            Op::OrReg(r) => acc |= regs[r as usize],
            Op::XorImm(v) => acc ^= v,
            Op::XorReg(r) => acc ^= regs[r as usize],

            Op::RolImm(n) => acc = acc.rotate_left((n & 31) as u32),
            Op::RorImm(n) => acc = acc.rotate_right((n & 31) as u32),
            Op::RolReg(r) => acc = acc.rotate_left(regs[r as usize] & 31),
            Op::RorReg(r) => acc = acc.rotate_right(regs[r as usize] & 31),
            Op::ShlImm(n) => acc <<= (n & 31) as u32,
            Op::ShrImm(n) => acc >>= (n & 31) as u32,

            Op::SwapWords => acc = acc.rotate_left(16),
            Op::SwapBytes => acc = acc.swap_bytes(),
            Op::SwapNibbles => acc = swap_nibbles(acc),

            Op::Call(target) => {
                if call_stack.len() >= MAX_CALL_DEPTH {
                    return Err(Sa2Error::CallStackOverflow(MAX_CALL_DEPTH));
                }
                call_stack.push(next);
                next = program.resolve_target(target)?;
            }
            Op::Ret => {
                next = call_stack.pop().ok_or(Sa2Error::CallStackUnderflow)?;
            }
            Op::Loop { reg, target } => {
                let v = regs[reg as usize].wrapping_sub(1);
                regs[reg as usize] = v;
                if v != 0 {
                    next = program.resolve_target(target)?;
                }
            }
            Op::Jmp(target) => next = program.resolve_target(target)?,

            Op::OutKey(n) => {
                let be = acc.to_be_bytes();
                out.extend_from_slice(&be[(4 - n as usize)..]);
            }
            Op::Halt => break,
        }

        pc = next;
    }

    if out.is_empty() {
        out.extend_from_slice(&acc.to_be_bytes());
    }
    Ok(out)
}

/// Swap the high and low nibble of every byte of `v`.
fn swap_nibbles(v: u32) -> u32 {
    ((v & 0x0F0F_0F0F) << 4) | ((v & 0xF0F0_F0F0) >> 4)
}
