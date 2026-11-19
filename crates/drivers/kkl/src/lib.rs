//! Raw K-line driver for the cheap "KKL 409.1 / VAG-COM" cables.
//!
//! A KKL cable is **dumb**: a USB-serial bridge (FTDI FT232 or CH340) wired
//! straight to a K-line transceiver (e.g. an L9637). There is no ELM-style
//! firmware, so — unlike [`opentune-driver-elm327`] — this driver performs the
//! ISO 9141 / ISO 14230 (KWP2000) **bus init and message framing itself** over
//! the raw serial port, then exposes the result as a
//! [`DiagTransport`](opentune_core::protocol::diag::DiagTransport) so the fixed
//! `opentune-core` service clients ([`UdsClient`] / [`Kwp2000Client`]) run on
//! top unchanged.
//!
//! ## What the driver does over the wire
//!
//! * **Serial**: opens the port at the K-line baud (default 10400), 8N1, with
//!   per-byte read timeouts.
//! * **Init** (one of two, selected by `options["init"]`):
//!   * *fast init* (ISO 14230): a wake-up pattern of 25 ms bus-low + 25 ms
//!     bus-high driven with the serial **BREAK** line, then a
//!     `StartCommunication` (0x81) request.
//!   * *5-baud init* (ISO 9141 / ISO 14230 slow): the ECU address byte shifted
//!     out at 5 bit/s by bit-banging BREAK (~200 ms per bit), then read the
//!     `0x55` sync byte + 2 key bytes and send back the inverted second key
//!     byte.
//! * **Half-duplex echo cancellation**: the K-line echoes everything the tester
//!   transmits, so after every write the driver reads and discards exactly the
//!   echoed bytes before reading the ECU's reply.
//! * **KWP2000 framing** (ISO 14230): request frames are
//!   `[fmt][target][source][data…][checksum]`, where the format byte carries
//!   physical addressing (0x80) and, when the payload is ≤ 63 bytes, the length
//!   in its low 6 bits; longer payloads use a separate length byte. The
//!   checksum is the 8-bit sum of all preceding bytes. Responses are parsed the
//!   same way and the checksum is verified. Both the length-in-format-byte and
//!   separate-length-byte forms are supported.
//!
//! ## Config ([`ConnectConfig`])
//!
//! * `options["port"]`: serial device (**required**, e.g. `/dev/ttyUSB0`).
//! * `options["init"]`: `fast` (default) or `5baud`.
//! * `options["baud"]` or `bitrate`: K-line baud (default `10400`).
//! * `tx_id` (low byte): ECU target address (KWP physical addressing). The
//!   tester source address is `0xF1`.
//! * `diag_layer`: `Kwp2000` (service 0x1A / 0x23 / 0x34…) or `Uds` (RDBI /
//!   upload / download), run on top of the same K-line transport.
//!
//! ## BREAK support
//!
//! The `serialport` crate's `SerialPort` trait **does** expose break control
//! (`set_break()` / `clear_break()`, both `&self`), so both init methods use the
//! real BREAK line. `set_break()` holds the TX line in the spacing (low) state;
//! on a standard KKL transceiver that pulls the K-line low, which is what the
//! wake-up pattern and the 5-baud start/`0` bits require. The timing is done
//! with `std::thread::sleep`, so it is best-effort on a non-realtime OS (an OS
//! scheduling hiccup can stretch a bit); this is the normal limitation of
//! bit-banging a K-line from userspace and matches what freediag / VAG-COM do.

use opentune_core::protocol::diag::{is_response_pending, DiagTransport};
use opentune_core::protocol::kwp2000::Kwp2000Client;
use opentune_core::protocol::seedkey::SeedKey;
use opentune_core::protocol::uds::UdsClient;
use opentune_core::{
    Capabilities, ConnectConfig, DiagLayer, EcuId, Error, Interface, InterfaceInfo, MemoryRegion,
    Phase, Progress, Result,
};
use std::io::{Read, Write};
use std::thread::sleep;
use std::time::{Duration, Instant};

