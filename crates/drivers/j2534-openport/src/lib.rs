//! Tactrix OpenPort 2.0 driver for OpenTune.
//!
//! The OpenPort 2.0 (OP2) enumerates as a USB CDC-ACM serial device
//! (`/dev/ttyACM*`, USB id `0403:cc4d`), so it needs no kernel driver on Linux.
//! It speaks a line-oriented ASCII protocol documented in
//! `/opt/nvme-fw/tactrix-op2/FINDINGS.md` (recovered by the roffe/bisak
//! clean-room projects): commands are `at<letter> ... <seq>` with a wrapping
//! sequence number, acknowledged by `aro <seq>` (ok) / `are <seq> <code>`
//! (error), and received CAN frames arrive as a binary record
//! `'a' 'r' <proto-digit> <len> <status> <ts32 BE> <id32 BE> <data>`.
//!
//! This driver opens the OP2 in **raw CAN** mode (J2534 protocol id 5) and
//! implements [`CanLink`]. The ISO-TP segmentation, UDS services and the
//! read/write flows are provided by `opentune-core` ([`IsoTp`] + [`UdsClient`]),
//! so this crate only owns the transport.
//!
//! Config ([`ConnectConfig::options`]):
//! - `port`: serial device path (default `/dev/ttyACM0`).
//!
//! Hardware note: the exact field widths below (single-byte `len`, `len`
//! counting payload bytes *after* the 4-byte id) follow the literal field list
//! in FINDINGS.md. A hardware owner should confirm them against the OP2
//! firmware with `TX_IOCTL_SET_DLL_DEBUG_FLAGS`/`ALL_DEV_COMMS` traces; the byte
//! layout is isolated in [`Op2Serial::recv_frame`]/[`Op2Serial::transmit`] so it
//! is a one-line change if a trace disagrees.

use opentune_core::protocol::isotp::{CanLink, IsoTp};
use opentune_core::protocol::uds::UdsClient;
use opentune_core::{
    Capabilities, ConnectConfig, EcuId, Error, Interface, InterfaceInfo, MemoryRegion, Phase,
    Progress, Protocol, Result,
};
use std::cell::RefCell;
use std::io::{Read, Write};
use std::rc::Rc;
use std::time::{Duration, Instant};

/// J2534 protocol id for raw CAN frames (see `j2534_tactrix.h`).
const PROTO_CAN: u8 = 5;
/// J2534 connect flag: use 29-bit (extended) CAN identifiers.
const FLAG_CAN_29BIT: u32 = 0x0000_0100;
/// Analog input index for battery voltage (`atr 16`, FINDINGS.md).
const ANALOG_VBAT: u8 = 16;
/// Per-byte read timeout while framing one reply, in milliseconds.
const BYTE_TIMEOUT_MS: u64 = 50;

/// Low-level OP2 serial transport: owns the CDC-ACM port, the sequence counter
/// and the opened channel handle. All ASCII-command framing and binary RX-frame
/// parsing lives here.
struct Op2Serial {
    port: Box<dyn serialport::SerialPort>,
    seq: u8,
    channel: u32,
    extended_id: bool,
}

/// A decoded message read back from the OP2.
enum Op2Msg {
    /// Positive ack `aro <seq> [values...]`.
    AckOk { values: Vec<String> },
    /// Negative ack `are <seq> [code...]`.
    AckErr { values: Vec<String> },
    /// A command-specific ASCII reply line such as `ari <version>`.
    Reply { text: String },
    /// A received CAN frame.
    Frame { id: u32, data: Vec<u8> },
}

impl Op2Serial {
    fn open_port(path: &str) -> Result<Self> {
        // The USB CDC-ACM baud rate is cosmetic, but serialport requires one.
        let port = serialport::new(path, 115_200)
            .timeout(Duration::from_millis(BYTE_TIMEOUT_MS))
            .open()
            .map_err(|e| Error::Config(format!("cannot open OP2 port {path}: {e}")))?;
        Ok(Self {
            port,
            seq: 0,
            channel: 0,
            extended_id: false,
        })
    }

