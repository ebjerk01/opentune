//! ELM327 **serial** diagnostics driver for OpenTune.
//!
//! This driver targets a genuine or clone ELM327 USB adapter (CH340 serial) for
//! diagnostics over ISO 15765-4 (UDS/OBD-II on CAN): identify the ECU, read and
//! decode Diagnostic Trouble Codes, read live OBD-II PIDs, clear stored DTCs,
//! and expose the UDS transport via [`Interface::diag`] for the service engines.
//!
//! It also has a **block read** path ([`Interface::read_memory`]): optional
//! session, SecurityAccess with a caller-supplied [`SeedKey`], then
//! `ReadMemoryByAddress` (0x23) or `RequestUpload`/`TransferData`/
//! `RequestTransferExit` (0x35/0x36/0x37). See [`memory`] for how each request
//! is fitted into the single frame an ELM327 can transmit.
//!
//! **Flashing over an ELM327 is not recommended.** The adapter cannot transmit
//! multi-frame ISO-TP, so [`Interface::write_memory`] downloads in 5-byte
//! single-frame blocks (one serial round trip each), only to 16/24-bit
//! addresses, and only with the bench opt-in `elm_write=single-frame`;
//! [`Capabilities::write_memory`] stays `false`. Use a raw-CAN interface
//! (slcan, gs_usb, SocketCAN, J2534) for real flashing.
//!
//! The ELM firmware performs ISO-TP (ISO 15765-2) segmentation itself when CAN
//! auto-formatting is on (`ATCAF1`), so this driver implements
//! [`DiagTransport`](opentune_core::protocol::diag::DiagTransport) directly and
//! runs the fixed `opentune-core` [`UdsClient`] on top. Multi-frame replies are
//! reassembled from the ELM `NN:` line-counter text, and the ISO-TP flow-control
//! frame the adapter sends is configured explicitly via `ATFCSH`/`ATFCSD`/`ATFCSM`
//! so the manual path works on firmware that does not auto-answer flow control.
//!
//! ## Configuration ([`ConnectConfig::options`])
//! - `port`: serial device (default `/dev/ttyUSB0`).
//! - `baud`: serial baud (default `38400`, verified for the v1.5 CH340 clones).
//! - `bus`: `hs`/`hscan` (default) selects high-speed CAN via ELM protocol 6
//!   (`ATSP6`, ISO 15765-4, 11-bit, 500 kbit); `ms`/`mscan` selects medium-speed
//!   CAN via ELM user protocol B (`ATSPB`, 125 kbit). **MS-CAN additionally
//!   requires the adapter's physical HS/MS hardware switch set to MS** — software
//!   alone cannot route to the MS-CAN pins. See [`CanBus`].
//! - `tx_id`/`rx_id` come from [`ConnectConfig`] (default 0x7E0/0x7E8). A
//!   functional request to 0x7DF also works when set as `tx_id`.
//!
//! Block read/write options (all optional):
//! - `session`: `none`, `default`, `extended`, `programming` or a session byte.
//!   Default: extended (0x03) for reads, programming (0x02) for writes.
//! - `seedkey` (an algorithm id) or `security=required`: the ECU needs
//!   SecurityAccess; the operation is refused before anything is sent unless a
//!   handler was given with [`Elm327Obd::set_seed_key`].
//! - `seedkey_level`: odd request-seed level (default 0x11).
//! - `read_mode`: `rmba` (0x23, default) or `upload` (0x35/0x36/0x37).
//! - `read_chunk`: bytes per 0x23 request (default 128 when the adapter
//!   accepted user flow control, else 32; capped by the size field).
//! - `alfid`: `addressAndLengthFormatIdentifier` override (hex, e.g. `14`);
//!   must fit the 7-byte single frame.
//! - `data_format`: `dataFormatIdentifier` for 0x34/0x35 (hex, default `00`).
//! - `fc_stmin`: STmin in ms the ELM asks the ECU for during a block read
//!   (default: derived from the serial baud so the UART keeps up).
//! - `elm_write=single-frame`: bench opt-in for the slow download path.
//!
//! ## Validation status
//! Unit-tested against scripted ELM responses (see the tests module). The AT
//! init (including `ATCAF1`, `ATFCSH`/`ATFCSD`/`ATFCSM`, `ATST`, `ATCRA` and
//! `ATSPB`/`ATPB C1 04`, which the clone reports as `USER1 (CAN 11/125)`) has
//! been checked on a v1.5 PIC18F25K80 + CH340 clone with no vehicle attached;
//! nothing has been exchanged with a real ECU yet.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use opentune_core::protocol::diag::check_response;
use opentune_core::protocol::diag::DiagTransport;
use opentune_core::protocol::elm;
use opentune_core::protocol::seedkey::SeedKey;
use opentune_core::protocol::uds::UdsClient;
use opentune_core::{
    Capabilities, ConnectConfig, EcuId, Error, Interface, InterfaceInfo, MemoryRegion, Phase,
    Progress, Result,
};
use opentune_dtc::{decode_uds, format_code, lookup, Dtc, DtcRecord, System};
use std::io::{Read, Write};
use std::time::{Duration, Instant};

pub mod memory;
pub mod monitor;

/// Default serial device for a USB ELM327.
const DEFAULT_PORT: &str = "/dev/ttyUSB0";
/// Default serial baud (verified for the v1.5 CH340 clones).
const DEFAULT_BAUD: u32 = 38_400;
/// Per-read serial timeout while assembling one ELM reply.
const READ_CHUNK_MS: u64 = 100;
/// Default per-exchange response timeout.
const REQUEST_TIMEOUT_MS: u64 = 2000;

// --- CAN bus selection ------------------------------------------------------

/// Which vehicle CAN bus the ELM327 should talk to.
///
/// The genuine v1.5 adapters with a hardware HS/MS switch route the CAN
/// transceiver to one pin pair or the other; the switch position and the ELM
/// protocol must agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanBus {
    /// High-speed CAN: ISO 15765-4, 11-bit identifiers, 500 kbit, via ELM
    /// protocol 6 (`ATSP6`). This is the standard OBD-II diagnostic bus.
    HsCan,
    /// Medium-speed CAN: 125 kbit via ELM user protocol B (`ATSPB`). **Also
    /// requires the adapter's physical HS/MS switch set to MS** — selecting the
    /// protocol in software does not move the transceiver to the MS-CAN pins.
    MsCan,
}

impl CanBus {
    /// Parse a `bus` option value. Accepts `hs`/`hscan`/`high` and
    /// `ms`/`mscan`/`medium` (case-insensitive); anything else, including an
    /// absent value, defaults to [`CanBus::HsCan`].
    pub fn from_option(value: Option<&str>) -> CanBus {
        match value.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("ms" | "mscan" | "ms-can" | "medium") => CanBus::MsCan,
            _ => CanBus::HsCan,
        }
    }
}

// --- Byte transport ---------------------------------------------------------

/// Byte transport the ELM327 driver reads and writes over.
///
/// A real adapter is a serial port; tests inject a scripted mock. Implementations
/// return a `WouldBlock`/`TimedOut` error (or a zero-length read) when no data is
/// available within their configured timeout.
pub trait ElmStream: Send {
    /// Read up to `buf.len()` bytes, blocking at most the stream's timeout.
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize>;
    /// Write the whole buffer to the adapter.
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()>;
    /// Flush any buffered output.
    fn flush(&mut self) -> std::io::Result<()>;
    /// Best-effort discard of any buffered input; a no-op is acceptable.
    fn drain_input(&mut self) {}
}

/// A USB/serial ELM327.
struct SerialElm {
    port: Box<dyn serialport::SerialPort>,
}

impl ElmStream for SerialElm {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.port.read(buf)
    }
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.port.write_all(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.port.flush()
    }
    fn drain_input(&mut self) {
        let _ = self.port.clear(serialport::ClearBuffer::Input);
    }
}

/// Owns the byte transport and speaks the ELM327 AT dialect. Implements
/// [`DiagTransport`] so the core [`UdsClient`] can drive it.
struct ElmTransport {
    stream: Box<dyn ElmStream>,
    /// `ATI` banner captured at init, e.g. `ELM327 v1.5`.
    version: String,
    /// `AT@1` device description captured at init.
    device: String,
    /// The adapter accepted user-defined flow control (`ATFCSH`, `ATFCSD`,
    /// `ATFCSM 1`) at init, so a block read can pace the ECU with an STmin.
    fc_user: bool,
    /// Serial baud, used to derive the block-read STmin.
    baud: u32,
}

