//! A simulated VW TP2.0 module (the ECU side of a channel) and an in-memory
//! CAN link to it, for testing drivers and engines without hardware.
//! Synthetic: the behaviour follows the public TP2.0 description (see
//! [`vwtp20`](crate::protocol::vwtp20)), not any particular module.
//!
//! [`Tp20Ecu`] is a pure frame-in / frames-out state machine: answer the
//! channel setup for its logical address, the `A0`/`A3` parameter frames and
//! the `A8` disconnect, ACK the tester's blocks, hand each complete request
//! telegram to an application callback (e.g. a KWP2000 simulator) and send
//! the reply segmented in the tester's requested block size, waiting for the
//! tester's ACK at every block end. It records protocol violations by the
//! tester in [`Tp20Ecu::errors`] so a test can assert there were none.
//!
//! [`Tp20SimLink`] is a `Send` [`CanLink`] onto a shared `Tp20Ecu`, optionally
//! with unrelated bus traffic mixed in.
use crate::protocol::isotp::CanLink;
use crate::protocol::vwtp20::BROADCAST_ID;
use crate::{Error, Result};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The application behind the channel: request telegram in, reply out.
pub type App = Box<dyn FnMut(&[u8]) -> Vec<u8> + Send>;

/// How the simulated module behaves.
#[derive(Debug, Clone)]
pub struct Tp20EcuConfig {
    /// Logical address it answers the channel setup for.
    pub addr: u8,
    /// CAN id it assigns the tester to transmit on.
    pub tester_tx_id: u32,
    /// Block size in its `A1`: the tester must wait for an ACK after this
    /// many frames.
    pub block_size: u8,
    /// End reply telegrams with opcode `0x3` (last, no ACK) instead of `0x1`.
    pub last_without_ack: bool,
    /// Answer each tester block with a "not ready" ACK (`0x9`) before the `0xB`.
    pub ack_not_ready_first: bool,
    /// Drop the channel after this long without a frame from the tester.
    pub idle_timeout: Option<Duration>,
    /// Never answer anything (a module that is absent or asleep).
    pub silent: bool,
}

impl Default for Tp20EcuConfig {
    fn default() -> Self {
        Self {
            addr: 0x01,
            tester_tx_id: 0x740,
            block_size: 0x0F,
            last_without_ack: false,
            ack_not_ready_first: false,
            idle_timeout: None,
            silent: false,
        }
    }
}

/// The simulated module.
pub struct Tp20Ecu {
    pub cfg: Tp20EcuConfig,
    app: App,
    /// CAN id the module transmits on (the tester's receive id).
    ecu_tx: u32,
    assigned: bool,
    open: bool,
    /// The tester's requested block size (from its `A0`).
    tester_bs: u8,
    rx_acc: Vec<u8>,
    rx_seq: Option<u8>,
    tx_seq: u8,
    pending: VecDeque<Vec<u8>>,
    awaiting_ack: bool,
    last_rx: Instant,
    /// Every frame the tester sent, in order.
    pub log: Vec<(u32, Vec<u8>)>,
    /// Complete request telegrams handed to the application.
    pub requests: Vec<Vec<u8>>,
    pub setups: usize,
    pub keep_alives: usize,
    pub disconnects: usize,
    /// ACKs the tester sent for the module's blocks.
    pub acks_received: usize,
    /// Protocol violations by the tester (sequence, ACK, framing).
    pub errors: Vec<String>,
}

impl Tp20Ecu {
    pub fn new(cfg: Tp20EcuConfig, app: App) -> Self {
        Self {
            cfg,
            app,
            ecu_tx: 0,
            assigned: false,
            open: false,
            tester_bs: 0x0F,
            rx_acc: Vec::new(),
            rx_seq: None,
            tx_seq: 0,
            pending: VecDeque::new(),
            awaiting_ack: false,
            last_rx: Instant::now(),
            log: Vec::new(),
            requests: Vec::new(),
            setups: 0,
            keep_alives: 0,
            disconnects: 0,
            acks_received: 0,
            errors: Vec::new(),
        }
    }

