//! A simulated VW TP2.0 module (the ECU side of a channel) and an in-memory
//! CAN link to it, for testing drivers and engines without hardware.
//! Synthetic: the behaviour follows the public TP2.0 description (see
//! [`vwtp20`](crate::protocol::vwtp20)), not any particular module.
//!
//! [`Tp20Ecu`] is a frame-in / frames-out state machine: answer the channel
//! setup for its logical address, the `A0`/`A3` parameter frames, the `A4`
//! break and the `A8` disconnect, ACK the tester's blocks, hand each complete
//! request telegram to an application callback (e.g. a KWP2000 simulator) and
//! send the reply segmented in the tester's requested block size, waiting for
//! the tester's ACK at every block end. It records protocol violations by the
//! tester in [`Tp20Ecu::errors`] so a test can assert there were none.
//!
//! ## Virtual time and fault injection
//!
//! The module keeps a *virtual* clock that only moves when the link waits
//! ([`Tp20Ecu::advance`], which [`Tp20SimLink::recv_frame`] calls when it has
//! nothing queued), so time-based behaviour costs no wall-clock time:
//!
//! * delayed replies ([`Tp20EcuConfig::reply_delay_ms`]) and `7F <sid> 78`
//!   responsePending chains ([`Tp20EcuConfig::response_pending`]);
//! * the module's own ACK timeout: a reply block the tester does not ACK
//!   within the tester's `T1` (from its `A0`) is broken (`A4`) and re-sent,
//!   at most [`Tp20EcuConfig::max_retransmits`] times, then the module
//!   disconnects (`A8`);
//! * scripted bus faults ([`Tp20EcuConfig::faults`]): drop, duplicate, delay
//!   (which reorders), wrong sequence number, or a module disconnect, on the
//!   n-th frame of a class in either direction.
//!
//! With no faults configured the module is *strict*: every sequence gap or
//! stray ACK from the tester lands in [`Tp20Ecu::errors`]. With faults the
//! gaps are expected (the bus lost a frame) and only counted.
//!
//! [`Tp20SimLink`] is a `Send` [`CanLink`] onto a shared `Tp20Ecu`, optionally
//! with unrelated bus traffic mixed in. Driving [`Tp20Ecu::on_frame`]
//! directly (as the slcan simulator does) gives every immediate behaviour;
//! time-based ones need [`Tp20Ecu::advance`] calls.
use crate::protocol::isotp::CanLink;
use crate::protocol::vwtp20::{decode_timing, BROADCAST_ID};
use crate::{Error, Result};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The application behind the channel: request telegram in, reply out.
pub type App = Box<dyn FnMut(&[u8]) -> Vec<u8> + Send>;

/// Which way a scripted fault applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultDir {
    /// Frames the tester sends to the module.
    ToEcu,
    /// Frames the module sends to the tester.
    ToTester,
}

/// Which frames a scripted fault counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameClass {
    /// Every channel frame (setup included).
    Any,
    /// Data frames (opcodes `0x0_`..`0x3_`).
    Data,
    /// ACKs (`0x9_`, `0xB_`).
    Ack,
    /// Parameter frames `A0`/`A1`.
    Params,
    /// Channel test `A3`.
    ChannelTest,
    /// Break `A4`.
    Break,
    /// Disconnect `A8`.
    Disconnect,
    /// Broadcast channel setup request/response.
    Setup,
}

/// What a scripted fault does to the frame it hits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultAction {
    /// The frame vanishes.
    Drop,
    /// The frame arrives twice.
    Duplicate,
    /// The frame arrives this many virtual ms late (after later frames).
    Delay(u64),
    /// Add this to the low nibble (sequence) of a data/ACK frame.
    WrongSeq(u8),
    /// The frame vanishes and the module closes the channel (`A8` to the
    /// tester) at this point.
    Disconnect,
}