impl ElmTransport {
    fn new(stream: Box<dyn ElmStream>) -> Self {
        Self {
            stream,
            version: String::new(),
            device: String::new(),
            fc_user: false,
            baud: DEFAULT_BAUD,
        }
    }

    /// Ask the ECU for `stmin` ms between consecutive frames during a block
    /// read, so a long multi-frame reply does not overrun the adapter's UART
    /// (`BUFFER FULL`). Returns whether the pacing is in effect.
    fn pace_flow_control(&mut self, stmin: u8) -> Result<bool> {
        if !self.fc_user {
            return Ok(false);
        }
        let reply = self.at(&format!("ATFCSD 30 00 {stmin:02X}"))?;
        Ok(!elm::at_rejected(&reply))
    }

    /// Put the init flow control (CTS, BS 0, STmin 0) back after a block read.
    fn restore_flow_control(&mut self) {
        let _ = self.at("ATFCSD 30 00 00");
    }

    /// Read characters until the ELM command prompt `>` or the deadline.
    fn read_until_prompt(&mut self, timeout_ms: u64) -> Result<String> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(READ_CHUNK_MS));
        let mut out = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            match self.stream.read(&mut byte) {
                Ok(1) => {
                    if byte[0] == b'>' {
                        break;
                    }
                    out.push(byte[0]);
                }
                Ok(_) => {} // 0 bytes: idle tick
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) => {}
                Err(e) => return Err(Error::Io(e)),
            }
            if Instant::now() >= deadline {
                // Some clones omit the prompt; return what we have if non-empty.
                if out.is_empty() {
                    return Err(Error::Timeout(timeout_ms));
                }
                break;
            }
        }
        Ok(String::from_utf8_lossy(&out).to_string())
    }

    fn write_line(&mut self, line: &str) -> Result<()> {
        self.stream.write_all(line.as_bytes()).map_err(Error::Io)?;
        self.stream.write_all(b"\r").map_err(Error::Io)?;
        self.stream.flush().map_err(Error::Io)?;
        Ok(())
    }

    /// Send an `AT` command and return the adapter's text reply.
    fn at(&mut self, cmd: &str) -> Result<String> {
        self.stream.drain_input();
        self.write_line(cmd)?;
        let reply = self.read_until_prompt(REQUEST_TIMEOUT_MS)?;
        Ok(reply.trim().to_string())
    }

    /// Send an `AT` command and require a non-error reply.
    fn at_ok(&mut self, cmd: &str) -> Result<()> {
        let reply = self.at(cmd)?;
        if elm::at_rejected(&reply) {
            return Err(Error::Protocol(format!("ELM rejected {cmd}: {reply}")));
        }
        Ok(())
    }

    /// Battery voltage via `ATRV` (e.g. `12.3V`), returned in millivolts.
    fn battery_mv(&mut self) -> Result<u32> {
        let reply = self.at("ATRV")?;
        let num: String = reply
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        num.parse::<f32>()
            .map(|v| (v * 1000.0).round() as u32)
            .map_err(|_| Error::Protocol(format!("cannot parse ATRV: {reply}")))
    }

    /// Initialise the adapter and select the bus/headers for `cfg`.
    ///
    /// Handshake order: reset (`ATZ`), echo off (`ATE0`) straight away so the
    /// identity strings are not polluted by the echo, capture identity (`ATI`,
    /// `AT@1`), linefeeds off (`ATL0`), spaces off (`ATS0`), headers off
    /// (`ATH0`), adaptive timing (`ATAT1`), protocol select, CAN auto-formatting
    /// (`ATCAF1`), request/receive headers (`ATSH`/`ATCRA`), ISO-TP flow control
    /// (`ATFCSH`/`ATFCSD`/`ATFCSM`) and the response timeout (`ATST`).
    fn init(&mut self, cfg: &ConnectConfig, bus: CanBus) -> Result<()> {
        let _ = self.at("ATZ"); // reset; reply is the banner
        self.at_ok("ATE0")?; // echo off, before anything whose reply we keep
        self.version = self.at("ATI").unwrap_or_default();
        self.device = self.at("AT@1").unwrap_or_default();
        self.at_ok("ATL0")?; // linefeeds off
        self.at_ok("ATS0")?; // spaces off (compact hex)
        let _ = self.at("ATH0"); // headers off (clones may not ack)
        self.at_ok("ATAT1")?; // adaptive timing

        match bus {
            CanBus::HsCan => {
                self.at_ok("ATSP6")?; // ISO 15765-4, 11-bit, 500 kbit
            }
            CanBus::MsCan => {
                self.at_ok("ATSPB")?; // user protocol B
                                      // Program protocol B: options byte + baud divisor. The ELM327
                                      // datasheet sets the CAN rate to (500 / divisor) kbit when the
                                      // options byte's high bit is clear, so divisor 4 => 125 kbit.
                                      // Options 0xC1 selects 11-bit/variable-DLC per the datasheet's
                                      // CAN options nibble. Both bytes are overridable because clones
                                      // vary; these defaults need bench confirmation (see crate docs).
                let opts = cfg
                    .options
                    .get("can_pb_options")
                    .and_then(|s| parse_hex_u8(s))
                    .unwrap_or(0xC1);
                let div = cfg
                    .options
                    .get("can_pb_divisor")
                    .and_then(|s| parse_hex_u8(s))
                    .unwrap_or(0x04);
                let _ = self.at(&format!("ATPB {opts:02X} {div:02X}"));
            }
        }

        self.at_ok("ATCAF1")?; // CAN auto-formatting (ISO-TP framing) on

        // 11-bit request/receive headers. (This driver targets the 11-bit
        // diagnostic bus; 29-bit targets belong to the memory-oriented drivers.)
        let tx = cfg.tx_id & 0x7FF;
        let rx = cfg.rx_id & 0x7FF;
        self.at_ok(&format!("ATSH {tx:03X}"))?;
        self.set_receive_filter(rx)?; // accept only the ECU rx id

        // ISO-TP flow control: answer the ECU's first frame with CTS, 0 block
        // size, 0 separation time, using our tx header. `ATFCSM 1` tells the ELM
        // to use these user-set values rather than auto.
        // Not fatal when refused (older clones), but remembered: without it a
        // block read cannot pace the ECU and uses small chunks instead.
        let fc = [
            self.at(&format!("ATFCSH {tx:03X}")),
            self.at("ATFCSD 30 00 00"),
            self.at("ATFCSM 1"),
        ];
        self.fc_user = fc
            .iter()
            .all(|r| matches!(r, Ok(reply) if !elm::at_rejected(reply)));

        // Response timeout: ATST units are 4 ms; 0x64 => ~400 ms, the ceiling
        // adaptive timing (ATAT1) works under.
        let _ = self.at("ATST 64");
        Ok(())
    }

    /// Accept only frames from `rx` (11-bit). `ATCRA` is the normal way; if the
    /// adapter rejects it, fall back explicitly to the older filter/mask pair
    /// (`ATCF` + `ATCM 7FF`). If both are rejected init fails: without a receive
    /// filter other ECUs' replies could be taken for this ECU's.
    fn set_receive_filter(&mut self, rx: u32) -> Result<()> {
        let reply = self.at(&format!("ATCRA {rx:03X}"))?;
        if !elm::at_rejected(&reply) {
            return Ok(());
        }
        self.at_ok(&format!("ATCF {rx:03X}"))
            .and_then(|_| self.at_ok("ATCM 7FF"))
            .map_err(|e| {
                Error::Protocol(format!(
                    "ELM327 rejected ATCRA {rx:03X} and the ATCF/ATCM fallback; \
                     cannot filter replies to the ECU ({e})"
                ))
            })
    }
}

