//! An offline, scripted OpenPort 2.0: speaks the documented `at…` protocol
//! (see [`crate::proto`]) and simulates one CAN bus behind it, with an ECU
//! that answers ISO-TP requests through a caller-supplied handler (in the
//! tests, the independent `opentune-udssim` UDS server).
//!
//! It models what the public notes report about the real cable, so the driver
//! is tested against it rather than against itself:
//! - lines end at `\n` (a bare `\r` ends nothing); the protocol digit must be
//!   glued to the verb; a command without a trailing sequence number gets an
//!   unnumbered reply (an `att` without one gets none at all);
//! - binary payloads follow the line; a wrong payload length is answered
//!   `are 10` *without* consuming the payload, so the stream desynchronises
//!   exactly as the hardware's does;
//! - no frame reaches the host until a PASS filter matches it; BLOCK filters
//!   win over PASS;
//! - a transmit needs a non-zero budget (`are 9` otherwise) and is followed by
//!   a `0x10` transmit indication carrying the CAN id;
//! - error replies, silence, stale replies, frames arriving before the
//!   transmit's `aro`, a bus with no other node, background traffic and
//!   leading garbage can all be injected ([`FakeFaults`]).
//!
//! [`FakePort`] plugs the fake straight into the driver
//! ([`crate::OpenPort2Driver::open_with_port`]) with configurable read
//! chunking; [`serve`] runs it on the far end of a real byte stream (a pty in
//! the CLI end-to-end test). No K-line: the driver does not open K-line
//! channels, so `ato3`/`ato4` are accepted but nothing answers on them.
use crate::port::Op2Port;
use crate::proto::{filter, status, CAN_29BIT_ID, PROTO_CAN};
use opentune_core::Result;
use std::collections::{BTreeMap, VecDeque};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The ECU behind the fake bus: one complete request payload in, the payloads
/// it answers with out (several for `0x78` responsePending + final reply, none
/// for a suppressed response).
pub type EcuHandler = Box<dyn FnMut(&[u8]) -> Vec<Vec<u8>> + Send>;

/// Faults to inject. Command names are the verb plus protocol digit as sent
/// (`"ato5"`, `"att5"`, `"atf5"`) or the bare verb (`"atz"`, `"atr"`).
#[derive(Debug, Clone, Default)]
pub struct FakeFaults {
    /// Answer these commands `are <code> <seq>` (after consuming any payload).
    pub error_reply: Vec<(String, u32)>,
    /// Never answer these commands.
    pub silent: Vec<String>,
    /// Emit this many stale replies (`aro`/`are` with a sequence number nobody
    /// is waiting for) before the next replies.
    pub stale_replies: usize,
    /// Put the ECU's answer frames on the stream before the `aro` of the
    /// transmit that triggered them.
    pub frames_before_ack: bool,
    /// No other node acknowledges on the bus: every transmit fails `are 9`.
    pub bus_no_ack: bool,
    /// Background frames another node broadcasts after each transmit (they
    /// reach the host only if a filter passes them).
    pub noise: Vec<(u32, Vec<u8>)>,
    /// Bytes the device emits before its first reply (a stale backlog).
    pub garbage_on_open: Vec<u8>,
    /// Echo every transmit as a `0x20` loopback record (as `LOOPBACK` = 1).
    pub loopback_echo: bool,
}

/// Static behaviour of the fake.
#[derive(Debug, Clone)]
pub struct FakeConfig {
    /// Text after `ari ` in the identify reply.
    pub version: String,
    /// Battery voltage reported on pin 16, millivolts.
    pub battery_mv: u32,
    /// The ECU's request (physical) and response CAN ids.
    pub ecu_request_id: u32,
    pub ecu_response_id: u32,
    /// Block size the ECU advertises in its flow control.
    pub ecu_block_size: u8,
    /// Byte the ECU pads its frames with.
    pub ecu_pad: u8,
    pub faults: FakeFaults,
}

impl Default for FakeConfig {
    fn default() -> Self {
        Self {
            version: "main code version : 1.17.4877".into(),
            battery_mv: 12_480,
            ecu_request_id: 0x7E0,
            ecu_response_id: 0x7E8,
            ecu_block_size: 0,
            ecu_pad: 0x55,
            faults: FakeFaults::default(),
        }
    }
}

