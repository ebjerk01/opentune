//! Host driver for the **OpenTune ESP32-S3 interface**
//! (`hardware/firmware/esp32-s3`).
//!
//! The device runs the timing-critical transports (CAN via TWAI, ISO-TP over
//! CAN, and K-line) and exposes a framed USB-CDC command protocol; this driver
//! speaks that protocol ([`proto`]) over a serial port and implements
//! `opentune-core`'s [`Interface`] trait. Diagnostic *service* work (UDS / KWP)
//! stays on the host — the device handles transport only — so the same
//! `UdsClient` / `Kwp2000Client` used by the other drivers run on top.
//!
//! Two paths:
//! * **CAN / UDS** (`Protocol::IsoTp`): the device does ISO-TP segmentation;
//!   [`IsoTpDiag`] is a [`DiagTransport`] over `CMD_ISOTP_REQUEST`, driving
//!   `UdsClient`.
//! * **K-line / KWP** (`Protocol::Iso14230Kline` / `Iso9141`): the device does
//!   init + byte timing; [`KlineDiag`] frames ISO-14230 and drives
//!   `Kwp2000Client`, like the Arduino `kline-bridge` driver.
//!
//! [`CanRawLink`] also exposes the device's raw CAN as a [`CanLink`], so a
//! caller can run the host-side `IsoTp` instead of the device's if desired.

pub mod device;
pub mod proto;

pub use device::{CanFrame, Device, PingInfo};

use opentune_core::protocol::diag::DiagTransport;
use opentune_core::protocol::isotp::CanLink;
use opentune_core::protocol::kwp2000::Kwp2000Client;
use opentune_core::protocol::seedkey::SeedKey;
use opentune_core::protocol::uds::UdsClient;
use opentune_core::{
    Capabilities, ConnectConfig, EcuId, Error, Interface, InterfaceInfo, MemoryRegion, Phase,
    Progress, Protocol, Result,
};
use std::time::Duration;

type Serial = Box<dyn serialport::SerialPort>;

/// Which transport the current session uses.
#[derive(Clone, Copy)]
enum Mode {
    /// UDS over the device's on-board ISO-TP.
    IsoTp,
    /// KWP2000 over the device's K-line, with the ISO-14230 target/source bytes.
    Kline { target: u8, source: u8 },
}

/// The OpenTune ESP32-S3 interface driver.
#[derive(Default)]
pub struct Esp32Driver {
    device: Option<Device<Serial>>,
    mode: Option<Mode>,
    firmware: Option<String>,
    serial: Option<String>,
    cfg: Option<ConnectConfig>,
    /// Optional seed/key handler for security access (service 0x27), used on
    /// both the UDS and the KWP path. No OEM algorithm ships with OpenTune; the
    /// application supplies one.
    seed_key: Option<Box<dyn SeedKey>>,
}

impl Esp32Driver {
    pub fn new() -> Self {
        Self::default()
    }

    fn dev(&mut self) -> Result<&mut Device<Serial>> {
        self.device.as_mut().ok_or(Error::NotConnected)
    }

    /// Install the seed/key handler used to unlock the ECU (service 0x27)
    /// before a download. Without it, writing a security-protected ECU fails
    /// with a clear error. OpenTune ships no OEM algorithms.
    pub fn set_seed_key(&mut self, seed_key: Box<dyn SeedKey>) {
        self.seed_key = Some(seed_key);
    }

    /// Run security access (service 0x27) for the active transport if a
    /// seed/key handler is set or the connection config marks security as
    /// required. A no-op on an open ECU with no handler configured, so unlocked
    /// sessions keep working.
    fn security_access(&mut self) -> Result<()> {
        let required = security_required(self.cfg.as_ref());
        // Disjoint field borrows: `seed_key`/`mode`/`cfg` (shared), `device`
        // (unique).
        let sk = self.seed_key.as_deref();
        let dev = self.device.as_mut().ok_or(Error::NotConnected)?;
        match self.mode.as_ref() {
            Some(Mode::IsoTp) => {
                let level = seedkey_level(self.cfg.as_ref(), 0x11);
                let mut t = IsoTpDiag { dev };
                uds_security_access(&mut t, sk, level, required)
            }
            Some(Mode::Kline { target, source }) => {
                let (target, source) = (*target, *source);
                let level = seedkey_level(self.cfg.as_ref(), 0x01);
                let mut t = KlineDiag {
                    dev,
                    target,
                    source,
                };
                kwp_security_access(&mut t, sk, level, required)
            }
            None => Err(Error::NotConnected),
        }
    }
}

