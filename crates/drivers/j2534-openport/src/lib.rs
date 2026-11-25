//! Tactrix OpenPort 2.0 driver for OpenTune.
//!
//! The OpenPort 2.0 (OP2) enumerates as a USB CDC-ACM serial device
//! (`/dev/ttyACM*`, USB id `0403:cc4d`), so it needs no kernel driver on Linux.
//! It speaks a line-oriented ASCII command protocol with binary payloads and
//! binary receive records, documented publicly by bisak/openport-j2534
//! (GPL-3.0), Mackanized/op2j2534 and roffe/libj2534_openport2 (Apache-2.0);
//! the codec is in [`proto`] with the source of each fact.
//!
//! This driver opens the OP2's **raw CAN** channel (J2534 protocol 5) and
//! implements [`CanLink`]; ISO-TP segmentation, UDS and the read/write flows
//! come from `opentune-core` ([`IsoTp`] + [`UdsClient`]). The cable's own
//! ISO 15765 channel (6) is not used.
//!
//! **K-line** (read path only): with `bus=kline` (or `iso9141`), or a
//! connection whose protocol is ISO 14230 / ISO 9141, it opens the cable's
//! K-line channel 4 (or 3) instead, wakes the ECU (fast or five-baud init,
//! done by the cable) and exposes KWP2000 framing as a [`DiagTransport`], so
//! the core `Kwp2000Client` and `identify` run on it as over the `kkl`
//! driver. See [`kline`] for the options and the wire use. Writes are refused
//! on K-line, and every request is checked against a read-only allow-list
//! ([`kline::refuse_non_read`], over the shared
//! `opentune_core::protocol::readonly` list) before anything is sent.
//!
//! **Capture**: [`Interface::can_monitor`] opens raw CAN receive-only (a
//! pass-all filter; the monitor has no transmit path), see [`monitor`].
//!
//! Session, per the public notes:
//! 1. `\r\n\r\n` (ends any half line a previous session left), `ati`
//!    (identify, unnumbered), `atz`, `ata`;
//! 2. `ato5 <flags> <baud> 0` (29-bit ids set `CAN_29BIT_ID`);
//! 3. `atf5 1 <txflags> 4` + mask + pattern: a PASS filter on the response id
//!    (the cable delivers nothing without a filter);
//! 4. per frame `att5 <4+n> <txflags> <budget_us>` + id + data, answered
//!    `aro <seq>` and followed by a `0x10` transmit indication (dropped);
//! 5. `atc5`, `ata` on close.
//!
//! Every command carries a sequence number; replies are matched on it and
//! stale ones dropped. Frames that arrive while a command waits for its reply
//! are queued, not lost.
//!
//! Config ([`ConnectConfig::options`]):
//! - `port`: serial device path (default `/dev/ttyACM0`).
//! - `bitrate`: CAN bit rate, overriding [`ConnectConfig::bitrate`]; one of
//!   50k, 83.333k, 100k, 125k, 250k, 500k or 1M (the cable does not check the
//!   rate, and a wrong rate on a live bus can hang it, so it is checked here).
//! - `tx_timeout_ms`: per-frame transmit budget handed to the cable (default
//!   100, 1..=5000).
//!
//! **Hardware validation pending**: tested only against the offline fake in
//! [`fake`]. See `docs/DRIVERS.md` §3l for the audit and the checklist.

pub mod fake;
pub mod kline;
pub mod monitor;
pub mod port;
pub mod proto;

use kline::{KLineLink, KLineSettings, KLineState};
use opentune_core::protocol::diag::DiagTransport;
use opentune_core::protocol::isotp::{CanLink, IsoTp};
use opentune_core::protocol::kwp2000::Kwp2000Client;
use opentune_core::protocol::seedkey::SeedKey;
use opentune_core::protocol::uds::UdsClient;
use opentune_core::{
    Capabilities, ConnectConfig, DiagLayer, EcuId, Error, Interface, InterfaceInfo, MemoryRegion,
    Phase, Progress, Protocol, Result,
};
use port::{Op2Port, SerialOp2Port};
use proto::{Msg, RawFrame, Reply, CAN_29BIT_ID, PIN_VBAT, PROTO_CAN};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// CAN bit rates the driver will open (see the module docs).
pub const BITRATES: &[u32] = &[
    50_000, 83_333, 100_000, 125_000, 250_000, 500_000, 1_000_000,
];
/// Default per-frame transmit budget (ms). The vendor DLL uses 50 ms on raw
/// CAN when the caller passes 0; Mackanized recommends a ~100 ms floor.
const DEFAULT_TX_TIMEOUT_MS: u32 = 100;
/// Reply wait for ordinary commands. The vendor DLL waits 500 ms (5 s for
/// `atr`/`atg`); the margin covers USB scheduling on a busy host.
const CMD_TIMEOUT_MS: u64 = 1000;
/// Reply wait for `ato` and `atz`.
const SETUP_TIMEOUT_MS: u64 = 3000;
/// Reply wait for `atr`.
const PIN_TIMEOUT_MS: u64 = 5000;
/// Extra wait past the transmit budget: with nothing on the bus a 1 s budget
/// was measured to answer `are 9` after ~1.3 s.
const TX_REPLY_GRACE_MS: u64 = 1500;

/// Counters of what the session dropped, for diagnostics and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionStats {
    /// Replies whose sequence number matched no waiting command.
    pub stale_replies: u64,
    /// Transmit indications, loopback echoes and START indications dropped.
    pub indications: u64,
    /// Records for another channel, too short, or over 8 data bytes.
    pub foreign_frames: u64,
    /// Bytes skipped while resynchronising on the `ar` preamble.
    pub garbage_bytes: u64,
}