/// Tester (diagnostic source) address used in KWP physical addressing.
const TESTER_ADDR: u8 = 0xF1;
/// Default K-line baud when neither `options["baud"]` nor a sane `bitrate` is given.
const DEFAULT_KLINE_BAUD: u32 = 10_400;
/// Fast-init wake-up pattern: bus low for 25 ms, then bus high for 25 ms.
const FAST_INIT_LOW_MS: u64 = 25;
const FAST_INIT_HIGH_MS: u64 = 25;
/// 5-baud init: one bit every ~200 ms (5 bit/s).
const FIVE_BAUD_BIT_MS: u64 = 200;
/// Per-byte serial read poll interval.
const READ_POLL_MS: u64 = 20;

// --- KWP2000 (ISO 14230) framing -------------------------------------------

/// 8-bit additive checksum: the sum of all bytes, modulo 256.
fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |acc, &b| acc.wrapping_add(b))
}

/// Build a KWP2000 request frame for physical addressing.
///
/// `[fmt][target][source][data…][checksum]`. The format byte sets the physical
/// addressing bits (`0x80`); payloads up to 63 bytes carry their length in the
/// low 6 bits, longer payloads set those bits to 0 and add a separate length
/// byte after the source address.
fn build_kwp_frame(target: u8, source: u8, data: &[u8]) -> Vec<u8> {
    let len = data.len();
    let mut frame = Vec::with_capacity(len + 5);
    if len <= 0x3F {
        // Physical addressing, length in the format byte.
        frame.push(0x80 | len as u8);
        frame.push(target);
        frame.push(source);
    } else {
        // Physical addressing, length in a separate byte.
        frame.push(0x80);
        frame.push(target);
        frame.push(source);
        frame.push(len as u8);
    }
    frame.extend_from_slice(data);
    frame.push(checksum(&frame));
    frame
}

/// Parse a complete KWP2000 frame, verify its checksum and return the payload
/// (the service id and its data, i.e. the bytes between the header and the
/// checksum). Handles frames with and without an address header, and with the
/// length in the format byte or in a separate byte.
fn parse_kwp_frame(frame: &[u8]) -> Result<Vec<u8>> {
    if frame.len() < 2 {
        return Err(Error::Protocol("K-line frame too short".into()));
    }
    let fmt = frame[0];
    let has_addr = fmt & 0xC0 != 0;
    let mut idx = 1usize;
    if has_addr {
        // target + source
        if frame.len() < idx + 2 {
            return Err(Error::Protocol("K-line frame missing address bytes".into()));
        }
        idx += 2;
    }
    let mut len = (fmt & 0x3F) as usize;
    if len == 0 {
        if frame.len() <= idx {
            return Err(Error::Protocol("K-line frame missing length byte".into()));
        }
        len = frame[idx] as usize;
        idx += 1;
    }
    let data_start = idx;
    let data_end = data_start
        .checked_add(len)
        .ok_or_else(|| Error::Protocol("K-line frame length overflow".into()))?;
    // Exactly one checksum byte must follow the payload.
    if frame.len() != data_end + 1 {
        return Err(Error::Protocol(format!(
            "K-line frame length mismatch: header says {len} data bytes, buffer is {}",
            frame.len()
        )));
    }
    let want = frame[data_end];
    let got = checksum(&frame[..data_end]);
    if want != got {
        return Err(Error::Protocol(format!(
            "K-line checksum mismatch: got 0x{got:02X}, frame carries 0x{want:02X}"
        )));
    }
    Ok(frame[data_start..data_end].to_vec())
}

// --- Serial transport -------------------------------------------------------

/// Owns the serial port and implements the raw K-line protocol: bus init, echo
/// cancellation and KWP2000 framing. Implements [`DiagTransport`] so the core
/// UDS/KWP clients drive it directly.
struct KklTransport {
    port: Box<dyn serialport::SerialPort>,
    target: u8,
    source: u8,
    /// Key bytes learned during init (fast: StartCommunication; 5-baud: KB1/KB2),
    /// surfaced through [`InterfaceInfo::firmware`] for diagnostics.
    key_bytes: Option<(u8, u8)>,
}

impl KklTransport {
    fn open(port_path: &str, baud: u32, target: u8) -> Result<Self> {
        let port = serialport::new(port_path, baud)
            .data_bits(serialport::DataBits::Eight)
            .parity(serialport::Parity::None)
            .stop_bits(serialport::StopBits::One)
            .flow_control(serialport::FlowControl::None)
            .timeout(Duration::from_millis(READ_POLL_MS))
            .open()
            .map_err(|e| Error::Config(format!("cannot open KKL port {port_path}: {e}")))?;
        Ok(Self {
            port,
            target,
            source: TESTER_ADDR,
            key_bytes: None,
        })
    }

