//! The read plan: every step and byte exchange, built before anything is
//! sent. [`crate::engine::run_plan`] executes exactly these steps, so what
//! `opentune bsl plan` prints is what `opentune bsl read` does.
use crate::presets::{Preset, Region};
use crate::protocol::{self, *};
use crate::{BslError, Result};
use std::fmt::Write as _;

/// Phase of the session a step belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Zero byte (autobaud) → identification byte.
    Hello,
    /// The 32-byte primary loader.
    Loader,
    /// The monitor kernel, pulled in by the loader.
    Kernel,
    /// Monitor test-communication.
    Check,
    /// Bus / chip-select register writes, each read back (volatile SFRs).
    Setup,
    /// Block reads with checksum.
    Read,
}

impl Phase {
    pub fn name(self) -> &'static str {
        match self {
            Phase::Hello => "hello",
            Phase::Loader => "loader",
            Phase::Kernel => "kernel",
            Phase::Check => "check",
            Phase::Setup => "setup",
            Phase::Read => "read",
        }
    }
}

/// What a step sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tx {
    Bytes(Vec<u8>),
    /// A user-supplied file not given to the planner (dry run only).
    Supplied {
        what: &'static str,
        len: Option<usize>,
    },
}

impl Tx {
    pub fn len(&self) -> Option<usize> {
        match self {
            Tx::Bytes(b) => Some(b.len()),
            Tx::Supplied { len, .. } => *len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == Some(0)
    }
}

/// What a step waits for after the echo (if any) of what it sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expect {
    /// One identification byte from `accept`; `AA` means a monitor kernel
    /// is already running (the loader and kernel steps are then skipped).
    IdByte { accept: Vec<u8> },
    /// Exactly one byte.
    Byte { value: u8, meaning: &'static str },
    /// Exactly these bytes.
    Bytes(Vec<u8>),
    /// `lo hi EA`; the word must equal `expect` (register read-back).
    Word {
        reg: &'static str,
        addr: u32,
        expect: u16,
    },
    /// `len` data bytes then `EA`.
    Block { addr: u32, len: u16 },
    /// `x chk EA`; `chk` must equal the XOR of the preceding block.
    Checksum { addr: u32 },
}

impl Expect {
    /// Bytes the reply has.
    pub fn reply_len(&self) -> usize {
        match self {
            Expect::IdByte { .. } | Expect::Byte { .. } => 1,
            Expect::Bytes(b) => b.len(),
            Expect::Word { .. } | Expect::Checksum { .. } => 3,
            Expect::Block { len, .. } => *len as usize + 1,
        }
    }

    fn describe(&self) -> String {
        match self {
            Expect::IdByte { accept } => format!(
                "1 byte: identification [{}] (C167 family); aa = kernel already running",
                protocol::hex(accept)
            ),
            Expect::Byte { value, meaning } => format!("{value:02x} ({meaning})"),
            Expect::Bytes(b) => protocol::hex(b),
            Expect::Word { expect, .. } => {
                let [lo, hi] = expect.to_le_bytes();
                format!("{lo:02x} {hi:02x} ea (read-back {expect:#06x})")
            }
            Expect::Block { len, .. } => format!("{len} data bytes + ea"),
            Expect::Checksum { .. } => "aa <xor of the block> ea".into(),
        }
    }
}

/// One exchange: send `tx`, consume its echo (K-line), read `expect`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// 1-based.
    pub index: usize,
    pub phase: Phase,
    pub label: String,
    pub tx: Tx,
    pub expect: Expect,
    pub note: Option<String>,
}

