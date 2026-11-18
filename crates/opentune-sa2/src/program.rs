//! Static view of an SA2 script: linear disassembly, lints, a listing for
//! humans and a small text assembler (used by tests and by anyone writing a
//! synthetic script by hand).
//!
//! Execution never depends on this module — the VM decodes at the program
//! counter — so a script with a lint still runs exactly as the VM defines.

use crate::error::{Sa2Error, Sa2Result};
use crate::op::{self, Op};
use crate::vm;
use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;

/// One instruction found by the linear sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Instr {
    /// Offset of the opcode byte.
    pub offset: usize,
    /// The decoded instruction.
    pub op: Op,
}

/// A structural oddity in a script. None stops execution; they flag scripts
/// that do not look like the well-formed ones seen in practice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lint {
    /// A branch lands inside another instruction's operand bytes.
    MisalignedBranch {
        /// Offset of the branch.
        offset: usize,
        /// Where it lands.
        target: usize,
    },
    /// A branch lands beyond the end of the script (it ends the run).
    BranchPastEnd {
        /// Offset of the branch.
        offset: usize,
        /// Where it lands.
        target: usize,
    },
    /// A `loop` with count 0 (refused at run time).
    ZeroLoopCount {
        /// Offset of the loop.
        offset: usize,
    },
    /// In program order, a `next` with no `loop` before it.
    UnmatchedNext {
        /// Offset of the `next`.
        offset: usize,
    },
    /// In program order, this many `loop`s are never closed.
    UnclosedLoops(usize),
    /// The script does not end with `finish`.
    MissingFinish,
    /// Instructions follow a `finish`.
    CodeAfterFinish {
        /// Offset of the first instruction after the `finish`.
        offset: usize,
    },
}

impl std::fmt::Display for Lint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Lint::MisalignedBranch { offset, target } => write!(
                f,
                "branch at {offset:04X} lands mid-instruction at {target:04X}"
            ),
            Lint::BranchPastEnd { offset, target } => {
                write!(
                    f,
                    "branch at {offset:04X} lands past the end ({target:04X})"
                )
            }
            Lint::ZeroLoopCount { offset } => {
                write!(f, "loop at {offset:04X} has count 0 (refused when run)")
            }
            Lint::UnmatchedNext { offset } => {
                write!(f, "next at {offset:04X} has no loop before it")
            }
            Lint::UnclosedLoops(n) => write!(f, "{n} loop(s) never closed by next"),
            Lint::MissingFinish => f.write_str("script does not end with finish (0x4C)"),
            Lint::CodeAfterFinish { offset } => {
                write!(f, "instructions after finish, from {offset:04X}")
            }
        }
    }
}

/// A script decoded by a linear sweep from offset 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    bytes: Vec<u8>,
    instrs: Vec<Instr>,
}