impl DiagTransport for ElmTransport {
    /// One request/response exchange. Requests longer than 7 bytes are refused
    /// up front (the ELM cannot send multi-frame ISO-TP); the reply is parsed
    /// per response by [`elm::read_reply`], which drops responsePending
    /// (`7F xx 78`) frames and waits up to P2* for the real answer.
    fn request(&mut self, req: &[u8], timeout_ms: u64) -> Result<Vec<u8>> {
        elm::check_can_request_len(req)?;
        let hex = elm::request_hex(req);
        self.stream.drain_input();
        self.write_line(&hex)?;
        let stream = &mut self.stream;
        let mut read = || read_byte(stream.as_mut());
        elm::read_reply(&mut read, &hex, timeout_ms, elm::P2_STAR_MS)
    }
}

/// Read one byte from the stream: `Ok(None)` on an idle tick.
fn read_byte(stream: &mut dyn ElmStream) -> Result<Option<u8>> {
    let mut b = [0u8; 1];
    match stream.read(&mut b) {
        Ok(1) => Ok(Some(b[0])),
        Ok(_) => Ok(None),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ) =>
        {
            Ok(None)
        }
        Err(e) => Err(Error::Io(e)),
    }
}

/// Parse a byte as decimal or `0x`-prefixed hex.
fn parse_hex_u8(s: &str) -> Option<u8> {
    let t = s.trim();
    match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(h) => u8::from_str_radix(h, 16).ok(),
        None => t.parse().ok().or_else(|| u8::from_str_radix(t, 16).ok()),
    }
}

// --- Driver -----------------------------------------------------------------

/// ELM327 adapter identity, from `ATI` and `AT@1`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdapterIdentity {
    /// The `ATI` version banner, e.g. `ELM327 v1.5`.
    pub version: String,
    /// The `AT@1` device description string.
    pub device: String,
}

/// Serial ELM327 diagnostics driver.
///
/// Opens via [`Interface::open`] with the options documented at the crate root,
/// or in tests via [`Elm327Obd::with_stream`]. Beyond the [`Interface`] trait
/// (identify, battery, block read, bench-only block write), the read-side
/// diagnostics live on the inherent methods:
/// [`read_dtcs`](Elm327Obd::read_dtcs)/[`read_obd_dtcs`](Elm327Obd::read_obd_dtcs),
/// [`clear_dtcs`](Elm327Obd::clear_dtcs), [`read_pid`](Elm327Obd::read_pid) and
/// the `live_*` helpers, plus [`diagnostic_session`](Elm327Obd::diagnostic_session)
/// and [`tester_present`](Elm327Obd::tester_present).
pub struct Elm327Obd {
    transport: Option<ElmTransport>,
    bus: CanBus,
    /// A pre-built stream injected for tests, used in place of opening serial.
    injected: Option<Box<dyn ElmStream>>,
    /// `ATMA` full-bus capture (independent of `open`).
    monitor: monitor::ElmMonitor,
    /// The configuration `open` was given (memory options live in it).
    cfg: Option<ConnectConfig>,
    /// Caller-supplied seed/key handler for SecurityAccess (0x27). OpenTune
    /// ships no OEM algorithms.
    seed_key: Option<Box<dyn SeedKey>>,
}

impl Default for Elm327Obd {
    fn default() -> Self {
        Self {
            transport: None,
            bus: CanBus::HsCan,
            injected: None,
            monitor: monitor::ElmMonitor::new(),
            cfg: None,
            seed_key: None,
        }
    }
}

/// Why [`Interface::write_memory`] is refused without the bench opt-in.
const WRITE_REFUSAL: &str = "flashing over an ELM327 is not recommended: the adapter cannot \
     transmit multi-frame ISO-TP, so a download crawls in 5-byte single-frame blocks and a \
     dropped serial link mid-download can leave the ECU unbootable. Use a raw-CAN interface \
     (slcan/gs_usb/SocketCAN/J2534). For a bench ECU only, set option elm_write=single-frame";

/// STmin (ms) that lets the UART drain one consecutive frame before the next:
/// an ELM prints a 7-byte CF as about 17 characters (`N:` + 14 hex + CR), 10
/// bits each, plus 1 ms of slack.
fn default_stmin(baud: u32) -> u8 {
    let ms = 170_000u32.div_ceil(baud.max(1)) + 1;
    ms.min(0x7F) as u8
}

impl Elm327Obd {
    /// A new, unopened driver (serial path).
    pub fn new() -> Self {
        Self::default()
    }

    /// Inject a pre-connected byte stream (e.g. a test mock). [`Interface::open`]
    /// then runs the AT init over it instead of opening a serial port.
    pub fn with_stream(stream: Box<dyn ElmStream>) -> Self {
        let mut d = Self::new();
        d.injected = Some(stream);
        d
    }

    /// The CAN bus this driver was opened against.
    pub fn bus(&self) -> CanBus {
        self.bus
    }

    /// Install the seed/key handler used for SecurityAccess (0x27) before a
    /// block read or write. Without it, an ECU marked as needing security
    /// (`seedkey`/`security=required`) is refused before anything is sent.
    /// OpenTune ships no OEM algorithms; the caller or a plugin supplies one.
    pub fn set_seed_key(&mut self, seed_key: Box<dyn SeedKey>) {
        self.seed_key = Some(seed_key);
    }

    /// Session + security for a memory operation. Returns whether an unlock
    /// was performed.
    fn prepare_memory_access(
        &mut self,
        opts: &memory::MemOptions,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<bool> {
        let sk = self.seed_key.as_deref();
        let t = self.transport.as_mut().ok_or(Error::NotConnected)?;
        progress(Progress {
            phase: Phase::Connect,
            done: 0,
            total: 1,
        });
        memory::enter_session(t, opts.session, opts.timeout_ms)?;
        progress(Progress {
            phase: Phase::SecurityAccess,
            done: 0,
            total: 1,
        });
        let unlocked = memory::security_access(
            t,
            sk,
            opts.seed_level,
            opts.security_required,
            opts.timeout_ms,
        )?;
        progress(Progress {
            phase: Phase::SecurityAccess,
            done: 1,
            total: 1,
        });
        Ok(unlocked)
    }

    /// Dry run of [`Interface::read_memory`] on the opened adapter: every line
    /// it would write (adapter `AT` pacing and the hex UDS requests, in order),
    /// with what each is for. **Sends nothing.** Fails exactly where the real
    /// read would fail before touching the bus (bad options, a request format
    /// that does not fit an ELM single frame, security required without a
    /// handler).
    pub fn plan_read_memory(&self, region: MemoryRegion) -> Result<Vec<memory::PlannedRequest>> {
        let cfg = self.cfg.as_ref().ok_or(Error::NotConnected)?;
        let t = self.transport.as_ref().ok_or(Error::NotConnected)?;
        let opts = memory::MemOptions::from_config(cfg, memory::Op::Read)?;
        let stmin = opts.fc_stmin.unwrap_or_else(|| default_stmin(t.baud));
        memory::dry_run_read(&opts, region, t.fc_user, stmin, self.seed_key.is_some())
    }

    fn transport(&mut self) -> Result<&mut ElmTransport> {
        self.transport.as_mut().ok_or(Error::NotConnected)
    }

    /// Open the configured byte stream (injected or serial).
    fn connect_stream(&mut self, cfg: &ConnectConfig) -> Result<Box<dyn ElmStream>> {
        if let Some(s) = self.injected.take() {
            return Ok(s);
        }
        let path = cfg
            .options
            .get("port")
            .map(String::as_str)
            .unwrap_or(DEFAULT_PORT);
        let baud: u32 = cfg
            .options
            .get("baud")
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_BAUD);
        let port = serialport::new(path, baud)
            .timeout(Duration::from_millis(READ_CHUNK_MS))
            .open()
            .map_err(|e| Error::Config(format!("cannot open ELM327 port {path}: {e}")))?;
        Ok(Box::new(SerialElm { port }))
    }

    /// Re-read the adapter identity live (`ATI` + `AT@1`).
    pub fn identify(&mut self) -> Result<AdapterIdentity> {
        let t = self.transport()?;
        let version = t.at("ATI")?;
        let device = t.at("AT@1")?;
        Ok(AdapterIdentity { version, device })
    }

    /// Enter a diagnostic session (`DiagnosticSessionControl`, service 0x10).
    ///
    /// Common sub-functions: 0x01 default, 0x03 extended. Reading DTCs and DIDs
    /// usually works in the default session; some ECUs gate extended data behind
    /// 0x03.
    pub fn diagnostic_session(&mut self, session: u8) -> Result<()> {
        let t = self.transport()?;
        UdsClient::new(t).session(session).map(|_| ())
    }