/// One scripted fault: `action` hits the `nth` (0-based) frame of `class`
/// travelling `dir`, counted from the module's creation. Fires once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fault {
    pub dir: FaultDir,
    pub class: FrameClass,
    pub nth: usize,
    pub action: FaultAction,
}

impl Fault {
    pub fn new(dir: FaultDir, class: FrameClass, nth: usize, action: FaultAction) -> Self {
        Self {
            dir,
            class,
            nth,
            action,
        }
    }
}

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
    /// Drop the channel after this long (wall clock) without a frame from
    /// the tester.
    pub idle_timeout: Option<Duration>,
    /// Never answer anything (a module that is absent or asleep).
    pub silent: bool,
    /// `T1` timing byte in its `A1` (how long the tester waits for an ACK).
    pub t1: u8,
    /// `T3` timing byte in its `A1` (separation between the tester's frames).
    pub t3: u8,
    /// Times an unacknowledged reply block is broken (`A4`) and re-sent
    /// before the module gives up and disconnects.
    pub max_retransmits: u8,
    /// `7F <sid> 78` responsePending replies sent before each real reply.
    pub response_pending: u32,
    /// Virtual ms between consecutive responsePending / final replies.
    pub pending_interval_ms: u64,
    /// Virtual ms between a complete request and the first reply.
    pub reply_delay_ms: u64,
    /// Send a channel test (`A3`) before each reply; the tester must answer
    /// with an `A1` ([`Tp20Ecu::channel_test_replies`]).
    pub channel_test_before_reply: bool,
    /// Instead of answering the n-th (0-based) request, close the channel
    /// (`A8`).
    pub break_on_request: Option<usize>,
    /// Scripted bus faults.
    pub faults: Vec<Fault>,
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
            // T1 = 100 ms, T3 = 0 (no separation, so tests never sleep).
            t1: 0x8A,
            t3: 0x00,
            max_retransmits: 2,
            response_pending: 0,
            pending_interval_ms: 0,
            reply_delay_ms: 0,
            channel_test_before_reply: false,
            break_on_request: None,
            faults: Vec::new(),
        }
    }
}

/// Something scheduled on the virtual clock.
#[derive(Debug)]
enum Ev {
    /// A tester frame the bus delayed.
    Input(u32, Vec<u8>),
    /// A module frame the bus delayed.
    Output(u32, Vec<u8>),
    /// A reply telegram (payload) ready to send.
    Reply(Vec<u8>),
}

/// Receive-side state for the tester's telegrams.
#[derive(Debug, Default)]
struct Rx {
    acc: Vec<u8>,
    expected: Option<u8>,
    ack_seq: Option<u8>,
    acked_len: usize,
    block_start: Option<u8>,
    last_block: Option<(u8, u8)>,
    last_ack: Option<u8>,
    at_boundary: bool,
    lost: bool,
    last_frame: Vec<u8>,
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
    /// The tester's `T1` from its `A0`, virtual ms (our ACK wait).
    tester_t1_ms: u64,
    rx: Rx,
    tx_seq: u8,
    pending: VecDeque<Vec<u8>>,
    awaiting_ack: bool,
    /// The block awaiting the tester's ACK (frames with sequence applied).
    block: Vec<Vec<u8>>,
    ack_deadline: Option<u64>,
    retransmits_left: u8,
    last_ack_in: Option<u8>,
    replies: VecDeque<Vec<u8>>,
    last_rx: Instant,
    now_ms: u64,
    events: Vec<(u64, u64, Ev)>,
    order: u64,
    fault_hits: Vec<usize>,
    fault_fired: Vec<bool>,
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
    /// Wall-clock times of the tester's keep-alives (`A3`).
    pub keep_alive_times: Vec<Instant>,
    /// Reply blocks the module re-sent after an ACK timeout.
    pub retransmissions: usize,
    /// `A4` breaks the tester sent.
    pub breaks_received: usize,
    /// Tester data frames discarded as duplicates / replays.
    pub duplicates: usize,
    /// Sequence gaps in the tester's data (frames the bus lost).
    pub gaps: usize,
    /// Channel tests (`A3`) the module sent, and the tester's `A1` answers.
    pub channel_tests_sent: usize,
    pub channel_test_replies: usize,
    /// responsePending (0x78) replies the module sent.
    pub pending_sent: usize,
    /// Times the module gave up on an unacknowledged block (sent `A8`).
    pub gave_up: usize,
    /// Frames the scripted faults hit.
    pub faults_fired: usize,
}