/// What to read and with which payloads.
#[derive(Debug, Clone)]
pub struct PlanInput<'a> {
    pub preset: &'static Preset,
    pub region: &'static Region,
    /// Offset into the region.
    pub offset: u32,
    /// Bytes to read (`None` = the region's default length).
    pub len: Option<u32>,
    /// Bytes per block read (1..=[`MAX_BLOCK`]).
    pub block: u16,
    pub loader: Option<&'a [u8]>,
    /// SHA-256 (hex) of the loader, to recognise known files.
    pub loader_sha256: Option<&'a str>,
    pub kernel: Option<&'a [u8]>,
    /// Program the region's bus / chip-select registers first.
    pub setup: bool,
}

/// A complete read plan.
#[derive(Debug, Clone)]
pub struct ReadPlan {
    pub preset: &'static Preset,
    pub region: &'static Region,
    /// First CPU address read.
    pub start: u32,
    pub len: u32,
    pub block: u16,
    pub steps: Vec<Step>,
    pub notes: Vec<String>,
}

fn plan_err(s: impl Into<String>) -> BslError {
    BslError::Plan(s.into())
}

/// Build the plan; validates lengths and payload sizes.
pub fn build_read_plan(input: &PlanInput<'_>) -> Result<ReadPlan> {
    let region = input.region;
    let len = input.len.unwrap_or(region.default_len);
    if len == 0 {
        return Err(plan_err("length is 0"));
    }
    if input.block == 0 || input.block > MAX_BLOCK {
        return Err(plan_err(format!(
            "block size {} is outside 1..={MAX_BLOCK}",
            input.block
        )));
    }
    let end = input.offset as u64 + len as u64;
    if end > region.max_len as u64 {
        return Err(plan_err(format!(
            "offset {:#x} + length {len:#x} runs past the {} window ({:#x} bytes)",
            input.offset, region.id, region.max_len
        )));
    }
    let mut notes = Vec::new();
    if let Some(l) = input.loader {
        if l.len() != PRIMARY_LOADER_LEN {
            return Err(plan_err(format!(
                "the primary loader must be exactly {PRIMARY_LOADER_LEN} bytes (the BSL loads 32 \
                 bytes, no more, no less); this one is {}",
                l.len()
            )));
        }
    }
    let known = input.loader_sha256.and_then(protocol::recognise);
    if let Some(k) = known {
        notes.push(format!("loader recognised: {} ({})", k.name, k.note));
    }
    if let Some(k) = input.kernel {
        if k.is_empty() {
            return Err(plan_err("the kernel file is empty"));
        }
        if let Some(want) = known.and_then(|k| k.expects_kernel_len) {
            if k.len() != want {
                return Err(plan_err(format!(
                    "the recognised loader receives exactly {want} kernel bytes; the kernel is {}",
                    k.len()
                )));
            }
        }
    }

    let start = region.base + input.offset;
    let mut steps = Vec::new();
    let mut push = |phase, label: String, tx: Tx, expect, note: Option<String>| {
        let index = steps.len() + 1;
        steps.push(Step {
            index,
            phase,
            label,
            tx,
            expect,
            note,
        });
    };

    push(
        Phase::Hello,
        "zero byte (autobaud)".into(),
        Tx::Bytes(vec![HELLO]),
        Expect::IdByte {
            accept: input.preset.accept_ids.to_vec(),
        },
        Some("the BSL times this byte to set its baud rate, then answers its ID".into()),
    );
    push(
        Phase::Loader,
        "primary loader (32 bytes to 00'FA40h)".into(),
        match input.loader {
            Some(l) => Tx::Bytes(l.to_vec()),
            None => Tx::Supplied {
                what: "primary loader (--loader)",
                len: Some(PRIMARY_LOADER_LEN),
            },
        },
        Expect::Byte {
            value: I_LOADER_STARTED,
            meaning: "loader started",
        },
        Some("the BSL jumps to 00'FA40h after the 32nd byte".into()),
    );
    push(
        Phase::Kernel,
        "monitor kernel".into(),
        match input.kernel {
            Some(k) => Tx::Bytes(k.to_vec()),
            None => Tx::Supplied {
                what: "monitor kernel (--kernel)",
                len: known.and_then(|k| k.expects_kernel_len),
            },
        },
        Expect::Byte {
            value: I_APPLICATION_STARTED,
            meaning: "kernel started",
        },
        None,
    );
    push(
        Phase::Check,
        "test communication".into(),
        Tx::Bytes(vec![C_TEST_COMM]),
        Expect::Bytes(vec![ACK1, ACK2]),
        None,
    );
    if input.setup {
        for w in region.setup {
            push(
                Phase::Setup,
                format!("write {} command", w.name),
                Tx::Bytes(vec![C_WRITE_WORD]),
                Expect::Byte {
                    value: ACK1,
                    meaning: "ack",
                },
                None,
            );
            let desc = protocol::describe_register(w.addr, w.value);
            push(
                Phase::Setup,
                format!("{} := {:#06x}", w.name, w.value),
                Tx::Bytes(protocol::write_word_args(w.addr, w.value)),
                Expect::Byte {
                    value: ACK2,
                    meaning: "done",
                },
                (!desc.is_empty()).then_some(desc),
            );
            push(
                Phase::Setup,
                format!("read back {} command", w.name),
                Tx::Bytes(vec![C_READ_WORD]),
                Expect::Byte {
                    value: ACK1,
                    meaning: "ack",
                },
                None,
            );
            push(
                Phase::Setup,
                format!("read back {}", w.name),
                Tx::Bytes(protocol::addr24(w.addr).to_vec()),
                Expect::Word {
                    reg: w.name,
                    addr: w.addr,
                    expect: w.value,
                },
                None,
            );
        }
    } else {
        notes.push("register setup skipped (--no-setup): the bus keeps the BSL defaults".into());
    }
    let blocks = len.div_ceil(input.block as u32);
    let mut off = 0u32;
    for i in 0..blocks {
        let n = (len - off).min(input.block as u32) as u16;
        let addr = start + off;
        push(
            Phase::Read,
            format!("block {}/{blocks} command", i + 1),
            Tx::Bytes(vec![C_READ_BLOCK]),
            Expect::Byte {
                value: ACK1,
                meaning: "ack",
            },
            None,
        );
        push(
            Phase::Read,
            format!("block {}/{blocks} @ {addr:#08x} len {n:#x}", i + 1),
            Tx::Bytes(protocol::read_block_args(addr, n)),
            Expect::Block { addr, len: n },
            None,
        );
        push(
            Phase::Read,
            format!("block {}/{blocks} checksum", i + 1),
            Tx::Bytes(vec![C_GETCHECKSUM]),
            Expect::Checksum { addr },
            None,
        );
        off += n as u32;
    }

    Ok(ReadPlan {
        preset: input.preset,
        region,
        start,
        len,
        block: input.block,
        steps,
        notes,
    })
}