impl Program {
    /// Decode every instruction from offset 0 to the end. Strict: an unknown
    /// opcode or truncated operand anywhere is an error (even where the VM
    /// would never reach it).
    pub fn parse(bytes: &[u8]) -> Sa2Result<Self> {
        let mut instrs = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            let op = Op::decode(bytes, at)?;
            instrs.push(Instr { offset: at, op });
            at += op.len();
        }
        Ok(Program {
            bytes: bytes.to_vec(),
            instrs,
        })
    }

    /// Decode a hex string (see [`parse_hex`]) and parse it.
    pub fn from_hex(hex: &str) -> Sa2Result<Self> {
        Self::parse(&parse_hex(hex)?)
    }

    /// The raw script bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The instructions, in offset order.
    pub fn instrs(&self) -> &[Instr] {
        &self.instrs
    }

    /// Just the instructions, in offset order.
    pub fn ops(&self) -> Vec<Op> {
        self.instrs.iter().map(|i| i.op).collect()
    }

    /// Number of instructions.
    pub fn len(&self) -> usize {
        self.instrs.len()
    }

    /// Whether the script is empty.
    pub fn is_empty(&self) -> bool {
        self.instrs.is_empty()
    }

    /// Run over a 32-bit seed (see [`vm::run`]).
    pub fn run(&self, seed: u32) -> Sa2Result<u32> {
        vm::run(&self.bytes, seed)
    }

    fn boundaries(&self) -> BTreeSet<usize> {
        let mut b: BTreeSet<usize> = self.instrs.iter().map(|i| i.offset).collect();
        b.insert(self.bytes.len());
        b
    }

    /// Structural lints, in offset order.
    pub fn lints(&self) -> Vec<Lint> {
        let bounds = self.boundaries();
        let mut out = Vec::new();
        let mut depth = 0usize;
        let mut finish_seen = false;
        for i in &self.instrs {
            if finish_seen {
                out.push(Lint::CodeAfterFinish { offset: i.offset });
                finish_seen = false; // report once
            }
            if let Some(t) = i.op.branch_target(i.offset) {
                if t > self.bytes.len() {
                    out.push(Lint::BranchPastEnd {
                        offset: i.offset,
                        target: t,
                    });
                } else if !bounds.contains(&t) {
                    out.push(Lint::MisalignedBranch {
                        offset: i.offset,
                        target: t,
                    });
                }
            }
            match i.op {
                Op::Loop(0) => {
                    out.push(Lint::ZeroLoopCount { offset: i.offset });
                    depth += 1;
                }
                Op::Loop(_) => depth += 1,
                Op::Next => {
                    if depth == 0 {
                        out.push(Lint::UnmatchedNext { offset: i.offset });
                    } else {
                        depth -= 1;
                    }
                }
                Op::Finish => finish_seen = true,
                _ => {}
            }
        }
        if depth > 0 {
            out.push(Lint::UnclosedLoops(depth));
        }
        if self.instrs.last().map(|i| i.op) != Some(Op::Finish) {
            out.push(Lint::MissingFinish);
        }
        out
    }

    /// Assembler source for this script: labels at aligned branch targets,
    /// one instruction per line. [`assemble_text`] turns it back into the same
    /// bytes.
    pub fn to_source(&self) -> String {
        let labels = self.label_map();
        let mut s = String::new();
        for i in &self.instrs {
            if let Some(l) = labels.get(&i.offset) {
                let _ = writeln!(s, "{l}:");
            }
            let _ = writeln!(s, "    {}", self.render(i, &labels));
        }
        if let Some(l) = labels.get(&self.bytes.len()) {
            let _ = writeln!(s, "{l}:");
        }
        s
    }

    /// A human listing: offset, raw bytes, instruction, loop depth and branch
    /// targets, followed by any lints.
    pub fn listing(&self) -> String {
        let labels = self.label_map();
        let mut s = String::new();
        let _ = writeln!(
            s,
            "; SA2 script: {} bytes, {} instructions",
            self.bytes.len(),
            self.instrs.len()
        );
        let mut depth = 0usize;
        for i in &self.instrs {
            if let Some(l) = labels.get(&i.offset) {
                let _ = writeln!(s, "{l}:");
            }
            if i.op == Op::Next {
                depth = depth.saturating_sub(1);
            }
            let raw = &self.bytes[i.offset..i.offset + i.op.len()];
            let hex: Vec<String> = raw.iter().map(|b| format!("{b:02X}")).collect();
            let _ = write!(
                s,
                "  {:04X}  {:<15} {}{}",
                i.offset,
                hex.join(" "),
                "  ".repeat(depth),
                self.render(i, &labels)
            );
            if let Some(t) = i.op.branch_target(i.offset) {
                let _ = write!(s, "    ; -> {t:04X}");
            }
            s.push('\n');
            if matches!(i.op, Op::Loop(_)) {
                depth += 1;
            }
        }
        if let Some(l) = labels.get(&self.bytes.len()) {
            let _ = writeln!(s, "{l}:");
        }
        for l in self.lints() {
            let _ = writeln!(s, "; lint: {l}");
        }
        s
    }

    fn label_map(&self) -> HashMap<usize, String> {
        let bounds = self.boundaries();
        self.instrs
            .iter()
            .filter_map(|i| i.op.branch_target(i.offset))
            .filter(|t| bounds.contains(t))
            .map(|t| (t, format!("L{t:04X}")))
            .collect()
    }

    fn render(&self, i: &Instr, labels: &HashMap<usize, String>) -> String {
        match i.op.branch_target(i.offset).and_then(|t| labels.get(&t)) {
            Some(l) => format!("{} {l}", i.op.mnemonic()),
            None => i.op.to_string(),
        }
    }
}

/// Decode a hex string into bytes. Whitespace, commas, colons and `0x`
/// prefixes are ignored, so `"68 05 81"`, `"0x68,0x05,0x81"` and `"680581"`
/// are equivalent.
pub fn parse_hex(s: &str) -> Sa2Result<Vec<u8>> {
    let cleaned = s.replace("0x", "").replace("0X", "");
    let digits: Vec<u8> = cleaned
        .bytes()
        .filter(|b| !(b.is_ascii_whitespace() || *b == b',' || *b == b':'))
        .collect();
    if digits.len() % 2 != 0 {
        return Err(Sa2Error::BadHex("odd number of hex digits".into()));
    }
    digits
        .chunks_exact(2)
        .map(|p| {
            let hi = (p[0] as char).to_digit(16);
            let lo = (p[1] as char).to_digit(16);
            match (hi, lo) {
                (Some(h), Some(l)) => Ok((h * 16 + l) as u8),
                _ => Err(Sa2Error::BadHex(format!(
                    "non-hex characters {:?}",
                    String::from_utf8_lossy(p)
                ))),
            }
        })
        .collect()
}