const DATA_ACK_NOW: u8 = 0x00;
const DATA_LAST: u8 = 0x10;
const DATA_MORE: u8 = 0x20;
const DATA_LAST_NO_ACK: u8 = 0x30;

fn classify(id: u32, data: &[u8]) -> FrameClass {
    if (BROADCAST_ID..=BROADCAST_ID + 0xFF).contains(&id) {
        return FrameClass::Setup;
    }
    match data.first().copied().unwrap_or(0) {
        b if b < 0x40 => FrameClass::Data,
        b if b & 0xF0 == 0x90 || b & 0xF0 == 0xB0 => FrameClass::Ack,
        0xA0 | 0xA1 => FrameClass::Params,
        0xA3 => FrameClass::ChannelTest,
        0xA4 => FrameClass::Break,
        0xA8 => FrameClass::Disconnect,
        _ => FrameClass::Any,
    }
}

impl Tp20Ecu {
    pub fn new(cfg: Tp20EcuConfig, app: App) -> Self {
        let n = cfg.faults.len();
        Self {
            cfg,
            app,
            ecu_tx: 0,
            assigned: false,
            open: false,
            tester_bs: 0x0F,
            tester_t1_ms: 100,
            rx: Rx {
                at_boundary: true,
                ..Rx::default()
            },
            tx_seq: 0,
            pending: VecDeque::new(),
            awaiting_ack: false,
            block: Vec::new(),
            ack_deadline: None,
            retransmits_left: 0,
            last_ack_in: None,
            replies: VecDeque::new(),
            last_rx: Instant::now(),
            now_ms: 0,
            events: Vec::new(),
            order: 0,
            fault_hits: vec![0; n],
            fault_fired: vec![false; n],
            log: Vec::new(),
            requests: Vec::new(),
            setups: 0,
            keep_alives: 0,
            disconnects: 0,
            acks_received: 0,
            errors: Vec::new(),
            keep_alive_times: Vec::new(),
            retransmissions: 0,
            breaks_received: 0,
            duplicates: 0,
            gaps: 0,
            channel_tests_sent: 0,
            channel_test_replies: 0,
            pending_sent: 0,
            gave_up: 0,
            faults_fired: 0,
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

    /// The virtual clock, ms.
    pub fn now_ms(&self) -> u64 {
        self.now_ms
    }

    /// Whether gaps/stray ACKs count as tester errors (no faults scripted).
    fn strict(&self) -> bool {
        self.cfg.faults.is_empty()
    }

    fn params(&self) -> Vec<u8> {
        vec![0xA1, self.cfg.block_size, self.cfg.t1, 0xFF, self.cfg.t3, 0xFF]
    }

    fn reset_channel(&mut self) {
        self.assigned = false;
        self.open = false;
        self.rx = Rx {
            at_boundary: true,
            ..Rx::default()
        };
        self.tx_seq = 0;
        self.pending.clear();
        self.awaiting_ack = false;
        self.block.clear();
        self.ack_deadline = None;
        self.last_ack_in = None;
        self.replies.clear();
        // The module flushes its queues; nothing of the old channel is left
        // in flight.
        self.events.clear();
    }

    fn schedule(&mut self, at: u64, ev: Ev) {
        self.order += 1;
        self.events.push((at, self.order, ev));
    }

    /// The scripted fault (if any) hitting this frame.
    fn fault_for(&mut self, dir: FaultDir, id: u32, data: &[u8]) -> Option<FaultAction> {
        let class = classify(id, data);
        let mut hit = None;
        for i in 0..self.cfg.faults.len() {
            let f = self.cfg.faults[i];
            if f.dir != dir || (f.class != FrameClass::Any && f.class != class) {
                continue;
            }
            let n = self.fault_hits[i];
            self.fault_hits[i] += 1;
            if n == f.nth && !self.fault_fired[i] && hit.is_none() {
                self.fault_fired[i] = true;
                self.faults_fired += 1;
                hit = Some(f.action);
            }
        }
        hit
    }

    /// Feed one frame the tester sent; returns the frames the module sends
    /// in response (at once), in order.
    pub fn on_frame(&mut self, id: u32, data: &[u8]) -> Vec<(u32, Vec<u8>)> {
        self.log.push((id, data.to_vec()));
        if self.cfg.silent {
            return Vec::new();
        }
        let ours = (id == BROADCAST_ID && data.first() == Some(&self.cfg.addr))
            || (id == self.cfg.tester_tx_id && self.assigned);
        if !ours || self.cfg.faults.is_empty() {
            let out = self.process(id, data);
            return self.emit(out);
        }
        let mut data = data.to_vec();
        let out = match self.fault_for(FaultDir::ToEcu, id, &data) {
            None => self.process(id, &data),
            Some(FaultAction::Drop) => Vec::new(),
            Some(FaultAction::Duplicate) => {
                let mut out = self.process(id, &data);
                out.extend(self.process(id, &data));
                out
            }
            Some(FaultAction::Delay(ms)) => {
                self.schedule(self.now_ms + ms, Ev::Input(id, data));
                Vec::new()
            }
            Some(FaultAction::WrongSeq(d)) => {
                if matches!(classify(id, &data), FrameClass::Data | FrameClass::Ack) {
                    data[0] = (data[0] & 0xF0) | (data[0].wrapping_add(d) & 0x0F);
                }
                self.process(id, &data)
            }
            Some(FaultAction::Disconnect) => self.module_break(),
        };
        self.emit(out)
    }

    /// The module closes the channel.
    fn module_break(&mut self) -> Vec<(u32, Vec<u8>)> {
        if !self.assigned {
            return Vec::new();
        }
        let tx = self.ecu_tx;
        self.reset_channel();
        vec![(tx, vec![0xA8])]
    }

    /// Pass the module's frames through the scripted faults.
    fn emit(&mut self, frames: Vec<(u32, Vec<u8>)>) -> Vec<(u32, Vec<u8>)> {
        if self.cfg.faults.is_empty() {
            return frames;
        }
        let mut out = Vec::new();
        for (id, mut data) in frames {
            match self.fault_for(FaultDir::ToTester, id, &data) {
                None => out.push((id, data)),
                Some(FaultAction::Drop) => {}
                Some(FaultAction::Duplicate) => {
                    out.push((id, data.clone()));
                    out.push((id, data));
                }
                Some(FaultAction::Delay(ms)) => {
                    self.schedule(self.now_ms + ms, Ev::Output(id, data));
                }
                Some(FaultAction::WrongSeq(d)) => {
                    if matches!(classify(id, &data), FrameClass::Data | FrameClass::Ack) {
                        data[0] = (data[0] & 0xF0) | (data[0].wrapping_add(d) & 0x0F);
                    }
                    out.push((id, data));
                }
                Some(FaultAction::Disconnect) => {
                    out.extend(self.module_break());
                    break;
                }
            }
        }
        out
    }

    /// Let `max_ms` of virtual time pass, or less: stop at the first moment
    /// the module sends something, and return those frames (empty when
    /// nothing happened within `max_ms`).
    pub fn advance(&mut self, max_ms: u64) -> Vec<(u32, Vec<u8>)> {
        let until = self.now_ms.saturating_add(max_ms);
        loop {
            let next_ev = self.events.iter().map(|e| e.0).min();
            let next = match (next_ev, self.ack_deadline) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            let Some(t) = next.filter(|&t| t <= until) else {
                self.now_ms = until;
                return Vec::new();
            };
            self.now_ms = self.now_ms.max(t);
            let mut out = Vec::new();
            // Events due now, in scheduling order.
            loop {
                let due = self
                    .events
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| e.0 <= self.now_ms)
                    .min_by_key(|(_, e)| (e.0, e.1))
                    .map(|(i, _)| i);
                let Some(i) = due else { break };
                let (_, _, ev) = self.events.remove(i);
                match ev {
                    Ev::Input(id, data) => {
                        let o = self.process(id, &data);
                        out.extend(self.emit(o));
                    }
                    Ev::Output(id, data) => out.push((id, data)),
                    Ev::Reply(r) => {
                        self.replies.push_back(r);
                        let o = self.pump();
                        out.extend(self.emit(o));
                    }
                }
            }
            if self.ack_deadline.is_some_and(|d| d <= self.now_ms) {
                let o = self.ack_timeout();
                out.extend(self.emit(o));
            }
            if !out.is_empty() {
                return out;
            }
        }
    }

    /// The tester did not ACK our block within its T1: break and resend, or
    /// give up and close the channel.
    fn ack_timeout(&mut self) -> Vec<(u32, Vec<u8>)> {
        if !self.awaiting_ack {
            self.ack_deadline = None;
            return Vec::new();
        }
        if self.retransmits_left == 0 {
            self.gave_up += 1;
            return self.module_break();
        }
        self.retransmits_left -= 1;
        self.retransmissions += 1;
        self.ack_deadline = Some(self.now_ms + self.tester_t1_ms);
        let tx = self.ecu_tx;
        let mut out = vec![(tx, vec![0xA4])];
        out.extend(self.block.iter().map(|f| (tx, f.clone())));
        out
    }

    /// Handle one tester frame (after bus faults).
    fn process(&mut self, id: u32, data: &[u8]) -> Vec<(u32, Vec<u8>)> {
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
                if data.len() >= 3 {
                    self.tester_t1_ms = (decode_timing(data[2]) / 1000).max(1);
                }
                self.open = true;
                return vec![(tx, self.params())];
            }
            0xA1 => {
                self.channel_test_replies += 1;
                return Vec::new();
            }
            0xA3 => {
                self.keep_alives += 1;
                self.keep_alive_times.push(Instant::now());
                return vec![(tx, self.params())];
            }
            0xA4 => {
                // Discard everything since our last ACK.
                self.breaks_received += 1;
                let r = &mut self.rx;
                r.acc.truncate(r.acked_len);
                r.expected = r.ack_seq.or(r.expected);
                r.block_start = None;
                r.at_boundary = true;
                r.lost = false;
                r.last_frame.clear();
                return Vec::new();
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
            0xB0 | 0x90 => self.on_ack(kind, seq, data[0]),
            DATA_ACK_NOW | DATA_LAST | DATA_MORE | DATA_LAST_NO_ACK => self.on_data(kind, seq, data),
            _ => {
                self.errors.push(format!("unknown opcode {:02X}", data[0]));
                Vec::new()
            }
        }
    }

