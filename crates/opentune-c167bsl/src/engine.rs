//! Executes a [`ReadPlan`] over a [`Port`].
//!
//! Per step: send the bytes; on a half-duplex link read back exactly as many
//! echo bytes and compare them; then read the reply the step expects, with a
//! timeout scaled to its length at the link baud. The identification byte,
//! the acknowledges, every register read-back and every block checksum are
//! checked; the first mismatch stops the read. The [`Exchange`] transcript
//! is filled as it goes, so the caller keeps it on error too.
use crate::plan::{Expect, Phase, ReadPlan, Tx};
use crate::port::{recv_exact, Port};
use crate::protocol::{self, ChipFamily, ACK1, ACK2};
use crate::{BslError, Result};
use std::time::Duration;

/// Link settings.
#[derive(Debug, Clone, Copy)]
pub struct LinkOptions {
    pub baud: u32,
    /// Half-duplex (K-line): every sent byte comes back as an echo.
    pub echo: bool,
    /// Wait for a reply's first byte (on top of its wire time).
    pub reply_timeout: Duration,
}

impl LinkOptions {
    pub fn new(baud: u32, echo: bool) -> Self {
        Self {
            baud,
            echo,
            reply_timeout: Duration::from_millis(1500),
        }
    }

    /// Twice the time `n` bytes take on the wire (10 bits each).
    fn wire_time(&self, n: usize) -> Duration {
        let ms = (n as u64 * 10 * 1000 * 2) / self.baud.max(1) as u64;
        Duration::from_millis(ms)
    }
}

/// One step as it happened.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Exchange {
    pub step: usize,
    pub label: String,
    pub sent: Vec<u8>,
    pub echo: Vec<u8>,
    pub reply: Vec<u8>,
}

impl Exchange {
    /// One transcript line.
    pub fn line(&self) -> String {
        let mut s = format!(
            "{:>6} {}: tx [{}]",
            self.step,
            self.label,
            preview(&self.sent)
        );
        if !self.echo.is_empty() {
            s.push_str(&format!(" echo [{}]", preview(&self.echo)));
        }
        s.push_str(&format!(" rx [{}]", preview(&self.reply)));
        s
    }
}

fn preview(b: &[u8]) -> String {
    if b.len() <= 24 {
        protocol::hex(b)
    } else {
        format!(
            "{} … {} ({} bytes)",
            protocol::hex(&b[..8]),
            protocol::hex(&b[b.len() - 4..]),
            b.len()
        )
    }
}

/// Result of a successful read.
#[derive(Debug, Clone)]
pub struct ReadOutcome {
    pub data: Vec<u8>,
    pub id_byte: u8,
    /// `None` when the kernel was already running (no ID byte was sent).
    pub chip: Option<ChipFamily>,
    pub kernel_was_running: bool,
}

const NO_BSL_HINT: &str = " — no identification byte: is the ECU powered and strapped into \
                           bootstrap mode (P0L.4 low through reset), and the baud usable?";