    /// Send `TesterPresent` (service 0x3E) to keep a non-default session alive.
    pub fn tester_present(&mut self) -> Result<()> {
        let t = self.transport()?;
        UdsClient::new(t).tester_present()
    }

    /// Raw UDS/OBD request helper: send the service bytes, validate the positive
    /// response id, and return the response payload after the service id.
    fn service(&mut self, req: &[u8]) -> Result<Vec<u8>> {
        let t = self.transport()?;
        let resp = t.request(req, REQUEST_TIMEOUT_MS)?;
        check_response(req[0], &resp)?;
        Ok(resp[1..].to_vec())
    }

    /// Read stored DTCs via UDS `ReadDTCInformation` (service 0x19), sub-function
    /// 0x02 (`reportDTCByStatusMask`) with mask 0xFF, decoded to codes and
    /// descriptions via `opentune-dtc`.
    ///
    /// Each returned [`DtcRecord`] carries the decoded [`Dtc`] and its ISO 14229-1
    /// status byte.
    pub fn read_dtcs(&mut self) -> Result<Vec<DtcRecord>> {
        // Response body (after 0x59): [subfunction echo, DTCStatusAvailabilityMask,
        // then 4-byte records]. Strip the first two bytes before decoding.
        let body = self.service(&[0x19, 0x02, 0xFF])?;
        if body.len() < 2 {
            return Ok(Vec::new());
        }
        Ok(decode_uds(&body[2..]))
    }

    /// Read stored DTCs via generic OBD-II **mode 0x03**, a fallback for ECUs
    /// that do not implement UDS `ReadDTCInformation`.
    ///
    /// Mode 0x03 carries no per-DTC status byte, so this returns bare [`Dtc`]s.
    pub fn read_obd_dtcs(&mut self) -> Result<Vec<Dtc>> {
        let body = self.service(&[0x03])?;
        Ok(decode_obd_dtcs(&body))
    }

    /// Clear stored DTCs and freeze-frame data via UDS
    /// `ClearDiagnosticInformation` (service 0x14) for all groups (0xFFFFFF).
    ///
    /// This is the one state-changing operation the driver exposes; it clears
    /// emissions/diagnostic memory only and performs no reprogramming.
    pub fn clear_dtcs(&mut self) -> Result<()> {
        self.service(&[0x14, 0xFF, 0xFF, 0xFF]).map(|_| ())
    }

    /// Read a generic OBD-II **mode 0x01** live PID, returning the data bytes
    /// after the echoed PID (for a 4-byte PID like RPM, two bytes `[A, B]`).
    pub fn read_pid(&mut self, pid: u8) -> Result<Vec<u8>> {
        let body = self.service(&[0x01, pid])?;
        match body.split_first() {
            Some((&echo, rest)) if echo == pid => Ok(rest.to_vec()),
            _ => Err(Error::Protocol(format!(
                "mode 01 PID {pid:02X}: response did not echo the PID"
            ))),
        }
    }

    /// Engine speed in rpm from PID 0x0C (`((A*256)+B)/4`).
    pub fn live_rpm(&mut self) -> Result<f32> {
        let d = self.read_pid(0x0C)?;
        if d.len() < 2 {
            return Err(Error::Protocol("short RPM PID response".into()));
        }
        Ok(f32::from((u16::from(d[0]) << 8) | u16::from(d[1])) / 4.0)
    }

    /// Engine coolant temperature in °C from PID 0x05 (`A - 40`).
    pub fn live_coolant_celsius(&mut self) -> Result<i16> {
        let d = self.read_pid(0x05)?;
        let a = *d
            .first()
            .ok_or_else(|| Error::Protocol("short coolant PID".into()))?;
        Ok(i16::from(a) - 40)
    }

    /// Vehicle speed in km/h from PID 0x0D (`A`).
    pub fn live_speed_kph(&mut self) -> Result<u8> {
        let d = self.read_pid(0x0D)?;
        d.first()
            .copied()
            .ok_or_else(|| Error::Protocol("short speed PID".into()))
    }
}