    /// Hold the bus low (BREAK / spacing) for `ms` milliseconds.
    fn drive_low(&self, ms: u64) -> Result<()> {
        self.port
            .set_break()
            .map_err(|e| Error::Protocol(format!("set_break failed: {e}")))?;
        sleep(Duration::from_millis(ms));
        Ok(())
    }

    /// Release the bus to idle high (mark) for `ms` milliseconds.
    fn drive_high(&self, ms: u64) -> Result<()> {
        self.port
            .clear_break()
            .map_err(|e| Error::Protocol(format!("clear_break failed: {e}")))?;
        sleep(Duration::from_millis(ms));
        Ok(())
    }

    /// Read exactly `buf.len()` bytes, polling until `deadline`. Times out as
    /// [`Error::Timeout`] if the bytes do not all arrive.
    fn read_exact_until(&mut self, buf: &mut [u8], deadline: Instant) -> Result<()> {
        let mut filled = 0usize;
        while filled < buf.len() {
            match self.port.read(&mut buf[filled..]) {
                Ok(0) => {}
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => return Err(Error::Io(e)),
            }
            if filled < buf.len() && Instant::now() >= deadline {
                return Err(Error::Timeout(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .as_millis() as u64,
                ));
            }
        }
        Ok(())
    }

    /// Read a single byte, polling until `deadline`.
    fn read_byte_until(&mut self, deadline: Instant) -> Result<u8> {
        let mut b = [0u8; 1];
        self.read_exact_until(&mut b, deadline)?;
        Ok(b[0])
    }

    /// Write raw bytes to the bus and flush.
    fn write_raw(&mut self, bytes: &[u8]) -> Result<()> {
        self.port.write_all(bytes).map_err(Error::Io)?;
        self.port.flush().map_err(Error::Io)?;
        Ok(())
    }

    /// Transmit a frame, then consume its half-duplex echo. The K-line echoes
    /// every transmitted byte, so exactly `frame.len()` bytes are read back and
    /// discarded before the ECU's reply is read.
    fn send_framed(&mut self, data: &[u8]) -> Result<()> {
        let frame = build_kwp_frame(self.target, self.source, data);
        let _ = self.port.clear(serialport::ClearBuffer::Input);
        self.write_raw(&frame)?;
        self.eat_echo(frame.len())?;
        Ok(())
    }

    /// Read and discard `n` echoed bytes.
    fn eat_echo(&mut self, n: usize) -> Result<()> {
        if n == 0 {
            return Ok(());
        }
        let mut echo = vec![0u8; n];
        // The echo appears at bus speed; allow a byte-time plus margin each.
        let deadline = Instant::now() + Duration::from_millis(50 + 2 * n as u64);
        self.read_exact_until(&mut echo, deadline)
    }