/// Per-connection settings resolved from the [`ConnectConfig`].
#[derive(Debug, Clone, Copy)]
struct Settings {
    bitrate: u32,
    tx_timeout_ms: u32,
    extended: bool,
}

impl Settings {
    fn from_cfg(cfg: &ConnectConfig) -> Result<Self> {
        if !matches!(cfg.protocol, Protocol::IsoTp | Protocol::RawCan) {
            return Err(Error::Unsupported(
                "OpenPort 2.0 driver supports CAN/ISO-TP protocols only".into(),
            ));
        }
        let bitrate = match cfg.options.get("bitrate") {
            Some(s) => s
                .trim()
                .parse::<u32>()
                .map_err(|_| Error::Config(format!("openport2: bitrate '{s}' is not a number")))?,
            None => cfg.bitrate,
        };
        if !BITRATES.contains(&bitrate) {
            return Err(Error::Config(format!(
                "openport2: CAN bit rate {bitrate} not supported (use one of {}); the cable \
                 does not check the rate and a wrong one on a live bus can hang it",
                BITRATES
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        let tx_timeout_ms = match cfg.options.get("tx_timeout_ms") {
            Some(s) => s
                .trim()
                .parse::<u32>()
                .ok()
                .filter(|v| (1..=5000).contains(v))
                .ok_or_else(|| {
                    Error::Config(format!(
                        "openport2: tx_timeout_ms '{s}' must be 1..=5000 (a zero budget never \
                         transmits)"
                    ))
                })?,
            None => DEFAULT_TX_TIMEOUT_MS,
        };
        if cfg.tx_id > 0x1FFF_FFFF || cfg.rx_id > 0x1FFF_FFFF {
            return Err(Error::Config(
                "openport2: CAN ids must fit in 29 bits".into(),
            ));
        }
        Ok(Self {
            bitrate,
            tx_timeout_ms,
            extended: cfg.tx_id > 0x7FF || cfg.rx_id > 0x7FF,
        })
    }
}

/// One OP2 session: owns the byte port, the reply parser, the sequence
/// counter and the queue of received CAN frames.
struct Session {
    port: Box<dyn Op2Port>,
    parser: proto::Parser,
    seq: u16,
    settings: Settings,
    rx: VecDeque<(u32, Vec<u8>)>,
    stats: SessionStats,
    /// Set when the session runs a K-line channel instead of raw CAN.
    kline: Option<KLineState>,
    /// A capture session: every command that could put anything on a bus
    /// is refused before it is written (see [`Session::command`]).
    receive_only: bool,
}

/// Command verbs that transmit on a vehicle bus or drive a pin: transmit,
/// periodic message, fast and five-baud init, programming voltage, and the
/// bootloader. A receive-only session refuses them unsent.
const BUS_WRITING_VERBS: &[&str] = &["att", "atm", "aty", "atw", "atv", "atx", "atp"];

impl Session {
    fn new(port: Box<dyn Op2Port>, settings: Settings) -> Self {
        Self {
            port,
            parser: proto::Parser::new(),
            seq: 0,
            settings,
            rx: VecDeque::new(),
            stats: SessionStats::default(),
            kline: None,
            receive_only: false,
        }
    }

    /// Next sequence number, wrapping 0xFFFF → 1 (never 0).
    fn next_seq(&mut self) -> u16 {
        self.seq = if self.seq == u16::MAX {
            1
        } else {
            self.seq + 1
        };
        self.seq
    }

    fn id_flags(&self) -> u32 {
        if self.settings.extended {
            CAN_29BIT_ID
        } else {
            0
        }
    }

    /// The next decoded message, or `None` once `deadline` passes.
    fn read_msg(&mut self, deadline: Instant) -> Result<Option<Msg>> {
        let mut buf = [0u8; 512];
        loop {
            if let Some(m) = self.parser.next() {
                self.stats.garbage_bytes = self.parser.garbage;
                return Ok(Some(m));
            }
            let now = Instant::now();
            if now >= deadline {
                self.stats.garbage_bytes = self.parser.garbage;
                return Ok(None);
            }
            let n = self.port.read_some(&mut buf, deadline - now)?;
            self.parser.push(&buf[..n]);
        }
    }

    /// File a binary record: CAN data for our channel goes to the rx queue,
    /// everything else is counted and dropped.
    fn accept_frame(&mut self, f: RawFrame) {
        if let Some(k) = self.kline.as_ref() {
            if f.proto == k.settings.proto {
                self.accept_kline(f);
            } else {
                self.stats.foreign_frames += 1;
            }
            return;
        }
        if f.proto != PROTO_CAN {
            self.stats.foreign_frames += 1;
            return;
        }
        // Transmit indications (0x10), loopback echoes (0x20) and START
        // indications without END (0x80) are not received data. Raw CAN
        // records normally carry status 0x00 (bisak §7.8).
        let st = f.status;
        if st & (proto::status::TX_INDICATION | proto::status::LOOPBACK) != 0
            || (st & proto::status::START != 0 && st & proto::status::END == 0)
        {
            self.stats.indications += 1;
            return;
        }
        match f.can() {
            // The id is the full 4-byte field (an 11-bit id has the upper
            // bits clear); the 0x02 status bit is not relied on.
            Some(c) if c.data.len() <= 8 => self.rx.push_back((c.id & 0x1FFF_FFFF, c.data)),
            _ => self.stats.foreign_frames += 1,
        }
    }

    /// Send `body <seq>\r\n` + `payload` in one write and wait for the reply
    /// carrying that sequence number. `are` becomes an error; frames that
    /// arrive meanwhile are queued; replies for other numbers are dropped.
    fn command(&mut self, body: &str, payload: &[u8], timeout_ms: u64) -> Result<Reply> {
        if self.receive_only && BUS_WRITING_VERBS.iter().any(|v| body.starts_with(v)) {
            return Err(Error::Unsupported(format!(
                "OpenPort 2.0 capture is receive-only: `{}` refused, nothing was sent",
                cmd_name(body)
            )));
        }
        let seq = self.next_seq();
        self.port.write_all(&proto::numbered(body, seq, payload))?;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            match self.read_msg(deadline)? {
                None => {
                    return Err(Error::Protocol(format!(
                        "OpenPort 2.0 did not answer `{}` within {timeout_ms} ms",
                        cmd_name(body)
                    )))
                }
                Some(Msg::Frame(f)) => self.accept_frame(f),
                Some(Msg::Reply(r)) if r.verb != b'i' && r.seq() == Some(seq) => {
                    if r.verb == b'e' {
                        return Err(device_error(body, &r));
                    }
                    return Ok(r);
                }
                Some(Msg::Reply(_)) => self.stats.stale_replies += 1,
            }
        }
    }

    /// `ati` (unnumbered, as the vendor DLL sends it) → the version text.
    fn identify(&mut self, timeout_ms: u64) -> Result<String> {
        self.port.write_all(b"ati\r\n")?;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            match self.read_msg(deadline)? {
                None => {
                    return Err(Error::Protocol(format!(
                        "no answer to `ati` within {timeout_ms} ms: is this an OpenPort 2.0 \
                         (and is the microSD card removed)?"
                    )))
                }
                Some(Msg::Reply(r)) if r.verb == b'i' => {
                    // `ari main code version : 1.17.4877` → `1.17.4877`.
                    let v = match r.text.rsplit_once(':') {
                        Some((_, v)) if !v.trim().is_empty() => v.trim().to_string(),
                        _ => r.text.clone(),
                    };
                    return Ok(v);
                }
                Some(Msg::Frame(f)) => self.accept_frame(f),
                Some(Msg::Reply(_)) => self.stats.stale_replies += 1,
            }
        }
    }

    fn open_can(&mut self) -> Result<()> {
        let body = proto::open_channel(PROTO_CAN, self.id_flags(), self.settings.bitrate);
        self.command(&body, &[], SETUP_TIMEOUT_MS).map(|_| ())
    }

    /// PASS filter on the response id. Required: the cable forwards nothing
    /// until a filter matches.
    fn set_pass_filter(&mut self, rx_id: u32) -> Result<()> {
        let mask: u32 = if self.settings.extended {
            0x1FFF_FFFF
        } else {
            0x7FF
        };
        let (body, payload) = proto::filter(
            PROTO_CAN,
            proto::filter::PASS,
            self.id_flags(),
            &mask.to_be_bytes(),
            &rx_id.to_be_bytes(),
            None,
        )
        .ok_or_else(|| Error::Protocol("invalid filter".into()))?;
        self.command(&body, &payload, CMD_TIMEOUT_MS).map(|_| ())
    }

    fn transmit(&mut self, id: u32, data: &[u8]) -> Result<()> {
        if data.len() > 8 {
            return Err(Error::Protocol(format!(
                "classic CAN frame of {} bytes (max 8)",
                data.len()
            )));
        }
        let budget_ms = self.settings.tx_timeout_ms;
        let txflags = if self.settings.extended || id > 0x7FF {
            CAN_29BIT_ID
        } else {
            0
        };
        let (body, payload) = proto::transmit(PROTO_CAN, id, data, txflags, budget_ms * 1000);
        self.command(&body, &payload, budget_ms as u64 + TX_REPLY_GRACE_MS)
            .map(|_| ())
    }

    fn recv(&mut self, timeout_ms: u64) -> Result<(u32, Vec<u8>)> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if let Some(f) = self.rx.pop_front() {
                return Ok(f);
            }
            match self.read_msg(deadline)? {
                None => return Err(Error::Timeout(timeout_ms)),
                Some(Msg::Frame(f)) => self.accept_frame(f),
                Some(Msg::Reply(_)) => self.stats.stale_replies += 1,
            }
        }
    }