/// Decode a generic OBD-II mode 0x03 payload into DTCs.
///
/// Over ISO 15765-4 the response after the 0x43 service id is an optional
/// one-byte DTC count followed by 2 bytes per DTC. An odd length means the count
/// byte is present and is skipped; all-zero pairs (padding) are dropped. The two
/// DTC bytes share the ISO 15031-6 first-byte layout used by UDS, so they decode
/// through the same `opentune-dtc` formatter.
fn decode_obd_dtcs(data: &[u8]) -> Vec<Dtc> {
    let pairs = if data.len() % 2 == 1 {
        &data[1..]
    } else {
        data
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < pairs.len() {
        let (hi, lo) = (pairs[i], pairs[i + 1]);
        i += 2;
        if hi == 0 && lo == 0 {
            continue; // padding
        }
        let code = format_code(hi, lo, 0);
        out.push(lookup(&code).unwrap_or_else(|| Dtc {
            code,
            description: "Manufacturer-specific / unknown code".to_string(),
            system: System::Powertrain,
        }));
    }
    out
}

/// UDS DIDs read for identity, mapped to [`EcuId`] fields. VIN is 0xF190; the
/// others are the common ISO 14229 identification DIDs.
const ID_DIDS: &[(u16, &str)] = &[
    (0xF190, "vin"),
    (0xF194, "sw_number"),
    (0xF191, "hw_number"),
    (0xF18C, "ecu_serial"),
    (0xF18A, "supplier"),
    (0xF197, "ecu_name"),
];

/// Decode a DID record as text when printable, else as a hex string.
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

impl Interface for Elm327Obd {
    fn info(&self) -> InterfaceInfo {
        InterfaceInfo {
            id: "elm327obd".into(),
            display_name: "ELM327 serial (OBD read diagnostics)".into(),
            firmware: self
                .transport
                .as_ref()
                .map(|t| t.version.clone())
                .filter(|v| !v.is_empty()),
            serial: None,
        }
    }

    /// Passive `ATMA` bus capture; see [`monitor`].
    fn can_monitor(&mut self) -> Option<&mut dyn opentune_core::CanMonitor> {
        Some(&mut self.monitor)
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            can: true,
            battery_voltage: true,
            // Block read via 0x23 or 0x35/0x36/0x37 (see `memory`).
            read_memory: true,
            // No multi-frame transmit: downloads are 5-byte single-frame blocks
            // behind a bench opt-in, which is not a flashing capability.
            write_memory: false,
            ..Default::default()
        }
    }

    fn set_seed_key(&mut self, seed_key: Box<dyn SeedKey>) -> bool {
        Elm327Obd::set_seed_key(self, seed_key);
        true
    }

    /// The lines of [`Elm327Obd::plan_read_memory`] (adapter `AT` pacing and
    /// hex requests, in order). Sends nothing.
    fn plan_read_memory(&mut self, region: MemoryRegion) -> Option<Result<Vec<String>>> {
        Some(
            Elm327Obd::plan_read_memory(self, region)
                .map(|plan| plan.into_iter().map(|p| p.line).collect()),
        )
    }

    /// Expose the UDS/OBD diagnostic transport so the coding/adaptation/auto-scan/
    /// service/logging engines (and the raw-request console) can drive this
    /// adapter. Available once `open()` has brought the ELM327 up.
    fn diag(&mut self) -> Option<&mut dyn DiagTransport> {
        self.transport.as_mut().map(|t| t as &mut dyn DiagTransport)
    }

    fn open(&mut self, cfg: &ConnectConfig) -> Result<()> {
        let bus = CanBus::from_option(cfg.options.get("bus").map(String::as_str));
        let stream = self.connect_stream(cfg)?;
        let mut transport = ElmTransport::new(stream);
        transport.baud = cfg
            .options
            .get("baud")
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_BAUD);
        transport.init(cfg, bus)?;
        self.transport = Some(transport);
        self.bus = bus;
        self.cfg = Some(cfg.clone());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.cfg = None;
        if let Some(mut t) = self.transport.take() {
            let _ = t.at("ATPC"); // protocol close
        }
        Ok(())
    }

    fn battery_millivolts(&mut self) -> Result<Option<u32>> {
        let t = self.transport()?;
        t.battery_mv().map(Some)
    }

    fn read_ecu_id(&mut self) -> Result<EcuId> {
        let t = self.transport()?;
        let mut id = EcuId::default();
        let mut uds = UdsClient::new(t);
        let mut got_any = false;
        for (did, name) in ID_DIDS {
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

    /// Block read: session, SecurityAccess (when a handler is set or the config
    /// requires it), then 0x23 chunks or a 0x35/0x36/0x37 upload. Every request
    /// format is validated against the ELM's 7-byte single-frame limit before
    /// anything is sent.
    fn read_memory(
        &mut self,
        region: MemoryRegion,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<Vec<u8>> {
        let cfg = self.cfg.as_ref().ok_or(Error::NotConnected)?;
        let opts = memory::MemOptions::from_config(cfg, memory::Op::Read)?;
        if region.length == 0 {
            return Ok(Vec::new());
        }
        let t = self.transport.as_mut().ok_or(Error::NotConnected)?;
        let plan = opts.plan_read(region, t.fc_user)?;
        if opts.security_required && self.seed_key.is_none() {
            return Err(memory::missing_handler());
        }
        let stmin = opts.fc_stmin.unwrap_or_else(|| default_stmin(t.baud));
        let paced = t.pace_flow_control(stmin)?;
        let result = self
            .prepare_memory_access(&opts, progress)
            .and_then(|unlocked| {
                let t = self.transport.as_mut().ok_or(Error::NotConnected)?;
                memory::read(t, plan, &opts, unlocked, region, progress)
            });
        if paced {
            if let Some(t) = self.transport.as_mut() {
                t.restore_flow_control();
            }
        }
        result
    }

    /// Bench-only download (0x34/0x36/0x37) in 5-byte single-frame blocks,
    /// refused unless `elm_write=single-frame` is set. **Not recommended**: see
    /// the crate docs. The flash-safety policy (voltage, backup, verify) is the
    /// caller's, as for every driver.
    fn write_memory(
        &mut self,
        region: MemoryRegion,
        data: &[u8],
        progress: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        let cfg = self.cfg.as_ref().ok_or(Error::NotConnected)?;
        let opts = memory::MemOptions::from_config(cfg, memory::Op::Write)?;
        if !opts.bench_write {
            return Err(Error::Unsupported(WRITE_REFUSAL.into()));
        }
        if data.len() as u64 != u64::from(region.length) {
            return Err(Error::Config(format!(
                "write of {} bytes does not match the region length {}",
                data.len(),
                region.length
            )));
        }
        if data.is_empty() {
            return Ok(());
        }
        let (alfid, window) = opts.plan_write(region)?;
        if opts.security_required && self.seed_key.is_none() {
            return Err(memory::missing_handler());
        }
        self.prepare_memory_access(&opts, progress)?;
        let t = self.transport.as_mut().ok_or(Error::NotConnected)?;
        memory::write(t, (alfid, window), &opts, region, data, progress)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// A scripted ELM mock. AT commands are answered automatically (`OK`, or a
    /// canned identity/voltage); each hex *diagnostic* request pops the next
    /// queued reply. Replies are byte strings exactly as an ELM prints them,
    /// terminated by the `>` prompt added here. The log of bytes the driver
    /// writes is shared via [`MockElm::written`] so a test can inspect the
    /// handshake after the mock has been boxed into the driver.
    struct MockElm {
        diag_replies: VecDeque<Vec<u8>>,
        outbox: VecDeque<u8>,
        line: Vec<u8>,
        written: Arc<Mutex<Vec<u8>>>,
        /// AT command prefixes answered with `?` (adapter rejects them).
        reject: Vec<String>,
    }

    impl MockElm {
        fn new(diag_replies: &[&str]) -> Self {
            Self {
                diag_replies: diag_replies.iter().map(|s| s.as_bytes().to_vec()).collect(),
                outbox: VecDeque::new(),
                line: Vec::new(),
                written: Arc::new(Mutex::new(Vec::new())),
                reject: Vec::new(),
            }
        }

        /// A handle to the shared write log.
        fn written(&self) -> Arc<Mutex<Vec<u8>>> {
            Arc::clone(&self.written)
        }

        fn load(&mut self, reply: &[u8]) {
            self.outbox.extend(reply.iter().copied());
            self.outbox.push_back(b'>');
        }

        fn handle_command(&mut self) {
            let cmd = String::from_utf8_lossy(&self.line).trim().to_string();
            self.line.clear();
            if cmd.is_empty() {
                return;
            }
            let up = cmd.to_uppercase();
            if up.starts_with("AT") {
                let reply: &[u8] = if self.reject.iter().any(|r| up.starts_with(r.as_str())) {
                    b"?"
                } else if up.starts_with("ATI") || up == "ATZ" {
                    b"ELM327 v1.5"
                } else if up.starts_with("AT@1") {
                    b"OBDII to RS232 Interpreter"
                } else if up.starts_with("ATRV") {
                    b"12.3V"
                } else {
                    b"OK"
                };
                let reply = reply.to_vec();
                self.load(&reply);
            } else {
                // Hex diagnostic request: pop the next scripted reply.
                let reply = self
                    .diag_replies
                    .pop_front()
                    .unwrap_or_else(|| b"NO DATA".to_vec());
                self.load(&reply);
            }
        }
    }

    impl ElmStream for MockElm {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            match self.outbox.pop_front() {
                Some(b) => {
                    buf[0] = b;
                    Ok(1)
                }
                None => Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, "idle")),
            }
        }
        fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
            self.written.lock().unwrap().extend_from_slice(buf);
            for &b in buf {
                if b == b'\r' {
                    self.handle_command();
                } else {
                    self.line.push(b);
                }
            }
            Ok(())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn open_mock(diag_replies: &[&str]) -> Elm327Obd {
        let mut d = Elm327Obd::with_stream(Box::new(MockElm::new(diag_replies)));
        d.open(&ConnectConfig::default()).unwrap();
        d
    }

    /// Run the full `open` over a mock and return the captured write log as text.
    fn handshake_log(cfg: &ConnectConfig) -> String {
        let mock = MockElm::new(&[]);
        let written = mock.written();
        let mut d = Elm327Obd::with_stream(Box::new(mock));
        d.open(cfg).unwrap();
        let bytes = written.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn init_handshake_captures_identity_and_sends_hs_can_commands() {
        let log = handshake_log(&ConnectConfig::default());
        // Core init + HS-CAN protocol + ISO-TP framing + flow control + headers.
        for expect in [
            "ATZ",
            "ATE0",
            "ATL0",
            "ATS0",
            "ATAT1",
            "ATSP6",
            "ATCAF1",
            "ATSH 7E0",
            "ATCRA 7E8",
            "ATFCSH 7E0",
            "ATFCSD 30 00 00",
            "ATFCSM 1",
            "ATST 64",
        ] {
            assert!(log.contains(expect), "handshake missing {expect}: {log}");
        }
        // HS-CAN must not program protocol B.
        assert!(!log.contains("ATSPB"));
        assert!(!log.contains("ATPB"));
    }

    #[test]
    fn init_handshake_ms_can_programs_protocol_b() {
        let mut cfg = ConnectConfig::default();
        cfg.options.insert("bus".into(), "ms".into());
        let log = handshake_log(&cfg);
        assert!(
            log.contains("ATSPB"),
            "MS-CAN must select protocol B: {log}"
        );
        // Default protocol-B programming: options 0xC1, divisor 0x04 (=125 kbit).
        assert!(log.contains("ATPB C1 04"), "MS-CAN PB programming: {log}");
    }

    #[test]
    fn open_captures_adapter_version() {
        let d = open_mock(&[]);
        assert_eq!(d.info().firmware.as_deref(), Some("ELM327 v1.5"));
        assert_eq!(d.bus(), CanBus::HsCan);
    }

    #[test]
    fn read_vin_over_isotp_multiframe() {
        // 22 F1 90 -> 62 F1 90 + "WVWZZZ1KZAW000001" (20 bytes) as an ELM
        // multi-frame reply, then NO DATA for the next DID to stop identity scan.
        let vin_mf = "014\r0:62F190575657\r1:5A5A5A314B5A41\r2:573030303030 31";
        let mut d = open_mock(&[vin_mf, "NO DATA"]);
        let id = d.read_ecu_id().unwrap();
        assert_eq!(id.vin.as_deref(), Some("WVWZZZ1KZAW000001"));
    }

    #[test]
    fn read_dtcs_decodes_uds_records() {
        // 19 02 FF -> 59 02 FF + P0101(status 08) + U0101(status 2F).
        let reply = "5902FF01010008C101002F";
        let mut d = open_mock(&[reply]);
        let dtcs = d.read_dtcs().unwrap();
        assert_eq!(dtcs.len(), 2);
        assert_eq!(dtcs[0].dtc.code, "P0101");
        assert_eq!(dtcs[0].status, 0x08);
        assert_eq!(dtcs[1].dtc.code, "U0101");
        assert_eq!(dtcs[1].status, 0x2F);
        assert!(dtcs[0]
            .dtc
            .description
            .to_lowercase()
            .contains("mass or volume air"));
    }

    #[test]
    fn read_obd_mode03_dtcs_fallback() {
        // 03 -> 43 <count=02> P0101 U0101.
        let reply = "43020101C101";
        let mut d = open_mock(&[reply]);
        let dtcs = d.read_obd_dtcs().unwrap();
        assert_eq!(
            dtcs.iter().map(|d| d.code.as_str()).collect::<Vec<_>>(),
            vec!["P0101", "U0101"]
        );
    }

    #[test]
    fn live_rpm_decodes_pid() {
        // 01 0C -> 41 0C 1A F8 => (0x1AF8)/4 = 1726 rpm.
        let mut d = open_mock(&["410C1AF8"]);
        assert_eq!(d.live_rpm().unwrap(), 1726.0);
    }

    #[test]
    fn live_coolant_and_speed_decode() {
        // Coolant 01 05 -> 41 05 5A => 0x5A-40 = 50 C. Speed 01 0D -> 41 0D 64 => 100.
        let mut d = open_mock(&["41055A", "410D64"]);
        assert_eq!(d.live_coolant_celsius().unwrap(), 50);
        assert_eq!(d.live_speed_kph().unwrap(), 100);
    }

    #[test]
    fn battery_voltage_parsed() {
        let mut d = open_mock(&[]);
        assert_eq!(d.battery_millivolts().unwrap(), Some(12_300));
    }

    #[test]
    fn identify_reads_version_and_device() {
        let mut d = open_mock(&[]);
        let idn = d.identify().unwrap();
        assert_eq!(idn.version, "ELM327 v1.5");
        assert_eq!(idn.device, "OBDII to RS232 Interpreter");
    }

    #[test]
    fn negative_response_surfaces_as_error() {
        // 19 02 FF -> 7F 19 31 (requestOutOfRange).
        let mut d = open_mock(&["7F1931"]);
        assert!(matches!(
            d.read_dtcs(),
            Err(Error::NegativeResponse {
                service: 0x19,
                code: 0x31,
                ..
            })
        ));
    }

    #[test]
    fn parse_single_and_multiframe() {
        let one = elm::parse_block("410C1AF8\r", "010C").unwrap();
        assert_eq!(one.response, Some(vec![0x41, 0x0C, 0x1A, 0xF8]));
        let mf = "014\r0:62F19031323334\r1:35363738393031\r2:323334353637\r";
        let out = elm::parse_block(mf, "22F190").unwrap().response.unwrap();
        assert_eq!(out.len(), 20);
        assert_eq!(&out[..3], &[0x62, 0xF1, 0x90]);
    }

    #[test]
    fn parse_rejects_error_strings() {
        assert!(elm::parse_block("NO DATA\r", "0100").is_err());
        assert!(elm::parse_block("CAN ERROR\r", "0100").is_err());
    }

    #[test]
    fn response_pending_then_final_in_one_prompt_block() {
        // Genuine ELM: `7F 19 78` then the real answer before one prompt.
        let mut d = open_mock(&["7F1978\r5902FF01010008\r"]);
        let dtcs = d.read_dtcs().unwrap();
        assert_eq!(dtcs.len(), 1);
        assert_eq!(dtcs[0].dtc.code, "P0101");
    }

    #[test]
    fn response_pending_then_multiframe_vin() {
        let vin = "7F2278\r014\r0:62F190575657\r1:5A5A5A314B5A41\r2:57303030303031";
        let mut d = open_mock(&[vin, "NO DATA"]);
        let id = d.read_ecu_id().unwrap();
        assert_eq!(id.vin.as_deref(), Some("WVWZZZ1KZAW000001"));
    }

    #[test]
    fn truncated_multiframe_is_rejected() {
        // Length 0x14 but only 10 bytes before the prompt.
        let mut d = open_mock(&["014\r0:62F19031323334\r1:353637"]);
        let t = d.diag().unwrap();
        let err = t.request(&[0x22, 0xF1, 0x90], 300).unwrap_err();
        assert!(
            matches!(&err, Error::Protocol(m) if m.contains("truncated")),
            "{err:?}"
        );
    }

    #[test]
    fn oversized_request_rejected_before_writing() {
        let mock = MockElm::new(&[]);
        let written = mock.written();
        let mut d = Elm327Obd::with_stream(Box::new(mock));
        d.open(&ConnectConfig::default()).unwrap();
        let before = written.lock().unwrap().len();
        let req = [0x2E, 0xF1, 0x90, 1, 2, 3, 4, 5];
        let err = d.diag().unwrap().request(&req, 300).unwrap_err();
        assert!(matches!(err, Error::Unsupported(_)), "{err:?}");
        assert_eq!(written.lock().unwrap().len(), before, "nothing written");
    }

    #[test]
    fn question_mark_and_bus_busy_are_errors() {
        let mut d = open_mock(&["?", "BUS BUSY", "ERR94"]);
        for _ in 0..3 {
            let err = d.diag().unwrap().request(&[0x01, 0x0C], 300).unwrap_err();
            assert!(matches!(err, Error::Protocol(_)), "{err:?}");
        }
    }

    #[test]
    fn adapter_reset_mid_session_is_reported() {
        let mut d = open_mock(&["\r\rELM327 v1.5", "010C\r410C1AF8"]);
        let err = d.diag().unwrap().request(&[0x01, 0x0C], 300).unwrap_err();
        assert!(matches!(err, Error::AdapterReset(_)), "{err:?}");
        // Echo back on (a reset whose banner was missed) is a reset too.
        let err = d.diag().unwrap().request(&[0x01, 0x0C], 300).unwrap_err();
        assert!(matches!(err, Error::AdapterReset(_)), "{err:?}");
    }

    #[test]
    fn echo_off_is_sent_before_identity_queries() {
        let log = handshake_log(&ConnectConfig::default());
        let pos = |c: &str| log.find(c).unwrap_or_else(|| panic!("{c} missing: {log}"));
        assert!(pos("ATZ") < pos("ATE0"));
        assert!(pos("ATE0") < pos("ATI"));
        assert!(pos("ATE0") < pos("AT@1"));
    }

    #[test]
    fn atcra_rejected_falls_back_to_filter_and_mask() {
        let mut mock = MockElm::new(&[]);
        mock.reject = vec!["ATCRA".into()];
        let written = mock.written();
        let mut d = Elm327Obd::with_stream(Box::new(mock));
        d.open(&ConnectConfig::default()).unwrap();
        let log = String::from_utf8(written.lock().unwrap().clone()).unwrap();
        assert!(
            log.contains("ATCF 7E8") && log.contains("ATCM 7FF"),
            "{log}"
        );
    }

    #[test]
    fn atcra_and_fallback_rejected_fails_init() {
        let mut mock = MockElm::new(&[]);
        mock.reject = vec!["ATCRA".into(), "ATCF".into()];
        let mut d = Elm327Obd::with_stream(Box::new(mock));
        assert!(d.open(&ConnectConfig::default()).is_err());
    }

    #[test]
    fn bus_option_parsing() {
        assert_eq!(CanBus::from_option(None), CanBus::HsCan);
        assert_eq!(CanBus::from_option(Some("hs")), CanBus::HsCan);
        assert_eq!(CanBus::from_option(Some("MS")), CanBus::MsCan);
        assert_eq!(CanBus::from_option(Some("mscan")), CanBus::MsCan);
    }

    #[test]
    fn decode_obd_dtcs_handles_even_and_padding() {
        // No count byte (even), with a trailing all-zero pad pair.
        assert_eq!(
            decode_obd_dtcs(&[0x01, 0x01, 0x00, 0x00])
                .iter()
                .map(|d| d.code.clone())
                .collect::<Vec<_>>(),
            vec!["P0101"]
        );
    }

    // --- Block read/write ---------------------------------------------------

    /// DEMO ONLY - not an OEM algorithm. XOR 0x5A then +1 per byte.
    struct DemoKey;
    impl SeedKey for DemoKey {
        fn id(&self) -> &str {
            "demo-xor-add"
        }
        fn compute(&self, seed: &[u8], _level: u8) -> Result<Vec<u8>> {
            Ok(seed.iter().map(|b| (b ^ 0x5A).wrapping_add(1)).collect())
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02X}")).collect()
    }

    /// A response as an ELM prints it with `ATCAF1 ATH0 ATS0`: single frame as
    /// plain hex, multi-frame as a 3-digit length line plus `N:` frame lines.
    fn elm_reply(bytes: &[u8]) -> String {
        if bytes.len() <= 7 {
            return hex(bytes);
        }
        let mut s = format!("{:03X}\r0:{}\r", bytes.len(), hex(&bytes[..6]));
        for (i, c) in bytes[6..].chunks(7).enumerate() {
            s.push_str(&format!("{:X}:{}\r", (i + 1) & 0x0F, hex(c)));
        }
        s
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 + 3) as u8).collect()
    }

    fn cfg_with(pairs: &[(&str, &str)]) -> ConnectConfig {
        let mut c = ConnectConfig::default();
        for (k, v) in pairs {
            c.options.insert((*k).into(), (*v).into());
        }
        c
    }

    /// Open over a mock with scripted diagnostic replies; returns the driver
    /// and the shared write log.
    fn open_with(
        replies: &[String],
        cfg: &ConnectConfig,
        reject: &[&str],
    ) -> (Elm327Obd, Arc<Mutex<Vec<u8>>>) {
        let refs: Vec<&str> = replies.iter().map(String::as_str).collect();
        let mut mock = MockElm::new(&refs);
        mock.reject = reject.iter().map(|s| s.to_string()).collect();
        let written = mock.written();
        let mut d = Elm327Obd::with_stream(Box::new(mock));
        d.open(cfg).unwrap();
        written.lock().unwrap().clear(); // keep only post-open traffic
        (d, written)
    }

    /// Every line written since open, in order.
    fn lines(written: &Arc<Mutex<Vec<u8>>>) -> Vec<String> {
        String::from_utf8(written.lock().unwrap().clone())
            .unwrap()
            .split('\r')
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// Only the hex diagnostic requests (what goes on the bus).
    fn bus_requests(written: &Arc<Mutex<Vec<u8>>>) -> Vec<String> {
        lines(written)
            .into_iter()
            .filter(|l| !l.starts_with("AT"))
            .collect()
    }

    fn seed_reply() -> String {
        "671111223344".to_string()
    }

    fn demo_key_req() -> String {
        let key: Vec<u8> = [0x11u8, 0x22, 0x33, 0x44]
            .iter()
            .map(|b| (b ^ 0x5A).wrapping_add(1))
            .collect();
        format!("2712{}", hex(&key))
    }

    #[test]
    fn capabilities_report_block_read_but_not_flashing() {
        let caps = Elm327Obd::new().capabilities();
        assert!(caps.read_memory);
        assert!(!caps.write_memory);
        assert!(!caps.bench_boot);
    }

    #[test]
    fn trait_object_accepts_a_seed_key() {
        let mut d: Box<dyn Interface> = Box::new(Elm327Obd::new());
        assert!(d.set_seed_key(Box::new(DemoKey)));
    }

    #[test]
    fn rmba_read_runs_session_and_security_then_chunks_and_reassembles() {
        let image = pattern(300);
        let mut replies = vec!["5003003201F4".to_string(), seed_reply(), "6712".to_string()];
        for c in image.chunks(0x80) {
            let mut r = vec![0x63];
            r.extend_from_slice(c);
            replies.push(elm_reply(&r));
        }
        let cfg = cfg_with(&[("seedkey", "demo-xor-add")]);
        let (mut d, written) = open_with(&replies, &cfg, &[]);
        d.set_seed_key(Box::new(DemoKey));
        let mut seen = Vec::new();
        let got = d
            .read_memory(MemoryRegion::new(0x0001_0000, 300), &mut |p| seen.push(p))
            .unwrap();
        assert_eq!(got, image);
        assert_eq!(
            bus_requests(&written),
            vec![
                "1003".to_string(),
                "2711".to_string(),
                demo_key_req(),
                "23140001000080".to_string(),
                "23140001008080".to_string(),
                "2314000101002C".to_string(),
            ]
        );
        // Every bus request fits one ELM single frame.
        assert!(bus_requests(&written)
            .iter()
            .all(|r| r.len() <= 2 * elm::MAX_CAN_REQUEST));
        // Paced flow control around the read, restored afterwards.
        let all = lines(&written);
        assert_eq!(all.first().map(String::as_str), Some("ATFCSD 30 00 06"));
        assert_eq!(all.last().map(String::as_str), Some("ATFCSD 30 00 00"));
        // Progress: security reported, then per chunk up to the total.
        assert!(seen.iter().any(|p| p.phase == Phase::SecurityAccess));
        let reads: Vec<u64> = seen
            .iter()
            .filter(|p| p.phase == Phase::Read)
            .map(|p| p.done)
            .collect();
        assert_eq!(reads, vec![0, 128, 256, 300]);
        assert!(seen
            .iter()
            .all(|p| p.phase != Phase::Read || p.total == 300));
    }

    #[test]
    fn plan_read_memory_matches_what_read_sends_and_sends_nothing() {
        let image = pattern(0x90);
        let mut replies = vec!["5003".to_string()];
        for c in image.chunks(0x80) {
            let mut r = vec![0x63];
            r.extend_from_slice(c);
            replies.push(elm_reply(&r));
        }
        let (mut d, written) = open_with(&replies, &ConnectConfig::default(), &[]);
        let region = MemoryRegion::new(0x8000_0000, 0x90);
        let plan = d.plan_read_memory(region).unwrap();
        assert!(lines(&written).is_empty(), "a dry run writes nothing");
        // The trait method (what a registry-opened driver offers) gives the same lines.
        let via_trait = Interface::plan_read_memory(&mut d, region)
            .expect("elm327obd can plan")
            .unwrap();
        assert!(
            lines(&written).is_empty(),
            "the trait dry run writes nothing"
        );
        d.read_memory(region, &mut |_| {}).unwrap();
        let planned: Vec<String> = plan.iter().map(|p| p.line.clone()).collect();
        assert_eq!(planned, via_trait);
        assert_eq!(planned, lines(&written));
    }

    #[test]
    fn invalid_key_nrc_35_stops_without_retry() {
        let replies = ["5003".to_string(), seed_reply(), "7F2735".to_string()];
        let (mut d, written) = open_with(&replies, &cfg_with(&[("seedkey", "x")]), &[]);
        d.set_seed_key(Box::new(DemoKey));
        let err = d
            .read_memory(MemoryRegion::new(0, 16), &mut |_| {})
            .unwrap_err();
        assert!(matches!(err, Error::SecurityDenied), "{err:?}");
        assert_eq!(
            bus_requests(&written),
            vec!["1003".to_string(), "2711".to_string(), demo_key_req()],
            "one key attempt, no read after a rejected key"
        );
    }

    #[test]
    fn security_delay_nrc_37_is_reported_not_retried() {
        let replies = ["5003".to_string(), "7F2737".to_string()];
        let (mut d, written) = open_with(&replies, &cfg_with(&[("seedkey", "x")]), &[]);
        d.set_seed_key(Box::new(DemoKey));
        let err = d
            .read_memory(MemoryRegion::new(0, 16), &mut |_| {})
            .unwrap_err();
        assert!(
            matches!(&err, Error::Protocol(m) if m.contains("0x37") && m.contains("does not retry")),
            "{err:?}"
        );
        assert_eq!(
            bus_requests(&written),
            vec!["1003".to_string(), "2711".to_string()]
        );
    }

    #[test]
    fn missing_seed_key_is_refused_before_anything_is_sent() {
        let (mut d, written) = open_with(&[], &cfg_with(&[("security", "required")]), &[]);
        let err = d
            .read_memory(MemoryRegion::new(0, 16), &mut |_| {})
            .unwrap_err();
        assert!(
            matches!(&err, Error::Config(m) if m.contains("seed/key")),
            "{err:?}"
        );
        assert!(lines(&written).is_empty(), "{:?}", lines(&written));
    }

    #[test]
    fn security_denied_without_handler_explains_what_is_missing() {
        let replies = ["5003".to_string(), "7F2333".to_string()];
        let (mut d, _) = open_with(&replies, &ConnectConfig::default(), &[]);
        let err = d
            .read_memory(MemoryRegion::new(0, 16), &mut |_| {})
            .unwrap_err();
        assert!(
            matches!(&err, Error::Config(m) if m.contains("0x33") && m.contains("set_seed_key")),
            "{err:?}"
        );
    }

    #[test]
    fn rmba_not_supported_suggests_upload() {
        let replies = ["5003".to_string(), "7F2311".to_string()];
        let (mut d, _) = open_with(&replies, &ConnectConfig::default(), &[]);
        let err = d
            .read_memory(MemoryRegion::new(0, 16), &mut |_| {})
            .unwrap_err();
        assert!(
            matches!(&err, Error::Unsupported(m) if m.contains("read_mode=upload")),
            "{err:?}"
        );
    }

    #[test]
    fn no_data_mid_read_names_the_address() {
        let mut first = vec![0x63];
        first.extend(pattern(0x20));
        let replies = [elm_reply(&first), "NO DATA".to_string()];
        let (mut d, _) = open_with(&replies, &cfg_with(&[("session", "none")]), &["ATFCSD"]);
        let err = d
            .read_memory(MemoryRegion::new(0x1000, 0x40), &mut |_| {})
            .unwrap_err();
        assert!(
            matches!(&err, Error::Protocol(m) if m.contains("0x00001020") && m.contains("32 of 64")),
            "{err:?}"
        );
    }

    #[test]
    fn without_user_flow_control_reads_use_small_chunks_and_no_pacing() {
        let image = pattern(0x30);
        let mut replies = Vec::new();
        for c in image.chunks(0x20) {
            let mut r = vec![0x63];
            r.extend_from_slice(c);
            replies.push(elm_reply(&r));
        }
        let (mut d, written) = open_with(&replies, &cfg_with(&[("session", "none")]), &["ATFCSD"]);
        let got = d
            .read_memory(MemoryRegion::new(0x2000, 0x30), &mut |_| {})
            .unwrap();
        assert_eq!(got, image);
        assert_eq!(lines(&written), vec!["23140000200020", "23140000202010"]);
    }

    #[test]
    fn upload_path_uses_35_36_37() {
        let image = pattern(20);
        let mut b1 = vec![0x76, 0x01];
        b1.extend_from_slice(&image[..14]);
        let mut b2 = vec![0x76, 0x02];
        b2.extend_from_slice(&image[14..]);
        let replies = [
            "75200010".to_string(),
            elm_reply(&b1),
            elm_reply(&b2),
            "77".to_string(),
        ];
        let cfg = cfg_with(&[("read_mode", "upload"), ("session", "none")]);
        let (mut d, written) = open_with(&replies, &cfg, &[]);
        let mut done = Vec::new();
        let got = d
            .read_memory(MemoryRegion::new(0x8000, 20), &mut |p| {
                if p.phase == Phase::Read {
                    done.push(p.done)
                }
            })
            .unwrap();
        assert_eq!(got, image);
        assert_eq!(
            bus_requests(&written),
            vec!["35002280000014", "3601", "3602", "37"]
        );
        assert_eq!(done, vec![0, 14, 20]);
    }

    #[test]
    fn upload_block_counter_wraps_ff_to_00() {
        // maxNumberOfBlockLength 3 => one data byte per block => 300 blocks.
        let image = pattern(300);
        let mut replies = vec!["751003".to_string()];
        let mut seq = 1u8;
        for b in &image {
            replies.push(hex(&[0x76, seq, *b]));
            seq = seq.wrapping_add(1);
        }
        replies.push("77".to_string());
        let cfg = cfg_with(&[("read_mode", "upload"), ("session", "none")]);
        let (mut d, written) = open_with(&replies, &cfg, &[]);
        let got = d
            .read_memory(MemoryRegion::new(0x0100, 300), &mut |_| {})
            .unwrap();
        assert_eq!(got, image);
        let reqs = bus_requests(&written);
        let ff = reqs.iter().position(|r| r == "36FF").unwrap();
        assert_eq!(reqs[ff + 1], "3600");
        assert_eq!(reqs[ff + 2], "3601");
    }

    #[test]
    fn upload_of_a_32_bit_address_is_refused_before_sending() {
        let cfg = cfg_with(&[("read_mode", "upload")]);
        let (mut d, written) = open_with(&[], &cfg, &[]);
        let err = d
            .read_memory(MemoryRegion::new(0x8000_0000, 0x10), &mut |_| {})
            .unwrap_err();
        assert!(matches!(err, Error::Unsupported(_)), "{err:?}");
        assert!(lines(&written).is_empty());
    }

    #[test]
    fn write_is_refused_without_the_bench_opt_in() {
        let (mut d, written) = open_with(&[], &ConnectConfig::default(), &[]);
        let err = d
            .write_memory(MemoryRegion::new(0x4000, 4), &[1, 2, 3, 4], &mut |_| {})
            .unwrap_err();
        assert!(
            matches!(&err, Error::Unsupported(m) if m.contains("not recommended")),
            "{err:?}"
        );
        assert!(lines(&written).is_empty());
    }

    #[test]
    fn bench_write_downloads_in_single_frame_blocks() {
        let data = pattern(12);
        let replies = [
            "5002".to_string(),
            seed_reply(),
            "6712".to_string(),
            "74200FFF".to_string(),
            "7601".to_string(),
            "7602".to_string(),
            "7603".to_string(),
            "77".to_string(),
        ];
        let cfg = cfg_with(&[("elm_write", "single-frame"), ("seedkey", "demo")]);
        let (mut d, written) = open_with(&replies, &cfg, &[]);
        d.set_seed_key(Box::new(DemoKey));
        let mut done = Vec::new();
        d.write_memory(MemoryRegion::new(0x4000, 12), &data, &mut |p| {
            if p.phase == Phase::Write {
                done.push(p.done)
            }
        })
        .unwrap();
        assert_eq!(
            bus_requests(&written),
            vec![
                "1002".to_string(),
                "2711".to_string(),
                demo_key_req(),
                "3400224000000C".to_string(),
                format!("3601{}", hex(&data[..5])),
                format!("3602{}", hex(&data[5..10])),
                format!("3603{}", hex(&data[10..])),
                "37".to_string(),
            ]
        );
        assert!(bus_requests(&written)
            .iter()
            .all(|r| r.len() <= 2 * elm::MAX_CAN_REQUEST));
        assert_eq!(done, vec![0, 5, 10, 12]);
    }

    #[test]
    fn bench_write_block_counter_wraps() {
        // 1300 bytes in 5-byte blocks = 260 blocks: counter 01..FF, 00, 01..
        let data = pattern(1300);
        let mut replies = vec!["741007".to_string()];
        let mut seq = 1u8;
        for _ in 0..260 {
            replies.push(hex(&[0x76, seq]));
            seq = seq.wrapping_add(1);
        }
        replies.push("77".to_string());
        let cfg = cfg_with(&[("elm_write", "single-frame"), ("session", "none")]);
        let (mut d, written) = open_with(&replies, &cfg, &[]);
        d.write_memory(MemoryRegion::new(0x4000, 1300), &data, &mut |_| {})
            .unwrap();
        let reqs = bus_requests(&written);
        assert_eq!(reqs[0], "34002240000514");
        let ff = reqs.iter().position(|r| r.starts_with("36FF")).unwrap();
        assert!(reqs[ff + 1].starts_with("3600"));
        assert!(reqs[ff + 2].starts_with("3601"));
        assert_eq!(reqs.last().map(String::as_str), Some("37"));
    }

    #[test]
    fn default_stmin_keeps_the_uart_ahead() {
        assert_eq!(default_stmin(38_400), 6);
        assert_eq!(default_stmin(115_200), 3);
        assert_eq!(default_stmin(9_600), 19);
    }
}
