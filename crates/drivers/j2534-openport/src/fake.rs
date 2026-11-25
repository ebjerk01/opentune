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
//! the CLI end-to-end test).
//!
//! K-line (channels 3 and 4): `ats` configuration, `aty` fast init (answered
//! `ary<ch> <n> <seq>` + the StartCommunication response), `atw` five-baud
//! init (answered `arw<ch> <kb1> <kb2> <seq>`), and `att` messages without a
//! CAN id, answered by a **synthetic** KWP2000 ECU ([`synthetic_kwp_ecu`]:
//! made-up identity strings, no real ECU data) only after a successful init.
//! Answers arrive as K-line records: data without timestamp between 4-byte
//! START/END markers ([`KLineRxLayout`] varies that), checksum appended by
//! the host (`ISO9141_NO_CHECKSUM`) or by the cable. These K-line replies are
//! modelled from the public notes; no cable capture of one exists. Which
//! request headers the ECU answers is configurable ([`KLineEcuHeader`]:
//! any addressed header, an OBD-style ECU that answers functional requests
//! to 0x33 only after a five-baud init at 0x33, or the 1-byte header without
//! address), as is a length-byte-only ECU.
//!
//! [`FakeConfig::bus_traffic`] puts frames from other nodes on the CAN bus
//! as soon as a raw-CAN filter is installed, for receive-only capture tests.
use crate::port::Op2Port;
use crate::proto::{
    filter, param, status, CAN_29BIT_ID, ISO9141_NO_CHECKSUM, PROTO_CAN, PROTO_ISO14230,
    PROTO_ISO9141,
};
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
    /// The K-line ECU never answers: inits fail `are 7`, requests time out.
    pub kline_silent_ecu: bool,
    /// Answer `aty`/`atw` without the sequence number (the one `arw` seen
    /// on a cable was unnumbered, bisak §3).
    pub kline_unnumbered_init: bool,
    /// Before every K-line answer, a frame for another tester address.
    pub kline_foreign_frame: bool,
}

/// How the fake delivers a K-line message to the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KLineRxLayout {
    /// START marker, one data record, END marker (the vendor DLL's reading).
    #[default]
    StartDataEnd,
    /// One data record, no markers.
    DataOnly,
    /// START, the data split over several 3-byte records, END.
    Fragmented,
}

/// Which request headers the fake's K-line ECU answers (ISO 14230-2 format
/// byte address mode A1A0); every other request is ignored, as a real ECU
/// ignores a header it does not expect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KLineEcuHeader {
    /// Any header with address information (physical or functional) to
    /// `kline_ecu_addr`; answered physically. The fake's original behaviour.
    #[default]
    AnyAddressed,
    /// An OBD-style ECU: a five-baud init at this functional address wakes
    /// it for functional requests (`0xC0 | len`) to that address only; an
    /// init at `kline_ecu_addr` (five-baud or fast) for physical requests
    /// (`0x80 | len`) to `kline_ecu_addr` only. Answers are physical, to the
    /// request's source (ISO 14230-4:2000 §4.2).
    Obd(u8),
    /// Only the 1-byte header without address information (`0x00 | len`),
    /// answered the same way.
    NoAddress,
    /// An ECU that wakes as a legislated ISO 9141-2 OBD participant: only a
    /// five-baud init at 0x33 wakes it (set `kline_key_bytes` to 08 08 or
    /// 94 94), no fast init; afterwards it answers only ISO 9141-2 OBD
    /// requests `68 6A <tester> <mode> …` ([`synthetic_obd_ecu`]) with
    /// `48 6B <source> …`, and stays silent to every KWP2000 header.
    Iso9141Obd { source: u8 },
}