    /// Read one response frame from the bus and return its payload.
    fn read_framed(&mut self, timeout_ms: u64) -> Result<Vec<u8>> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(READ_POLL_MS));
        let fmt = self.read_byte_until(deadline)?;
        let mut frame = vec![fmt];
        if fmt & 0xC0 != 0 {
            frame.push(self.read_byte_until(deadline)?); // target (tester)
            frame.push(self.read_byte_until(deadline)?); // source (ECU)
        }
        let mut len = (fmt & 0x3F) as usize;
        if len == 0 {
            let lb = self.read_byte_until(deadline)?;
            frame.push(lb);
            len = lb as usize;
        }
        for _ in 0..len {
            frame.push(self.read_byte_until(deadline)?);
        }
        frame.push(self.read_byte_until(deadline)?); // checksum
        parse_kwp_frame(&frame)
    }

    // --- Bus initialisation -------------------------------------------------

    /// ISO 14230 fast init: 25 ms low + 25 ms high wake-up on the BREAK line,
    /// then a `StartCommunication` (0x81) request.
    fn fast_init(&mut self) -> Result<()> {
        // Idle the bus high briefly so the ECU sees a clean falling edge.
        self.drive_high(FAST_INIT_HIGH_MS)?;
        self.drive_low(FAST_INIT_LOW_MS)?;
        self.drive_high(FAST_INIT_HIGH_MS)?;
        let _ = self.port.clear(serialport::ClearBuffer::Input);

        // StartCommunication: service 0x81, physical addressing.
        self.send_framed(&[0x81])?;
        let resp = self.read_framed(1500)?;
        // Positive response is 0xC1 followed by the two key bytes.
        match resp.first() {
            Some(0xC1) if resp.len() >= 3 => {
                self.key_bytes = Some((resp[1], resp[2]));
                Ok(())
            }
            Some(0xC1) => Ok(()),
            Some(0x7F) => Err(Error::Protocol(
                "ECU rejected StartCommunication (fast init)".into(),
            )),
            _ => Err(Error::Protocol(format!(
                "unexpected StartCommunication response: {resp:02X?}"
            ))),
        }
    }

    /// ISO 9141 / ISO 14230 slow (5-baud) init: shift the ECU address byte out
    /// at 5 bit/s on the BREAK line (start bit + 8 data bits LSB-first + stop
    /// bit), then read the 0x55 sync byte and two key bytes and acknowledge with
    /// the inverted second key byte.
    fn five_baud_init(&mut self) -> Result<()> {
        // Idle high before the start bit.
        self.drive_high(FIVE_BAUD_BIT_MS)?;

        // Start bit (low), then 8 data bits LSB-first, then stop bit (high).
        self.drive_low(FIVE_BAUD_BIT_MS)?;
        for i in 0..8 {
            if (self.target >> i) & 1 == 1 {
                self.drive_high(FIVE_BAUD_BIT_MS)?; // '1' = mark = line high
            } else {
                self.drive_low(FIVE_BAUD_BIT_MS)?; // '0' = space = line low
            }
        }
        self.drive_high(FIVE_BAUD_BIT_MS)?; // stop bit
        let _ = self.port.clear(serialport::ClearBuffer::Input);

        // The ECU now streams, at the bus baud: 0x55 sync, then KB1, KB2.
        let deadline = Instant::now() + Duration::from_millis(2000);
        let sync = self.read_byte_until(deadline)?;
        if sync != 0x55 {
            return Err(Error::Protocol(format!(
                "5-baud init: expected 0x55 sync, got 0x{sync:02X}"
            )));
        }
        let kb1 = self.read_byte_until(deadline)?;
        let kb2 = self.read_byte_until(deadline)?;
        self.key_bytes = Some((kb1, kb2));

        // W4 (25–50 ms) before the tester answers with the inverted KB2.
        sleep(Duration::from_millis(30));
        let inv_kb2 = !kb2;
        self.write_raw(&[inv_kb2])?;
        self.eat_echo(1)?;

        // The ECU replies with the inverted address byte; read it if it comes,
        // but do not fail the init if the adapter swallowed it.
        let ack_deadline = Instant::now() + Duration::from_millis(300);
        let _ = self.read_byte_until(ack_deadline);
        Ok(())
    }
}

impl DiagTransport for KklTransport {
    fn request(&mut self, req: &[u8], timeout_ms: u64) -> Result<Vec<u8>> {
        self.send_framed(req)?;
        // Loop while the ECU answers requestCorrectlyReceived-ResponsePending
        // (NRC 0x78) to this request's service; a 0x78 echoing another
        // service is returned (mis-correlated), not waited on.
        for _ in 0..40 {
            let resp = self.read_framed(timeout_ms)?;
            if is_response_pending(req, &resp) {
                continue;
            }
            return Ok(resp);
        }
        Err(Error::Timeout(timeout_ms))
    }
}

// --- Driver -----------------------------------------------------------------

pub struct KklDriver {
    transport: Option<KklTransport>,
    diag_layer: DiagLayer,
    cfg: Option<ConnectConfig>,
    /// Optional seed/key handler for security access (service 0x27), used on
    /// both the UDS and KWP2000 paths. No OEM algorithm ships with OpenTune.
    seed_key: Option<Box<dyn SeedKey>>,
}

impl Default for KklDriver {
    fn default() -> Self {
        Self {
            transport: None,
            diag_layer: DiagLayer::Kwp2000,
            cfg: None,
            seed_key: None,
        }
    }
}

