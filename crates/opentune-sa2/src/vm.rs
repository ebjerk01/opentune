//! The SA2 virtual machine.
//!
//! # Machine model
//!
//! * One 32-bit **register**, loaded with the 4-byte seed (big-endian) before
//!   the first instruction. When the script ends, the register is the key
//!   (emitted as 4 big-endian bytes).
//! * One **carry** flag, clear at start. `rsl`/`rsr` set it to the bit
//!   rotated out, `add` to the unsigned overflow and `sub` to the borrow;
//!   `eor`, loops and branches leave it unchanged. Only `bcc` reads it.
//! * A **loop stack**. `loop n` pushes (body start, `n - 1` iterations left)
//!   where the body starts right after the `loop` instruction; `next` jumps
//!   back to the innermost body while iterations remain and otherwise pops it
//!   and falls through. Loops nest; `next` always pairs with the innermost
//!   open loop at run time.
//! * Instructions are decoded **at the program counter as execution reaches
//!   them** — there is no up-front parse, because branches are raw forward byte
//!   skips. A branch that lands on or past the end of the script ends it, as
//!   does `finish` or running off the end.
//!
//! Bounded: a step budget ([`MAX_STEPS`] by default) and a loop-nesting limit
//! ([`MAX_LOOP_DEPTH`]) stop runaway scripts with a typed error. Nothing here
//! panics on any input.

use crate::error::{Sa2Error, Sa2Result};
use crate::op::Op;

/// Default maximum number of executed instructions per run. Real scripts run
/// in the low hundreds; three nested 255-iteration loops stay far below this.
pub const MAX_STEPS: u64 = 1_000_000;

/// Maximum number of simultaneously open loops.
pub const MAX_LOOP_DEPTH: usize = 64;

/// One executed instruction, as reported to a trace callback (state *after*
/// the instruction).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    /// Script offset the instruction started at.
    pub offset: usize,
    /// The decoded instruction.
    pub op: Op,
    /// Register value after it executed.
    pub register: u32,
    /// Carry flag after it executed.
    pub carry: bool,
    /// Number of open loops after it executed.
    pub loop_depth: usize,
}

/// Result of a completed run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    /// Final register value: the key.
    pub key: u32,
    /// Instructions executed.
    pub steps: u64,
}

/// Run `script` over a 32-bit `seed` and return the 32-bit key, with the
/// default step budget.
pub fn run(script: &[u8], seed: u32) -> Sa2Result<u32> {
    run_with(script, seed, MAX_STEPS, None).map(|o| o.key)
}

/// Run `script` over a 4-byte seed and return the 4-byte key (both
/// big-endian, as they travel in UDS/KWP service `0x27`).
pub fn execute(seed: &[u8], script: &[u8]) -> Sa2Result<Vec<u8>> {
    let seed: [u8; 4] = seed
        .try_into()
        .map_err(|_| Sa2Error::SeedLength(seed.len()))?;
    let key = run(script, u32::from_be_bytes(seed))?;
    Ok(key.to_be_bytes().to_vec())
}

/// Run `script` over `seed` with an explicit step `budget`, optionally calling
/// `trace` after every executed instruction.
pub fn run_with(
    script: &[u8],
    seed: u32,
    budget: u64,
    mut trace: Option<&mut dyn FnMut(&Step)>,
) -> Sa2Result<Outcome> {
    let mut register = seed;
    let mut carry = false;
    // (body start offset, iterations still to run after the current one)
    let mut loops: Vec<(usize, u8)> = Vec::new();
    let mut pc = 0usize;
    let mut steps = 0u64;

    while pc < script.len() {
        if steps >= budget {
            return Err(Sa2Error::StepBudgetExceeded(budget));
        }
        steps += 1;

        let at = pc;
        let op = Op::decode(script, at)?;
        pc = at + op.len();
        let mut done = false;

        match op {
            Op::Rsl => {
                carry = register & 0x8000_0000 != 0;
                register = register.rotate_left(1);
            }
            Op::Rsr => {
                carry = register & 1 != 0;
                register = register.rotate_right(1);
            }
            Op::Add(v) => (register, carry) = register.overflowing_add(v),
            Op::Sub(v) => (register, carry) = register.overflowing_sub(v),
            Op::Eor(v) => register ^= v,
            Op::Loop(n) => {
                if n == 0 {
                    return Err(Sa2Error::ZeroLoopCount { offset: at });
                }
                if loops.len() >= MAX_LOOP_DEPTH {
                    return Err(Sa2Error::LoopStackOverflow(MAX_LOOP_DEPTH));
                }
                loops.push((pc, n - 1));
            }
            Op::Next => {
                let top = loops
                    .last_mut()
                    .ok_or(Sa2Error::NextWithoutLoop { offset: at })?;
                if top.1 > 0 {
                    top.1 -= 1;
                    pc = top.0;
                } else {
                    loops.pop();
                }
            }
            Op::Bcc(_) => {
                if !carry {
                    pc = op.branch_target(at).unwrap_or(pc);
                }
            }
            Op::Bra(_) => pc = op.branch_target(at).unwrap_or(pc),
            Op::Finish => done = true,
        }

        if let Some(t) = trace.as_deref_mut() {
            t(&Step {
                offset: at,
                op,
                register,
                carry,
                loop_depth: loops.len(),
            });
        }
        if done {
            break;
        }
    }

    Ok(Outcome {
        key: register,
        steps,
    })
}
