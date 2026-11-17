//! # opentune-sa2
//!
//! An **SA2 (Security Access, method 2) seed->key bytecode virtual machine**.
//!
//! VAG and Mercedes diagnostic security access (UDS/KWP service `0x27`) can
//! define the seed->key transform as a small **bytecode program** that the
//! tester runs over the seed the ECU returns. OpenTune implements the
//! **interpreter (VM) only**. The per-ECU SA2 program is **always
//! caller-supplied** — it comes from the vehicle's own A2L/ODX description or
//! from the user. This crate therefore ships **no** OEM bytecode, no keys, and
//! no fixed manufacturer algorithm set: the VM executing a user-supplied program
//! is clean and public, and it stays that way.
//!
//! ## What this crate provides
//!
//! * [`Program::parse`] — decode a raw SA2 byte program into typed [`Op`]s.
//! * [`execute`] — parse and run a program over a 2..=8 byte seed in one call.
//! * [`run`] — run an already-parsed [`Program`].
//! * [`assemble`] — the inverse of parsing, for building programs from [`Op`]s.
//! * [`Sa2SeedKey`] — an [`opentune_core::protocol::SeedKey`] implementation
//!   (id `"sa2"`) wrapping a caller-supplied program.
//!
//! ## Machine model
//!
//! * A 32-bit accumulator `acc` and [`REG_COUNT`] general 32-bit registers.
//! * The seed is split into big-endian 32-bit words; `acc` is seeded with the
//!   first word and the registers with the rest (zero-filled beyond the seed).
//! * Instructions cover load/store, add/sub, and/or/xor, rotate/shift (by an
//!   immediate or a register), byte/nibble swaps, `CALL`/`RET` subroutines,
//!   count-down `LOOP`, and `OUTKEY` to emit key bytes. See [`op`] for the full
//!   opcode table, each opcode documented.
//! * Runaway programs are bounded by a step budget ([`MAX_STEPS`]) and a call
//!   depth limit ([`MAX_CALL_DEPTH`]); all failure modes are typed
//!   ([`Sa2Error`]) and never panic.
//!
//! ## Attribution and clean-room stance
//!
//! The SA2 *computational model* and its operation classes follow the publicly
//! documented SA2 description popularised by the `jglim/UnlockECU` project
//! (MIT-licensed). This crate is a **clean-room reimplementation of that model
//! from its documented semantics**: the opcode byte assignments here are
//! OpenTune's own, readable encoding, and no source was copied verbatim. In
//! keeping with OpenTune's "you supply the algorithm" stance, nothing in this
//! crate identifies or reproduces any specific OEM's SA2 program or key.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod error;
pub mod op;
pub mod program;
pub mod seedkey;
pub mod vm;

pub use error::{Sa2Error, Sa2Result};
pub use op::{assemble, Op, REG_COUNT};
pub use program::Program;
pub use seedkey::Sa2SeedKey;
pub use vm::{execute, run, MAX_CALL_DEPTH, MAX_STEPS};