/// Assemble SA2 assembler source into a script.
///
/// One instruction per line: `rsl`, `rsr`, `add N`, `sub N`, `eor N`,
/// `loop N`, `next`, `bcc T`, `bra T`, `finish` (case-insensitive). Numbers
/// are decimal or `0x` hex. A branch target `T` is a forward skip (`5` or
/// `+5`) or a label defined later as `name:` (on its own line or before an
/// instruction). `.byte B B ...` emits raw bytes (for malformed test
/// scripts). `;` and `#` start comments.
pub fn assemble_text(src: &str) -> Sa2Result<Vec<u8>> {
    enum Item<'a> {
        Op(Op),
        Branch { opcode: u8, label: &'a str },
        Bytes(Vec<u8>),
    }
    let err = |line: usize, msg: String| Sa2Error::Asm { line, msg };

    let mut items: Vec<(usize, usize, Item)> = Vec::new(); // (line, offset, item)
    let mut labels: HashMap<&str, usize> = HashMap::new();
    let mut offset = 0usize;

    for (idx, raw) in src.lines().enumerate() {
        let line = idx + 1;
        let mut text = raw.split([';', '#']).next().unwrap_or("").trim();
        if let Some((name, rest)) = text.split_once(':') {
            let name = name.trim();
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Err(err(line, format!("bad label {name:?}")));
            }
            if labels.insert(name, offset).is_some() {
                return Err(err(line, format!("label {name:?} defined twice")));
            }
            text = rest.trim();
        }
        if text.is_empty() {
            continue;
        }
        let mut words = text.split_whitespace();
        let mnemonic = words.next().unwrap_or("").to_ascii_lowercase();
        let args: Vec<&str> = words.collect();
        let one_arg = || -> Sa2Result<&str> {
            match args.as_slice() {
                [a] => Ok(*a),
                _ => Err(err(line, format!("{mnemonic} takes exactly one operand"))),
            }
        };
        let no_arg = |op: Op| -> Sa2Result<Item> {
            if args.is_empty() {
                Ok(Item::Op(op))
            } else {
                Err(err(line, format!("{mnemonic} takes no operand")))
            }
        };
        let num = |s: &str, max: u64| -> Sa2Result<u64> {
            let v = parse_number(s).ok_or_else(|| err(line, format!("bad number {s:?}")))?;
            if v > max {
                return Err(err(line, format!("{s} out of range (max {max})")));
            }
            Ok(v)
        };

        let item = match mnemonic.as_str() {
            "rsl" => no_arg(Op::Rsl)?,
            "rsr" => no_arg(Op::Rsr)?,
            "next" => no_arg(Op::Next)?,
            "finish" => no_arg(Op::Finish)?,
            "add" => Item::Op(Op::Add(num(one_arg()?, u32::MAX as u64)? as u32)),
            "sub" => Item::Op(Op::Sub(num(one_arg()?, u32::MAX as u64)? as u32)),
            "eor" => Item::Op(Op::Eor(num(one_arg()?, u32::MAX as u64)? as u32)),
            "loop" => Item::Op(Op::Loop(num(one_arg()?, 255)? as u8)),
            "bcc" | "bra" => {
                let opcode = if mnemonic == "bcc" {
                    op::OP_BCC
                } else {
                    op::OP_BRA
                };
                let a = one_arg()?;
                let n = a.strip_prefix('+').unwrap_or(a);
                if n.starts_with(|c: char| c.is_ascii_digit()) {
                    let d = num(n, 255)? as u8;
                    Item::Op(if opcode == op::OP_BCC {
                        Op::Bcc(d)
                    } else {
                        Op::Bra(d)
                    })
                } else {
                    Item::Branch { opcode, label: a }
                }
            }
            ".byte" => {
                if args.is_empty() {
                    return Err(err(line, ".byte needs at least one value".into()));
                }
                let bytes = args
                    .iter()
                    .map(|a| num(a, 255).map(|v| v as u8))
                    .collect::<Sa2Result<Vec<u8>>>()?;
                Item::Bytes(bytes)
            }
            other => return Err(err(line, format!("unknown mnemonic {other:?}"))),
        };
        let size = match &item {
            Item::Op(op) => op.len(),
            Item::Branch { .. } => 2,
            Item::Bytes(b) => b.len(),
        };
        items.push((line, offset, item));
        offset += size;
    }

    let mut out = Vec::with_capacity(offset);
    for (line, at, item) in items {
        match item {
            Item::Op(op) => op.encode(&mut out),
            Item::Bytes(b) => out.extend_from_slice(&b),
            Item::Branch { opcode, label } => {
                let target = *labels
                    .get(label)
                    .ok_or_else(|| err(line, format!("undefined label {label:?}")))?;
                let d = target
                    .checked_sub(at + 2)
                    .filter(|d| *d <= 255)
                    .ok_or_else(|| {
                        err(
                            line,
                            format!(
                                "label {label:?} is not 0..=255 bytes ahead (SA2 branches only skip forward)"
                            ),
                        )
                    })?;
                out.push(opcode);
                out.push(d as u8);
            }
        }
    }
    Ok(out)
}

fn parse_number(s: &str) -> Option<u64> {
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(&h.replace('_', ""), 16).ok()
    } else {
        s.replace('_', "").parse().ok()
    }
}
