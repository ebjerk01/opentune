//! A simulated C167 in bootstrap mode, running a MiniMon-style monitor
//! kernel once loaded — for tests and `--port sim`. It implements [`Port`].
//!
//! Behaviour: waits for the zero byte (anything else is ignored, like a
//! failed autobaud), answers the identification byte, takes exactly 32 loader
//! bytes and answers `01`, takes the configured number of kernel bytes and
//! answers `03`, then serves the monitor commands. On a half-duplex link it
//! echoes every byte it receives (the K-line does). Memory is a set of
//! regions; a region can require register values (e.g. the CS3 window) and
//! reads as `FF` until they are programmed.
use crate::port::Port;
use crate::protocol::*;
use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::time::Duration;

/// Injected failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fault {
    #[default]
    None,
    /// Corrupt the echo of the n-th received byte (0-based, whole session).
    CorruptEcho { at: usize },
    /// Go silent (no echo, no reply) from the n-th received byte on.
    SilentFrom { at: usize },
    /// Report a wrong checksum for the n-th block read (0-based).
    BadChecksum { block: usize },
    /// Answer the n-th monitor command (0-based) with the NAK `FD`
    /// instead of `AA`.
    NakCommand { nth: usize },
    /// End the n-th block (0-based) with `00` instead of `EA`.
    BadBlockAck { block: usize },
    /// The loader never reports.
    LoaderSilent,
    /// Register writes to this address are ignored (read back the old value).
    StuckRegister { addr: u32 },
}

/// How the simulated ECU behaves.
#[derive(Debug, Clone)]
pub struct SimConfig {
    pub id_byte: u8,
    /// Kernel bytes the simulated loader receives.
    pub kernel_len: usize,
    /// Echo every received byte (K-line).
    pub echo: bool,
    /// A monitor kernel is already running (a previous session).
    pub kernel_running: bool,
    pub fault: Fault,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            id_byte: ChipFamily::C167.id_byte(),
            kernel_len: 394,
            echo: true,
            kernel_running: false,
            fault: Fault::None,
        }
    }
}

#[derive(Debug)]
enum State {
    AwaitHello,
    Loader(Vec<u8>),
    Kernel(Vec<u8>),
    Command,
    Args { cmd: u8, need: usize, buf: Vec<u8> },
    Dead,
}

struct MemRegion {
    base: u32,
    bytes: Vec<u8>,
    requires: Vec<(u32, u16)>,
}

/// The simulated ECU.
pub struct SimC167 {
    cfg: SimConfig,
    state: State,
    out: VecDeque<u8>,
    rx_count: usize,
    commands: usize,
    blocks: usize,
    last_checksum: u8,
    regs: BTreeMap<u32, u16>,
    memory: Vec<MemRegion>,
    /// Every register write, in order.
    pub writes: Vec<(u32, u16)>,
    /// The loader and kernel as received.
    pub loader: Vec<u8>,
    pub kernel: Vec<u8>,
}

impl SimC167 {
    pub fn new(cfg: SimConfig) -> Self {
        let state = if cfg.kernel_running {
            State::Command
        } else {
            State::AwaitHello
        };
        Self {
            cfg,
            state,
            out: VecDeque::new(),
            rx_count: 0,
            commands: 0,
            blocks: 0,
            last_checksum: 0,
            regs: BTreeMap::new(),
            memory: Vec::new(),
            writes: Vec::new(),
            loader: Vec::new(),
            kernel: Vec::new(),
        }
    }

    /// Map `bytes` at `base`, readable once every `(register, value)` in
    /// `requires` has been programmed.
    pub fn with_memory(mut self, base: u32, bytes: Vec<u8>, requires: &[(u32, u16)]) -> Self {
        self.memory.push(MemRegion {
            base,
            bytes,
            requires: requires.to_vec(),
        });
        self
    }

    /// Current value of a register (as last written).
    pub fn register(&self, addr: u32) -> Option<u16> {
        self.regs.get(&addr).copied()
    }

    fn read_byte(&self, addr: u32) -> u8 {
        for m in &self.memory {
            let end = m.base as u64 + m.bytes.len() as u64;
            if (addr as u64) >= m.base as u64 && (addr as u64) < end {
                let mapped = m.requires.iter().all(|(r, v)| self.regs.get(r) == Some(v));
                return if mapped {
                    m.bytes[(addr - m.base) as usize]
                } else {
                    0xFF
                };
            }
        }
        0xFF
    }

    fn read_word(&self, addr: u32) -> u16 {
        match self.regs.get(&addr) {
            Some(v) => *v,
            None => u16::from_le_bytes([self.read_byte(addr), self.read_byte(addr + 1)]),
        }
    }