/// Run `plan`. `progress(done, total)` reports bytes read.
pub fn run_plan(
    port: &mut dyn Port,
    plan: &ReadPlan,
    opts: &LinkOptions,
    transcript: &mut Vec<Exchange>,
    progress: &mut dyn FnMut(u32, u32),
) -> Result<ReadOutcome> {
    port.discard_input()?;
    let mut data = Vec::with_capacity(plan.len as usize);
    let mut id_byte = 0u8;
    let mut chip = None;
    let mut kernel_running = false;
    let mut last_block: Option<(u32, Vec<u8>)> = None;

    for step in &plan.steps {
        if kernel_running && matches!(step.phase, Phase::Loader | Phase::Kernel) {
            continue;
        }
        let tx = match &step.tx {
            Tx::Bytes(b) => b.clone(),
            Tx::Supplied { what, .. } => {
                return Err(BslError::MissingPayload {
                    step: step.index,
                    label: step.label.clone(),
                    what: (*what).to_string(),
                })
            }
        };
        transcript.push(Exchange {
            step: step.index,
            label: step.label.clone(),
            sent: tx.clone(),
            ..Exchange::default()
        });
        let ex = transcript.last_mut().expect("just pushed");
        port.send(&tx)?;

        if opts.echo {
            let echo = recv_exact(
                port,
                tx.len(),
                opts.reply_timeout + opts.wire_time(tx.len()),
            )?;
            ex.echo = echo.clone();
            if echo != tx {
                if echo.len() < tx.len() && tx.starts_with(&echo) {
                    return Err(BslError::Timeout {
                        step: step.index,
                        label: step.label.clone(),
                        expected: tx.len(),
                        got: echo,
                        hint: " (echo) — is the K-line powered and the cable a K-line (KKL) \
                               interface? For a full-duplex link use --no-echo",
                    });
                }
                return Err(BslError::EchoMismatch {
                    step: step.index,
                    label: step.label.clone(),
                    sent: tx,
                    got: echo,
                });
            }
        }

        let want = step.expect.reply_len();
        let reply = recv_exact(port, want, opts.reply_timeout + opts.wire_time(want))?;
        ex.reply = reply.clone();
        let timeout = |hint: &'static str| BslError::Timeout {
            step: step.index,
            label: step.label.clone(),
            expected: want,
            got: reply.clone(),
            hint,
        };
        let nak = reply.last() == Some(&protocol::NAK)
            || (reply.len() <= 3 && reply.contains(&protocol::NAK));
        let unexpected = |expected: String| BslError::UnexpectedReply {
            step: step.index,
            label: step.label.clone(),
            expected: if nak {
                format!(
                    "{expected} (fd = NAK: often baud/timing or a CH340 cable; \
                     try 38400 or an FTDI cable)"
                )
            } else {
                expected
            },
            got: reply.clone(),
        };
        if reply.len() < want {
            let hint = match step.phase {
                Phase::Hello => NO_BSL_HINT,
                Phase::Loader => " — the loader did not report; wrong loader for this CPU?",
                Phase::Kernel => " — the kernel did not report; does its length match the loader?",
                _ => "",
            };
            return Err(timeout(hint));
        }

        match &step.expect {
            Expect::IdByte { accept } => {
                id_byte = reply[0];
                if id_byte == ACK1 {
                    kernel_running = true;
                } else if accept.contains(&id_byte) {
                    chip = ChipFamily::from_id(id_byte);
                } else {
                    return Err(BslError::WrongId {
                        got: id_byte,
                        accepted: accept.clone(),
                    });
                }
            }
            Expect::Byte { value, meaning } => {
                if reply[0] != *value {
                    return Err(unexpected(format!("{value:02x} ({meaning})")));
                }
            }
            Expect::Bytes(b) => {
                if &reply != b {
                    return Err(unexpected(protocol::hex(b)));
                }
            }
            Expect::Word { reg, addr, expect } => {
                if reply[2] != ACK2 {
                    return Err(unexpected("lo hi ea".into()));
                }
                let read = u16::from_le_bytes([reply[0], reply[1]]);
                if read != *expect {
                    return Err(BslError::SetupReadback {
                        reg: (*reg).to_string(),
                        addr: *addr,
                        wrote: *expect,
                        read,
                    });
                }
            }
            Expect::Block { addr, len } => {
                let n = *len as usize;
                if reply[n] != ACK2 {
                    return Err(unexpected(format!("{n} data bytes + ea")));
                }
                last_block = Some((*addr, reply[..n].to_vec()));
            }
            Expect::Checksum { addr } => {
                if reply[2] != ACK2 {
                    return Err(unexpected("aa <xor> ea".into()));
                }
                let (baddr, block) = match last_block.take() {
                    Some(b) if b.0 == *addr => b,
                    _ => return Err(BslError::Plan("checksum step without its block".into())),
                };
                let computed = protocol::xor_checksum(&block);
                if computed != reply[1] {
                    return Err(BslError::ChecksumMismatch {
                        addr: baddr,
                        computed,
                        reported: reply[1],
                    });
                }
                data.extend_from_slice(&block);
                progress(data.len() as u32, plan.len);
            }
        }
    }

    Ok(ReadOutcome {
        data,
        id_byte,
        chip,
        kernel_was_running: kernel_running,
    })
}