    /// Battery voltage: `atr 16` → `arr 16 <mV> <seq>`.
    fn battery_mv(&mut self) -> Result<u32> {
        let r = self.command(&proto::read_pin(PIN_VBAT), &[], PIN_TIMEOUT_MS)?;
        match (r.verb, r.fields.as_slice()) {
            (b'r', [pin, mv, _seq]) if pin == "16" => mv
                .parse()
                .map_err(|_| Error::Protocol(format!("bad millivolts in `arr {}`", r.text))),
            _ => Err(Error::Protocol(format!(
                "unexpected reply to `atr 16`: ar{} {}",
                r.verb as char, r.text
            ))),
        }
    }

    fn close(&mut self) {
        let channel = self.kline.as_ref().map_or(PROTO_CAN, |k| k.settings.proto);
        let _ = self.command(&proto::close_channel(channel), &[], CMD_TIMEOUT_MS);
        // Close everything and switch outputs off, as the reference drivers do.
        let _ = self.command("ata", &[], CMD_TIMEOUT_MS);
    }
}

fn cmd_name(body: &str) -> &str {
    body.split_whitespace().next().unwrap_or(body)
}

fn device_error(body: &str, r: &Reply) -> Error {
    let cmd = cmd_name(body);
    let Some(code) = r.error_code() else {
        return Error::Protocol(format!("OpenPort 2.0 rejected `{cmd}`: are {}", r.text));
    };
    let name = proto::j2534_error_name(code);
    let hint = match (code, cmd.as_bytes().get(2)) {
        (9, Some(b't')) => {
            ": the frame was not acknowledged on the bus (ignition on? wiring, bit rate, \
             another node present?)"
        }
        (20, _) => ": channel already open (another program using the cable?)",
        (3, Some(b'o')) => ": protocol not supported by this cable/firmware",
        (7, Some(b'y' | b'w')) => {
            ": no answer from the ECU to the K-line init (K-line wired to OBD pin 7? \
             ignition / terminal 15 on? right address and init type?)"
        }
        (1, Some(b's')) => ": parameter not supported on this channel",
        _ => "",
    };
    Error::Protocol(format!(
        "OpenPort 2.0 rejected `{cmd}`: {name} (are {code}){hint}"
    ))
}