    fn on_ack(&mut self, kind: u8, seq: u8, b: u8) -> Vec<(u32, Vec<u8>)> {
        self.acks_received += 1;
        if !self.awaiting_ack || seq != self.tx_seq {
            // A duplicate of the ACK we already took is harmless.
            if Some(seq) != self.last_ack_in && self.strict() {
                if self.awaiting_ack {
                    self.errors.push(format!(
                        "ACK {b:02X} for the wrong frame (next is {})",
                        self.tx_seq
                    ));
                } else {
                    self.errors.push(format!("unexpected ACK {b:02X}"));
                }
            }
            return Vec::new();
        }
        if kind == 0x90 {
            // Received, not ready: stop the retransmit timer and wait.
            self.ack_deadline = None;
            return Vec::new();
        }
        self.awaiting_ack = false;
        self.ack_deadline = None;
        self.last_ack_in = Some(seq);
        let mut out = self.emit_block();
        out.extend(self.pump());
        out
    }

    fn on_data(&mut self, kind: u8, seq: u8, data: &[u8]) -> Vec<(u32, Vec<u8>)> {
        let tx = self.ecu_tx;
        let expected = *self.rx.expected.get_or_insert(seq);
        if self.rx.ack_seq.is_none() {
            self.rx.ack_seq = Some(seq);
        }
        if !self.rx.lost && seq == expected {
            let next = (seq + 1) & 0x0F;
            self.rx.expected = Some(next);
            self.rx.acc.extend_from_slice(&data[1..]);
            self.rx.block_start.get_or_insert(seq);
            self.rx.at_boundary = false;
            self.rx.last_frame = data.to_vec();
            let mut out = Vec::new();
            if kind == DATA_ACK_NOW || kind == DATA_LAST {
                if self.cfg.ack_not_ready_first {
                    out.push((tx, vec![0x90 | next]));
                }
                out.push((tx, vec![0xB0 | next]));
                let start = self.rx.block_start.take().unwrap_or(seq);
                let n = (next.wrapping_sub(start) & 0x0F).max(1);
                self.rx.last_block = Some((start, n));
                self.rx.last_ack = Some(next);
                self.rx.ack_seq = Some(next);
                self.rx.acked_len = self.rx.acc.len();
                self.rx.at_boundary = true;
            }
            if kind == DATA_LAST || kind == DATA_LAST_NO_ACK {
                if kind == DATA_LAST_NO_ACK {
                    self.rx.block_start = None;
                    self.rx.last_block = None;
                    self.rx.ack_seq = Some(next);
                    self.rx.at_boundary = true;
                }
                let acc = std::mem::take(&mut self.rx.acc);
                self.rx.acked_len = 0;
                out.extend(self.telegram(acc));
            }
            return out;
        }
        // A bus duplicate, or a replay of the block we ACKed (our ACK was lost).
        let dup_frame = !self.rx.last_frame.is_empty() && data == self.rx.last_frame.as_slice();
        let in_last = self
            .rx
            .last_block
            .is_some_and(|(s, n)| (seq.wrapping_sub(s) & 0x0F) < n);
        if dup_frame || (self.rx.at_boundary && in_last) {
            self.duplicates += 1;
            if let (false, Some((s, n)), Some(a)) = (dup_frame, self.rx.last_block, self.rx.last_ack)
            {
                if seq == (s + n - 1) & 0x0F && (kind == DATA_ACK_NOW || kind == DATA_LAST) {
                    return vec![(tx, vec![0xB0 | a])];
                }
            }
            return Vec::new();
        }
        if !self.rx.lost {
            self.gaps += 1;
            if self.strict() {
                self.errors
                    .push(format!("data sequence {seq}, expected {expected}"));
            }
            self.rx.lost = true;
        }
        Vec::new()
    }