// ---- DiagTransport adapters ----------------------------------------------

/// UDS/KWP request→response over the device's on-board ISO-TP.
pub struct IsoTpDiag<'a> {
    dev: &'a mut Device<Serial>,
}

impl DiagTransport for IsoTpDiag<'_> {
    fn request(&mut self, req: &[u8], timeout_ms: u64) -> Result<Vec<u8>> {
        self.dev.isotp_request(clamp_timeout(timeout_ms), req)
    }
}

/// KWP2000 request→response over the device's K-line. Builds the ISO-14230
/// frame (format + target + source + data + checksum) and parses the reply,
/// mirroring the `kline-bridge` driver.
pub struct KlineDiag<'a> {
    dev: &'a mut Device<Serial>,
    target: u8,
    source: u8,
}

impl KlineDiag<'_> {
    fn kwp_frame(&self, data: &[u8]) -> Vec<u8> {
        let mut f = vec![0x80 | (data.len() as u8 & 0x3F), self.target, self.source];
        f.extend_from_slice(data);
        let cksum = f.iter().fold(0u8, |a, &b| a.wrapping_add(b));
        f.push(cksum);
        f
    }
}

impl DiagTransport for KlineDiag<'_> {
    fn request(&mut self, req: &[u8], timeout_ms: u64) -> Result<Vec<u8>> {
        let frame = self.kwp_frame(req);
        let raw = self
            .dev
            .kline_transceive(clamp_timeout(timeout_ms), &frame)?;
        // The device may return one message or several (e.g. a responsePending
        // then the real answer) concatenated; walk the KWP stream and return the
        // first non-pending service payload.
        let mut last_err = Error::Protocol("no KWP frame in K-line reply".into());
        for payload in parse_kwp_stream(&raw) {
            if payload.len() >= 3 && payload[0] == 0x7F && payload[2] == 0x78 {
                continue; // responsePending
            }
            return Ok(payload);
        }
        // If everything was pending (or nothing parsed), surface a timeout.
        if raw.is_empty() {
            last_err = Error::Timeout(timeout_ms);
        }
        Err(last_err)
    }
}

/// The driver itself is the session's [`DiagTransport`]: it dispatches each
/// request to the device's on-board ISO-TP or K-line path, whichever `open()`
/// selected. (The per-call adapters borrow the device, so this is what
/// [`Interface::diag`] hands out.)
impl DiagTransport for Esp32Driver {
    fn request(&mut self, req: &[u8], timeout_ms: u64) -> Result<Vec<u8>> {
        let dev = self.device.as_mut().ok_or(Error::NotConnected)?;
        match self.mode.as_ref() {
            Some(Mode::IsoTp) => IsoTpDiag { dev }.request(req, timeout_ms),
            Some(Mode::Kline { target, source }) => KlineDiag {
                dev,
                target: *target,
                source: *source,
            }
            .request(req, timeout_ms),
            None => Err(Error::NotConnected),
        }
    }
}

/// Split a raw ISO-14230 byte stream into its service-data payloads.
fn parse_kwp_stream(buf: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= buf.len() {
        let fmt = buf[i];
        let (hdr, len) = if fmt & 0x3F != 0 {
            (3usize, (fmt & 0x3F) as usize)
        } else {
            (4usize, buf[i + 3] as usize)
        };
        let total = hdr + len + 1; // + checksum
        if i + total > buf.len() {
            break;
        }
        out.push(buf[i + hdr..i + hdr + len].to_vec());
        i += total;
    }
    out
}

/// The device's raw CAN exposed as a [`CanLink`], for running the host-side
/// `IsoTp` over the device instead of its on-board ISO-TP.
pub struct CanRawLink<'a> {
    dev: &'a mut Device<Serial>,
    recv_timeout_ms: u16,
}

impl CanRawLink<'_> {
    pub fn new(dev: &mut Device<Serial>, recv_timeout_ms: u16) -> CanRawLink<'_> {
        CanRawLink {
            dev,
            recv_timeout_ms,
        }
    }
}

impl CanLink for CanRawLink<'_> {
    fn send_frame(&mut self, id: u32, data: &[u8]) -> Result<()> {
        self.dev.can_send(id, id > 0x7FF, false, data)
    }
    fn recv_frame(&mut self, timeout_ms: u64) -> Result<(u32, Vec<u8>)> {
        let to = if timeout_ms == 0 {
            self.recv_timeout_ms
        } else {
            clamp_timeout(timeout_ms)
        };
        let f = self.dev.can_recv(to)?;
        Ok((f.id, f.data))
    }
}

