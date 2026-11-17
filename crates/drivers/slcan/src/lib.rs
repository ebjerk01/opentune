//! slcan (LAWICEL) USB-CAN driver — for cheap serial CAN adapters like CANable
//! / CANtact running slcan firmware, and the CH340 "USB-CAN" clones that speak
//! the slcan ASCII protocol. Presents as a serial port; this driver implements
//! the core `CanLink` over it and runs UDS/ISO-TP on top.
//!
//! slcan ASCII protocol: `Sn\r` set bitrate preset, `O\r` open, `C\r` close,
//! `t<iii><l><dd…>\r` standard frame, `T<iiiiiiii><l><dd…>\r` extended frame.
use opentune_core::protocol::diag::DiagTransport;
use opentune_core::protocol::isotp::{CanLink, IsoTp};
use opentune_core::protocol::seedkey::SeedKey;
use opentune_core::protocol::uds::UdsClient;
use opentune_core::{
    Capabilities, ConnectConfig, EcuId, Error, Interface, InterfaceInfo, MemoryRegion, Phase,
    Progress, Result,
};
use std::io::{Read, Write};
use std::time::Duration;

pub mod monitor;

struct Slcan {
    port: Box<dyn serialport::SerialPort>,
    ext: bool,
}

fn bitrate_preset(bitrate: u32) -> &'static str {
    match bitrate {
        10_000 => "S0",
        20_000 => "S1",
        50_000 => "S2",
        100_000 => "S3",
        125_000 => "S4",
        250_000 => "S5",
        500_000 => "S6",
        800_000 => "S7",
        1_000_000 => "S8",
        _ => "S6",
    }
}

impl Slcan {
    fn line(&mut self, s: &str) -> Result<()> {
        self.port
            .write_all(format!("{s}\r").as_bytes())
            .map_err(Error::Io)?;
        self.port.flush().map_err(Error::Io)?;
        Ok(())
    }

    /// Read one `\r`-terminated response line.
    fn read_line(&mut self, timeout_ms: u64) -> Result<String> {
        self.port
            .set_timeout(Duration::from_millis(timeout_ms))
            .ok();
        let mut out = Vec::new();
        let mut b = [0u8; 1];
        loop {
            match self.port.read_exact(&mut b) {
                Ok(()) => {
                    if b[0] == b'\r' || b[0] == 0x07 {
                        if !out.is_empty() {
                            break;
                        }
                    } else {
                        out.push(b[0]);
                    }
                }
                Err(_) => return Err(Error::Timeout(timeout_ms)),
            }
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }
}

impl CanLink for Slcan {
    fn send_frame(&mut self, id: u32, data: &[u8]) -> Result<()> {
        let mut s = String::new();
        if self.ext || id > 0x7FF {
            s.push('T');
            s.push_str(&format!("{id:08X}"));
        } else {
            s.push('t');
            s.push_str(&format!("{id:03X}"));
        }
        s.push_str(&format!("{:X}", data.len().min(8)));
        for b in data.iter().take(8) {
            s.push_str(&format!("{b:02X}"));
        }
        self.line(&s)
    }

    fn recv_frame(&mut self, timeout_ms: u64) -> Result<(u32, Vec<u8>)> {
        loop {
            let line = self.read_line(timeout_ms)?;
            let bytes = line.as_bytes();
            let (id_len, ext) = match bytes.first() {
                Some(b't') => (3usize, false),
                Some(b'T') => (8usize, true),
                _ => continue, // z/Z acks or noise
            };
            if line.len() < 1 + id_len + 1 {
                continue;
            }
            let id = u32::from_str_radix(&line[1..1 + id_len], 16)
                .map_err(|_| Error::Protocol("bad slcan id".into()))?;
            let dlc = usize::from_str_radix(&line[1 + id_len..2 + id_len], 16).unwrap_or(0);
            let data_hex = &line[2 + id_len..];
            let mut data = Vec::with_capacity(dlc);
            for i in 0..dlc {
                if 2 * i + 2 <= data_hex.len() {
                    data.push(u8::from_str_radix(&data_hex[2 * i..2 * i + 2], 16).unwrap_or(0));
                }
            }
            let _ = ext;
            return Ok((id, data));
        }
    }
}

#[derive(Default)]
pub struct SlcanDriver {
    isotp: Option<IsoTp>,
    cfg: Option<ConnectConfig>,
    /// Optional seed/key handler for UDS security access (service 0x27). No OEM
    /// algorithm ships with OpenTune; the application supplies one.
    seed_key: Option<Box<dyn SeedKey>>,
    /// Listen-only full-bus capture (independent of `open`).
    monitor: monitor::SlcanMonitor,
}

impl SlcanDriver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install the seed/key handler used to unlock the ECU (service 0x27)
    /// before a download. Without it, writing a security-protected ECU fails
    /// with a clear error. OpenTune ships no OEM algorithms.
    pub fn set_seed_key(&mut self, seed_key: Box<dyn SeedKey>) {
        self.seed_key = Some(seed_key);
    }