/// [`CanLink`] over a [`Session`] shared with the owning driver.
struct Op2Link {
    session: Rc<RefCell<Session>>,
}

impl CanLink for Op2Link {
    fn send_frame(&mut self, id: u32, data: &[u8]) -> Result<()> {
        self.session.borrow_mut().transmit(id, data)
    }

    fn recv_frame(&mut self, timeout_ms: u64) -> Result<(u32, Vec<u8>)> {
        self.session.borrow_mut().recv(timeout_ms)
    }
}

#[derive(Default)]
pub struct OpenPort2Driver {
    session: Option<Rc<RefCell<Session>>>,
    transport: Option<IsoTp>,
    /// The K-line link, when the session runs K-line instead of CAN.
    kline: Option<KLineLink>,
    /// Diagnostic layer spoken over the K-line link.
    kline_layer: Option<DiagLayer>,
    /// Receive-only raw-CAN capture ([`Interface::can_monitor`]).
    monitor: monitor::Op2Monitor,
    firmware: Option<String>,
    cfg: Option<ConnectConfig>,
    /// Optional seed/key handler for UDS security access (service 0x27). No OEM
    /// algorithm ships with OpenTune; the application supplies one.
    seed_key: Option<Box<dyn SeedKey>>,
}

impl OpenPort2Driver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install the seed/key handler used to unlock the ECU (service 0x27)
    /// before a download. Without it, writing a security-protected ECU fails
    /// with a clear error. OpenTune ships no OEM algorithms.
    pub fn set_seed_key(&mut self, seed_key: Box<dyn SeedKey>) {
        self.seed_key = Some(seed_key);
    }

    /// Open over an already-connected byte port (the offline fake in tests,
    /// or any transport that carries the OP2 protocol) instead of the serial
    /// device named by the `port` option.
    pub fn open_with_port(&mut self, port: Box<dyn Op2Port>, cfg: &ConnectConfig) -> Result<()> {
        if let Some(k) = kline_settings(cfg)? {
            return self.open_kline_with_port(port, cfg, k);
        }
        let settings = Settings::from_cfg(cfg)?;
        if self.session.is_some() {
            self.close()?;
        }
        let mut s = Session::new(port, settings);
        // End any half line a previous session left (bisak §9.1).
        s.port.write_all(b"\r\n\r\n")?;
        let firmware = s.identify(1500)?;
        let setup = (|| -> Result<()> {
            s.command("atz", &[], SETUP_TIMEOUT_MS)?;
            s.command("ata", &[], CMD_TIMEOUT_MS)?;
            s.open_can()?;
            s.set_pass_filter(cfg.rx_id)
        })();
        if let Err(e) = setup {
            let _ = s.command("ata", &[], CMD_TIMEOUT_MS);
            return Err(e);
        }
        // Anything received before the filter existed is stale.
        s.rx.clear();

        let shared = Rc::new(RefCell::new(s));
        let link = Op2Link {
            session: Rc::clone(&shared),
        };
        let isotp = IsoTp::new(Box::new(link), cfg.tx_id, cfg.rx_id);

        self.firmware = Some(firmware);
        self.session = Some(shared);
        self.transport = Some(isotp);
        self.cfg = Some(cfg.clone());
        Ok(())
    }

    /// The K-line session: open the cable, configure and filter channel 4
    /// (or 3), wake the ECU, and expose the KWP2000 link.
    fn open_kline_with_port(
        &mut self,
        port: Box<dyn Op2Port>,
        cfg: &ConnectConfig,
        k: KLineSettings,
    ) -> Result<()> {
        if self.session.is_some() {
            self.close()?;
        }
        let layer = k.layer;
        // The CAN settings are unused on K-line; the budget is the K-line one.
        let placeholder = Settings {
            bitrate: 0,
            tx_timeout_ms: k.tx_timeout_ms,
            extended: false,
        };
        let mut s = Session::new(port, placeholder);
        s.kline = Some(KLineState::new(k));
        s.port.write_all(b"\r\n\r\n")?;
        let firmware = s.identify(1500)?;
        let setup = (|| -> Result<()> {
            s.command("atz", &[], SETUP_TIMEOUT_MS)?;
            s.command("ata", &[], CMD_TIMEOUT_MS)?;
            s.open_kline()
        })();
        if let Err(e) = setup {
            let _ = s.command("ata", &[], CMD_TIMEOUT_MS);
            return Err(e);
        }
        let shared = Rc::new(RefCell::new(s));
        self.kline = Some(KLineLink {
            session: Rc::clone(&shared),
        });
        self.kline_layer = Some(layer);
        self.firmware = Some(firmware);
        self.session = Some(shared);
        self.cfg = Some(cfg.clone());
        Ok(())
    }

    /// Key bytes the ECU sent during the K-line init (`None` on CAN, or with
    /// `init=none`).
    pub fn kline_key_bytes(&self) -> Option<(u8, u8)> {
        let s = self.session.as_ref()?.borrow();
        s.kline.as_ref().and_then(|k| k.key_bytes)
    }

    /// Use `port` instead of the serial device the next time the capture
    /// monitor starts (the offline fake in tests).
    pub fn inject_monitor_port(&mut self, port: Box<dyn Op2Port>) {
        self.monitor.inject_port(port);
    }

    /// What the session dropped so far (stale replies, indications, …).
    pub fn stats(&self) -> Option<SessionStats> {
        self.session.as_ref().map(|s| s.borrow().stats)
    }

    fn transport(&mut self) -> Result<&mut IsoTp> {
        self.transport.as_mut().ok_or(Error::NotConnected)
    }

    /// Begin an extended diagnostic session so upload/download is accepted.
    fn begin_session(&mut self) -> Result<()> {
        let t = self.transport()?;
        let mut uds = UdsClient::new(t);
        // 0x03 = extendedDiagnosticSession. Ignore ECUs that only accept it in
        // programming mode by surfacing the error to the caller.
        uds.session(0x03).map(|_| ())
    }

    /// Run UDS security access (service 0x27) if a seed/key handler is set or
    /// the connection config marks security as required. A no-op on an open
    /// ECU with no handler configured, so unlocked sessions keep working.
    fn security_access(&mut self) -> Result<()> {
        let required = security_required(self.cfg.as_ref());
        let level = seedkey_level(self.cfg.as_ref(), 0x11);
        // Disjoint field borrows: `seed_key` (shared) and `transport` (unique).
        let sk = self.seed_key.as_deref();
        let transport = self.transport.as_mut().ok_or(Error::NotConnected)?;
        uds_security_access(transport, sk, level, required)
    }
}