impl KklDriver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install the seed/key handler used to unlock the ECU (service 0x27)
    /// before a download. Without it, writing a security-protected ECU fails
    /// with a clear error. OpenTune ships no OEM algorithms.
    pub fn set_seed_key(&mut self, seed_key: Box<dyn SeedKey>) {
        self.seed_key = Some(seed_key);
    }

    fn transport(&mut self) -> Result<&mut KklTransport> {
        self.transport.as_mut().ok_or(Error::NotConnected)
    }

    /// Run security access (service 0x27) for the active diagnostic layer if a
    /// seed/key handler is set or the config marks security as required. A
    /// no-op on an open ECU with no handler configured.
    fn security_access(&mut self) -> Result<()> {
        let layer = self.diag_layer;
        let required = security_required(self.cfg.as_ref());
        // UDS programming sessions typically use level 0x11; KWP2000 ECUs
        // (EDC15/16) commonly use level 0x01. Both are overridable.
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

impl Interface for KklDriver {
    fn set_seed_key(&mut self, seed_key: Box<dyn SeedKey>) -> bool {
        KklDriver::set_seed_key(self, seed_key);
        true
    }

    fn info(&self) -> InterfaceInfo {
        InterfaceInfo {
            id: "kkl".into(),
            display_name: "KKL 409.1 K-line (raw)".into(),
            firmware: self
                .transport
                .as_ref()
                .and_then(|t| t.key_bytes)
                .map(|(a, b)| format!("keybytes {a:02X} {b:02X}")),
            serial: None,
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            kline: true,
            read_memory: true,
            write_memory: true,
            ..Default::default()
        }
    }

    /// Expose the KWP2000/K-line transport (after the ECU wake-up) as a [`DiagTransport`] so the coding/adaptation/
    /// auto-scan/service/logging engines (and the raw-request console) can
    /// drive this adapter. `None` until `open()` succeeds.
    fn diag(&mut self) -> Option<&mut dyn DiagTransport> {
        self.transport.as_mut().map(|t| t as &mut dyn DiagTransport)
    }

    fn open(&mut self, cfg: &ConnectConfig) -> Result<()> {
        let port = cfg
            .options
            .get("port")
            .ok_or_else(|| Error::Config("kkl driver needs options[\"port\"]".into()))?;
        let baud = kline_baud(cfg);
        let target = cfg.tx_id as u8;

        let mut transport = KklTransport::open(port, baud, target)?;
        let init = cfg
            .options
            .get("init")
            .map(String::as_str)
            .unwrap_or("fast");
        match init {
            "fast" => transport.fast_init()?,
            "5baud" | "slow" => transport.five_baud_init()?,
            other => {
                return Err(Error::Config(format!(
                    "unknown init \"{other}\" (expected \"fast\" or \"5baud\")"
                )))
            }
        }

        self.transport = Some(transport);
        self.diag_layer = cfg.diag_layer;
        self.cfg = Some(cfg.clone());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        // Release the port; leave the bus idle-high.
        if let Some(t) = self.transport.take() {
            let _ = t.port.clear_break();
        }
        Ok(())
    }

    fn battery_millivolts(&mut self) -> Result<Option<u32>> {
        // A raw KKL cable has no voltage sense line.
        Ok(None)
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
        // Unlock the ECU (service 0x27) before requestDownload. `security_access`
        // runs the installed handler (or errors when the ECU requires one and
        // none is set); it is a no-op on an open ECU with no handler configured.
        self.security_access()?;
        let layer = self.diag_layer;
        let t = self.transport()?;
        match layer {
            DiagLayer::Uds => uds_write_memory(t, region, data, progress),
            DiagLayer::Kwp2000 => kwp_write_memory(t, region, data, progress),
        }
    }
}

/// K-line baud: `options["baud"]`, else a plausible `bitrate`, else 10400.
fn kline_baud(cfg: &ConnectConfig) -> u32 {
    if let Some(b) = cfg.options.get("baud").and_then(|s| s.parse::<u32>().ok()) {
        return b;
    }
    // `ConnectConfig::bitrate` carries the K-line baud when it is in a sane
    // K-line range; the CAN-oriented default (500000) falls through to 10400.
    if (1_200..=125_000).contains(&cfg.bitrate) {
        cfg.bitrate
    } else {
        DEFAULT_KLINE_BAUD
    }
}

// --- UDS flows (K-line, run on top of the KWP framing) ----------------------

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

// Security access (service 0x27) runs in `KklDriver::write_memory` via
// `uds_security_access` before this function.
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
        // A short block is fine: the next request starts right after it (the
        // block is never empty, so every pass makes progress). Stopping here
        // would return a short image.
    }
    out.truncate(region.length as usize);
    Ok(out)
}

// Security access (service 0x27) runs in `KklDriver::write_memory` via
// `kwp_security_access` before this function.
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

