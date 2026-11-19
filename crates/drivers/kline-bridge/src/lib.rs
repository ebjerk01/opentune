//! Driver for OpenTune's Arduino "K-line bridge" firmware
//! (`hardware/firmware/kline-bridge`). The device does the timing-critical
//! K-line work (fast/5-baud init, inter-byte timing, echo removal) and exposes
//! a small USB frame protocol; this driver speaks that protocol and builds the
//! ISO-14230 (KWP2000) framing, then runs `Kwp2000Client` on top.
//!
//! Device frames: `0xB5 <cmd> <len> <payload…>`.
//!   H->D: 0x01 fast-init | 0x02 5baud <addr> | 0x03 set-baud <hi><lo> |
//!         0x10 transceive <kwp-frame…>
//!   D->H: 0x81 init-result <status…> | 0x90 rx <kwp-frame…> | 0xEE error <code>
use opentune_core::protocol::diag::{is_response_pending, DiagTransport};
use opentune_core::protocol::kwp2000::Kwp2000Client;
use opentune_core::protocol::seedkey::SeedKey;
use opentune_core::{
    Capabilities, ConnectConfig, EcuId, Error, Interface, InterfaceInfo, MemoryRegion, Phase,
    Progress, Result,
};
use std::io::{Read, Write};
use std::time::Duration;

const SYNC: u8 = 0xB5;

struct Bridge {
    port: Box<dyn serialport::SerialPort>,
    target: u8,
    source: u8,
}

impl Bridge {
    fn send(&mut self, cmd: u8, payload: &[u8]) -> Result<()> {
        let mut f = vec![SYNC, cmd, payload.len() as u8];
        f.extend_from_slice(payload);
        self.port.write_all(&f).map_err(Error::Io)?;
        Ok(())
    }

    /// Read one device frame, returning `(cmd, payload)`.
    fn recv(&mut self, timeout_ms: u64) -> Result<(u8, Vec<u8>)> {
        self.port
            .set_timeout(Duration::from_millis(timeout_ms))
            .ok();
        let mut b = [0u8; 1];
        // resync to SYNC
        loop {
            self.port
                .read_exact(&mut b)
                .map_err(|_| Error::Timeout(timeout_ms))?;
            if b[0] == SYNC {
                break;
            }
        }
        let mut hdr = [0u8; 2];
        self.port
            .read_exact(&mut hdr)
            .map_err(|_| Error::Timeout(timeout_ms))?;
        let (cmd, len) = (hdr[0], hdr[1] as usize);
        let mut data = vec![0u8; len];
        if len > 0 {
            self.port
                .read_exact(&mut data)
                .map_err(|_| Error::Timeout(timeout_ms))?;
        }
        Ok((cmd, data))
    }

    fn kwp_frame(&self, data: &[u8]) -> Vec<u8> {
        // ISO-14230 physical addressing, length in the format byte.
        let mut f = vec![0x80 | (data.len() as u8 & 0x3F), self.target, self.source];
        f.extend_from_slice(data);
        let cksum = f.iter().fold(0u8, |a, &b| a.wrapping_add(b));
        f.push(cksum);
        f
    }

    fn parse_kwp(&self, f: &[u8]) -> Result<Vec<u8>> {
        if f.len() < 4 {
            return Err(Error::Protocol("short KWP frame".into()));
        }
        let fmt = f[0];
        let (hdr, len) = if fmt & 0x3F != 0 {
            (3usize, (fmt & 0x3F) as usize)
        } else {
            (4usize, f[3] as usize)
        };
        if f.len() < hdr + len + 1 {
            return Err(Error::Protocol("truncated KWP frame".into()));
        }
        Ok(f[hdr..hdr + len].to_vec())
    }
}

impl DiagTransport for Bridge {
    fn request(&mut self, req: &[u8], timeout_ms: u64) -> Result<Vec<u8>> {
        let frame = self.kwp_frame(req);
        self.send(0x10, &frame)?;
        for _ in 0..32 {
            let (cmd, data) = self.recv(timeout_ms)?;
            match cmd {
                0x90 => {
                    let resp = self.parse_kwp(&data)?;
                    if is_response_pending(req, &resp) {
                        continue; // response pending for this service
                    }
                    return Ok(resp);
                }
                0xEE => {
                    return Err(Error::Protocol(format!(
                        "device error 0x{:02X}",
                        data.first().copied().unwrap_or(0)
                    )))
                }
                _ => continue,
            }
        }
        Err(Error::Timeout(timeout_ms))
    }
}

#[derive(Default)]
pub struct BridgeDriver {
    bridge: Option<Bridge>,
    cfg: Option<ConnectConfig>,
    /// Optional seed/key handler for KWP2000 security access (service 0x27). No
    /// OEM algorithm ships with OpenTune; the application supplies one.
    seed_key: Option<Box<dyn SeedKey>>,
}