    /// A complete request telegram arrived.
    fn telegram(&mut self, acc: Vec<u8>) -> Vec<(u32, Vec<u8>)> {
        if acc.len() < 2 {
            self.errors.push("telegram shorter than its length".into());
            return Vec::new();
        }
        let len = ((acc[0] as usize) << 8 | acc[1] as usize) & 0x0FFF;
        if acc.len() - 2 < len {
            self.errors
                .push(format!("telegram declares {len}, got {}", acc.len() - 2));
            return Vec::new();
        }
        let req = acc[2..2 + len].to_vec();
        self.requests.push(req.clone());
        if self.cfg.break_on_request == Some(self.requests.len() - 1) {
            return self.module_break();
        }
        let resp = (self.app)(&req);
        let sid = req.first().copied().unwrap_or(0);
        let mut at = self.now_ms + self.cfg.reply_delay_ms;
        let mut immediate = Vec::new();
        let mut telegrams: Vec<Vec<u8>> = (0..self.cfg.response_pending)
            .map(|_| vec![0x7F, sid, 0x78])
            .collect();
        telegrams.push(resp);
        for (i, t) in telegrams.into_iter().enumerate() {
            if i > 0 {
                at += self.cfg.pending_interval_ms;
            }
            if at == self.now_ms {
                immediate.push(t);
            } else {
                self.schedule(at, Ev::Reply(t));
            }
        }
        self.replies.extend(immediate);
        self.pump()
    }