    /// Run UDS security access (service 0x27) if a seed/key handler is set or
    /// the connection config marks security as required. A no-op on an open ECU
    /// with no handler configured, so unlocked sessions keep working.
    fn security_access(&mut self) -> Result<()> {
        let required = security_required(self.cfg.as_ref());
        let level = seedkey_level(self.cfg.as_ref(), 0x11);
        // Disjoint field borrows: `seed_key` (shared), `isotp` (unique).
        let sk = self.seed_key.as_deref();
        let tp = self.isotp.as_mut().ok_or(Error::NotConnected)?;
        uds_security_access(tp, sk, level, required)
    }
}

impl Interface for SlcanDriver {
    fn set_seed_key(&mut self, seed_key: Box<dyn SeedKey>) -> bool {
        SlcanDriver::set_seed_key(self, seed_key);
        true
    }

    fn info(&self) -> InterfaceInfo {
        InterfaceInfo {
            id: "slcan".into(),
            display_name: "slcan USB-CAN (CANable/CANtact)".into(),
            firmware: None,
            serial: None,
        }
    }

    fn can_monitor(&mut self) -> Option<&mut dyn opentune_core::CanMonitor> {
        Some(&mut self.monitor)
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            can: true,
            read_memory: true,
            write_memory: true,
            ..Default::default()
        }
    }

    /// Expose the host-side ISO-TP over the slcan link as a [`DiagTransport`] so the coding/adaptation/
    /// auto-scan/service/logging engines (and the raw-request console) can
    /// drive this adapter. `None` until `open()` succeeds.
    fn diag(&mut self) -> Option<&mut dyn DiagTransport> {
        self.isotp.as_mut().map(|t| t as &mut dyn DiagTransport)
    }

    fn open(&mut self, cfg: &ConnectConfig) -> Result<()> {
        let path = cfg
            .options
            .get("port")
            .ok_or_else(|| Error::Config("slcan needs options[\"port\"]".into()))?;
        let port = serialport::new(path, 115_200)
            .timeout(Duration::from_millis(1000))
            .open()
            .map_err(|e| Error::Io(std::io::Error::other(e)))?;
        let mut link = Slcan {
            port,
            ext: cfg.tx_id > 0x7FF,
        };
        link.line("C").ok(); // ensure closed
        link.line(bitrate_preset(cfg.bitrate))?;
        link.line("O")?; // open channel
        self.isotp = Some(IsoTp::new(Box::new(link), cfg.tx_id, cfg.rx_id));
        self.cfg = Some(cfg.clone());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.isotp = None;
        self.cfg = None;
        Ok(())
    }

    fn read_ecu_id(&mut self) -> Result<EcuId> {
        let tp = self.isotp.as_mut().ok_or(Error::NotConnected)?;
        let mut uds = UdsClient::new(tp);
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
        Ok(id)
    }

    fn read_memory(
        &mut self,
        region: MemoryRegion,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<Vec<u8>> {
        // A full read from a locked ECU needs security access too; a no-op when
        // no handler is set and security is not required.
        self.security_access()?;
        let tp = self.isotp.as_mut().ok_or(Error::NotConnected)?;
        let mut uds = UdsClient::new(tp);
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
        Ok(out)
    }

    fn write_memory(
        &mut self,
        region: MemoryRegion,
        data: &[u8],
        progress: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        // Unlock the ECU (service 0x27) before requestDownload if required.
        self.security_access()?;
        let tp = self.isotp.as_mut().ok_or(Error::NotConnected)?;
        let mut uds = UdsClient::new(tp);
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
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Security access (service 0x27) hook.
//
// OpenTune ships NO OEM seed/key algorithms. A seed/key handler is supplied by
// the application via [`SlcanDriver::set_seed_key`] (or a plugin), keyed by the
// `seedkey` id in an ECU definition. This helper runs the request-seed →
// compute → send-key handshake over the existing [`UdsClient`] before a
// download. (Kept self-contained per the convention of the other OpenTune
// drivers, which carry their own copy rather than share through core.)
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
        let mut d = SlcanDriver::new();
        assert!(d.diag().is_none(), "no transport before open");
        d.isotp = Some(IsoTp::new(Box::new(IdleLink), 0x7E0, 0x7E8));
        assert!(
            d.diag().is_some(),
            "diag() must expose the ISO-TP transport"
        );
    }
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
    fn already_unlocked_seed_skips_key() {
        // All-zero seed => already unlocked; no key is sent.
        let seed_resp = vec![0x67, 0x11, 0x00, 0x00, 0x00, 0x00];
        let mut t = ScriptedTransport {
            replies: VecDeque::from(vec![seed_resp]),
            requests: Vec::new(),
        };
        uds_security_access(&mut t, Some(&DemoXorAddKey), 0x11, true).unwrap();
        assert_eq!(t.requests.len(), 1); // only the request-seed, no send-key
        assert_eq!(t.requests[0], vec![0x27, 0x11]);
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
        let mut cfg = ConnectConfig::default();
        assert!(!security_required(Some(&cfg)));
        assert_eq!(seedkey_level(Some(&cfg), 0x11), 0x11);
        cfg.options.insert("seedkey".into(), "edc17-sa2".into());
        cfg.options.insert("seedkey_level".into(), "0x09".into());
        assert!(security_required(Some(&cfg)));
        assert_eq!(seedkey_level(Some(&cfg), 0x11), 0x09);
    }
}