impl BridgeDriver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install the seed/key handler used to unlock the ECU (service 0x27)
    /// before a download. Without it, writing a security-protected ECU fails
    /// with a clear error. OpenTune ships no OEM algorithms.
    pub fn set_seed_key(&mut self, seed_key: Box<dyn SeedKey>) {
        self.seed_key = Some(seed_key);
    }

    /// Run KWP2000 security access (service 0x27) if a seed/key handler is set
    /// or the connection config marks security as required. A no-op on an open
    /// ECU with no handler configured, so unlocked sessions keep working.
    fn security_access(&mut self) -> Result<()> {
        let required = security_required(self.cfg.as_ref());
        let level = seedkey_level(self.cfg.as_ref(), 0x01);
        // Disjoint field borrows: `seed_key` (shared), `bridge` (unique).
        let sk = self.seed_key.as_deref();
        let b = self.bridge.as_mut().ok_or(Error::NotConnected)?;
        kwp_security_access(b, sk, level, required)
    }
}

impl Interface for BridgeDriver {
    fn set_seed_key(&mut self, seed_key: Box<dyn SeedKey>) -> bool {
        BridgeDriver::set_seed_key(self, seed_key);
        true
    }

    fn info(&self) -> InterfaceInfo {
        InterfaceInfo {
            id: "kline-bridge".into(),
            display_name: "Arduino K-line bridge".into(),
            firmware: None,
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

    /// Expose the K-line bridge's KWP2000 transport as a [`DiagTransport`] so the coding/adaptation/
    /// auto-scan/service/logging engines (and the raw-request console) can
    /// drive this adapter. `None` until `open()` succeeds.
    fn diag(&mut self) -> Option<&mut dyn DiagTransport> {
        self.bridge.as_mut().map(|t| t as &mut dyn DiagTransport)
    }

    fn open(&mut self, cfg: &ConnectConfig) -> Result<()> {
        let path = cfg
            .options
            .get("port")
            .ok_or_else(|| Error::Config("kline-bridge needs options[\"port\"]".into()))?;
        let port = serialport::new(path, 115_200)
            .timeout(Duration::from_millis(1000))
            .open()
            .map_err(|e| Error::Io(std::io::Error::other(e)))?;
        let mut b = Bridge {
            port,
            target: cfg.tx_id as u8,
            source: 0xF1,
        };
        // optional K-line baud
        if let Some(baud) = cfg.options.get("baud").and_then(|s| s.parse::<u32>().ok()) {
            b.send(0x03, &[(baud >> 8) as u8, baud as u8])?;
        }
        // init
        let five = cfg
            .options
            .get("init")
            .map(|s| s == "5baud")
            .unwrap_or(false);
        if five {
            b.send(0x02, &[b.target])?;
        } else {
            b.send(0x01, &[])?;
        }
        let (cmd, data) = b.recv(2000)?;
        if cmd != 0x81 || data.first().copied().unwrap_or(0xFF) != 0 {
            return Err(Error::Protocol("K-line init failed".into()));
        }
        self.bridge = Some(b);
        self.cfg = Some(cfg.clone());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.bridge = None;
        self.cfg = None;
        Ok(())
    }

    fn read_ecu_id(&mut self) -> Result<EcuId> {
        let b = self.bridge.as_mut().ok_or(Error::NotConnected)?;
        let mut kwp = Kwp2000Client::new(b);
        // KWP readEcuIdentification 0x1A 0x9B (common) — best-effort.
        let raw = kwp.read_ecu_id(0x9B).unwrap_or_default();
        let mut id = EcuId::default();
        if !raw.is_empty() {
            id.raw.insert("1A9B".into(), hex(&raw));
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
        let b = self.bridge.as_mut().ok_or(Error::NotConnected)?;
        let mut kwp = Kwp2000Client::new(b);
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
        let b = self.bridge.as_mut().ok_or(Error::NotConnected)?;
        let mut kwp = Kwp2000Client::new(b);
        kwp.request_download(region.address, data.len() as u32, 0x00)?;
        let mut seq = 0usize;
        for block in data.chunks(0xFE) {
            kwp.transfer_data(block)?;
            seq += block.len();
            progress(Progress {
                phase: Phase::Write,
                done: seq as u64,
                total: data.len() as u64,
            });
        }
        kwp.transfer_exit()?;
        Ok(())
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

// ---------------------------------------------------------------------------
// Security access (service 0x27) hook.
//
// OpenTune ships NO OEM seed/key algorithms. A seed/key handler is supplied by
// the application via [`BridgeDriver::set_seed_key`] (or a plugin), keyed by the
// `seedkey` id in an ECU definition. This helper runs the request-seed →
// compute → send-key handshake over the existing [`Kwp2000Client`] before a
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

/// Perform KWP2000 service 0x27 seed/key if a handler is set; otherwise error
/// only when security is required.
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
                // Open ECU, or already unlocked: nothing to do.
                Ok(())
            }
        }
        Some(sk) => {
            let mut kwp = Kwp2000Client::new(transport);
            let seed = kwp.request_seed(request_level)?;
            // An all-zero seed conventionally means "already unlocked".
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

    #[cfg(unix)]
    #[test]
    fn diag_exposes_bridge_after_connect() {
        // A pseudo-terminal pair stands in for the adapter's serial port.
        let (host, _adapter) = serialport::TTYPort::pair().expect("pty pair");
        let port: Box<dyn serialport::SerialPort> = Box::new(host);
        let mut d = BridgeDriver::new();
        assert!(d.diag().is_none(), "no transport before open");
        d.bridge = Some(Bridge {
            port,
            target: 0x10,
            source: 0xF1,
        });
        assert!(d.diag().is_some(), "diag() must expose the K-line bridge");
    }
    use std::collections::VecDeque;

    /// One request through a pseudo-terminal bridge whose device has already
    /// queued `replies` (each a KWP payload in a `0x90` rx frame).
    #[cfg(unix)]
    fn bridge_exchange(req: &[u8], replies: &[&[u8]]) -> Result<Vec<u8>> {
        use std::io::Write;
        let (host, mut adapter) = serialport::TTYPort::pair().expect("pty pair");
        let mut b = Bridge {
            port: Box::new(host),
            target: 0x10,
            source: 0xF1,
        };
        for r in replies {
            // ISO-14230 frame from the ECU to the tester.
            let mut kwp = vec![0x80 | r.len() as u8, 0xF1, 0x10];
            kwp.extend_from_slice(r);
            kwp.push(kwp.iter().fold(0u8, |a, &x| a.wrapping_add(x)));
            let mut dev = vec![SYNC, 0x90, kwp.len() as u8];
            dev.extend(kwp);
            adapter.write_all(&dev).unwrap();
        }
        adapter.flush().unwrap();
        b.request(req, 300)
    }

    #[cfg(unix)]
    #[test]
    fn response_pending_is_echo_checked() {
        let real: &[u8] = &[0x63, 0x01, 0x02];
        let r = bridge_exchange(
            &[0x23, 0x00, 0x10, 0x00, 0x02],
            &[&[0x7F, 0x23, 0x78], real],
        );
        assert_eq!(r.unwrap(), real, "0x78 for this service is waited out");
        // A 0x78 for another service (0x10) is not treated as pending.
        let r = bridge_exchange(
            &[0x23, 0x00, 0x10, 0x00, 0x02],
            &[&[0x7F, 0x10, 0x78], real],
        );
        assert_eq!(r.unwrap(), vec![0x7F, 0x10, 0x78]);
    }

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
        let seed = [0x01u8, 0x02, 0x03, 0x04];
        let mut seed_resp = vec![0x67, 0x01]; // positiveResponse, echoed level
        seed_resp.extend_from_slice(&seed);
        let key_ok = vec![0x67, 0x02]; // send-key accepted
        let mut t = ScriptedTransport {
            replies: VecDeque::from(vec![seed_resp, key_ok]),
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
    fn already_unlocked_seed_skips_key() {
        let seed_resp = vec![0x67, 0x01, 0x00, 0x00, 0x00, 0x00];
        let mut t = ScriptedTransport {
            replies: VecDeque::from(vec![seed_resp]),
            requests: Vec::new(),
        };
        kwp_security_access(&mut t, Some(&DemoXorAddKey), 0x01, true).unwrap();
        assert_eq!(t.requests.len(), 1); // only the request-seed, no send-key
    }

    #[test]
    fn errors_when_required_but_no_handler() {
        let mut t = ScriptedTransport {
            replies: VecDeque::new(),
            requests: Vec::new(),
        };
        let err = kwp_security_access(&mut t, None, 0x01, true).unwrap_err();
        assert!(matches!(err, Error::Config(_)));
        assert!(t.requests.is_empty());
    }

    #[test]
    fn noop_when_open_ecu_and_no_handler() {
        let mut t = ScriptedTransport {
            replies: VecDeque::new(),
            requests: Vec::new(),
        };
        kwp_security_access(&mut t, None, 0x01, false).unwrap();
        assert!(t.requests.is_empty());
    }

    #[test]
    fn config_helpers_parse_options() {
        let mut cfg = ConnectConfig::default();
        assert!(!security_required(Some(&cfg)));
        assert_eq!(seedkey_level(Some(&cfg), 0x01), 0x01);
        cfg.options.insert("seedkey".into(), "edc16-vag".into());
        cfg.options.insert("seedkey_level".into(), "0x05".into());
        assert!(security_required(Some(&cfg)));
        assert_eq!(seedkey_level(Some(&cfg), 0x01), 0x05);
    }
}
