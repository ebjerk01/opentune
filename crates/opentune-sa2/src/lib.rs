//! # opentune-sa2
//!
//! A virtual machine for **VAG SA2 seed/key scripts**.
//!
//! VW-Group control units unlock programming (UDS service `0x27`, level
//! `0x11`) with a challenge whose answer is computed by a tiny bytecode
//! program — the "SA2 script" — that travels inside the module's own flash
//! container (the ODX `SECURITY` block whose method is `SA2`, or the SA2 field
//! of an SGO header). The tester loads the 4-byte seed into a 32-bit register,
//! runs the script, and sends the register back as the key.
//!
//! This crate runs those scripts. It ships **no** script: the bytecode always
//! comes from the user's own container or input. Every test vector here is a
//! synthetic program written for the test suite.
//!
//! ## What this crate provides
//!
//! * [`execute`] / [`run`] / [`vm::run_with`] — run a script over a seed
//!   (with a step budget and an optional per-instruction trace).
//! * [`Op`] and the opcode table in [`op`] — the ten real SA2 opcodes.
//! * [`Program`] — linear disassembly, [`Program::listing`],
//!   [`Program::lints`] and [`Program::to_source`].
//! * [`assemble`] / [`assemble_text`] — build scripts from [`Op`]s or text.
//! * [`Sa2SeedKey`] — an [`opentune_core::protocol::SeedKey`] (id `"sa2"`)
//!   wrapping a caller-supplied script.
//!
//! ## Machine model
//!
//! See [`vm`] for the register/carry/loop-stack model and [`op`] for the
//! encoding. In short: one 32-bit register (seed in, key out, big-endian),
//! one carry flag, ten opcodes (`rsl`, `rsr`, `add`, `sub`, `eor`, `loop`,
//! `next`, `bcc`, `bra`, `finish`), counted loops that nest, and forward-only
//! branches measured in bytes.
//!
//! Two deliberate choices where the public references are silent or disagree:
//! `finish` (`0x4C`) ends the run (both references treat it as a no-op that
//! falls through, which only differs if something follows it — no known
//! script does), and `loop 0` is refused with [`Sa2Error::ZeroLoopCount`]
//! (one reference runs the body once, the other 256 times).
//!
//! ## Sources and clean-room stance
//!
//! The instruction set is learned from public, open-source descriptions:
//!
//! * `bri3d/sa2_seed_key` (MIT) — the Python reference implementation of the
//!   SA2 VM, also published on PyPI as `sa2-seed-key`. It is the **test
//!   oracle**: `tools/sa2/` runs it beside this crate on synthetic programs and
//!   the results are the fixture in `tests/fixtures/sa2_vectors.txt`.
//! * `jpbede/sa2go` (MIT) — a Go port of it, used to cross-check the
//!   semantics.
//! * `jglim/UnlockECU` (MIT) — its `VolkswagenSA2` provider is a C# port of
//!   `bri3d/sa2_seed_key` (it says so). Cross-check only; note that its `sub`
//!   never sets carry on a borrow, unlike the Python and Go versions, which
//!   this crate follows.
//! * `bri3d/VW_Flash` (BSD-2-Clause) — documents that the script lives in the
//!   ODX flash container and is run against the seed, with the seed and key as
//!   big-endian 32-bit values.
//!
//! The Rust here is written from those semantics, not translated from their
//! code. (Earlier versions of this crate said their model "follows
//! `jglim/UnlockECU`" while using an invented opcode encoding that could not
//! run a real script. The real lineage is `bri3d/sa2_seed_key`; UnlockECU
//! carries a port of it.)
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod error;
pub mod op;
pub mod program;
pub mod seedkey;
pub mod vm;

pub use error::{Sa2Error, Sa2Result};
pub use op::{assemble, Op};
pub use program::{assemble_text, parse_hex, Instr, Lint, Program};
pub use seedkey::Sa2SeedKey;
pub use vm::{execute, run, Outcome, Step, MAX_LOOP_DEPTH, MAX_STEPS};