/// One frame seen on the simulated bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusFrame {
    /// `true` when the host (through the adapter) sent it.
    pub from_host: bool,
    pub id: u32,
    pub data: Vec<u8>,
}

/// An installed filter, as the fake recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterRec {
    pub id: u32,
    pub proto: u8,
    pub filter_type: u32,
    pub txflags: u32,
    pub mask: Vec<u8>,
    pub pattern: Vec<u8>,
}

/// What the fake observed, shared with the test through [`FakeHandle`].
#[derive(Debug, Clone, Default)]
pub struct FakeLog {
    /// Every command line received, without its line ending.
    pub lines: Vec<String>,
    /// Every frame on the bus, in order.
    pub bus: Vec<BusFrame>,
    /// `(proto, flags, baud)` of every successful `ato`.
    pub opened: Vec<(u8, u32, u32)>,
    /// `(proto, txflags, budget_us)` of every accepted `att`.
    pub transmits: Vec<(u8, u32, u32)>,
    /// Filters currently installed.
    pub filters: Vec<FilterRec>,
    /// Channels currently open.
    pub open_channels: Vec<u8>,
    /// Set if the bootloader command `atx` was ever received.
    pub saw_bootloader: bool,
}

/// Test-side view of a fake that has been moved into a driver or thread.
#[derive(Clone)]
pub struct FakeHandle(Arc<Mutex<FakeLog>>);

impl FakeHandle {
    pub fn log(&self) -> FakeLog {
        self.0.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

#[derive(Debug)]
struct Channel {
    flags: u32,
}

/// The ECU's half of ISO-TP.
#[derive(Default)]
struct EcuTp {
    /// Request being reassembled: (expected length, bytes, next SN).
    rx: Option<(usize, Vec<u8>, u8)>,
    /// Response being sent: (bytes, next index, next SN), waiting for an FC.
    tx: Option<(Vec<u8>, usize, u8)>,
    queue: VecDeque<Vec<u8>>,
}

/// The fake device.
pub struct FakeOp2 {
    cfg: FakeConfig,
    log: Arc<Mutex<FakeLog>>,
    ecu: EcuHandler,
    tp: EcuTp,
    inbuf: Vec<u8>,
    /// Binary payload still owed by the host: (command, args, seq, length).
    awaiting: Option<(String, Vec<i64>, Option<i64>, usize)>,
    out: VecDeque<u8>,
    channels: BTreeMap<u8, Channel>,
    next_filter_id: u32,
    clock_us: u32,
    garbage_sent: bool,
    stale_left: usize,
}

impl FakeOp2 {
    pub fn new(cfg: FakeConfig, ecu: EcuHandler) -> (Self, FakeHandle) {
        let log = Arc::new(Mutex::new(FakeLog::default()));
        let stale_left = cfg.faults.stale_replies;
        (
            Self {
                cfg,
                log: Arc::clone(&log),
                ecu,
                tp: EcuTp::default(),
                inbuf: Vec::new(),
                awaiting: None,
                out: VecDeque::new(),
                channels: BTreeMap::new(),
                next_filter_id: 0,
                clock_us: 0x1000,
                garbage_sent: false,
                stale_left,
            },
            FakeHandle(log),
        )
    }

    fn with_log(&self, f: impl FnOnce(&mut FakeLog)) {
        if let Ok(mut g) = self.log.lock() {
            f(&mut g);
        }
    }

    /// Host → device bytes.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.inbuf.extend_from_slice(bytes);
        self.process();
    }

    /// Device → host bytes produced so far (all of them).
    pub fn take_output(&mut self) -> Vec<u8> {
        self.out.drain(..).collect()
    }

    /// Up to `max` device → host bytes.
    pub fn take_up_to(&mut self, max: usize) -> Vec<u8> {
        let n = max.min(self.out.len());
        self.out.drain(..n).collect()
    }

    pub fn has_output(&self) -> bool {
        !self.out.is_empty()
    }

    fn emit(&mut self, bytes: &[u8]) {
        if !self.garbage_sent {
            self.garbage_sent = true;
            let g = self.cfg.faults.garbage_on_open.clone();
            self.out.extend(g);
        }
        self.out.extend(bytes.iter().copied());
    }

    fn reply(&mut self, head: &str, seq: Option<i64>) {
        if self.stale_left > 0 {
            self.stale_left -= 1;
            // A reply for a command the host has long given up on.
            let stale = format!(
                "aro 6{:04}\r\nare 7 5{:04}\r\n",
                self.stale_left, self.stale_left
            );
            self.emit(stale.as_bytes());
        }
        let line = match seq {
            Some(s) => format!("{head} {s}\r\n"),
            None => format!("{head}\r\n"),
        };
        self.emit(line.as_bytes());
    }

    fn tick(&mut self) -> u32 {
        self.clock_us = self.clock_us.wrapping_add(250);
        self.clock_us
    }

    fn record(&mut self, proto: u8, st: u8, id: u32, data: &[u8]) {
        let ts = self.tick();
        let mut body = vec![st];
        body.extend_from_slice(&ts.to_be_bytes());
        body.extend_from_slice(&id.to_be_bytes());
        body.extend_from_slice(data);
        let mut rec = vec![b'a', b'r', b'0' + proto, body.len() as u8];
        rec.extend(body);
        self.emit(&rec);
    }

    fn process(&mut self) {
        loop {
            if let Some((_, _, _, n)) = &self.awaiting {
                if self.inbuf.len() < *n {
                    return;
                }
                let (cmd, args, seq, n) = self.awaiting.take().unwrap();
                let payload: Vec<u8> = self.inbuf.drain(..n).collect();
                self.execute_with_payload(&cmd, &args, seq, &payload);
                continue;
            }
            let Some(nl) = self.inbuf.iter().position(|&b| b == b'\n') else {
                return;
            };
            let raw: Vec<u8> = self.inbuf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&raw[..nl])
                .trim_matches(|c| c == '\r' || c == ' ')
                .to_string();
            if line.is_empty() {
                continue;
            }
            self.with_log(|l| l.lines.push(line.clone()));
            self.handle_line(&line);
        }
    }