// --- Security access (service 0x27) hook ------------------------------------
//
// OpenTune ships NO OEM seed/key algorithms. A handler is supplied by the
// application via [`KklDriver::set_seed_key`] (or a plugin), keyed by the
// `seedkey` id in an ECU definition. These helpers run request-seed → compute →
// send-key over the existing [`UdsClient`] / [`Kwp2000Client`] before download.
//
// Config keys read from [`ConnectConfig::options`]:
// - `seedkey`:       expected algorithm id; its presence marks security required.
// - `security`:      `required`/`true`/`1`/`yes` marks security required.
// - `seedkey_level`: the request-seed access level (odd), decimal or `0x`-hex.

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

/// UDS service 0x27 seed/key. Errors only when security is required and no
/// handler is set.
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

/// KWP2000 service 0x27 seed/key. Errors only when security is required and no
/// handler is set.
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

    #[cfg(unix)]
    #[test]
    fn diag_exposes_kline_transport_after_connect() {
        // A pseudo-terminal pair stands in for the adapter's serial port.
        let (host, _adapter) = serialport::TTYPort::pair().expect("pty pair");
        let port: Box<dyn serialport::SerialPort> = Box::new(host);
        let mut d = KklDriver::new();
        assert!(d.diag().is_none(), "no transport before open");
        d.transport = Some(KklTransport {
            port,
            target: 0x10,
            source: TESTER_ADDR,
            key_bytes: None,
        });
        assert!(d.diag().is_some(), "diag() must expose the KKL transport");
    }
    use std::collections::VecDeque;

    #[test]
    fn frame_len_in_format_byte_roundtrips() {
        // Short payload: physical addressing, length in the format byte.
        let frame = build_kwp_frame(0x10, 0xF1, &[0x1A, 0x90]);
        assert_eq!(frame[0], 0x82); // 0x80 | len(2)
        assert_eq!(&frame[1..3], &[0x10, 0xF1]);
        assert_eq!(&frame[3..5], &[0x1A, 0x90]);
        assert_eq!(frame[5], checksum(&frame[..5]));
        assert_eq!(parse_kwp_frame(&frame).unwrap(), vec![0x1A, 0x90]);
    }

    #[test]
    fn frame_separate_length_byte_roundtrips() {
        // Payload > 63 bytes forces a separate length byte (fmt low bits 0).
        let data: Vec<u8> = (0..100u8).collect();
        let frame = build_kwp_frame(0x10, 0xF1, &data);
        assert_eq!(frame[0], 0x80); // physical addressing, len in separate byte
        assert_eq!(&frame[1..3], &[0x10, 0xF1]);
        assert_eq!(frame[3], 100);
        assert_eq!(parse_kwp_frame(&frame).unwrap(), data);
    }

    #[test]
    fn parse_rejects_bad_checksum() {
        let mut frame = build_kwp_frame(0xF1, 0x10, &[0x7F, 0x1A, 0x11]);
        *frame.last_mut().unwrap() ^= 0xFF;
        assert!(matches!(parse_kwp_frame(&frame), Err(Error::Protocol(_))));
    }

    #[test]
    fn parse_rejects_length_mismatch() {
        // Claims 2 data bytes but only one is present before the checksum.
        let frame = [0x82u8, 0x10, 0xF1, 0x1A, 0x00];
        assert!(matches!(parse_kwp_frame(&frame), Err(Error::Protocol(_))));
    }

    /// One request over a pseudo-terminal "K-line": the ECU side reads the
    /// request frame, echoes it (half duplex) and answers with `replies`.
    #[cfg(unix)]
    fn kline_exchange(req: &[u8], replies: Vec<Vec<u8>>) -> Result<Vec<u8>> {
        use serialport::SerialPort;
        use std::io::{Read, Write};
        let (host, mut adapter) = serialport::TTYPort::pair().expect("pty pair");
        let mut t = KklTransport {
            port: Box::new(host),
            target: 0x10,
            source: TESTER_ADDR,
            key_bytes: None,
        };
        let n = build_kwp_frame(0x10, TESTER_ADDR, req).len();
        let ecu = std::thread::spawn(move || {
            adapter.set_timeout(Duration::from_secs(2)).unwrap();
            let mut out = vec![0u8; n];
            adapter.read_exact(&mut out).unwrap(); // becomes the echo
            for r in &replies {
                out.extend(build_kwp_frame(TESTER_ADDR, 0x10, r));
            }
            adapter.write_all(&out).unwrap();
            adapter.flush().unwrap();
            adapter
        });
        let r = t.request(req, 300);
        let _adapter = ecu.join().unwrap();
        r
    }

    #[cfg(unix)]
    #[test]
    fn response_pending_is_echo_checked() {
        let real = vec![0x5A, 0x9B, 0x01];
        let r = kline_exchange(&[0x1A, 0x9B], vec![vec![0x7F, 0x1A, 0x78], real.clone()]);
        assert_eq!(r.unwrap(), real, "0x78 for this service is waited out");
        // A 0x78 for another service (0x10) is not treated as pending.
        let r = kline_exchange(&[0x1A, 0x9B], vec![vec![0x7F, 0x10, 0x78], real]);
        assert_eq!(r.unwrap(), vec![0x7F, 0x10, 0x78]);
    }

    /// Scripted transport to exercise the seed/key hook without hardware.
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

    #[test]
    fn kwp_read_memory_keeps_reading_after_a_short_block() {
        // The ECU returns 2 of the 4 bytes asked for: the next request starts
        // right after them and the image is complete, never short.
        let mut t = ScriptedTransport {
            replies: VecDeque::from(vec![vec![0x63, 1, 2], vec![0x63, 3, 4]]),
            requests: Vec::new(),
        };
        let out = kwp_read_memory(&mut t, MemoryRegion::new(0x01_0000, 4), &mut |_| {}).unwrap();
        assert_eq!(out, vec![1, 2, 3, 4]);
        assert_eq!(t.requests[0], vec![0x23, 0x01, 0x00, 0x00, 0x04]);
        assert_eq!(t.requests[1], vec![0x23, 0x01, 0x00, 0x02, 0x02]);
        // An ECU that stops answering fails the read instead of truncating it.
        let mut t = ScriptedTransport {
            replies: VecDeque::from(vec![vec![0x63, 1, 2]]),
            requests: Vec::new(),
        };
        assert!(kwp_read_memory(&mut t, MemoryRegion::new(0x01_0000, 4), &mut |_| {}).is_err());
    }

    /// DEMO ONLY — not a real OEM algorithm.
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
    fn kwp_security_access_sends_computed_key() {
        let seed = [0x01u8, 0x02, 0x03, 0x04];
        let mut seed_resp = vec![0x67, 0x01];
        seed_resp.extend_from_slice(&seed);
        let mut t = ScriptedTransport {
            replies: VecDeque::from(vec![seed_resp, vec![0x67, 0x02]]),
            requests: Vec::new(),
        };
        kwp_security_access(&mut t, Some(&DemoXorAddKey), 0x01, true).unwrap();
        assert_eq!(t.requests[0], vec![0x27, 0x01]);
        let expected_key: Vec<u8> = seed.iter().map(|b| (b ^ 0x5A).wrapping_add(1)).collect();
        let mut expected_req = vec![0x27u8, 0x02];
        expected_req.extend_from_slice(&expected_key);
        assert_eq!(t.requests[1], expected_req);
    }

    #[test]
    fn security_access_errors_when_required_but_no_handler() {
        let mut t = ScriptedTransport {
            replies: VecDeque::new(),
            requests: Vec::new(),
        };
        assert!(matches!(
            kwp_security_access(&mut t, None, 0x01, true),
            Err(Error::Config(_))
        ));
        assert!(t.requests.is_empty());
    }

    #[test]
    fn security_access_noop_when_open_and_no_handler() {
        let mut t = ScriptedTransport {
            replies: VecDeque::new(),
            requests: Vec::new(),
        };
        uds_security_access(&mut t, None, 0x11, false).unwrap();
        kwp_security_access(&mut t, None, 0x01, false).unwrap();
        assert!(t.requests.is_empty());
    }

    #[test]
    fn kline_baud_prefers_option_then_bitrate_then_default() {
        let mut cfg = ConnectConfig::default(); // bitrate 500000 (CAN default)
        assert_eq!(kline_baud(&cfg), DEFAULT_KLINE_BAUD);
        cfg.bitrate = 10_400;
        assert_eq!(kline_baud(&cfg), 10_400);
        cfg.options.insert("baud".into(), "9600".into());
        assert_eq!(kline_baud(&cfg), 9_600);
    }
}