/// How [`ReadPlan::render`] prints.
#[derive(Debug, Clone, Copy, Default)]
pub struct RenderOptions {
    /// Half-duplex link: every sent byte comes back as an echo.
    pub echo: bool,
    pub baud: u32,
    /// CPU clock, for the autobaud deviation check.
    pub fcpu_hz: Option<u32>,
    /// Print every block read instead of the first and last.
    pub all_blocks: bool,
}

/// CPU clocks shown when the real one is not given.
const COMMON_CLOCKS_MHZ: [u32; 4] = [16, 20, 24, 32];

fn hex_preview(b: &[u8]) -> String {
    if b.len() <= 16 {
        protocol::hex(b)
    } else {
        format!("{} … ({} bytes)", protocol::hex(&b[..8]), b.len())
    }
}

impl ReadPlan {
    /// Blocks in the read.
    pub fn blocks(&self) -> usize {
        self.steps
            .iter()
            .filter(|s| matches!(s.expect, Expect::Checksum { .. }))
            .count()
    }

    /// Bytes on the wire (sent + received, echo not counted), if every
    /// payload size is known.
    pub fn wire_bytes(&self) -> Option<u64> {
        let mut n = 0u64;
        for s in &self.steps {
            n += s.tx.len()? as u64 + s.expect.reply_len() as u64;
        }
        Some(n)
    }