    fn handle_line(&mut self, line: &str) {
        let b = line.as_bytes();
        if b.len() < 3 || &b[..2] != b"at" {
            return; // not a command: the cable stays silent
        }
        let verb = b[2] as char;
        let rest = &line[3..];
        // Channel verbs carry the protocol digit glued to the verb.
        let channel_verb = matches!(
            verb,
            'o' | 'c' | 't' | 'f' | 'g' | 's' | 'k' | 'm' | 'n' | 'l'
        );
        let (proto, args_text) = if channel_verb {
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() {
                // `ato 5 …`: the cable rejects a spaced protocol digit.
                self.reply("are 7", None);
                return;
            }
            (digits.parse::<u8>().unwrap_or(0), &rest[digits.len()..])
        } else {
            (0u8, rest)
        };
        let name = if channel_verb {
            format!("at{verb}{proto}")
        } else {
            format!("at{verb}")
        };
        let Ok(args) = args_text
            .split_whitespace()
            .map(str::parse::<i64>)
            .collect::<std::result::Result<Vec<i64>, _>>()
        else {
            self.reply("are 7", None);
            return;
        };
        if verb == 'x' {
            self.with_log(|l| l.saw_bootloader = true);
            return;
        }
        if verb == 'i' {
            if self.cfg.faults.silent.iter().any(|s| s == "ati") {
                return;
            }
            let v = format!("ari {}", self.cfg.version);
            self.reply(&v, None);
            return;
        }
        let expected = match verb {
            'a' | 'z' | 'c' => 0,
            'r' => 1,
            'o' | 't' | 'f' => 3,
            _ => return, // undocumented here: no reply, like an unknown verb
        };
        let (args, seq) = if args.len() == expected + 1 {
            (args[..expected].to_vec(), Some(args[expected]))
        } else if args.len() == expected {
            (args, None)
        } else {
            self.reply("are 7", None);
            return;
        };
        if self.cfg.faults.silent.iter().any(|s| s == &name) {
            // Still swallow a payload so later lines stay aligned.
            if let Some(n) = payload_len(verb, &args) {
                self.awaiting = Some(("<silent>".into(), Vec::new(), None, n));
            }
            return;
        }
        if let Some(n) = payload_len(verb, &args) {
            if n == usize::MAX {
                // A malformed length: refused, payload NOT consumed.
                self.reply("are 10", seq);
                return;
            }
            self.awaiting = Some((
                name,
                args.iter().copied().chain([proto as i64]).collect(),
                seq,
                n,
            ));
            return;
        }
        if let Some(code) = self.injected_error(&name) {
            self.reply(&format!("are {code}"), seq);
            return;
        }
        match verb {
            'a' | 'z' => {
                self.channels.clear();
                self.with_log(|l| {
                    l.filters.clear();
                    l.open_channels.clear();
                });
                if verb == 'z' {
                    self.tp = EcuTp::default();
                }
                self.reply("aro", seq);
            }
            'c' => {
                self.channels.remove(&proto);
                self.with_log(|l| {
                    l.filters.retain(|f| f.proto != proto);
                    l.open_channels.retain(|&p| p != proto);
                });
                self.reply("aro", seq);
            }
            'r' => {
                let pin = args[0];
                match pin {
                    16 => {
                        let mv = self.cfg.battery_mv;
                        self.reply(&format!("arr 16 {mv}"), seq);
                    }
                    17 => self.reply("arr 17 5750", seq),
                    8 | 12 => self.reply(&format!("arr {pin} 0"), seq),
                    _ => self.reply("are 19", seq),
                }
            }
            'o' => {
                let (flags, baud) = (args[0] as u32, args[1] as u32);
                if !(3..=9).contains(&proto) {
                    self.reply("are 3", seq);
                    return;
                }
                if let std::collections::btree_map::Entry::Vacant(e) = self.channels.entry(proto) {
                    e.insert(Channel { flags });
                    self.with_log(|l| {
                        l.opened.push((proto, flags, baud));
                        l.open_channels.push(proto);
                    });
                    self.reply("aro", seq);
                } else {
                    self.reply("are 20", seq);
                }
            }
            _ => {}
        }
    }

