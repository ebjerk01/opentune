//! ELM327 driver for OpenTune (CAN/ISO15765 and K-line/ISO14230).
//!
//! An ELM327 adapter (genuine or clone) on a serial port speaks AT commands and
//! moves a request/response diagnostic message per exchange. Unlike the CAN
//! drivers this one does **not** go through [`IsoTp`](opentune_core::protocol::isotp::IsoTp):
//! the ELM firmware performs ISO-TP segmentation (with CAN auto-formatting) and
//! K-line framing itself, so this driver implements
//! [`DiagTransport`](opentune_core::protocol::diag::DiagTransport) directly and
//! lets the fixed `opentune-core` service clients ([`UdsClient`] / [`Kwp2000Client`])
//! run on top.
//!
//! Config ([`ConnectConfig`]):
//! - `options["port"]`: serial device (default `/dev/ttyUSB0`).
//! - `options["baud"]`: serial baud (default `38400`, common for clones).
//! - `protocol`: selects the ELM protocol (`ATSP`): ISO-TP → SP6/SP7 by id
//!   width, ISO14230 K-line → SP5 (fast init), ISO9141 → SP3.
//! - `diag_layer`: `Uds` uses [`UdsClient`]; `Kwp2000` uses [`Kwp2000Client`].
//! - `tx_id`/`rx_id`: CAN tx/rx ids, or (K-line) the ECU target address in
//!   `tx_id`.

use opentune_core::protocol::diag::DiagTransport;
use opentune_core::protocol::kwp2000::Kwp2000Client;
use opentune_core::protocol::uds::UdsClient;
use opentune_core::{
    Capabilities, ConnectConfig, DiagLayer, EcuId, Error, Interface, InterfaceInfo, MemoryRegion,
    Phase, Progress, Protocol, Result,
};
use std::io::{Read, Write};
use std::time::{Duration, Instant};

/// K-line tester (diagnostic source) address.
const TESTER_ADDR: u8 = 0xF1;
/// Per-read serial timeout while assembling one ELM reply.
const READ_CHUNK_MS: u64 = 100;

/// Owns the serial port and talks the ELM327 AT dialect. Implements
/// [`DiagTransport`] so the core UDS/KWP clients can drive it.
struct ElmTransport {
    port: Box<dyn serialport::SerialPort>,
    version: String,
}

impl ElmTransport {
    fn open(port_path: &str, baud: u32) -> Result<Self> {
        let port = serialport::new(port_path, baud)
            .timeout(Duration::from_millis(READ_CHUNK_MS))
            .open()
            .map_err(|e| Error::Config(format!("cannot open ELM327 port {port_path}: {e}")))?;
        Ok(Self {
            port,
            version: String::new(),
        })
    }

    /// Read characters until the ELM command prompt `>` or the deadline.
    fn read_until_prompt(&mut self, timeout_ms: u64) -> Result<String> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(READ_CHUNK_MS));
        let mut out = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            match self.port.read(&mut byte) {
                Ok(1) => {
                    if byte[0] == b'>' {
                        break;
                    }
                    out.push(byte[0]);
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
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
        self.port.write_all(line.as_bytes()).map_err(Error::Io)?;
        self.port.write_all(b"\r").map_err(Error::Io)?;
        self.port.flush().map_err(Error::Io)?;
        Ok(())
    }

    /// Send an `AT` command and return the adapter's text reply.
    fn at(&mut self, cmd: &str) -> Result<String> {
        let _ = self.port.clear(serialport::ClearBuffer::Input);
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
                // SP6 = ISO15765 11/500, SP7 = ISO15765 29/500.
                self.at_ok(if extended { "ATSP7" } else { "ATSP6" })?;
                let _ = self.at("ATCAF1"); // CAN auto-formatting (ISO-TP) on
                self.set_can_header(cfg.tx_id, extended)?;
                // Accept responses from the ECU rx id.
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
            // 29-bit: priority byte via ATCP, remaining 24 bits via ATSH.
            let _ = self.at(&format!("ATCP {:02X}", (tx_id >> 24) & 0x1F));
            self.at_ok(&format!("ATSH {:06X}", tx_id & 0xFF_FFFF))
        } else {
            self.at_ok(&format!("ATSH {:03X}", tx_id & 0x7FF))
        }
    }

    /// K-line header: physical addressing, length in a separate byte
    /// (format 0x80), ECU target, tester source.
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
        let _ = self.port.clear(serialport::ClearBuffer::Input);
        self.write_line(&hex)?;

        // Keep reading blocks while the ECU answers requestCorrectlyReceived-
        // ResponsePending (NRC 0x78).
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
///
/// Handles single-frame replies, CAN multi-frame replies printed with the
/// `NN: ...` line-counter format (with a leading total-length line), and the
/// common error strings.
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

    let lines: Vec<&str> = text
        .lines()
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