    fn feed(&mut self, b: u8) {
        let index = self.rx_count;
        self.rx_count += 1;
        if let Fault::SilentFrom { at } = self.cfg.fault {
            if index >= at {
                return;
            }
        }
        if matches!(self.state, State::Dead) {
            return;
        }
        if self.cfg.echo {
            let corrupt = self.cfg.fault == Fault::CorruptEcho { at: index };
            self.out.push_back(if corrupt { b ^ 0xFF } else { b });
        }
        let state = std::mem::replace(&mut self.state, State::Dead);
        self.state = match state {
            State::AwaitHello => {
                if b == HELLO {
                    self.out.push_back(self.cfg.id_byte);
                    State::Loader(Vec::new())
                } else {
                    State::AwaitHello
                }
            }
            State::Loader(mut buf) => {
                buf.push(b);
                if buf.len() < PRIMARY_LOADER_LEN {
                    State::Loader(buf)
                } else {
                    self.loader = buf;
                    if self.cfg.fault == Fault::LoaderSilent {
                        State::Dead
                    } else {
                        self.out.push_back(I_LOADER_STARTED);
                        State::Kernel(Vec::new())
                    }
                }
            }
            State::Kernel(mut buf) => {
                buf.push(b);
                if buf.len() < self.cfg.kernel_len {
                    State::Kernel(buf)
                } else {
                    self.kernel = buf;
                    self.out.push_back(I_APPLICATION_STARTED);
                    State::Command
                }
            }
            State::Command => self.command(b),
            State::Args { cmd, need, mut buf } => {
                buf.push(b);
                if buf.len() < need {
                    State::Args { cmd, need, buf }
                } else {
                    self.execute(cmd, &buf);
                    State::Command
                }
            }
            State::Dead => State::Dead,
        };
    }

    fn command(&mut self, cmd: u8) -> State {
        let nth = self.commands;
        self.commands += 1;
        if self.cfg.fault == (Fault::NakCommand { nth }) {
            self.out.push_back(NAK);
            return State::Command;
        }
        match cmd {
            C_TEST_COMM => {
                self.out.extend([ACK1, ACK2]);
                State::Command
            }
            C_GETCHECKSUM => {
                let mut chk = self.last_checksum;
                if self.blocks > 0
                    && self.cfg.fault
                        == (Fault::BadChecksum {
                            block: self.blocks - 1,
                        })
                {
                    chk ^= 0x5A;
                }
                self.out.extend([ACK1, chk, ACK2]);
                State::Command
            }
            C_WRITE_WORD | C_READ_BLOCK => {
                self.out.push_back(ACK1);
                State::Args {
                    cmd,
                    need: 5,
                    buf: Vec::new(),
                }
            }
            C_READ_WORD => {
                self.out.push_back(ACK1);
                State::Args {
                    cmd,
                    need: 3,
                    buf: Vec::new(),
                }
            }
            // A zero byte to a running kernel: the public tools read `AA`
            // here as "kernel already running".
            HELLO => {
                self.out.push_back(ACK1);
                State::Command
            }
            _ => State::Command,
        }
    }

    fn execute(&mut self, cmd: u8, a: &[u8]) {
        let addr = u32::from_le_bytes([a[0], a[1], a[2], 0]);
        match cmd {
            C_WRITE_WORD => {
                let v = u16::from_le_bytes([a[3], a[4]]);
                self.writes.push((addr, v));
                if self.cfg.fault != (Fault::StuckRegister { addr }) {
                    self.regs.insert(addr, v);
                }
                self.out.push_back(ACK2);
            }
            C_READ_WORD => {
                let [lo, hi] = self.read_word(addr).to_le_bytes();
                self.out.extend([lo, hi, ACK2]);
            }
            C_READ_BLOCK => {
                let len = u16::from_le_bytes([a[3], a[4]]) as u32;
                let data: Vec<u8> = (0..len).map(|i| self.read_byte(addr + i)).collect();
                self.last_checksum = xor_checksum(&data);
                let bad_ack = self.cfg.fault == (Fault::BadBlockAck { block: self.blocks });
                self.blocks += 1;
                self.out.extend(data);
                self.out.push_back(if bad_ack { 0x00 } else { ACK2 });
            }
            _ => {}
        }
    }
}

impl Port for SimC167 {
    fn send(&mut self, data: &[u8]) -> io::Result<()> {
        for &b in data {
            self.feed(b);
        }
        Ok(())
    }

    fn recv(&mut self, buf: &mut [u8], _timeout: Duration) -> io::Result<usize> {
        let n = buf.len().min(self.out.len());
        for slot in buf.iter_mut().take(n) {
            *slot = self.out.pop_front().expect("counted");
        }
        Ok(n)
    }

    fn discard_input(&mut self) -> io::Result<()> {
        self.out.clear();
        Ok(())
    }
}