    fn injected_error(&self, name: &str) -> Option<u32> {
        self.cfg
            .faults
            .error_reply
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, c)| *c)
    }

    fn execute_with_payload(&mut self, name: &str, args: &[i64], seq: Option<i64>, payload: &[u8]) {
        if name == "<silent>" {
            return;
        }
        let proto = *args.last().unwrap_or(&0) as u8;
        if let Some(code) = self.injected_error(name) {
            self.reply(&format!("are {code}"), seq);
            return;
        }
        if name.starts_with("atf") {
            self.filter_cmd(proto, args, seq, payload);
        } else if name.starts_with("att") {
            self.transmit_cmd(proto, args, seq, payload);
        }
    }

    fn filter_cmd(&mut self, proto: u8, args: &[i64], seq: Option<i64>, payload: &[u8]) {
        let (ftype, txflags, len) = (args[0] as u32, args[1] as u32, args[2] as usize);
        if !self.channels.contains_key(&proto) {
            self.reply("are 2", seq);
            return;
        }
        if !(filter::PASS..=filter::FLOW_CONTROL).contains(&ftype) {
            self.reply("are 22", seq);
            return;
        }
        let id = self.next_filter_id;
        self.next_filter_id += 1;
        let rec = FilterRec {
            id,
            proto,
            filter_type: ftype,
            txflags,
            mask: payload[..len].to_vec(),
            pattern: payload[len..2 * len].to_vec(),
        };
        self.with_log(|l| l.filters.push(rec));
        self.reply(&format!("arf{proto} {id}"), seq);
    }

    fn transmit_cmd(&mut self, proto: u8, args: &[i64], seq: Option<i64>, payload: &[u8]) {
        let (txflags, budget) = (args[1] as u32, args[2] as u32);
        let Some(seq) = seq else {
            return; // an unnumbered att gets no reply at all (bisak §4)
        };
        if !self.channels.contains_key(&proto) {
            self.reply("are 3", Some(seq));
            return;
        }
        if budget == 0 || self.cfg.faults.bus_no_ack {
            self.reply("are 9", Some(seq));
            return;
        }
        if payload.len() < 4 || (proto == PROTO_CAN && payload.len() > 12) {
            self.reply("are 10", Some(seq));
            return;
        }
        let ext = txflags & CAN_29BIT_ID != 0;
        let raw_id = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
        let id = if ext {
            raw_id & 0x1FFF_FFFF
        } else {
            raw_id & 0x7FF
        };
        let data = payload[4..].to_vec();
        self.with_log(|l| {
            l.transmits.push((proto, txflags, budget));
            l.bus.push(BusFrame {
                from_host: true,
                id,
                data: data.clone(),
            });
        });
        let answers = if id == self.cfg.ecu_request_id {
            self.ecu_on_frame(&data)
        } else {
            Vec::new()
        };
        let id_bit = if ext { status::ID_29BIT } else { 0 };
        if self.cfg.faults.loopback_echo {
            self.record(proto, status::LOOPBACK, 0, &[]);
        }
        if self.cfg.faults.frames_before_ack {
            self.deliver_all(&answers);
            self.reply("aro", Some(seq));
            self.record(proto, status::TX_INDICATION | id_bit, id, &[]);
        } else {
            self.reply("aro", Some(seq));
            self.record(proto, status::TX_INDICATION | id_bit, id, &[]);
            self.deliver_all(&answers);
        }
        for (nid, ndata) in self.cfg.faults.noise.clone() {
            self.deliver(nid, &ndata, false);
        }
    }

    fn deliver_all(&mut self, frames: &[Vec<u8>]) {
        let rid = self.cfg.ecu_response_id;
        for f in frames {
            self.deliver(rid, f, true);
        }
    }

    /// A frame from another node: logged on the bus, then passed to the host
    /// through the raw-CAN channel's filters.
    fn deliver(&mut self, id: u32, data: &[u8], log: bool) {
        if log {
            self.with_log(|l| {
                l.bus.push(BusFrame {
                    from_host: false,
                    id,
                    data: data.to_vec(),
                })
            });
        }
        let Some(ch) = self.channels.get(&PROTO_CAN) else {
            return;
        };
        let ext_channel = ch.flags & CAN_29BIT_ID != 0;
        let filters = self
            .log
            .lock()
            .map(|l| l.filters.clone())
            .unwrap_or_default();
        let idb = id.to_be_bytes();
        let hit = |f: &FilterRec| {
            f.proto == PROTO_CAN
                && f.mask.len() == 4
                && (0..4).all(|i| idb[i] & f.mask[i] == f.pattern[i] & f.mask[i])
        };
        let passed = filters
            .iter()
            .any(|f| f.filter_type == filter::PASS && hit(f));
        let blocked = filters
            .iter()
            .any(|f| f.filter_type == filter::BLOCK && hit(f));
        if passed && !blocked {
            let st = if ext_channel || id > 0x7FF {
                status::ID_29BIT
            } else {
                0
            };
            self.record(PROTO_CAN, st, id, data);
        }
    }

    fn pad(&self, mut f: Vec<u8>) -> Vec<u8> {
        f.resize(8, self.cfg.ecu_pad);
        f
    }

    /// The ECU receives one CAN frame; returns the frames it sends back.
    fn ecu_on_frame(&mut self, d: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let Some(&pci) = d.first() else {
            return out;
        };
        match pci & 0xF0 {
            0x00 => {
                let len = (pci & 0x0F) as usize;
                if len >= 1 && len < d.len() {
                    self.tp.rx = None;
                    self.ecu_request(&d[1..1 + len], &mut out);
                }
            }
            0x10 if d.len() >= 8 => {
                let len = (((pci & 0x0F) as usize) << 8) | d[1] as usize;
                self.tp.rx = Some((len, d[2..8].to_vec(), 1));
                let bs = self.cfg.ecu_block_size;
                out.push(self.pad(vec![0x30, bs, 0x00]));
            }
            0x20 => {
                if let Some((len, mut buf, sn)) = self.tp.rx.take() {
                    if pci & 0x0F != sn {
                        return out; // sequence error: the ECU drops the message
                    }
                    let take = (len - buf.len()).min(d.len() - 1);
                    buf.extend_from_slice(&d[1..1 + take]);
                    if buf.len() >= len {
                        self.ecu_request(&buf, &mut out);
                    } else {
                        // A further FC after each block of `ecu_block_size` CFs.
                        let bs = self.cfg.ecu_block_size;
                        let received_cfs = (buf.len() - 6).div_ceil(7);
                        self.tp.rx = Some((len, buf, (sn + 1) & 0x0F));
                        if bs != 0 && received_cfs % bs as usize == 0 {
                            out.push(self.pad(vec![0x30, bs, 0x00]));
                        }
                    }
                }
            }
            // Only ClearToSend (0x30) releases the next block.
            0x30 if pci == 0x30 => {
                let bs = d.get(1).copied().unwrap_or(0);
                self.ecu_send_block(bs, &mut out);
            }
            _ => {}
        }
        out
    }

    fn ecu_request(&mut self, req: &[u8], out: &mut Vec<Vec<u8>>) {
        let answers = (self.ecu)(req);
        self.tp.queue.extend(answers);
        self.ecu_pump(out);
    }

    fn ecu_pump(&mut self, out: &mut Vec<Vec<u8>>) {
        while self.tp.tx.is_none() {
            let Some(r) = self.tp.queue.pop_front() else {
                return;
            };
            if r.is_empty() {
                continue;
            }
            if r.len() <= 7 {
                let mut f = vec![r.len() as u8];
                f.extend_from_slice(&r);
                out.push(self.pad(f));
            } else {
                let len = r.len().min(0xFFF);
                let mut f = vec![0x10 | (len >> 8) as u8, len as u8];
                f.extend_from_slice(&r[..6]);
                out.push(f);
                self.tp.tx = Some((r[..len].to_vec(), 6, 1));
            }
        }
    }

    fn ecu_send_block(&mut self, bs: u8, out: &mut Vec<Vec<u8>>) {
        let Some((data, mut idx, mut sn)) = self.tp.tx.take() else {
            return;
        };
        let mut sent = 0u32;
        while idx < data.len() {
            let end = (idx + 7).min(data.len());
            let mut f = vec![0x20 | sn];
            f.extend_from_slice(&data[idx..end]);
            out.push(self.pad(f));
            idx = end;
            sn = (sn + 1) & 0x0F;
            sent += 1;
            if bs != 0 && sent >= bs as u32 && idx < data.len() {
                self.tp.tx = Some((data, idx, sn));
                return;
            }
        }
        self.ecu_pump(out);
    }
}