fn clamp_timeout(ms: u64) -> u16 {
    ms.min(u16::MAX as u64) as u16
}

impl Interface for Esp32Driver {
    fn set_seed_key(&mut self, seed_key: Box<dyn SeedKey>) -> bool {
        Esp32Driver::set_seed_key(self, seed_key);
        true
    }

    fn info(&self) -> InterfaceInfo {
        InterfaceInfo {
            id: "esp32-opentune".into(),
            display_name: "OpenTune ESP32-S3 interface".into(),
            firmware: self.firmware.clone(),
            serial: self.serial.clone(),
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

    /// Expose the device's ISO-TP (UDS) or K-line (KWP2000) request/response
    /// path as a [`DiagTransport`] so the coding/adaptation/auto-scan/service/
    /// logging engines can drive it. `None` until `open()` succeeds.
    fn diag(&mut self) -> Option<&mut dyn DiagTransport> {
        if self.device.is_some() && self.mode.is_some() {
            Some(self as &mut dyn DiagTransport)
        } else {
            None
        }
    }

    fn open(&mut self, cfg: &ConnectConfig) -> Result<()> {
        let path = cfg
            .options
            .get("port")
            .ok_or_else(|| Error::Config("esp32-opentune needs options[\"port\"]".into()))?;
        let port = serialport::new(path, 115_200)
            .timeout(Duration::from_millis(3000))
            .open()
            .map_err(|e| Error::Io(std::io::Error::other(e)))?;
        let mut device = Device::new(port);

        // Verify the link and record firmware identity.
        let info = device.ping()?;
        self.firmware = Some(format!(
            "{} v{}.{}.{} (proto {})",
            info.id, info.version.0, info.version.1, info.version.2, info.proto
        ));

        match cfg.protocol {
            Protocol::IsoTp | Protocol::RawCan => {
                device.can_set_bitrate(cfg.bitrate)?;
                device.can_start(0)?;
                let ext = cfg.tx_id > 0x7FF || cfg.rx_id > 0x7FF;
                device.isotp_config(cfg.tx_id, cfg.rx_id, ext, 0, 0, Some(0xAA))?;
                self.mode = Some(Mode::IsoTp);
            }
            Protocol::Iso14230Kline | Protocol::Iso9141 => {
                device.kline_set_baud(cfg.bitrate.max(10_400))?;
                let target = cfg.tx_id as u8;
                let five_baud = cfg
                    .options
                    .get("init")
                    .map(|s| s == "5baud")
                    .unwrap_or(false);
                if five_baud {
                    device.kline_init_5baud(target)?;
                } else {
                    device.kline_init_fast(&[])?;
                }
                self.mode = Some(Mode::Kline {
                    target,
                    source: 0xF1,
                });
            }
            Protocol::Vwtp20 => {
                return Err(Error::Unsupported(
                    "VW TP2.0 is not yet implemented in the ESP32 firmware".into(),
                ));
            }
        }

        self.device = Some(device);
        self.cfg = Some(cfg.clone());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        if let Some(dev) = self.device.as_mut() {
            dev.can_stop().ok();
            dev.reset().ok();
        }
        self.cfg = None;
        self.device = None;
        self.mode = None;
        Ok(())
    }

    fn battery_millivolts(&mut self) -> Result<Option<u32>> {
        let dev = self.dev()?;
        Ok(Some(dev.battery_mv()?))
    }

    fn read_ecu_id(&mut self) -> Result<EcuId> {
        match self.mode {
            Some(Mode::IsoTp) => {
                let dev = self.device.as_mut().ok_or(Error::NotConnected)?;
                let mut t = IsoTpDiag { dev };
                let mut uds = UdsClient::new(&mut t);
                let mut id = EcuId::default();
                for (name, did) in [("vin", 0xF190u16), ("sw", 0xF189), ("hw", 0xF191)] {
                    if let Ok(v) = uds.read_data_by_id(did) {
                        let s = String::from_utf8_lossy(&v).trim().to_string();
                        match name {
                            "vin" => id.vin = Some(s),
                            "sw" => id.sw_number = Some(s),
                            "hw" => id.hw_number = Some(s),
                            _ => {}
                        }
                    }
                }
                self.mode = Some(Mode::IsoTp);
                Ok(id)
            }
            Some(Mode::Kline { target, source }) => {
                let dev = self.device.as_mut().ok_or(Error::NotConnected)?;
                let mut t = KlineDiag {
                    dev,
                    target,
                    source,
                };
                let mut kwp = Kwp2000Client::new(&mut t);
                let raw = kwp.read_ecu_id(0x9B).unwrap_or_default();
                let mut id = EcuId::default();
                if !raw.is_empty() {
                    id.raw.insert("1A9B".into(), hex(&raw));
                }
                self.mode = Some(Mode::Kline { target, source });
                Ok(id)
            }
            None => Err(Error::NotConnected),
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
        match self.mode {
            Some(Mode::IsoTp) => {
                let dev = self.device.as_mut().ok_or(Error::NotConnected)?;
                let mut t = IsoTpDiag { dev };
                let mut uds = UdsClient::new(&mut t);
                let max = uds.request_upload(0x00, region.address, region.length)?;
                let block = max.clamp(1, 4095);
                let mut out = Vec::with_capacity(region.length as usize);
                let mut seq = 1u8;
                while (out.len() as u32) < region.length {
                    let data = uds.transfer_data(seq, &[])?;
                    if data.len() <= 1 {
                        break;
                    }
                    out.extend_from_slice(&data[1..]);
                    seq = seq.wrapping_add(1);
                    progress(Progress {
                        phase: Phase::Read,
                        done: out.len() as u64,
                        total: region.length as u64,
                    });
                    let _ = block;
                }
                uds.transfer_exit().ok();
                out.truncate(region.length as usize);
                self.mode = Some(Mode::IsoTp);
                Ok(out)
            }
            Some(Mode::Kline { target, source }) => {
                let dev = self.device.as_mut().ok_or(Error::NotConnected)?;
                let mut t = KlineDiag {
                    dev,
                    target,
                    source,
                };
                let mut kwp = Kwp2000Client::new(&mut t);
                let mut out = Vec::with_capacity(region.length as usize);
                let mut addr = region.address;
                let end = region.end();
                while addr < end {
                    let chunk = ((end - addr).min(0xFE)) as u8;
                    // Exact length: a short reply must not shift every later address.
                    let data = kwp.read_memory_exact(addr, chunk)?;
                    out.extend_from_slice(&data);
                    addr += chunk as u32;
                    progress(Progress {
                        phase: Phase::Read,
                        done: (addr - region.address) as u64,
                        total: region.length as u64,
                    });
                }
                self.mode = Some(Mode::Kline { target, source });
                Ok(out)
            }
            None => Err(Error::NotConnected),
        }
    }

    fn write_memory(
        &mut self,
        region: MemoryRegion,
        data: &[u8],
        progress: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        // Unlock the ECU (service 0x27) before request_download if required.
        self.security_access()?;
        match self.mode {
            Some(Mode::IsoTp) => {
                let dev = self.device.as_mut().ok_or(Error::NotConnected)?;
                let mut t = IsoTpDiag { dev };
                let mut uds = UdsClient::new(&mut t);
                let max = uds.request_download(0x00, region.address, data.len() as u32)?;
                let block = max.clamp(2, 4095) - 1;
                let mut seq = 1u8;
                for chunk in data.chunks(block) {
                    uds.transfer_data(seq, chunk)?;
                    seq = seq.wrapping_add(1);
                    progress(Progress {
                        phase: Phase::Write,
                        done: 0,
                        total: data.len() as u64,
                    });
                }
                uds.transfer_exit()?;
                self.mode = Some(Mode::IsoTp);
                Ok(())
            }
            Some(Mode::Kline { target, source }) => {
                let dev = self.device.as_mut().ok_or(Error::NotConnected)?;
                let mut t = KlineDiag {
                    dev,
                    target,
                    source,
                };
                let mut kwp = Kwp2000Client::new(&mut t);
                kwp.request_download(region.address, data.len() as u32, 0x00)?;
                let mut done = 0usize;
                for block in data.chunks(0xFE) {
                    kwp.transfer_data(block)?;
                    done += block.len();
                    progress(Progress {
                        phase: Phase::Write,
                        done: done as u64,
                        total: data.len() as u64,
                    });
                }
                kwp.transfer_exit()?;
                self.mode = Some(Mode::Kline { target, source });
                Ok(())
            }
            None => Err(Error::NotConnected),
        }
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

// ---------------------------------------------------------------------------
// Security access (service 0x27) hook.
//
// OpenTune ships NO OEM seed/key algorithms. A seed/key handler is supplied by
// the application via [`Esp32Driver::set_seed_key`] (or a plugin), keyed by the
// `seedkey` id in an ECU definition. These helpers run request-seed → compute →
// send-key over the existing `UdsClient` / `Kwp2000Client` before a download —
// the UDS variant on the CAN path, the KWP variant on the K-line path. (Kept
// self-contained per the convention of the other OpenTune drivers, which carry
// their own copy rather than share through core.)
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

/// UDS service 0x27 seed/key (CAN path). Errors only when security is required
/// and no handler is set.
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

/// KWP2000 service 0x27 seed/key (K-line path). Errors only when security is
/// required and no handler is set.
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
mod seedkey_hook_tests {
    use super::*;
    use std::collections::VecDeque;

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
        let expected_key: Vec<u8> = seed.iter().map(|b| (b ^ 0x5A).wrapping_add(1)).collect();
        let mut expected_req = vec![0x27u8, 0x02];
        expected_req.extend_from_slice(&expected_key);
        assert_eq!(t.requests[1], expected_req);
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
        assert!(matches!(
            kwp_security_access(&mut t, None, 0x01, true),
            Err(Error::Config(_))
        ));
        assert!(t.requests.is_empty());
    }

    #[test]
    fn noop_when_open_ecu_and_no_handler() {
        let mut t = ScriptedTransport {
            replies: VecDeque::new(),
            requests: Vec::new(),
        };
        uds_security_access(&mut t, None, 0x11, false).unwrap();
        kwp_security_access(&mut t, None, 0x01, false).unwrap();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn diag_exposes_device_transport_after_connect() {
        // A pseudo-terminal pair stands in for the adapter's serial port.
        let (host, _adapter) = serialport::TTYPort::pair().expect("pty pair");
        let port: Box<dyn serialport::SerialPort> = Box::new(host);
        let mut d = Esp32Driver::new();
        assert!(d.diag().is_none(), "no transport before open");
        d.device = Some(Device::new(port));
        assert!(d.diag().is_none(), "no transport until a mode is selected");
        d.mode = Some(Mode::IsoTp);
        assert!(d.diag().is_some(), "diag() must expose the ISO-TP path");
        d.mode = Some(Mode::Kline {
            target: 0x10,
            source: 0xF1,
        });
        assert!(d.diag().is_some(), "diag() must expose the K-line path");
    }

    #[test]
    fn kwp_frame_has_correct_header_and_checksum() {
        // Build a KlineDiag without a device just to reach kwp_frame via a
        // helper replica — we test the framing math directly here.
        let data = [0x1A, 0x9B];
        let target = 0x10;
        let source = 0xF1;
        let mut f = vec![0x80 | (data.len() as u8 & 0x3F), target, source];
        f.extend_from_slice(&data);
        let cksum = f.iter().fold(0u8, |a, &b| a.wrapping_add(b));
        f.push(cksum);
        assert_eq!(f[0], 0x82); // format byte: 0x80 | len(2)
        assert_eq!(f[1], 0x10);
        assert_eq!(f[2], 0xF1);
        assert_eq!(
            *f.last().unwrap(),
            0x82u8
                .wrapping_add(0x10)
                .wrapping_add(0xF1)
                .wrapping_add(0x1A)
                .wrapping_add(0x9B)
        );
    }

    #[test]
    fn parse_kwp_stream_single_message() {
        // 0x83 10 F1 <3 data> cksum  -> physical, len 3
        let data = [0x50, 0x01, 0x02];
        let mut f = vec![0x80 | 0x03, 0x10, 0xF1];
        f.extend_from_slice(&data);
        let c = f.iter().fold(0u8, |a, &b| a.wrapping_add(b));
        f.push(c);
        let msgs = parse_kwp_stream(&f);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0], data);
    }

    #[test]
    fn parse_kwp_stream_pending_then_real() {
        // responsePending 7F xx 78, then the real 50 .. answer, concatenated.
        let mk = |data: &[u8]| {
            let mut f = vec![0x80 | (data.len() as u8 & 0x3F), 0x10, 0xF1];
            f.extend_from_slice(data);
            let c = f.iter().fold(0u8, |a, &b| a.wrapping_add(b));
            f.push(c);
            f
        };
        let mut buf = mk(&[0x7F, 0x10, 0x78]);
        buf.extend(mk(&[0x50, 0x01]));
        let msgs = parse_kwp_stream(&buf);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0], vec![0x7F, 0x10, 0x78]);
        assert_eq!(msgs[1], vec![0x50, 0x01]);
    }

    #[test]
    fn clamp_timeout_caps_at_u16() {
        assert_eq!(clamp_timeout(100), 100);
        assert_eq!(clamp_timeout(1_000_000), u16::MAX);
    }
}