impl Interface for OpenPort2Driver {
    fn set_seed_key(&mut self, seed_key: Box<dyn SeedKey>) -> bool {
        OpenPort2Driver::set_seed_key(self, seed_key);
        true
    }

    fn info(&self) -> InterfaceInfo {
        let firmware = match (self.firmware.clone(), self.kline_key_bytes()) {
            (Some(f), Some((a, b))) => Some(format!("{f} (K-line keybytes {a:02X} {b:02X})")),
            (f, _) => f,
        };
        InterfaceInfo {
            id: "openport2".into(),
            display_name: "Tactrix OpenPort 2.0 (J2534/CDC-ACM)".into(),
            firmware,
            serial: None,
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            can: true,
            kline: true,
            read_memory: true,
            // K-line is read-only; CAN keeps its write path.
            write_memory: self.kline.is_none(),
            battery_voltage: true,
            // The cable can drive a pin (`atv`), but the Interface has no
            // programming-voltage call and this driver sends no `atv`.
            programming_voltage: false,
            ..Default::default()
        }
    }

    /// Expose the host-side ISO-TP over the OpenPort 2.0 CAN channel as a [`DiagTransport`] so the coding/adaptation/
    /// auto-scan/service/logging engines (and the raw-request console) can
    /// drive this adapter. `None` until `open()` succeeds.
    fn diag(&mut self) -> Option<&mut dyn DiagTransport> {
        if let Some(t) = self.transport.as_mut() {
            return Some(t as &mut dyn DiagTransport);
        }
        // On K-line: the KWP2000 link, which admits read services only.
        self.kline.as_mut().map(|k| k as &mut dyn DiagTransport)
    }

    /// Receive-only raw-CAN capture: pass-all filter, no transmit path.
    fn can_monitor(&mut self) -> Option<&mut dyn opentune_core::CanMonitor> {
        Some(&mut self.monitor)
    }

    fn open(&mut self, cfg: &ConnectConfig) -> Result<()> {
        // Refuse bad settings before the serial device is touched.
        if kline_settings(cfg)?.is_none() {
            Settings::from_cfg(cfg)?;
        }
        let path = cfg
            .options
            .get("port")
            .map(String::as_str)
            .unwrap_or("/dev/ttyACM0");
        let port = SerialOp2Port::open(path)?;
        self.open_with_port(Box::new(port), cfg)
    }

    fn close(&mut self) -> Result<()> {
        // Drop the ISO-TP / K-line link before touching the shared session.
        self.transport = None;
        self.kline = None;
        self.kline_layer = None;
        if let Some(session) = self.session.take() {
            if let Ok(mut s) = Rc::try_unwrap(session).map(RefCell::into_inner) {
                s.close();
            }
        }
        self.cfg = None;
        Ok(())
    }

    fn battery_millivolts(&mut self) -> Result<Option<u32>> {
        let session = self.session.as_ref().ok_or(Error::NotConnected)?;
        let mv = session.borrow_mut().battery_mv()?;
        Ok(Some(mv))
    }

    fn read_ecu_id(&mut self) -> Result<EcuId> {
        if let Some(k) = self.kline.as_mut() {
            return match self.kline_layer.unwrap_or(DiagLayer::Kwp2000) {
                DiagLayer::Kwp2000 => read_kwp_ecu_id(k),
                DiagLayer::Uds => read_uds_ecu_id(k),
            };
        }
        if self.transport.is_none() {
            return Err(Error::NotConnected);
        }
        read_uds_ecu_id(self.transport()?)
    }