    /// Wrap in the shared handle [`Tp20SimLink`] uses.
    pub fn shared(self) -> Arc<Mutex<Tp20Ecu>> {
        Arc::new(Mutex::new(self))
    }

    /// Whether the channel is currently open.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The CAN id the module transmits on (learned from the setup request).
    pub fn ecu_tx_id(&self) -> u32 {
        self.ecu_tx
    }

    fn params(&self) -> Vec<u8> {
        // A1: block size, T1 = 100 ms, T3 = 0 (no separation, so tests never sleep).
        vec![0xA1, self.cfg.block_size, 0x8A, 0xFF, 0x00, 0xFF]
    }

    fn reset_channel(&mut self) {
        self.assigned = false;
        self.open = false;
        self.rx_acc.clear();
        self.rx_seq = None;
        self.tx_seq = 0;
        self.pending.clear();
        self.awaiting_ack = false;
    }

    /// Feed one frame the tester sent; returns the frames the module sends
    /// in response, in order.
    pub fn on_frame(&mut self, id: u32, data: &[u8]) -> Vec<(u32, Vec<u8>)> {
        self.log.push((id, data.to_vec()));
        if self.cfg.silent {
            return Vec::new();
        }
        if id == BROADCAST_ID {
            if data.len() >= 7 && data[0] == self.cfg.addr && data[1] == 0xC0 {
                self.reset_channel();
                self.ecu_tx = data[4] as u32 | ((data[5] as u32 & 0x0F) << 8);
                self.assigned = true;
                self.setups += 1;
                self.last_rx = Instant::now();
                let t = self.cfg.tester_tx_id;
                return vec![(
                    BROADCAST_ID + self.cfg.addr as u32,
                    vec![
                        0x00,
                        0xD0,
                        data[4],
                        data[5],
                        (t & 0xFF) as u8,
                        ((t >> 8) & 0x0F) as u8,
                        data[6],
                    ],
                )];
            }
            return Vec::new();
        }
        if id != self.cfg.tester_tx_id || data.is_empty() || !self.assigned {
            return Vec::new();
        }
        if let Some(t) = self.cfg.idle_timeout {
            if self.last_rx.elapsed() > t {
                // The channel timed out: the module no longer listens on it.
                self.reset_channel();
                return Vec::new();
            }
        }
        self.last_rx = Instant::now();
        let tx = self.ecu_tx;
        match data[0] {
            0xA0 => {
                if data.len() >= 2 {
                    self.tester_bs = data[1];
                }
                self.open = true;
                return vec![(tx, self.params())];
            }
            0xA3 => {
                self.keep_alives += 1;
                return vec![(tx, self.params())];
            }
            0xA8 => {
                self.disconnects += 1;
                self.reset_channel();
                return vec![(tx, vec![0xA8])];
            }
            _ => {}
        }
        if !self.open {
            self.errors
                .push(format!("frame {data:02X?} before the A0/A1 exchange"));
            return Vec::new();
        }
        let (kind, seq) = (data[0] & 0xF0, data[0] & 0x0F);
        match kind {
            0xB0 | 0x90 => {
                self.acks_received += 1;
                if !self.awaiting_ack {
                    self.errors.push(format!("unexpected ACK {:02X}", data[0]));
                    return Vec::new();
                }
                if seq != self.tx_seq {
                    self.errors.push(format!(
                        "ACK {:02X} for the wrong frame (next is {})",
                        data[0], self.tx_seq
                    ));
                }
                if kind == 0x90 {
                    return Vec::new();
                }
                self.awaiting_ack = false;
                self.emit_block()
            }
            0x00..=0x30 => {
                if let Some(exp) = self.rx_seq {
                    if seq != exp {
                        self.errors
                            .push(format!("data sequence {seq}, expected {exp}"));
                    }
                }
                let next = (seq + 1) & 0x0F;
                self.rx_seq = Some(next);
                self.rx_acc.extend_from_slice(&data[1..]);
                let mut out = Vec::new();
                if kind == 0x00 || kind == 0x10 {
                    if self.cfg.ack_not_ready_first {
                        out.push((tx, vec![0x90 | next]));
                    }
                    out.push((tx, vec![0xB0 | next]));
                }
                if kind == 0x10 || kind == 0x30 {
                    let acc = std::mem::take(&mut self.rx_acc);
                    if acc.len() < 2 {
                        self.errors.push("telegram shorter than its length".into());
                        return out;
                    }
                    let len = ((acc[0] as usize) << 8 | acc[1] as usize) & 0x0FFF;
                    if acc.len() - 2 < len {
                        self.errors
                            .push(format!("telegram declares {len}, got {}", acc.len() - 2));
                        return out;
                    }
                    let req = acc[2..2 + len].to_vec();
                    self.requests.push(req.clone());
                    let resp = (self.app)(&req);
                    self.queue_reply(&resp);
                    out.extend(self.emit_block());
                }
                out
            }
            _ => {
                self.errors.push(format!("unknown opcode {:02X}", data[0]));
                Vec::new()
            }
        }
    }

