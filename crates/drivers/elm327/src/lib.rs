//! ELM327 **serial** read-diagnostics driver for OpenTune.
//!
//! This driver targets a genuine or clone ELM327 USB adapter (CH340 serial) for
//! the **READ** side of diagnostics over ISO 15765-4 (UDS/OBD-II on CAN). It is
//! deliberately scoped to reading: identify the ECU, read and decode Diagnostic
//! Trouble Codes, read live OBD-II PIDs, and clear stored DTCs. It performs **no**
//! ECU reprogramming — there is no `WriteDataByIdentifier`, `SecurityAccess`,
//! `RequestDownload`/`TransferData`, or `RoutineControl`. For firmware
//! read/flash over an ELM327 see the sibling crates `opentune-driver-elm327`
//! (K-line/CAN, memory oriented) and `opentune-driver-elm327net` (WiFi/Bluetooth).
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
//! ## Validation status
//! Compiles and is unit-tested against scripted ELM responses (see the tests
//! module); it has **not** yet been validated against real hardware. The MS-CAN
//! protocol-B programming bytes (`ATPB`) in particular are derived from the
//! ELM327 datasheet formula and need bench confirmation on the medium-speed bus.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use opentune_core::protocol::diag::check_response;
use opentune_core::protocol::diag::DiagTransport;
use opentune_core::protocol::uds::UdsClient;
use opentune_core::{
    Capabilities, ConnectConfig, EcuId, Error, Interface, InterfaceInfo, MemoryRegion, Progress,
    Result,
};
use opentune_dtc::{decode_uds, format_code, lookup, Dtc, DtcRecord, System};
use std::io::{Read, Write};
use std::time::{Duration, Instant};

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
}

impl ElmTransport {
    fn new(stream: Box<dyn ElmStream>) -> Self {
        Self {
            stream,
            version: String::new(),
            device: String::new(),
        }
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
        let up = reply.to_uppercase();
        if up.contains('?') || up.contains("ERROR") {
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
    /// Handshake order: reset (`ATZ`), capture identity (`ATI`, `AT@1`), echo off
    /// (`ATE0`), linefeeds off (`ATL0`), spaces off (`ATS0`), headers off
    /// (`ATH0`), adaptive timing (`ATAT1`), protocol select, CAN auto-formatting
    /// (`ATCAF1`), request/receive headers (`ATSH`/`ATCRA`), ISO-TP flow control
    /// (`ATFCSH`/`ATFCSD`/`ATFCSM`) and the response timeout (`ATST`).
    fn init(&mut self, cfg: &ConnectConfig, bus: CanBus) -> Result<()> {
        let _ = self.at("ATZ"); // reset; reply is the banner
        self.version = self.at("ATI").unwrap_or_default();
        self.device = self.at("AT@1").unwrap_or_default();
        self.at_ok("ATE0")?; // echo off
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
        let _ = self.at(&format!("ATCRA {rx:03X}")); // accept only the ECU rx id

        // ISO-TP flow control: answer the ECU's first frame with CTS, 0 block
        // size, 0 separation time, using our tx header. `ATFCSM 1` tells the ELM
        // to use these user-set values rather than auto.
        let _ = self.at(&format!("ATFCSH {tx:03X}"));
        let _ = self.at("ATFCSD 30 00 00");
        let _ = self.at("ATFCSM 1");

        // Response timeout: ATST units are 4 ms; 0x64 => ~400 ms, the ceiling
        // adaptive timing (ATAT1) works under.
        let _ = self.at("ATST 64");
        Ok(())
    }
}

impl DiagTransport for ElmTransport {
    fn request(&mut self, req: &[u8], timeout_ms: u64) -> Result<Vec<u8>> {
        let hex: String = req.iter().map(|b| format!("{b:02X}")).collect();
        self.stream.drain_input();
        self.write_line(&hex)?;

        // Keep reading blocks while the ECU answers responsePending (NRC 0x78).
        for _ in 0..40 {
            let text = self.read_until_prompt(timeout_ms)?;
            let resp = parse_elm_response(&text)?;
            if resp.len() >= 3 && resp[0] == 0x7F && resp[2] == 0x78 {
                continue;
            }
            return Ok(resp);
        }
        Err(Error::Timeout(timeout_ms))
    }
}

/// Parse an ELM327 textual reply into raw diagnostic response bytes.
///
/// Handles single-frame replies (spaced or unspaced hex), CAN multi-frame
/// replies printed with the `NN:` line-counter format (with a leading
/// total-length line), and the common error strings (`NO DATA`, `CAN ERROR`, …).
fn parse_elm_response(text: &str) -> Result<Vec<u8>> {
    let up = text.to_uppercase();
    for err in [
        "NO DATA",
        "UNABLE TO CONNECT",
        "BUS INIT: ...ERROR",
        "BUS INIT: ERROR",
        "BUS ERROR",
        "CAN ERROR",
        "DATA ERROR",
        "BUFFER FULL",
        "FB ERROR",
        "STOPPED",
        "<RX ERROR",
    ] {
        if up.contains(err) {
            return Err(Error::Protocol(format!("ELM: {}", text.trim())));
        }
    }

    // Split on both CR and LF: with linefeeds off (ATL0) the clones delimit
    // frames with a lone '\r', which `str::lines()` would not split.
    let lines: Vec<&str> = text
        .split(['\r', '\n'])
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && *l != "SEARCHING..." && *l != "OK")
        .collect();
    if lines.is_empty() {
        return Err(Error::Protocol("empty ELM response".into()));
    }

    // Multi-frame: at least one line carries an "NN:" frame counter.
    if lines.iter().any(|l| l.contains(':')) {
        let mut total: Option<usize> = None;
        let mut data = Vec::new();
        for l in &lines {
            if let Some((_idx, rest)) = l.split_once(':') {
                data.extend(hex_bytes(rest));
            } else if let Some(tok) = l.split_whitespace().next() {
                // The lone leading line is the total length in hex.
                if let Ok(n) = usize::from_str_radix(tok, 16) {
                    total = Some(n);
                }
            }
        }
        if let Some(n) = total {
            if n <= data.len() {
                data.truncate(n);
            }
        }
        if data.is_empty() {
            return Err(Error::Protocol("no data in ELM multi-frame reply".into()));
        }
        return Ok(data);
    }

    // Single frame (possibly wrapped over several lines): concatenate hex.
    let mut data = Vec::new();
    for l in &lines {
        data.extend(hex_bytes(l));
    }
    if data.is_empty() {
        return Err(Error::Protocol(format!(
            "no hex bytes in ELM reply: {}",
            text.trim()
        )));
    }
    Ok(data)
}

/// Extract hex byte pairs from a line, tolerating spaced or unspaced hex.
fn hex_bytes(s: &str) -> Vec<u8> {
    let nibbles: Vec<u8> = s
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .map(|c| c.to_digit(16).unwrap() as u8)
        .collect();
    nibbles
        .chunks(2)
        .filter(|p| p.len() == 2)
        .map(|p| (p[0] << 4) | p[1])
        .collect()
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

/// Serial ELM327 read-diagnostics driver.
///
/// Opens via [`Interface::open`] with the options documented at the crate root,
/// or in tests via [`Elm327Obd::with_stream`]. Beyond the [`Interface`] trait
/// (identify + battery), the read-side diagnostics live on the inherent methods:
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
}

impl Default for Elm327Obd {
    fn default() -> Self {
        Self {
            transport: None,
            bus: CanBus::HsCan,
            injected: None,
            monitor: monitor::ElmMonitor::new(),
        }
    }
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
            // Read-diagnostics only: no firmware memory read/write here.
            read_memory: false,
            write_memory: false,
            ..Default::default()
        }
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
        transport.init(cfg, bus)?;
        self.transport = Some(transport);
        self.bus = bus;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
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