pub struct Elm327Driver {
    transport: Option<ElmTransport>,
    diag_layer: DiagLayer,
}

impl Default for Elm327Driver {
    fn default() -> Self {
        Self {
            transport: None,
            diag_layer: DiagLayer::Uds,
        }
    }
}

impl Elm327Driver {
    pub fn new() -> Self {
        Self::default()
    }

    fn transport(&mut self) -> Result<&mut ElmTransport> {
        self.transport.as_mut().ok_or(Error::NotConnected)
    }
}

impl Interface for Elm327Driver {
    fn info(&self) -> InterfaceInfo {
        InterfaceInfo {
            id: "elm327".into(),
            display_name: "ELM327 (CAN / K-line)".into(),
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
            // ELM327 is read-oriented; writes work on an unlocked ECU but many
            // clones are unreliable for flashing. Advertise the capability; the
            // flow is implemented below.
            write_memory: true,
            battery_voltage: true,
            ..Default::default()
        }
    }

    fn open(&mut self, cfg: &ConnectConfig) -> Result<()> {
        let port = cfg
            .options
            .get("port")
            .map(String::as_str)
            .unwrap_or("/dev/ttyUSB0");
        let baud: u32 = cfg
            .options
            .get("baud")
            .and_then(|s| s.parse().ok())
            .unwrap_or(38_400);

        let mut transport = ElmTransport::open(port, baud)?;
        transport.init(cfg)?;
        self.transport = Some(transport);
        self.diag_layer = cfg.diag_layer;
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
        let layer = self.diag_layer;
        let t = self.transport()?;
        match layer {
            DiagLayer::Uds => uds_write_memory(t, region, data, progress),
            DiagLayer::Kwp2000 => kwp_write_memory(t, region, data, progress),
        }
    }
}

// --- UDS flows (CAN) --------------------------------------------------------

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

// TODO: security access via SeedKey plugin (service 0x27) before requestDownload.
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

// --- KWP2000 flows (K-line) -------------------------------------------------

fn read_kwp_ecu_id(transport: &mut dyn DiagTransport) -> Result<EcuId> {
    let mut id = EcuId::default();
    let mut kwp = Kwp2000Client::new(transport);
    // Common readEcuIdentification options: 0x80 = ECU id, 0x90 = VIN,
    // 0x9B = supplier/sw. Not all ECUs implement every option.
    let options: &[(u8, &str)] = &[(0x90, "vin"), (0x80, "ecu_id"), (0x9B, "sw_number")];
    let mut got_any = false;
    for (opt, name) in options {
        match kwp.read_ecu_id(*opt) {
            Ok(resp) => {
                got_any = true;
                // resp = [option_echo, data...]
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

/// K-line read: KWP `readMemoryByAddress`, chunked to fit one message.
fn kwp_read_memory(
    transport: &mut dyn DiagTransport,
    region: MemoryRegion,
    progress: &mut dyn FnMut(Progress),
) -> Result<Vec<u8>> {
    const MAX_CHUNK: u32 = 250; // readMemoryByAddress length is a single byte
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
            break; // ECU returned a short block; stop to avoid looping.
        }
    }
    out.truncate(region.length as usize);
    Ok(out)
}

// TODO: security access via SeedKey plugin (service 0x27) before requestDownload.
fn kwp_write_memory(
    transport: &mut dyn DiagTransport,
    region: MemoryRegion,
    data: &[u8],
    progress: &mut dyn FnMut(Progress),
) -> Result<()> {
    let total = data.len() as u64;
    let mut kwp = Kwp2000Client::new(transport);
    // format 0x00 = uncompressed/unencrypted.
    kwp.request_download(region.address, region.length, 0x00)?;
    progress(Progress {
        phase: Phase::Write,
        done: 0,
        total,
    });

    let mut done = 0u64;
    // KWP transferData has no per-block counter; keep blocks small for K-line.
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