    /// Start the next queued reply when the previous one is fully sent.
    fn pump(&mut self) -> Vec<(u32, Vec<u8>)> {
        if !self.pending.is_empty() || self.awaiting_ack || !self.open {
            return Vec::new();
        }
        let Some(r) = self.replies.pop_front() else {
            return Vec::new();
        };
        if r.len() == 3 && r[0] == 0x7F && r[2] == 0x78 {
            self.pending_sent += 1;
        }
        let mut out = Vec::new();
        if self.cfg.channel_test_before_reply {
            self.channel_tests_sent += 1;
            out.push((self.ecu_tx, vec![0xA3]));
        }
        self.queue_reply(&r);
        out.extend(self.emit_block());
        // A reply ending without an ACK lets the next one start at once.
        if !self.awaiting_ack && self.pending.is_empty() {
            out.extend(self.pump());
        }
        out
    }

    fn queue_reply(&mut self, resp: &[u8]) {
        let mut telegram = vec![((resp.len() >> 8) & 0x0F) as u8, resp.len() as u8];
        telegram.extend_from_slice(resp);
        let n = telegram.len().div_ceil(7);
        let bs = self.tester_bs as usize;
        for (i, chunk) in telegram.chunks(7).enumerate() {
            let op = if i + 1 == n {
                if self.cfg.last_without_ack {
                    DATA_LAST_NO_ACK
                } else {
                    DATA_LAST
                }
            } else if bs != 0 && (i + 1) % bs == 0 {
                DATA_ACK_NOW
            } else {
                DATA_MORE
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
        self.block.clear();
        while let Some(mut f) = self.pending.pop_front() {
            f[0] |= self.tx_seq;
            self.tx_seq = (self.tx_seq + 1) & 0x0F;
            let kind = f[0] & 0xF0;
            self.block.push(f.clone());
            out.push((self.ecu_tx, f));
            if kind == DATA_ACK_NOW || kind == DATA_LAST {
                self.awaiting_ack = true;
                self.retransmits_left = self.cfg.max_retransmits;
                self.ack_deadline = Some(self.now_ms + self.tester_t1_ms);
                break;
            }
        }
        out
    }
}

/// A `Send` [`CanLink`] onto a shared [`Tp20Ecu`]. The module answers
/// synchronously when a frame is sent; waiting on an empty bus lets the
/// module's virtual clock run (delayed replies, retransmissions) and times
/// out at once when nothing is due within the wait.
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

    fn queue(&mut self, out: Vec<(u32, Vec<u8>)>) {
        for f in out {
            if let Some(n) = &self.noise {
                self.inbox.push_back(n.clone());
            }
            self.inbox.push_back(f);
        }
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
        self.queue(out);
        Ok(())
    }

    fn recv_frame(&mut self, timeout_ms: u64) -> Result<(u32, Vec<u8>)> {
        if let Some(f) = self.inbox.pop_front() {
            return Ok(f);
        }
        let wait = if self.noise.is_some() { 1 } else { timeout_ms };
        let out = self
            .ecu
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .advance(wait);
        self.queue(out);
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