    /// Next sequence number, wrapping and skipping 0 (FINDINGS: "wrapping
    /// sequence number").
    fn next_seq(&mut self) -> u8 {
        self.seq = self.seq.wrapping_add(1);
        if self.seq == 0 {
            self.seq = 1;
        }
        self.seq
    }

    fn read_byte(&mut self, deadline: Instant) -> Result<u8> {
        let mut b = [0u8; 1];
        loop {
            match self.port.read(&mut b) {
                Ok(1) => return Ok(b[0]),
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => return Err(Error::Io(e)),
            }
            if Instant::now() >= deadline {
                return Err(Error::Timeout(BYTE_TIMEOUT_MS));
            }
        }
    }

    fn read_exact(&mut self, n: usize, deadline: Instant) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            out.push(self.read_byte(deadline)?);
        }
        Ok(out)
    }

    fn read_line(&mut self, deadline: Instant) -> Result<String> {
        let mut s = Vec::new();
        loop {
            let b = self.read_byte(deadline)?;
            if b == b'\r' || b == b'\n' {
                if s.is_empty() {
                    continue; // swallow leading CR/LF
                }
                break;
            }
            s.push(b);
        }
        Ok(String::from_utf8_lossy(&s).trim().to_string())
    }

    /// Read and decode exactly one message from the device.
    ///
    /// All messages start with `a` `r`; the third byte selects the kind:
    /// `o`/`e` are generic acks, a protocol digit is a binary RX frame, and any
    /// other letter is a command-specific ASCII reply line.
    fn read_message(&mut self, timeout_ms: u64) -> Result<Op2Msg> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(BYTE_TIMEOUT_MS));
        // Resynchronise on the 'a' 'r' preamble.
        loop {
            if self.read_byte(deadline)? != b'a' {
                continue;
            }
            if self.read_byte(deadline)? == b'r' {
                break;
            }
        }
        let kind = self.read_byte(deadline)?;
        match kind {
            b'o' | b'e' => {
                let line = self.read_line(deadline)?;
                let values: Vec<String> = line.split_whitespace().map(|s| s.to_string()).collect();
                // values[0] is the echoed sequence number.
                if kind == b'o' {
                    Ok(Op2Msg::AckOk { values })
                } else {
                    Ok(Op2Msg::AckErr { values })
                }
            }
            d @ b'0'..=b'9' => {
                let proto = d - b'0';
                let _ = proto; // raw-CAN only; kept for clarity.
                let len = self.read_byte(deadline)? as usize;
                let _status = self.read_byte(deadline)?;
                let ts = self.read_exact(4, deadline)?;
                let _timestamp = u32::from_be_bytes([ts[0], ts[1], ts[2], ts[3]]);
                let idb = self.read_exact(4, deadline)?;
                let id = u32::from_be_bytes([idb[0], idb[1], idb[2], idb[3]]);
                // `len` counts the payload bytes that follow the 4-byte id.
                let data = self.read_exact(len, deadline)?;
                Ok(Op2Msg::Frame { id, data })
            }
            _ => {
                let rest = self.read_line(deadline)?;
                let text = format!("{}{}", kind as char, rest);
                Ok(Op2Msg::Reply { text })
            }
        }
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        self.port.write_all(bytes).map_err(Error::Io)?;
        self.port.flush().map_err(Error::Io)?;
        Ok(())
    }

    /// Send an ASCII command (seq appended automatically) and wait for its
    /// `aro`/`are` ack, returning the ok-ack values.
    fn command(&mut self, cmd: &str, timeout_ms: u64) -> Result<Vec<String>> {
        let seq = self.next_seq();
        let line = format!("{cmd} {seq}\r");
        self.write_all(line.as_bytes())?;
        self.await_ack(timeout_ms)
    }

    /// Wait for the next ack, skipping any RX frames that arrive first.
    fn await_ack(&mut self, timeout_ms: u64) -> Result<Vec<String>> {
        loop {
            match self.read_message(timeout_ms)? {
                Op2Msg::AckOk { values } => return Ok(values),
                Op2Msg::AckErr { values } => {
                    return Err(Error::Protocol(format!(
                        "OP2 error ack: {}",
                        values.join(" ")
                    )))
                }
                Op2Msg::Reply { text } => return Ok(vec![text]),
                Op2Msg::Frame { .. } => continue,
            }
        }
    }

    fn version(&mut self) -> Result<String> {
        let seq = self.next_seq();
        self.write_all(format!("ati {seq}\r").as_bytes())?;
        match self.read_message(1000)? {
            Op2Msg::Reply { text } => Ok(text),
            Op2Msg::AckOk { values } => Ok(values.join(" ")),
            _ => Ok(String::new()),
        }
    }

    /// Open a raw-CAN channel. Per FINDINGS.md the `ato` argument order is
    /// protocol, flags, baud ("flags before baud").
    fn open_channel(&mut self, baud: u32, extended_id: bool) -> Result<()> {
        self.extended_id = extended_id;
        let flags = if extended_id { FLAG_CAN_29BIT } else { 0 };
        let values = self.command(&format!("ato {PROTO_CAN} {flags} {baud}"), 2000)?;
        // The ok-ack carries the echoed seq then the assigned channel handle.
        self.channel = values.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        Ok(())
    }

    /// Install a pass filter so the diagnostic rx id reaches us.
    fn set_pass_filter(&mut self, rx_id: u32) -> Result<()> {
        let mask = if self.extended_id { 0x1FFF_FFFF } else { 0x7FF };
        self.command(
            &format!("atf {} 1 0 {:08X} {:08X}", self.channel, mask, rx_id),
            1000,
        )
        .map(|_| ())
    }

    fn close_channel(&mut self) -> Result<()> {
        let ch = self.channel;
        self.command(&format!("atc {ch}"), 1000).map(|_| ())
    }

    /// Transmit one CAN frame. The payload is framed as
    /// `att <channel> <txflags> <timeout_us> ` then binary
    /// `<len><id BE><payload>` then ` <seq>\r`, where `len` is the payload
    /// length (matching the RX-frame convention above).
    fn transmit(&mut self, id: u32, payload: &[u8], timeout_us: u32) -> Result<()> {
        let seq = self.next_seq();
        let mut buf = format!("att {} 0 {} ", self.channel, timeout_us.max(1)).into_bytes();
        buf.push(payload.len() as u8);
        buf.extend_from_slice(&id.to_be_bytes());
        buf.extend_from_slice(payload);
        buf.extend_from_slice(format!(" {seq}\r").as_bytes());
        self.write_all(&buf)?;
        self.await_ack(1000).map(|_| ())
    }

    /// Read battery voltage in millivolts via `atr 16`.
    fn battery_mv(&mut self) -> Result<u32> {
        let values = self.command(&format!("atr {ANALOG_VBAT}"), 1000)?;
        // ok-ack: <seq> <millivolts>.
        values
            .get(1)
            .and_then(|s| s.parse::<u32>().ok())
            .ok_or_else(|| Error::Protocol("no voltage in atr reply".into()))
    }
}