    fn read_memory(
        &mut self,
        region: MemoryRegion,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<Vec<u8>> {
        if self.kline.is_some() {
            return self.kline_read_memory(region, progress);
        }
        if self.transport.is_none() {
            return Err(Error::NotConnected);
        }
        let _ = self.begin_session();
        // A full read from a locked ECU needs security access too; a no-op when
        // no handler is set and security is not required.
        self.security_access()?;
        uds_read_memory(self.transport()?, region, progress)
    }

    fn write_memory(
        &mut self,
        region: MemoryRegion,
        data: &[u8],
        progress: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        if self.kline.is_some() {
            return Err(Error::Unsupported(
                "openport2 K-line is read-only: no write path (nothing was sent)".into(),
            ));
        }
        if self.transport.is_none() {
            return Err(Error::NotConnected);
        }
        if data.len() != region.length as usize {
            return Err(Error::Config("data length != region length".into()));
        }
        let _ = self.begin_session();
        // Unlock the ECU (service 0x27) before requestDownload if required.
        self.security_access()?;
        uds_write_memory(self.transport()?, region, data, progress)
    }
}

/// The K-line settings when the config selects K-line, `None` for CAN.
fn kline_settings(cfg: &ConnectConfig) -> Result<Option<KLineSettings>> {
    match kline::selected_channel(cfg)? {
        Some(proto) => KLineSettings::from_cfg(cfg, proto).map(Some),
        None => Ok(None),
    }
}

impl OpenPort2Driver {
    /// K-line read: KWP2000 `readMemoryByAddress` (0x23) in chunks, after a
    /// KWP security access when a seed/key handler is set or the config
    /// requires one. UDS over K-line has no read path here.
    fn kline_read_memory(
        &mut self,
        region: MemoryRegion,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<Vec<u8>> {
        if self.kline_layer != Some(DiagLayer::Kwp2000) {
            return Err(Error::Unsupported(
                "openport2 K-line reads memory with KWP2000 only (layer=kwp2000)".into(),
            ));
        }
        let required = security_required(self.cfg.as_ref());
        let level = seedkey_level(self.cfg.as_ref(), 0x01);
        let sk = self.seed_key.as_deref();
        let link = self.kline.as_mut().ok_or(Error::NotConnected)?;
        kwp_security_access(link, sk, level, required)?;
        kwp_read_memory(link, region, progress)
    }
}

/// KWP2000 security access (0x27) if a handler is set; otherwise an error
/// only when security is required. Reads only: nothing is written.
fn kwp_security_access(
    transport: &mut dyn DiagTransport,
    seed_key: Option<&dyn SeedKey>,
    request_level: u8,
    required: bool,
) -> Result<()> {
    let Some(sk) = seed_key else {
        if required {
            return Err(Error::Config(
                "ECU requires security access but no seed/key handler is set \
                 (call set_seed_key with an algorithm for this ECU)"
                    .into(),
            ));
        }
        return Ok(());
    };
    let mut kwp = Kwp2000Client::new(transport);
    let seed = kwp.request_seed(request_level)?;
    if !seed.is_empty() && seed.iter().all(|&b| b == 0) {
        return Ok(());
    }
    let key = sk.compute(&seed, request_level)?;
    kwp.send_key(request_level.wrapping_add(1), &key)
}

/// KWP2000 `readEcuIdentification` (0x1A) options → [`EcuId`]: the ISO
/// 14230-3 records (0x90 VIN, 0x91 hardware number, 0x94 software number,
/// 0x97 system name), the ECU identification table (0x80) and the VAG record
/// (0x9B). Options the ECU does not implement are skipped.
fn read_kwp_ecu_id(transport: &mut dyn DiagTransport) -> Result<EcuId> {
    let mut id = EcuId::default();
    let mut kwp = Kwp2000Client::new(transport);
    let options: &[(u8, &str)] = &[
        (0x80, "ecu_id"),
        (0x90, "vin"),
        (0x91, "hw_number"),
        (0x94, "sw_number"),
        (0x97, "ecu_name"),
        (0x9B, "vag_record"),
    ];
    let mut got_any = false;
    for (opt, name) in options {
        match kwp.read_ecu_id(*opt) {
            Ok(resp) => {
                got_any = true;
                // resp = [option echo, data...]
                let value = decode_text(resp.get(1..).unwrap_or_default());
                match *name {
                    "vin" => id.vin = Some(value.clone()),
                    "hw_number" => id.hw_number = Some(value.clone()),
                    "sw_number" => id.sw_number = Some(value.clone()),
                    "ecu_name" => id.ecu_name = Some(value.clone()),
                    "vag_record" if id.sw_number.is_none() => id.sw_number = Some(value.clone()),
                    _ => {}
                }
                id.raw.insert(name.to_string(), value);
            }
            Err(Error::NegativeResponse { .. }) => continue,
            Err(e) => {
                if got_any {
                    break;
                }
                return Err(e);
            }
        }
    }
    // The identification table names the ECU when no system name came.
    if id.ecu_name.is_none() {
        id.ecu_name = id.raw.get("ecu_id").cloned();
    }
    Ok(id)
}

/// KWP2000 `readMemoryByAddress` read loop, chunked to fit one K-line
/// message (the length is a single byte).
fn kwp_read_memory(
    transport: &mut dyn DiagTransport,
    region: MemoryRegion,
    progress: &mut dyn FnMut(Progress),
) -> Result<Vec<u8>> {
    const MAX_CHUNK: u32 = 250;
    let total = region.length as u64;
    let mut kwp = Kwp2000Client::new(transport);
    progress(Progress {
        phase: Phase::Read,
        done: 0,
        total,
    });
    let mut out = Vec::with_capacity(region.length as usize);
    let mut addr = region.address;
    let mut remaining = region.length;
    while remaining > 0 {
        let block = kwp.read_memory(addr, remaining.min(MAX_CHUNK) as u8)?;
        // Never empty (the client refuses that), so every pass progresses.
        out.extend_from_slice(&block);
        addr = addr.wrapping_add(block.len() as u32);
        remaining = remaining.saturating_sub(block.len() as u32);
        progress(Progress {
            phase: Phase::Read,
            done: out.len().min(region.length as usize) as u64,
            total,
        });
    }
    out.truncate(region.length as usize);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Security access (service 0x27) hook.
//
// OpenTune ships NO OEM seed/key algorithms. A seed/key handler is supplied by
// the application via [`OpenPort2Driver::set_seed_key`] (or a plugin), keyed by
// the `seedkey` id in an ECU definition. This helper runs the request-seed →
// compute → send-key handshake over the existing [`UdsClient`] before a
// download.
//
// Config keys read from [`ConnectConfig::options`]:
// - `seedkey`:       expected algorithm id; its presence marks security required.
// - `security`:      `required`/`true`/`1`/`yes` marks security required.
// - `seedkey_level`: the request-seed access level (odd), decimal or `0x`-hex.
// ---------------------------------------------------------------------------

/// True if the config marks the ECU as requiring security access.
fn security_required(cfg: Option<&ConnectConfig>) -> bool {
    cfg.map(|c| {
        c.options.contains_key("seedkey")
            || matches!(
                c.options.get("security").map(String::as_str),
                Some("required" | "true" | "1" | "yes")
            )
    })
    .unwrap_or(false)
}

/// Request-seed access level from config, or `default` (send-key level is +1).
fn seedkey_level(cfg: Option<&ConnectConfig>, default: u8) -> u8 {
    cfg.and_then(|c| c.options.get("seedkey_level"))
        .and_then(|s| parse_access_level(s))
        .unwrap_or(default)
}

/// Parse an access level as decimal or `0x`-prefixed hex.
fn parse_access_level(s: &str) -> Option<u8> {
    let t = s.trim();
    match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(h) => u8::from_str_radix(h, 16).ok(),
        None => t.parse().ok(),
    }
}

/// Perform UDS service 0x27 seed/key if a handler is set; otherwise error only
/// when security is required.
fn uds_security_access(
    transport: &mut dyn DiagTransport,
    seed_key: Option<&dyn SeedKey>,
    request_level: u8,
    required: bool,
) -> Result<()> {
    match seed_key {
        None => {
            if required {
                Err(Error::Config(
                    "ECU requires security access but no seed/key handler is set \
                     (call set_seed_key with an algorithm for this ECU)"
                        .into(),
                ))
            } else {
                // Open ECU, or already unlocked: nothing to do.
                Ok(())
            }
        }
        Some(sk) => {
            let mut uds = UdsClient::new(transport);
            let seed = uds.request_seed(request_level)?;
            // An all-zero seed conventionally means "already unlocked".
            if !seed.is_empty() && seed.iter().all(|&b| b == 0) {
                return Ok(());
            }
            let key = sk.compute(&seed, request_level)?;
            uds.send_key(request_level.wrapping_add(1), &key)
        }
    }
}

// ---------------------------------------------------------------------------
// Shared UDS flows (also reused by the SocketCAN driver via opentune-core).
// These live here because they are small and keep the driver self-contained;
// they only use the fixed `opentune-core` API.
// ---------------------------------------------------------------------------

/// Common identification DIDs → [`EcuId`] fields.
pub(crate) fn read_uds_ecu_id(transport: &mut dyn DiagTransport) -> Result<EcuId> {
    let mut id = EcuId::default();
    let mut uds = UdsClient::new(transport);
    let dids: &[(u16, &str)] = &[
        (0xF190, "vin"),
        (0xF194, "sw_number"),
        (0xF191, "hw_number"),
        (0xF18C, "ecu_serial"),
        (0xF18A, "supplier"),
        (0xF197, "ecu_name"),
    ];
    let mut got_any = false;
    for (did, name) in dids {
        match uds.read_data_by_id(*did) {
            Ok(bytes) => {
                got_any = true;
                let value = decode_text(&bytes);
                match *name {
                    "vin" => id.vin = Some(value.clone()),
                    "sw_number" => id.sw_number = Some(value.clone()),
                    "hw_number" => id.hw_number = Some(value.clone()),
                    "supplier" => id.supplier = Some(value.clone()),
                    "ecu_name" => id.ecu_name = Some(value.clone()),
                    _ => {}
                }
                id.raw.insert(name.to_string(), value);
            }
            // A DID the ECU does not implement is not fatal.
            Err(Error::NegativeResponse { .. }) => continue,
            Err(e) => {
                if got_any {
                    break;
                }
                return Err(e);
            }
        }
    }
    Ok(id)
}

/// Render bytes as text if printable ASCII, else as hex.
fn decode_text(bytes: &[u8]) -> String {
    let printable = !bytes.is_empty()
        && bytes
            .iter()
            .all(|&b| (0x20..=0x7E).contains(&b) || b == 0x00);
    if printable {
        String::from_utf8_lossy(bytes)
            .trim_matches(|c| c == '\0' || c == ' ')
            .to_string()
    } else {
        bytes.iter().map(|b| format!("{b:02X}")).collect()
    }
}

/// UDS `requestUpload` + `transferData` read loop.
pub(crate) fn uds_read_memory(
    transport: &mut IsoTp,
    region: MemoryRegion,
    progress: &mut dyn FnMut(Progress),
) -> Result<Vec<u8>> {
    let total = region.length as u64;
    let mut uds = UdsClient::new(transport);
    // dataFormatIdentifier 0x00 = uncompressed/unencrypted.
    let _max_block = uds.request_upload(0x00, region.address, region.length)?;
    progress(Progress {
        phase: Phase::Read,
        done: 0,
        total,
    });

    let mut out = Vec::with_capacity(region.length as usize);
    let mut seq: u8 = 1;
    while (out.len() as u32) < region.length {
        // For an upload the request carries no data; the ECU returns the next
        // block in its positive response.
        let resp = uds.transfer_data(seq, &[])?;
        // resp = [blockSequenceCounter, data...]; strip the echoed counter.
        let chunk = if resp.is_empty() {
            &resp[..]
        } else {
            &resp[1..]
        };
        if chunk.is_empty() {
            return Err(Error::Protocol("empty transferData block".into()));
        }
        out.extend_from_slice(chunk);
        seq = seq.wrapping_add(1);
        progress(Progress {
            phase: Phase::Read,
            done: out.len().min(region.length as usize) as u64,
            total,
        });
    }
    out.truncate(region.length as usize);
    let _ = uds.transfer_exit();
    Ok(out)
}

/// UDS `requestDownload` + `transferData` + `transferExit` write loop.
///
/// Assumes the ECU is already in a programming session with security access
/// granted. [`OpenPort2Driver::write_memory`] runs the service 0x27 seed/key
/// handshake (see [`uds_security_access`]) before calling this, so a locked ECU
/// is unlocked first when a seed/key handler is configured.
pub(crate) fn uds_write_memory(
    transport: &mut IsoTp,
    region: MemoryRegion,
    data: &[u8],
    progress: &mut dyn FnMut(Progress),
) -> Result<()> {
    let total = data.len() as u64;
    let mut uds = UdsClient::new(transport);
    let max_block = uds.request_download(0x00, region.address, region.length)?;
    // maxNumberOfBlockLength covers SID (0x36) + sequence byte + data.
    let chunk_size = max_block.saturating_sub(2).max(1);
    progress(Progress {
        phase: Phase::Write,
        done: 0,
        total,
    });

    let mut seq: u8 = 1;
    let mut done = 0u64;
    for chunk in data.chunks(chunk_size) {
        uds.transfer_data(seq, chunk)?;
        seq = seq.wrapping_add(1);
        done += chunk.len() as u64;
        progress(Progress {
            phase: Phase::Write,
            done,
            total,
        });
    }
    uds.transfer_exit()?;
    Ok(())
}

#[cfg(test)]
mod seedkey_hook_tests {
    use super::*;