/// Bytes of binary payload a command line announces; `usize::MAX` for a
/// malformed length; `None` for commands without payload.
fn payload_len(verb: char, args: &[i64]) -> Option<usize> {
    match verb {
        't' => {
            let n = args[0];
            Some(if (1..=4128).contains(&n) {
                n as usize
            } else {
                usize::MAX
            })
        }
        'f' => {
            let (ftype, n) = (args[0], args[2]);
            if !(1..=12).contains(&n) {
                return Some(usize::MAX);
            }
            let msgs = if ftype == filter::FLOW_CONTROL as i64 {
                3
            } else {
                2
            };
            Some(n as usize * msgs)
        }
        _ => None,
    }
}

/// How [`FakePort`] hands the device's output to the driver.
#[derive(Debug, Clone)]
pub enum Chunking {
    /// Everything available in one read (merged messages).
    Whole,
    /// At most this many bytes per read (split messages).
    Bytes(usize),
    /// Cycle through these read sizes.
    Pattern(Vec<usize>),
}

/// The fake as an in-process [`Op2Port`].
pub struct FakePort {
    dev: FakeOp2,
    chunking: Chunking,
    step: usize,
}

impl FakePort {
    pub fn new(dev: FakeOp2, chunking: Chunking) -> Self {
        Self {
            dev,
            chunking,
            step: 0,
        }
    }
}