    /// Unsupported: this is a read-diagnostics driver, not a firmware reader. Use
    /// `opentune-driver-elm327` / `opentune-driver-elm327net` for memory reads.
    fn read_memory(
        &mut self,
        _region: MemoryRegion,
        _progress: &mut dyn FnMut(Progress),
    ) -> Result<Vec<u8>> {
        Err(Error::Unsupported(
            "ELM327 OBD driver reads diagnostics (DTCs/PIDs/IDs), not ECU memory".into(),
        ))
    }

    /// Unsupported: this driver never writes to the ECU.
    fn write_memory(
        &mut self,
        _region: MemoryRegion,
        _data: &[u8],
        _progress: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        Err(Error::Unsupported(
            "ELM327 OBD driver is read-only; it does not write ECU memory".into(),
        ))
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
    }

    impl MockElm {
        fn new(diag_replies: &[&str]) -> Self {
            Self {
                diag_replies: diag_replies.iter().map(|s| s.as_bytes().to_vec()).collect(),
                outbox: VecDeque::new(),
                line: Vec::new(),
                written: Arc::new(Mutex::new(Vec::new())),
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
                let reply: &[u8] = if up.starts_with("ATI") || up == "ATZ" {
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
        assert_eq!(
            parse_elm_response("410C1AF8\r").unwrap(),
            vec![0x41, 0x0C, 0x1A, 0xF8]
        );
        let mf = "014\r0:62F19031323334\r1:35363738393031\r2:323334353637";
        let out = parse_elm_response(mf).unwrap();
        assert_eq!(out.len(), 20);
        assert_eq!(&out[..3], &[0x62, 0xF1, 0x90]);
    }

    #[test]
    fn parse_rejects_error_strings() {
        assert!(parse_elm_response("NO DATA\r").is_err());
        assert!(parse_elm_response("CAN ERROR\r").is_err());
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
}