/// The fake's synthetic ISO 9141-2 OBD answers: mode 01 PID 00 (supported
/// PIDs 01–20, a made-up bitmap) only; anything else gets no answer.
pub fn synthetic_obd_ecu(req: &[u8]) -> Vec<Vec<u8>> {
    match req {
        [0x01, 0x00] => vec![vec![0x41, 0x00, 0xBE, 0x1F, 0xA8, 0x13]],
        _ => Vec::new(),
    }
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
    /// K-line ECU address (KWP2000 header and five-baud address).
    pub kline_ecu_addr: u8,
    /// Key bytes the K-line ECU reports at init.
    pub kline_key_bytes: (u8, u8),
    /// With the cable doing the checksum (flags 0): whether received K-line
    /// data still carries it (the sources do not say).
    pub kline_rx_checksum: bool,
    pub kline_rx_layout: KLineRxLayout,
    /// Which request headers the K-line ECU answers.
    pub kline_ecu_header: KLineEcuHeader,
    /// The K-line ECU ignores requests with the length in the format byte
    /// and answers with a separate length byte (a KB1 with AL1 only).
    pub kline_len_byte_only: bool,
    /// Frames other nodes put on the CAN bus once a raw-CAN filter is set
    /// (id, data); delivered through the filters, after one transmit
    /// indication and one bare START record that a receiver must drop.
    pub bus_traffic: Vec<(u32, Vec<u8>)>,
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
            kline_ecu_addr: 0x01,
            kline_key_bytes: (0xEF, 0x8F),
            kline_rx_checksum: true,
            kline_rx_layout: KLineRxLayout::default(),
            kline_ecu_header: KLineEcuHeader::default(),
            kline_len_byte_only: false,
            bus_traffic: Vec::new(),
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
    /// `(proto, param, value)` of every accepted `ats`.
    pub config: Vec<(u8, u32, u32)>,
    /// K-line inits that woke the ECU: `fast <ch> <addr>` / `5baud <ch> <addr>`.
    pub inits: Vec<String>,
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
    loopback: bool,
}

fn is_kline(proto: u8) -> bool {
    proto == PROTO_ISO14230 || proto == PROTO_ISO9141
}

fn kchecksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |a, &b| a.wrapping_add(b))
}

/// A host K-line message as the fake ECU sees it.
struct KMsg {
    /// Format byte bits A1A0 in place (`0x00`, `0x40`, `0x80`, `0xC0`).
    mode: u8,
    target: Option<u8>,
    source: Option<u8>,
    /// The length came in a separate byte.
    len_byte: bool,
    data: Vec<u8>,
}

/// A synthetic KWP2000 ECU for the fake K-line: made-up identity strings and
/// a made-up memory pattern, nothing from a real unit. Answers
/// readEcuIdentification (0x1A 0x90/0x91/0x94/0x97), readDataByLocalId
/// (0x21), readMemoryByAddress (0x23, `addr ^ (addr >> 8)` bytes),
/// startDiagnosticSession (0x10), testerPresent (0x3E) and
/// StopCommunication (0x82); every other service gets NRC 0x11.
pub fn synthetic_kwp_ecu() -> EcuHandler {
    Box::new(|req: &[u8]| {
        let pos = |sid: u8, data: &[u8]| {
            let mut r = vec![sid + 0x40];
            r.extend_from_slice(data);
            vec![r]
        };
        match req {
            [0x1A, opt] => {
                let text: &[u8] = match opt {
                    0x90 => b"OTKLINEFAKE000001",
                    0x91 => b"OT-HW-KL01",
                    0x94 => b"OT-SW-KL01",
                    0x97 => b"SYNTH KWP ECU",
                    _ => return vec![vec![0x7F, 0x1A, 0x12]],
                };
                let mut d = vec![*opt];
                d.extend_from_slice(text);
                pos(0x1A, &d)
            }
            [0x21, id] => pos(0x21, &[*id, 0x12, 0x34, *id ^ 0xFF]),
            [0x23, a, b, c, n] => {
                let base = u32::from_be_bytes([0, *a, *b, *c]);
                let data: Vec<u8> = (0..*n as u32)
                    .map(|i| {
                        let x = base + i;
                        (x ^ (x >> 8)) as u8
                    })
                    .collect();
                pos(0x23, &data)
            }
            [0x10, s] => pos(0x10, &[*s]),
            [0x3E, ..] => pos(0x3E, &[]),
            [0x82] => pos(0x82, &[]),
            [sid, ..] => vec![vec![0x7F, *sid, 0x11]],
            [] => Vec::new(),
        }
    })
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
    kline_ecu: EcuHandler,
    /// The K-line ECU answers only after an init woke it.
    kline_awake: bool,
    /// [`KLineEcuHeader::Obd`]: woken at the functional address.
    kline_functional: bool,
}