impl Op2Port for FakePort {
    fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        self.dev.feed(bytes);
        Ok(())
    }

    fn read_some(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        if !self.dev.has_output() {
            // Nothing will appear until the host writes: this read times out.
            std::thread::sleep(timeout.min(Duration::from_secs(2)));
            return Ok(0);
        }
        let want = match &self.chunking {
            Chunking::Whole => buf.len(),
            Chunking::Bytes(n) => (*n).max(1),
            Chunking::Pattern(p) => {
                let n = p.get(self.step % p.len().max(1)).copied().unwrap_or(1);
                self.step += 1;
                n.max(1)
            }
        };
        let chunk = self.dev.take_up_to(want.min(buf.len()));
        buf[..chunk.len()].copy_from_slice(&chunk);
        Ok(chunk.len())
    }
}

/// Run the fake on the far end of a byte stream (e.g. a pty master) until
/// `stop` is set or the stream fails. The stream should have a short read
/// timeout so `stop` is noticed.
pub fn serve<S: Read + Write>(mut dev: FakeOp2, mut io: S, stop: Arc<AtomicBool>) {
    let mut buf = [0u8; 512];
    while !stop.load(Ordering::Relaxed) {
        match io.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => dev.feed(&buf[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(_) => return,
        }
        if dev.has_output() {
            let out = dev.take_output();
            if io.write_all(&out).and_then(|_| io.flush()).is_err() {
                return;
            }
        }
    }
}