    /// The human-readable plan.
    pub fn render(&self, o: &RenderOptions) -> String {
        let mut out = String::new();
        let r = self.region;
        let _ = writeln!(
            out,
            "preset       {} — {}",
            self.preset.id, self.preset.name
        );
        let _ = writeln!(out, "region       {} — {}", r.id, r.description);
        let _ = writeln!(
            out,
            "read         {:#08x}..{:#08x} ({} bytes, {} block(s) of up to {:#x})",
            self.start,
            self.start as u64 + self.len as u64 - 1,
            self.len,
            self.blocks(),
            self.block
        );
        let _ = writeln!(out, "source       {}", r.source);
        let _ = writeln!(
            out,
            "status       UNVERIFIED on hardware — read only (no flash/EEPROM write or erase)"
        );
        let _ = writeln!(
            out,
            "link         {} baud 8N1, {}",
            o.baud,
            if o.echo {
                "half-duplex K-line: every sent byte is read back as an echo and compared"
            } else {
                "full-duplex (no echo expected)"
            }
        );
        match o.fcpu_hz {
            Some(f) => {
                let c = protocol::baud_check(f, o.baud);
                let _ = writeln!(out, "autobaud     {}", baud_line(&c));
            }
            None => {
                let _ = writeln!(
                    out,
                    "autobaud     CPU clock unknown (--fcpu-mhz); BSL deviation at:"
                );
                for mhz in COMMON_CLOCKS_MHZ {
                    let c = protocol::baud_check(mhz * 1_000_000, o.baud);
                    let _ = writeln!(out, "               {mhz:>2} MHz: {}", baud_line(&c));
                }
            }
        }
        if let Some(n) = self.wire_bytes() {
            let secs = n as f64 * 10.0 / o.baud.max(1) as f64;
            let _ = writeln!(
                out,
                "wire         {n} bytes, ≈ {secs:.0} s at {} baud",
                o.baud
            );
        }
        for n in &self.notes {
            let _ = writeln!(out, "note         {n}");
        }
        let _ = writeln!(out);

        let blocks = self.blocks();
        let elide = !o.all_blocks && blocks > 2;
        let mut elided_note = false;
        let mut block_no = 0usize;
        for s in &self.steps {
            // Each block is three steps, opened by the read-block command.
            if s.phase == Phase::Read && s.tx == Tx::Bytes(vec![C_READ_BLOCK]) {
                block_no += 1;
            }
            if elide && s.phase == Phase::Read && block_no > 1 && block_no < blocks {
                if !elided_note {
                    let _ = writeln!(
                        out,
                        "        … blocks 2..{} have the same three steps at increasing addresses \
                         ({} steps elided; --all-blocks prints every one)",
                        blocks - 1,
                        (blocks - 2) * 3
                    );
                    elided_note = true;
                }
                continue;
            }
            let tx = match &s.tx {
                Tx::Bytes(b) => hex_preview(b),
                Tx::Supplied { what, len } => match len {
                    Some(n) => format!("<{n} bytes: {what}>"),
                    None => format!("<{what}>"),
                },
            };
            let _ = writeln!(out, "{:>6}  {:<6}  {}", s.index, s.phase.name(), s.label);
            let _ = writeln!(
                out,
                "        send   {tx}{}",
                if o.echo { "   (echo checked)" } else { "" }
            );
            let _ = writeln!(out, "        expect {}", s.expect.describe());
            if let Some(n) = &s.note {
                let _ = writeln!(out, "        note   {n}");
            }
        }
        out
    }
}