/// [`CanLink`] over an [`Op2Serial`] shared with the owning driver.
struct Op2Link {
    serial: Rc<RefCell<Op2Serial>>,
}

impl CanLink for Op2Link {
    fn send_frame(&mut self, id: u32, data: &[u8]) -> Result<()> {
        // 50 ms tx timeout, in microseconds (FINDINGS: timeout must be nonzero).
        self.serial.borrow_mut().transmit(id, data, 50_000)
    }

    fn recv_frame(&mut self, timeout_ms: u64) -> Result<(u32, Vec<u8>)> {
        let mut serial = self.serial.borrow_mut();
        loop {
            match serial.read_message(timeout_ms)? {
                Op2Msg::Frame { id, data } => return Ok((id, data)),
                // Skip acks/replies that are not CAN data.
                _ => continue,
            }
        }
    }
}

#[derive(Default)]
pub struct OpenPort2Driver {
    serial: Option<Rc<RefCell<Op2Serial>>>,
    transport: Option<IsoTp>,
    firmware: Option<String>,
    cfg: Option<ConnectConfig>,
}

impl OpenPort2Driver {
    pub fn new() -> Self {
        Self::default()
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
}

impl Interface for OpenPort2Driver {
    fn info(&self) -> InterfaceInfo {
        InterfaceInfo {
            id: "openport2".into(),
            display_name: "Tactrix OpenPort 2.0 (J2534/CDC-ACM)".into(),
            firmware: self.firmware.clone(),
            serial: None,
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            can: true,
            read_memory: true,
            write_memory: true,
            battery_voltage: true,
            programming_voltage: true,
            ..Default::default()
        }
    }