impl FakeOp2 {
    /// Replace the K-line ECU (default: [`synthetic_kwp_ecu`]). The handler
    /// sees service id + data and returns the payloads to answer with.
    pub fn set_kline_ecu(&mut self, ecu: EcuHandler) {
        self.kline_ecu = ecu;
    }

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
                kline_ecu: synthetic_kwp_ecu(),
                kline_awake: false,
                kline_functional: false,
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
            'o' | 'c' | 't' | 'f' | 'g' | 's' | 'k' | 'm' | 'n' | 'l' | 'y' | 'w'
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
            'r' | 'w' => 1,
            's' | 'y' => 2,
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
                self.kline_awake = false;
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
                if is_kline(proto) {
                    self.kline_awake = false;
                }
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
                    e.insert(Channel {
                        flags,
                        loopback: false,
                    });
                    self.with_log(|l| {
                        l.opened.push((proto, flags, baud));
                        l.open_channels.push(proto);
                    });
                    self.reply("aro", seq);
                } else {
                    self.reply("are 20", seq);
                }
            }
            's' => self.set_config_cmd(proto, args[0] as u32, args[1] as u32, seq),
            'w' => self.five_baud_cmd(proto, args[0], seq),
            _ => {}
        }
    }

    fn set_config_cmd(&mut self, proto: u8, p: u32, value: u32, seq: Option<i64>) {
        let Some(ch) = self.channels.get_mut(&proto) else {
            self.reply("are 2", seq);
            return;
        };
        // What the cable accepted in the sweep (bisak §8.1).
        let ok = if is_kline(proto) {
            p != param::DATA_RATE && param::KLINE.contains(&p)
        } else {
            matches!(p, 3 | 23 | 24 | 30 | 31 | 34 | 35 | 37)
        };
        if !ok {
            self.reply("are 1", seq);
            return;
        }
        if p == param::LOOPBACK {
            ch.loopback = value != 0;
        }
        self.with_log(|l| l.config.push((proto, p, value)));
        self.reply("aro", seq);
    }

    /// The init reply's trailing number, unless the fault drops it.
    fn init_seq(&self, seq: Option<i64>) -> Option<i64> {
        if self.cfg.faults.kline_unnumbered_init {
            None
        } else {
            seq
        }
    }

    fn five_baud_cmd(&mut self, proto: u8, addr: i64, seq: Option<i64>) {
        if !is_kline(proto) || !self.channels.contains_key(&proto) {
            self.reply("are 2", seq);
            return;
        }
        let physical = addr == self.cfg.kline_ecu_addr as i64;
        let (wakes, functional) = match self.cfg.kline_ecu_header {
            KLineEcuHeader::Obd(fa) => (physical || addr == fa as i64, addr == fa as i64),
            KLineEcuHeader::Iso9141Obd { .. } => (addr == 0x33, true),
            _ => (physical, false),
        };
        if self.cfg.faults.kline_silent_ecu || !wakes {
            self.reply("are 7", seq);
            return;
        }
        self.kline_awake = true;
        self.kline_functional = functional;
        self.with_log(|l| l.inits.push(format!("5baud {proto} {addr}")));
        let (kb1, kb2) = self.cfg.kline_key_bytes;
        let s = self.init_seq(seq);
        self.reply(&format!("arw{proto} {kb1} {kb2}"), s);
    }

    fn host_checksum(&self, proto: u8) -> bool {
        self.channels
            .get(&proto)
            .is_some_and(|c| c.flags & ISO9141_NO_CHECKSUM != 0)
    }

    /// A host K-line message in any ISO 14230-2 header form, checksum
    /// checked when the host frames it. `None` for a malformed message.
    fn kline_parse(&self, proto: u8, msg: &[u8]) -> Option<KMsg> {
        let core = if self.host_checksum(proto) {
            let (cs, core) = msg.split_last()?;
            (kchecksum(core) == *cs).then_some(core)?
        } else {
            msg
        };
        let fmt = *core.first()?;
        let mode = fmt & 0xC0;
        let mut hdr = if mode == 0 { 1 } else { 3 };
        let (len, len_byte) = match (fmt & 0x3F) as usize {
            0 => {
                let l = *core.get(hdr)? as usize;
                hdr += 1;
                (l, true)
            }
            n => (n, false),
        };
        (len > 0 && core.len() == hdr + len).then(|| KMsg {
            mode,
            target: (mode != 0).then(|| core[1]),
            source: (mode != 0).then(|| core[2]),
            len_byte,
            data: core[hdr..].to_vec(),
        })
    }

    /// Whether the ECU, as woken, answers a request in this header form.
    fn kline_accepts(&self, m: &KMsg) -> bool {
        if self.cfg.kline_len_byte_only && !m.len_byte {
            return false;
        }
        let ecu = self.cfg.kline_ecu_addr;
        match self.cfg.kline_ecu_header {
            KLineEcuHeader::AnyAddressed => m.mode != 0 && m.target == Some(ecu),
            KLineEcuHeader::Obd(fa) if self.kline_functional => {
                m.mode == 0xC0 && m.target == Some(fa)
            }
            KLineEcuHeader::Obd(_) => m.mode == 0x80 && m.target == Some(ecu),
            KLineEcuHeader::NoAddress => m.mode == 0,
            KLineEcuHeader::Iso9141Obd { .. } => false,
        }
    }

    /// [`KLineEcuHeader::Iso9141Obd`]: an ISO 9141-2 OBD request
    /// `68 6A <tester> <data…>` (checksum checked in host mode) as (tester,
    /// data); `None` otherwise.
    fn iso9141_parse(&self, proto: u8, msg: &[u8]) -> Option<(u8, Vec<u8>)> {
        if !matches!(self.cfg.kline_ecu_header, KLineEcuHeader::Iso9141Obd { .. }) {
            return None;
        }
        let core = if self.host_checksum(proto) {
            let (cs, core) = msg.split_last()?;
            (kchecksum(core) == *cs).then_some(core)?
        } else {
            msg
        };
        (core.len() >= 4 && core[..2] == [0x68, 0x6A]).then(|| (core[2], core[3..].to_vec()))
    }

    /// An ECU answer framed for `tester` (physical, or without address in
    /// [`KLineEcuHeader::NoAddress`]), checksum per channel mode.
    fn kline_frame(&self, proto: u8, tester: u8, data: &[u8]) -> Vec<u8> {
        let cs = self.host_checksum(proto) || self.cfg.kline_rx_checksum;
        if let KLineEcuHeader::Iso9141Obd { source } = self.cfg.kline_ecu_header {
            let mut f = vec![0x48, 0x6B, source];
            f.extend_from_slice(data);
            if cs {
                f.push(kchecksum(&f));
            }
            return f;
        }
        let ecu = self.cfg.kline_ecu_addr;
        let n = data.len();
        let addr = self.cfg.kline_ecu_header != KLineEcuHeader::NoAddress;
        let len_byte = n > 0x3F || self.cfg.kline_len_byte_only;
        let mut f = vec![if addr { 0x80 } else { 0x00 } | if len_byte { 0 } else { n as u8 }];
        if addr {
            f.extend_from_slice(&[tester, ecu]);
        }
        if len_byte {
            f.push(n as u8);
        }
        f.extend_from_slice(data);
        if self.host_checksum(proto) || self.cfg.kline_rx_checksum {
            f.push(kchecksum(&f));
        }
        f
    }

    fn raw_record(&mut self, proto: u8, st: u8, body: &[u8]) {
        let mut rec = vec![b'a', b'r', b'0' + proto, (body.len() + 1) as u8, st];
        rec.extend_from_slice(body);
        self.emit(&rec);
    }

    /// Deliver one K-line message through the channel's filters, in the
    /// configured record layout (`lb` = the loopback bit for an echo).
    fn kline_deliver(&mut self, proto: u8, msg: &[u8], lb: u8) {
        let filters = self
            .log
            .lock()
            .map(|l| l.filters.clone())
            .unwrap_or_default();
        let hit = |f: &FilterRec| {
            f.proto == proto
                && msg.len() >= f.mask.len()
                && (0..f.mask.len()).all(|i| msg[i] & f.mask[i] == f.pattern[i] & f.mask[i])
        };
        let passed = filters
            .iter()
            .any(|f| f.filter_type == filter::PASS && hit(f));
        let blocked = filters
            .iter()
            .any(|f| f.filter_type == filter::BLOCK && hit(f));
        if !passed || blocked {
            return;
        }
        let ts = self.tick().to_be_bytes();
        match self.cfg.kline_rx_layout {
            KLineRxLayout::DataOnly => self.raw_record(proto, lb, msg),
            layout => {
                self.raw_record(proto, status::START | lb, &ts);
                let chunk = if layout == KLineRxLayout::Fragmented {
                    3
                } else {
                    250
                };
                for c in msg.chunks(chunk) {
                    self.raw_record(proto, lb, c);
                }
                let ts = self.tick().to_be_bytes();
                self.raw_record(proto, status::END | lb, &ts);
            }
        }
    }

    /// `aty<ch> <len> 0` + request: wake-up pattern, then StartCommunication.
    fn fast_init_cmd(&mut self, proto: u8, seq: Option<i64>, payload: &[u8]) {
        if !is_kline(proto) || !self.channels.contains_key(&proto) {
            self.reply("are 2", seq);
            return;
        }
        let parsed = self.kline_parse(proto, payload);
        let ecu = self.cfg.kline_ecu_addr;
        // StartCommunication: physical, to the ECU's own address.
        let ok = matches!(&parsed, Some(m) if m.mode == 0x80 && m.target == Some(ecu)
            && m.data.as_slice() == [0x81]);
        let iso9141 = matches!(self.cfg.kline_ecu_header, KLineEcuHeader::Iso9141Obd { .. });
        // `aty<ch> 0 0` is the wake-up alone: no answer to wait for.
        if payload.is_empty() || !ok || iso9141 || self.cfg.faults.kline_silent_ecu {
            self.reply("are 7", seq);
            return;
        }
        let tester = parsed.and_then(|m| m.source).unwrap_or(0xF1);
        self.kline_awake = true;
        self.kline_functional = false;
        self.with_log(|l| l.inits.push(format!("fast {proto} {ecu}")));
        let (kb1, kb2) = self.cfg.kline_key_bytes;
        let resp = self.kline_frame(proto, tester, &[0xC1, kb1, kb2]);
        let s = self.init_seq(seq);
        self.reply(&format!("ary{proto} {}", resp.len()), s);
        self.emit(&resp);
    }

    /// `att<ch>` on K-line: the message itself, no CAN id, no transmit
    /// indication (bisak §7.9).
    fn kline_transmit_cmd(&mut self, proto: u8, txflags: u32, budget: u32, seq: i64, msg: &[u8]) {
        let iso = self.iso9141_parse(proto, msg);
        let parsed = if iso.is_some() {
            None
        } else {
            self.kline_parse(proto, msg)
        };
        let id = match (&iso, &parsed) {
            (Some(_), _) => 0x6A,
            (_, Some(m)) => m.target.map_or(0, u32::from),
            _ => 0,
        };
        self.with_log(|l| {
            l.transmits.push((proto, txflags, budget));
            l.bus.push(BusFrame {
                from_host: true,
                id,
                data: msg.to_vec(),
            });
        });
        let mut answers = Vec::new();
        let ecu = self.cfg.kline_ecu_addr;
        let mut tester = 0xF1;
        if let Some((t, data)) = iso {
            tester = t;
            if self.kline_awake && !self.cfg.faults.kline_silent_ecu {
                answers = synthetic_obd_ecu(&data);
            }
        } else if let Some(m) = parsed {
            tester = m.source.unwrap_or(0xF1);
            if self.kline_awake && !self.cfg.faults.kline_silent_ecu && self.kline_accepts(&m) {
                answers = (self.kline_ecu)(&m.data);
            }
        }
        let frames: Vec<Vec<u8>> = answers
            .iter()
            .map(|a| self.kline_frame(proto, tester, a))
            .collect();
        if self.channels.get(&proto).is_some_and(|c| c.loopback) {
            let echo = msg.to_vec();
            self.kline_deliver(proto, &echo, status::LOOPBACK);
        }
        let deliver_all = |me: &mut Self| {
            for f in &frames {
                if me.cfg.faults.kline_foreign_frame {
                    let other = me.kline_frame(proto, tester ^ 0x01, &[0x7F, 0x3E, 0x11]);
                    me.kline_deliver(proto, &other, 0);
                }
                me.with_log(|l| {
                    l.bus.push(BusFrame {
                        from_host: false,
                        id: ecu as u32,
                        data: f.clone(),
                    })
                });
                me.kline_deliver(proto, f, 0);
            }
        };
        if self.cfg.faults.frames_before_ack {
            deliver_all(self);
            self.reply("aro", Some(seq));
        } else {
            self.reply("aro", Some(seq));
            deliver_all(self);
        }
    }

    /// Other nodes' traffic, once a raw-CAN filter exists: first a transmit
    /// indication and a bare START (both must be dropped by a receiver).
    fn emit_bus_traffic(&mut self) {
        let traffic = std::mem::take(&mut self.cfg.bus_traffic);
        if traffic.is_empty() {
            return;
        }
        self.record(PROTO_CAN, status::TX_INDICATION, 0x7DF, &[]);
        self.record(PROTO_CAN, status::START, 0x7DF, &[]);
        for (id, data) in &traffic {
            self.deliver(*id, data, true);
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
        } else if name.starts_with("aty") {
            self.fast_init_cmd(proto, seq, payload);
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
        if proto == PROTO_CAN {
            self.emit_bus_traffic();
        }
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
        if is_kline(proto) {
            self.kline_transmit_cmd(proto, txflags, budget, seq, payload);
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
        'y' => {
            let n = args[0];
            Some(if (0..=255).contains(&n) {
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