    /// A CAN link with no traffic: enough to stand up the ISO-TP layer.
    struct IdleLink;

    impl CanLink for IdleLink {
        fn send_frame(&mut self, _id: u32, _data: &[u8]) -> Result<()> {
            Ok(())
        }
        fn recv_frame(&mut self, timeout_ms: u64) -> Result<(u32, Vec<u8>)> {
            Err(Error::Timeout(timeout_ms))
        }
    }

    #[test]
    fn diag_exposes_isotp_after_connect() {
        let mut d = OpenPort2Driver::new();
        assert!(d.diag().is_none(), "no transport before open");
        d.transport = Some(IsoTp::new(Box::new(IdleLink), 0x7E0, 0x7E8));
        assert!(
            d.diag().is_some(),
            "diag() must expose the ISO-TP transport"
        );
    }
    use opentune_core::protocol::diag::DiagTransport;
    use std::collections::VecDeque;

    /// A scripted diagnostic transport: returns queued responses and records
    /// every request so a handshake can be asserted.
    struct ScriptedTransport {
        replies: VecDeque<Vec<u8>>,
        requests: Vec<Vec<u8>>,
    }

    impl DiagTransport for ScriptedTransport {
        fn request(&mut self, req: &[u8], _timeout_ms: u64) -> Result<Vec<u8>> {
            self.requests.push(req.to_vec());
            self.replies.pop_front().ok_or(Error::Timeout(0))
        }
    }