    fn open(&mut self, cfg: &ConnectConfig) -> Result<()> {
        if !matches!(cfg.protocol, Protocol::IsoTp | Protocol::RawCan) {
            return Err(Error::Unsupported(
                "OpenPort 2.0 driver supports CAN/ISO-TP protocols only".into(),
            ));
        }
        let port = cfg
            .options
            .get("port")
            .map(String::as_str)
            .unwrap_or("/dev/ttyACM0");

        let mut serial = Op2Serial::open_port(port)?;
        // Reset to a known state and read the firmware banner.
        let _ = serial.command("atz", 2000);
        self.firmware = serial.version().ok().filter(|s| !s.is_empty());

        let extended = cfg.tx_id > 0x7FF || cfg.rx_id > 0x7FF;
        serial.open_channel(cfg.bitrate, extended)?;
        serial.set_pass_filter(cfg.rx_id)?;

        let shared = Rc::new(RefCell::new(serial));
        let link = Op2Link {
            serial: Rc::clone(&shared),
        };
        let isotp = IsoTp::new(Box::new(link), cfg.tx_id, cfg.rx_id);

        self.serial = Some(shared);
        self.transport = Some(isotp);
        self.cfg = Some(cfg.clone());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        // Drop the ISO-TP (and its link) before touching the shared serial.
        self.transport = None;
        if let Some(serial) = self.serial.take() {
            if let Ok(mut s) = Rc::try_unwrap(serial).map(RefCell::into_inner) {
                let _ = s.close_channel();
                let _ = s.command("ata", 500); // close-all, belt and braces.
            }
        }
        self.cfg = None;
        Ok(())
    }

    fn battery_millivolts(&mut self) -> Result<Option<u32>> {
        let serial = self.serial.as_ref().ok_or(Error::NotConnected)?;
        let mv = serial.borrow_mut().battery_mv()?;
        Ok(Some(mv))
    }

    fn read_ecu_id(&mut self) -> Result<EcuId> {
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
        if self.transport.is_none() {
            return Err(Error::NotConnected);
        }
        let _ = self.begin_session();
        uds_read_memory(self.transport()?, region, progress)
    }

    fn write_memory(
        &mut self,
        region: MemoryRegion,
        data: &[u8],
        progress: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        if self.transport.is_none() {
            return Err(Error::NotConnected);
        }
        if data.len() != region.length as usize {
            return Err(Error::Config("data length != region length".into()));
        }
        let _ = self.begin_session();
        uds_write_memory(self.transport()?, region, data, progress)
    }
}

// ---------------------------------------------------------------------------
// Shared UDS flows (also reused by the SocketCAN driver via opentune-core).
// These live here because they are small and keep the driver self-contained;
// they only use the fixed `opentune-core` API.
// ---------------------------------------------------------------------------

/// Common identification DIDs → [`EcuId`] fields.
pub(crate) fn read_uds_ecu_id(transport: &mut IsoTp) -> Result<EcuId> {
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
/// granted. `write_memory` attempts the sequence regardless, so it succeeds on
/// an already-unlocked session.
// TODO: security access via SeedKey plugin (service 0x27 seed/key) before the
// requestDownload for ECUs that are still locked.
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
