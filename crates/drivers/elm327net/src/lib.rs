//! ELM327 driver over **WiFi (TCP)** and **Bluetooth RFCOMM**, for OpenTune.
//!
//! Functionally this is the USB `opentune-driver-elm327` driver with the byte
//! transport swapped out: the ELM327 AT dialect, the hex request framing and the
//! textual reply parser are the same, but instead of a local serial port the
//! bytes travel over a TCP socket (the common "WiFi ELM327" adapters that listen
//! on `192.168.0.10:35000`) or over a Bluetooth RFCOMM serial device.
//!
//! The ELM firmware performs ISO-TP segmentation (CAN auto-formatting) and
//! K-line framing itself, so — like the USB driver — this one implements
//! [`DiagTransport`] directly and lets the fixed `opentune-core` service clients
//! ([`UdsClient`] / [`Kwp2000Client`]) run on top.
//!
//! ## Transport selection ([`ConnectConfig::options`])
//! - `transport`: `tcp` | `wifi` (WiFi), or `bt` | `rfcomm` | `serial` (BT).
//!   When omitted it is inferred: `host` present ⇒ TCP, else `port` ⇒ serial.
//! - WiFi: `host` (default `192.168.0.10`) and `tcp_port` (default `35000`).
//! - Bluetooth: bind the adapter to an RFCOMM device first
//!   (`rfcomm bind /dev/rfcomm0 <MAC> 1`), then pass `port=/dev/rfcomm0`. A
//!   USB-serial ELM327 works through this same path too.
//! - `baud`: serial baud for the BT/serial path (default `38400`).
//!
//! Protocol/`diag_layer`/`tx_id`/`rx_id` are interpreted exactly as in the USB
//! `elm327` driver.
//!
//! Nothing here has been exercised against real hardware; the AT layer is
//! socket-mocked in the unit tests.

use opentune_core::protocol::diag::DiagTransport;
use opentune_core::protocol::kwp2000::Kwp2000Client;
use opentune_core::protocol::seedkey::SeedKey;
use opentune_core::protocol::uds::UdsClient;
use opentune_core::{
    Capabilities, ConnectConfig, DiagLayer, EcuId, Error, Interface, InterfaceInfo, MemoryRegion,
    Phase, Progress, Protocol, Result,
};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// K-line tester (diagnostic source) address.
const TESTER_ADDR: u8 = 0xF1;
/// Per-read socket timeout while assembling one ELM reply.
const READ_CHUNK_MS: u64 = 100;
/// Default WiFi ELM327 endpoint.
const DEFAULT_WIFI_HOST: &str = "192.168.0.10";
const DEFAULT_WIFI_PORT: u16 = 35000;

/// A byte transport for the ELM327: anything that reads and writes bytes and
/// returns a `WouldBlock`/`TimedOut` error (or a zero-length read) when no data
/// is available within its configured timeout. Implemented for TCP and serial.
pub trait ElmStream: Send {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize>;
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()>;
    fn flush(&mut self) -> std::io::Result<()>;
    /// Best-effort discard of any buffered input; a no-op is acceptable.
    fn drain_input(&mut self) {}
}

/// WiFi ELM327 over a TCP socket.
struct TcpElm {
    sock: TcpStream,
}

impl ElmStream for TcpElm {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        Read::read(&mut self.sock, buf)
    }
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        Write::write_all(&mut self.sock, buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Write::flush(&mut self.sock)
    }
}

/// Bluetooth RFCOMM (or any USB-serial) ELM327 over a serial device.
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

/// Owns the byte transport and talks the ELM327 AT dialect. Implements
/// [`DiagTransport`] so the core UDS/KWP clients can drive it.
struct ElmTransport {
    stream: Box<dyn ElmStream>,
    version: String,
}