    /// DEMO ONLY — not a real OEM algorithm. Exercises the [`SeedKey`] hook with
    /// a trivial reversible transform (XOR 0x5A then +1 per byte).
    struct DemoXorAddKey;

    impl SeedKey for DemoXorAddKey {
        fn id(&self) -> &str {
            "demo-xor-add"
        }
        fn compute(&self, seed: &[u8], _level: u8) -> Result<Vec<u8>> {
            Ok(seed.iter().map(|b| (b ^ 0x5A).wrapping_add(1)).collect())
        }
    }

    #[test]
    fn runs_seed_key_handshake_before_download() {
        let seed = [0x11u8, 0x22, 0x33, 0x44];
        let mut seed_resp = vec![0x67, 0x11]; // positiveResponse, echoed level
        seed_resp.extend_from_slice(&seed);
        let key_ok = vec![0x67, 0x12]; // send-key accepted
        let mut t = ScriptedTransport {
            replies: VecDeque::from(vec![seed_resp, key_ok]),
            requests: Vec::new(),
        };

        uds_security_access(&mut t, Some(&DemoXorAddKey), 0x11, true).unwrap();

        // First request asks for the seed; second sends the computed key.
        assert_eq!(t.requests[0], vec![0x27, 0x11]);
        let expected_key: Vec<u8> = seed.iter().map(|b| (b ^ 0x5A).wrapping_add(1)).collect();
        let mut expected_req = vec![0x27u8, 0x12];
        expected_req.extend_from_slice(&expected_key);
        assert_eq!(t.requests[1], expected_req);
    }

    #[test]
    fn errors_when_required_but_no_handler() {
        let mut t = ScriptedTransport {
            replies: VecDeque::new(),
            requests: Vec::new(),
        };
        let err = uds_security_access(&mut t, None, 0x11, true).unwrap_err();
        assert!(matches!(err, Error::Config(_)));
        assert!(t.requests.is_empty());
    }

    #[test]
    fn noop_when_open_ecu_and_no_handler() {
        let mut t = ScriptedTransport {
            replies: VecDeque::new(),
            requests: Vec::new(),
        };
        uds_security_access(&mut t, None, 0x11, false).unwrap();
        assert!(t.requests.is_empty());
    }

    #[test]
    fn config_helpers_parse_options() {
        use opentune_core::ConnectConfig;
        let mut cfg = ConnectConfig::default();
        assert!(!security_required(Some(&cfg)));
        assert_eq!(seedkey_level(Some(&cfg), 0x11), 0x11);
        cfg.options.insert("seedkey".into(), "edc17-sa2".into());
        cfg.options.insert("seedkey_level".into(), "0x09".into());
        assert!(security_required(Some(&cfg)));
        assert_eq!(seedkey_level(Some(&cfg), 0x11), 0x09);
    }
}