    fn queue_reply(&mut self, resp: &[u8]) {
        let mut telegram = vec![((resp.len() >> 8) & 0x0F) as u8, resp.len() as u8];
        telegram.extend_from_slice(resp);
        let n = telegram.len().div_ceil(7);
        let bs = self.tester_bs as usize;
        for (i, chunk) in telegram.chunks(7).enumerate() {
            let op = if i + 1 == n {
                if self.cfg.last_without_ack {
                    0x30
                } else {
                    0x10
                }
            } else if bs != 0 && (i + 1) % bs == 0 {
                0x00
            } else {
                0x20
            };
            let mut f = vec![op];
            f.extend_from_slice(chunk);
            self.pending.push_back(f);
        }
    }

    /// Send queued reply frames up to (and including) the next one that
    /// needs the tester's ACK.
    fn emit_block(&mut self) -> Vec<(u32, Vec<u8>)> {
        let mut out = Vec::new();
        while let Some(mut f) = self.pending.pop_front() {
            f[0] |= self.tx_seq;
            self.tx_seq = (self.tx_seq + 1) & 0x0F;
            let kind = f[0] & 0xF0;
            out.push((self.ecu_tx, f));
            if kind == 0x00 || kind == 0x10 {
                self.awaiting_ack = true;
                break;
            }
        }
        out
    }
}

/// A `Send` [`CanLink`] onto a shared [`Tp20Ecu`]. The module answers
/// synchronously when a frame is sent; an empty bus times out immediately.
pub struct Tp20SimLink {
    ecu: Arc<Mutex<Tp20Ecu>>,
    inbox: VecDeque<(u32, Vec<u8>)>,
    noise: Option<(u32, Vec<u8>)>,
}

impl Tp20SimLink {
    pub fn new(ecu: Arc<Mutex<Tp20Ecu>>) -> Self {
        Self {
            ecu,
            inbox: VecDeque::new(),
            noise: None,
        }
    }

    /// Simulate a busy bus: this frame precedes every module frame, and an
    /// otherwise idle bus keeps delivering it (about one per millisecond)
    /// instead of going quiet.
    pub fn with_noise(mut self, id: u32, data: &[u8]) -> Self {
        self.noise = Some((id, data.to_vec()));
        self
    }
}

impl CanLink for Tp20SimLink {
    fn send_frame(&mut self, id: u32, data: &[u8]) -> Result<()> {
        if data.len() > 8 {
            return Err(Error::Protocol("CAN frame too long".into()));
        }
        let out = self
            .ecu
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .on_frame(id, data);
        for f in out {
            if let Some(n) = &self.noise {
                self.inbox.push_back(n.clone());
            }
            self.inbox.push_back(f);
        }
        Ok(())
    }

    fn recv_frame(&mut self, timeout_ms: u64) -> Result<(u32, Vec<u8>)> {
        if let Some(f) = self.inbox.pop_front() {
            return Ok(f);
        }
        match &self.noise {
            Some(n) => {
                std::thread::sleep(Duration::from_millis(1));
                Ok(n.clone())
            }
            None => Err(Error::Timeout(timeout_ms)),
        }
    }
}