impl ElmTransport {
    fn new(stream: Box<dyn ElmStream>) -> Self {
        Self {
            stream,
            version: String::new(),
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
        let reply = self.read_until_prompt(2000)?;
        Ok(reply.trim().to_string())
    }

    /// Send an `AT` command and require an `OK`/non-error reply.
    fn at_ok(&mut self, cmd: &str) -> Result<()> {
        let reply = self.at(cmd)?;
        let up = reply.to_uppercase();
        if up.contains('?') || up.contains("ERROR") {
            return Err(Error::Protocol(format!("ELM rejected {cmd}: {reply}")));
        }
        Ok(())
    }

    /// Battery voltage via `ATRV` (e.g. "12.3V").
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

    /// Initialise the adapter and select the protocol/header for `cfg`.
    fn init(&mut self, cfg: &ConnectConfig) -> Result<()> {
        let _ = self.at("ATZ"); // reset; reply is the banner
        self.version = self.at("ATI").unwrap_or_default();
        self.at_ok("ATE0")?; // echo off
        self.at_ok("ATL0")?; // linefeeds off
        let _ = self.at("ATH0"); // headers off (clones may not ack)
        self.at_ok("ATAT1")?; // adaptive timing

        match cfg.protocol {
            Protocol::IsoTp | Protocol::RawCan => {
                let extended = cfg.tx_id > 0x7FF || cfg.rx_id > 0x7FF;
                self.at_ok(if extended { "ATSP7" } else { "ATSP6" })?;
                let _ = self.at("ATCAF1"); // CAN auto-formatting (ISO-TP) on
                self.set_can_header(cfg.tx_id, extended)?;
                if !extended {
                    let _ = self.at(&format!("ATCRA {:03X}", cfg.rx_id));
                }
            }
            Protocol::Iso14230Kline => {
                self.at_ok("ATSP5")?; // ISO14230, fast init
                self.set_kline_header(cfg.tx_id as u8)?;
            }
            Protocol::Iso9141 => {
                self.at_ok("ATSP3")?; // ISO9141-2
                self.set_kline_header(cfg.tx_id as u8)?;
            }
            Protocol::Vwtp20 => {
                return Err(Error::Unsupported("ELM327 cannot do VW TP2.0".into()));
            }
        }
        Ok(())
    }

    fn set_can_header(&mut self, tx_id: u32, extended: bool) -> Result<()> {
        if extended {
            let _ = self.at(&format!("ATCP {:02X}", (tx_id >> 24) & 0x1F));
            self.at_ok(&format!("ATSH {:06X}", tx_id & 0xFF_FFFF))
        } else {
            self.at_ok(&format!("ATSH {:03X}", tx_id & 0x7FF))
        }
    }

    fn set_kline_header(&mut self, target: u8) -> Result<()> {
        self.at_ok(&format!(
            "ATSH {:02X} {:02X} {:02X}",
            0x80, target, TESTER_ADDR
        ))
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

/// Parse an ELM327 textual reply into the raw diagnostic response bytes.
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

    // Split on both CR and LF: WiFi/BT ELM clones with ATL0 (linefeeds off)
    // delimit frames with a lone '\r', which `str::lines()` would not split.
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

/// ELM327-over-network interface driver.
pub struct Elm327NetDriver {
    transport: Option<ElmTransport>,
    diag_layer: DiagLayer,
    cfg: Option<ConnectConfig>,
    /// Optional seed/key handler for security access (service 0x27), used on
    /// both the UDS and KWP2000 paths. No OEM algorithm ships with OpenTune.
    seed_key: Option<Box<dyn SeedKey>>,
    /// A pre-built transport injected for tests, used in place of connecting.
    injected: Option<Box<dyn ElmStream>>,
}

impl Default for Elm327NetDriver {
    fn default() -> Self {
        Self {
            transport: None,
            diag_layer: DiagLayer::Uds,
            cfg: None,
            seed_key: None,
            injected: None,
        }
    }
}

impl Elm327NetDriver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install the seed/key handler used to unlock the ECU (service 0x27)
    /// before a download. Without it, writing a security-protected ECU fails
    /// with a clear error. OpenTune ships no OEM algorithms.
    pub fn set_seed_key(&mut self, seed_key: Box<dyn SeedKey>) {
        self.seed_key = Some(seed_key);
    }

    /// Inject a pre-connected byte stream (e.g. a test mock). `open` then runs
    /// the AT init over it instead of dialing TCP/serial.
    pub fn with_stream(stream: Box<dyn ElmStream>) -> Self {
        let mut d = Self::new();
        d.injected = Some(stream);
        d
    }

    fn transport(&mut self) -> Result<&mut ElmTransport> {
        self.transport.as_mut().ok_or(Error::NotConnected)
    }

    /// Open the configured byte stream (injected, TCP, or serial).
    fn connect_stream(&mut self, cfg: &ConnectConfig) -> Result<Box<dyn ElmStream>> {
        if let Some(s) = self.injected.take() {
            return Ok(s);
        }
        let mode = cfg
            .options
            .get("transport")
            .map(String::as_str)
            .unwrap_or_else(|| {
                if cfg.options.contains_key("host") {
                    "tcp"
                } else if cfg.options.contains_key("port") {
                    "serial"
                } else {
                    "tcp"
                }
            });
        match mode {
            "tcp" | "wifi" => {
                let host = cfg
                    .options
                    .get("host")
                    .map(String::as_str)
                    .unwrap_or(DEFAULT_WIFI_HOST);
                let port: u16 = cfg
                    .options
                    .get("tcp_port")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(DEFAULT_WIFI_PORT);
                let sock = TcpStream::connect((host, port)).map_err(|e| {
                    Error::Config(format!("cannot connect to WiFi ELM327 {host}:{port}: {e}"))
                })?;
                sock.set_read_timeout(Some(Duration::from_millis(READ_CHUNK_MS)))
                    .map_err(Error::Io)?;
                sock.set_nodelay(true).ok();
                Ok(Box::new(TcpElm { sock }))
            }
            "bt" | "rfcomm" | "serial" => {
                let path = cfg.options.get("port").ok_or_else(|| {
                    Error::Config(
                        "Bluetooth/serial ELM327 needs options[\"port\"] (e.g. /dev/rfcomm0)"
                            .into(),
                    )
                })?;
                let baud: u32 = cfg
                    .options
                    .get("baud")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(38_400);
                let port = serialport::new(path, baud)
                    .timeout(Duration::from_millis(READ_CHUNK_MS))
                    .open()
                    .map_err(|e| {
                        Error::Config(format!("cannot open ELM327 serial/RFCOMM {path}: {e}"))
                    })?;
                Ok(Box::new(SerialElm { port }))
            }
            other => Err(Error::Config(format!("unknown transport '{other}'"))),
        }
    }

    /// Run security access (service 0x27) for the active diagnostic layer if a
    /// seed/key handler is set or the config marks security as required.
    fn security_access(&mut self) -> Result<()> {
        let layer = self.diag_layer;
        let required = security_required(self.cfg.as_ref());
        let default_level = match layer {
            DiagLayer::Uds => 0x11,
            DiagLayer::Kwp2000 => 0x01,
        };
        let level = seedkey_level(self.cfg.as_ref(), default_level);
        // Disjoint field borrows: `seed_key` (shared), `transport` (unique).
        let sk = self.seed_key.as_deref();
        let t = self.transport.as_mut().ok_or(Error::NotConnected)?;
        match layer {
            DiagLayer::Uds => uds_security_access(t, sk, level, required),
            DiagLayer::Kwp2000 => kwp_security_access(t, sk, level, required),
        }
    }
}

impl Interface for Elm327NetDriver {
    fn info(&self) -> InterfaceInfo {
        InterfaceInfo {
            id: "elm327net".into(),
            display_name: "ELM327 over WiFi / Bluetooth".into(),
            firmware: self
                .transport
                .as_ref()
                .map(|t| t.version.clone())
                .filter(|v| !v.is_empty()),
            serial: None,
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            can: true,
            kline: true,
            read_memory: true,
            write_memory: true,
            battery_voltage: true,
            ..Default::default()
        }
    }

    fn open(&mut self, cfg: &ConnectConfig) -> Result<()> {
        let stream = self.connect_stream(cfg)?;
        let mut transport = ElmTransport::new(stream);
        transport.init(cfg)?;
        self.transport = Some(transport);
        self.diag_layer = cfg.diag_layer;
        self.cfg = Some(cfg.clone());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        if let Some(mut t) = self.transport.take() {
            let _ = t.at("ATPC"); // protocol close
        }
        self.cfg = None;
        Ok(())
    }

    fn battery_millivolts(&mut self) -> Result<Option<u32>> {
        let t = self.transport()?;
        t.battery_mv().map(Some)
    }

    fn read_ecu_id(&mut self) -> Result<EcuId> {
        let layer = self.diag_layer;
        let t = self.transport()?;
        match layer {
            DiagLayer::Uds => read_uds_ecu_id(t),
            DiagLayer::Kwp2000 => read_kwp_ecu_id(t),
        }
    }

    fn read_memory(
        &mut self,
        region: MemoryRegion,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<Vec<u8>> {
        // A full read from a locked ECU needs security access too; a no-op when
        // no handler is set and security is not required.
        self.security_access()?;
        let layer = self.diag_layer;
        let t = self.transport()?;
        match layer {
            DiagLayer::Uds => uds_read_memory(t, region, progress),
            DiagLayer::Kwp2000 => kwp_read_memory(t, region, progress),
        }
    }

    fn write_memory(
        &mut self,
        region: MemoryRegion,
        data: &[u8],
        progress: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        if data.len() != region.length as usize {
            return Err(Error::Config("data length != region length".into()));
        }
        // Unlock the ECU (service 0x27) before requestDownload if required.
        self.security_access()?;
        let layer = self.diag_layer;
        let t = self.transport()?;
        match layer {
            DiagLayer::Uds => uds_write_memory(t, region, data, progress),
            DiagLayer::Kwp2000 => kwp_write_memory(t, region, data, progress),
        }
    }
}

// --- shared text/flow helpers (fixed opentune-core API) ---------------------

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

fn read_uds_ecu_id(transport: &mut dyn DiagTransport) -> Result<EcuId> {
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

fn uds_read_memory(
    transport: &mut dyn DiagTransport,
    region: MemoryRegion,
    progress: &mut dyn FnMut(Progress),
) -> Result<Vec<u8>> {
    let total = region.length as u64;
    let mut uds = UdsClient::new(transport);
    let _max_block = uds.request_upload(0x00, region.address, region.length)?;
    progress(Progress {
        phase: Phase::Read,
        done: 0,
        total,
    });

    let mut out = Vec::with_capacity(region.length as usize);
    let mut seq: u8 = 1;
    while (out.len() as u32) < region.length {
        let resp = uds.transfer_data(seq, &[])?;
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

fn uds_write_memory(
    transport: &mut dyn DiagTransport,
    region: MemoryRegion,
    data: &[u8],
    progress: &mut dyn FnMut(Progress),
) -> Result<()> {
    let total = data.len() as u64;
    let mut uds = UdsClient::new(transport);
    let max_block = uds.request_download(0x00, region.address, region.length)?;
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

fn read_kwp_ecu_id(transport: &mut dyn DiagTransport) -> Result<EcuId> {
    let mut id = EcuId::default();
    let mut kwp = Kwp2000Client::new(transport);
    let options: &[(u8, &str)] = &[(0x90, "vin"), (0x80, "ecu_id"), (0x9B, "sw_number")];
    let mut got_any = false;
    for (opt, name) in options {
        match kwp.read_ecu_id(*opt) {
            Ok(resp) => {
                got_any = true;
                let data = if resp.is_empty() {
                    &resp[..]
                } else {
                    &resp[1..]
                };
                let value = decode_text(data);
                match *name {
                    "vin" => id.vin = Some(value.clone()),
                    "sw_number" => id.sw_number = Some(value.clone()),
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
        let n = remaining.min(MAX_CHUNK) as u8;
        let block = kwp.read_memory(addr, n)?;
        if block.is_empty() {
            return Err(Error::Protocol("empty readMemoryByAddress block".into()));
        }
        out.extend_from_slice(&block);
        addr = addr.wrapping_add(block.len() as u32);
        remaining = remaining.saturating_sub(block.len() as u32);
        progress(Progress {
            phase: Phase::Read,
            done: out.len().min(region.length as usize) as u64,
            total,
        });
        if block.len() < n as usize {
            break;
        }
    }
    out.truncate(region.length as usize);
    Ok(out)
}

fn kwp_write_memory(
    transport: &mut dyn DiagTransport,
    region: MemoryRegion,
    data: &[u8],
    progress: &mut dyn FnMut(Progress),
) -> Result<()> {
    let total = data.len() as u64;
    let mut kwp = Kwp2000Client::new(transport);
    kwp.request_download(region.address, region.length, 0x00)?;
    progress(Progress {
        phase: Phase::Write,
        done: 0,
        total,
    });

    let mut done = 0u64;
    for chunk in data.chunks(250) {
        kwp.transfer_data(chunk)?;
        done += chunk.len() as u64;
        progress(Progress {
            phase: Phase::Write,
            done,
            total,
        });
    }
    kwp.transfer_exit()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Security access (service 0x27) hook.
//
// OpenTune ships NO OEM seed/key algorithms. A handler is supplied by the
// application via [`Elm327NetDriver::set_seed_key`] (or a plugin), keyed by the
// `seedkey` id in an ECU definition. These helpers run request-seed → compute →
// send-key over the existing service clients before a download. (Kept
// self-contained per the convention of the other OpenTune drivers.)
//
// Config keys from [`ConnectConfig::options`]:
// - `seedkey`:       expected algorithm id; its presence marks security required.
// - `security`:      `required`/`true`/`1`/`yes` marks security required.
// - `seedkey_level`: the request-seed access level (odd), decimal or `0x`-hex.
// ---------------------------------------------------------------------------

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

fn seedkey_level(cfg: Option<&ConnectConfig>, default: u8) -> u8 {
    cfg.and_then(|c| c.options.get("seedkey_level"))
        .and_then(|s| parse_access_level(s))
        .unwrap_or(default)
}

fn parse_access_level(s: &str) -> Option<u8> {
    let t = s.trim();
    match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(h) => u8::from_str_radix(h, 16).ok(),
        None => t.parse().ok(),
    }
}

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
                Ok(())
            }
        }
        Some(sk) => {
            let mut uds = UdsClient::new(transport);
            let seed = uds.request_seed(request_level)?;
            if !seed.is_empty() && seed.iter().all(|&b| b == 0) {
                return Ok(());
            }
            let key = sk.compute(&seed, request_level)?;
            uds.send_key(request_level.wrapping_add(1), &key)
        }
    }
}

fn kwp_security_access(
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
                Ok(())
            }
        }
        Some(sk) => {
            let mut kwp = Kwp2000Client::new(transport);
            let seed = kwp.request_seed(request_level)?;
            if !seed.is_empty() && seed.iter().all(|&b| b == 0) {
                return Ok(());
            }
            let key = sk.compute(&seed, request_level)?;
            kwp.send_key(request_level.wrapping_add(1), &key)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// A mocked byte socket: feeds scripted bytes to reads, records writes, and
    /// returns `WouldBlock` once the scripted bytes are exhausted.
    struct MockStream {
        inbox: VecDeque<u8>,
        pub written: Vec<u8>,
    }

    impl MockStream {
        fn new(script: &[u8]) -> Self {
            Self {
                inbox: script.iter().copied().collect(),
                written: Vec::new(),
            }
        }
    }

    impl ElmStream for MockStream {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            match self.inbox.pop_front() {
                Some(b) => {
                    buf[0] = b;
                    Ok(1)
                }
                None => Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, "idle")),
            }
        }
        fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
            self.written.extend_from_slice(buf);
            Ok(())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn transport_sends_hex_request_and_parses_reply() {
        // ELM reply for 22 F1 90 (headers off): the service bytes then prompt.
        let mut t = ElmTransport::new(Box::new(MockStream::new(b"62 F1 90 31 32 33\r>")));
        let resp = t.request(&[0x22, 0xF1, 0x90], 500).unwrap();
        assert_eq!(resp, vec![0x62, 0xF1, 0x90, 0x31, 0x32, 0x33]);
    }

    #[test]
    fn parse_single_and_multiframe() {
        assert_eq!(
            parse_elm_response("41 00 BE 3F B8 13\r").unwrap(),
            vec![0x41, 0x00, 0xBE, 0x3F, 0xB8, 0x13]
        );
        // Multi-frame with total-length header line and NN: counters.
        let mf = "014\r0: 62 F1 90 31 32 33 34\r1: 35 36 37\r";
        assert_eq!(
            parse_elm_response(mf).unwrap(),
            vec![0x62, 0xF1, 0x90, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37]
        );
    }

    #[test]
    fn parse_rejects_error_strings() {
        assert!(parse_elm_response("NO DATA\r").is_err());
        assert!(parse_elm_response("CAN ERROR\r").is_err());
    }

    /// A scripted diagnostic transport for the seed/key handshake assertions.
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

    /// DEMO ONLY — not a real OEM algorithm. XOR 0x5A then +1 per byte.
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
    fn uds_handshake_sends_computed_key() {
        let seed = [0xDEu8, 0xAD, 0xBE, 0xEF];
        let mut seed_resp = vec![0x67, 0x11];
        seed_resp.extend_from_slice(&seed);
        let mut t = ScriptedTransport {
            replies: VecDeque::from(vec![seed_resp, vec![0x67, 0x12]]),
            requests: Vec::new(),
        };
        uds_security_access(&mut t, Some(&DemoXorAddKey), 0x11, true).unwrap();
        assert_eq!(t.requests[0], vec![0x27, 0x11]);
        let expected_key: Vec<u8> = seed.iter().map(|b| (b ^ 0x5A).wrapping_add(1)).collect();
        let mut expected_req = vec![0x27u8, 0x12];
        expected_req.extend_from_slice(&expected_key);
        assert_eq!(t.requests[1], expected_req);
    }

    #[test]
    fn kwp_handshake_sends_computed_key() {
        let seed = [0x01u8, 0x02, 0x03, 0x04];
        let mut seed_resp = vec![0x67, 0x01];
        seed_resp.extend_from_slice(&seed);
        let mut t = ScriptedTransport {
            replies: VecDeque::from(vec![seed_resp, vec![0x67, 0x02]]),
            requests: Vec::new(),
        };
        kwp_security_access(&mut t, Some(&DemoXorAddKey), 0x01, true).unwrap();
        assert_eq!(t.requests[0], vec![0x27, 0x01]);
    }

    #[test]
    fn errors_when_required_but_no_handler() {
        let mut t = ScriptedTransport {
            replies: VecDeque::new(),
            requests: Vec::new(),
        };
        assert!(matches!(
            uds_security_access(&mut t, None, 0x11, true),
            Err(Error::Config(_))
        ));
        assert!(t.requests.is_empty());
    }

    #[test]
    fn config_helpers_parse_options() {
        let mut cfg = ConnectConfig::default();
        assert!(!security_required(Some(&cfg)));
        assert_eq!(seedkey_level(Some(&cfg), 0x11), 0x11);
        cfg.options.insert("seedkey".into(), "edc17-sa2".into());
        cfg.options.insert("seedkey_level".into(), "0x09".into());
        assert!(security_required(Some(&cfg)));
        assert_eq!(seedkey_level(Some(&cfg), 0x11), 0x09);
    }
}