fn baud_line(c: &protocol::BaudCheck) -> String {
    match c.s0brl {
        Some(r) => format!(
            "S0BRL {r}, BSL runs at {:.0} baud, deviation {:+.2} % — {}",
            c.bsl_baud,
            c.deviation_pct,
            if c.ok {
                "ok (≤ 2.5 %)"
            } else {
                "TOO HIGH (> 2.5 %): pick another baud"
            }
        ),
        None => "outside the BSL's measurable range".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presets::find_preset;

    fn input(len: Option<u32>) -> PlanInput<'static> {
        let p = find_preset("edc15c6").unwrap();
        PlanInput {
            preset: p,
            region: p.region("ext-flash").unwrap(),
            offset: 0,
            len,
            block: 0x200,
            loader: None,
            loader_sha256: None,
            kernel: None,
            setup: true,
        }
    }

    #[test]
    fn plan_has_every_phase_in_order() {
        let plan = build_read_plan(&input(Some(0x400))).unwrap();
        let phases: Vec<Phase> = plan.steps.iter().map(|s| s.phase).collect();
        assert_eq!(phases[0], Phase::Hello);
        assert_eq!(phases[1], Phase::Loader);
        assert_eq!(phases[2], Phase::Kernel);
        assert_eq!(phases[3], Phase::Check);
        assert_eq!(
            plan.steps
                .iter()
                .filter(|s| s.phase == Phase::Setup)
                .count(),
            40
        );
        assert_eq!(plan.blocks(), 2);
        assert_eq!(plan.start, 0x40_0000);
        // Unknown kernel size → wire size unknown.
        assert_eq!(plan.wire_bytes(), None);
    }

    #[test]
    fn last_block_is_short() {
        let plan = build_read_plan(&input(Some(0x201))).unwrap();
        let lens: Vec<u16> = plan
            .steps
            .iter()
            .filter_map(|s| match s.expect {
                Expect::Block { len, .. } => Some(len),
                _ => None,
            })
            .collect();
        assert_eq!(lens, [0x200, 1]);
    }

    #[test]
    fn rejects_bad_inputs() {
        assert!(build_read_plan(&input(Some(0))).is_err());
        assert!(build_read_plan(&input(Some(0x10_0001))).is_err());
        let mut i = input(None);
        i.block = 0x201;
        assert!(build_read_plan(&i).is_err());
        let short = [0u8; 31];
        let mut i = input(None);
        i.loader = Some(&short);
        assert!(build_read_plan(&i)
            .unwrap_err()
            .to_string()
            .contains("exactly 32"));
    }

    #[test]
    fn recognised_loader_pins_the_kernel_length() {
        let loader = [0u8; 32];
        let kernel = [0u8; 100];
        let mut i = input(Some(0x10));
        i.loader = Some(&loader);
        i.loader_sha256 = Some(protocol::KNOWN_PAYLOADS[0].sha256);
        i.kernel = Some(&kernel);
        assert!(build_read_plan(&i).unwrap_err().to_string().contains("394"));
        let kernel = [0u8; 394];
        i.kernel = Some(&kernel);
        let p = build_read_plan(&i).unwrap();
        assert!(p.notes[0].contains("MiniMon"));
        assert!(p.wire_bytes().is_some());
    }

    #[test]
    fn render_elides_middle_blocks() {
        let plan = build_read_plan(&input(Some(0x1000))).unwrap();
        let o = RenderOptions {
            echo: true,
            baud: 57_600,
            fcpu_hz: None,
            all_blocks: false,
        };
        let text = plan.render(&o);
        assert!(text.contains("block 1/8 command"));
        assert!(text.contains("block 8/8 checksum"));
        assert!(!text.contains("block 4/8"));
        assert!(text.contains("18 steps elided"));
        assert!(text.contains("UNVERIFIED"));
        assert!(text.contains("<32 bytes: primary loader (--loader)>"));
        let all = plan.render(&RenderOptions {
            all_blocks: true,
            ..o
        });
        assert!(all.contains("block 4/8 @ 0x400600 len 0x200"));
    }
}
