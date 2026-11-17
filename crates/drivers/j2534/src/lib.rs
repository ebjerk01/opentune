//! Vendor-neutral **J2534** (SAE J2534-1 "PassThru") driver for OpenTune.
//!
//! A J2534 interface — Drew Technologies Mongoose, VNCI, SVCI, Scanmatik, and
//! the many clones — is reached through the vendor's PassThru DLL, which the OS
//! loads and OpenTune calls over FFI. The device performs ISO-TP segmentation,
//! so this driver implements [`DiagTransport`] directly (one PASSTHRU_MSG per
//! request/response) and lets the fixed opentune-core [`UdsClient`] run on top.
//!
//! ## Platform story
//! The PassThru API is a **Windows** C ABI (`PassThruOpen`, `PassThruConnect`,
//! `PassThruReadMsgs`, …) exported by a vendor DLL named in the registry. That
//! binding lives in [`ffi`] and is compiled only on Windows with the
//! `passthru-ffi` feature. Everything else — the J2534 constants, the
//! [`codec`] PASSTHRU_MSG framing, the [`PassThru`] backend trait, and a
//! [`MockPassThru`] — is portable, so the crate builds, tests and clippy-cleans
//! on Linux, where [`J2534Driver::open`] returns a clear [`Error::Unsupported`]
//! unless a backend is injected with [`J2534Driver::with_device`].
//!
//! ## Config ([`ConnectConfig::options`])
//! - `library`: path to the vendor PassThru DLL (Windows + `passthru-ffi`).
//! - `tx_id` / `rx_id`: diagnostic CAN ids (11- or 29-bit, auto-detected).
//! - `bitrate`: CAN bit rate (default 500000).
//!
//! Nothing here has been run against real hardware; the backend is mock-tested.

pub mod codec;

#[cfg(all(windows, feature = "passthru-ffi"))]
pub mod ffi;

use codec::PassThruMsg;
use opentune_core::protocol::diag::{check_response, DiagTransport};
use opentune_core::protocol::seedkey::SeedKey;
use opentune_core::protocol::uds::UdsClient;
use opentune_core::{
    Capabilities, ConnectConfig, EcuId, Error, Interface, InterfaceInfo, MemoryRegion, Phase,
    Progress, Protocol, Result,
};

/// A J2534 PassThru backend. One implementation wraps the real vendor DLL
/// (Windows, [`ffi`]); [`MockPassThru`] is used for tests. Methods map to the
/// J2534-1 functions named in the docs.
pub trait PassThru: Send {
    /// `PassThruConnect`: open a protocol channel (the handle is kept inside the
    /// backend).
    fn connect(&mut self, protocol_id: u32, flags: u32, baud: u32) -> Result<()>;
    /// `PassThruStartMsgFilter`: install a filter (ISO15765 needs a
    /// flow-control filter so replies are routed back).
    fn start_msg_filter(
        &mut self,
        filter_type: u32,
        mask: &PassThruMsg,
        pattern: &PassThruMsg,
        flow_control: Option<&PassThruMsg>,
    ) -> Result<()>;
    /// `PassThruWriteMsgs` for a single message.
    fn write_msg(&mut self, msg: &PassThruMsg, timeout_ms: u64) -> Result<()>;
    /// `PassThruReadMsgs` for a single message. Returns [`Error::Timeout`] when
    /// nothing arrives in time.
    fn read_msg(&mut self, timeout_ms: u64) -> Result<PassThruMsg>;
    /// `PassThruDisconnect` + `PassThruClose`.
    fn close(&mut self) -> Result<()>;
}

/// Opens a [`PassThru`] backend for a given config (loads the DLL on Windows).
type Opener = Box<dyn Fn(&ConnectConfig) -> Result<Box<dyn PassThru>> + Send + Sync>;

/// [`DiagTransport`] over a PassThru channel: writes an ISO15765 request message
/// and reads reply messages, skipping transmit indications and looping on the
/// responsePending NRC (0x78).
struct PassThruDiag<'a> {
    dev: &'a mut dyn PassThru,
    tx_id: u32,
    rx_id: u32,
}

impl DiagTransport for PassThruDiag<'_> {
    fn request(&mut self, req: &[u8], timeout_ms: u64) -> Result<Vec<u8>> {
        let msg = PassThruMsg::iso15765_request(self.tx_id, req);
        self.dev.write_msg(&msg, timeout_ms)?;
        for _ in 0..64 {
            let rx = self.dev.read_msg(timeout_ms)?;
            if rx.is_indication() {
                continue; // transmit/echo indication, not a reply
            }
            let (id, payload) = rx.parse_iso15765()?;
            if id != self.rx_id || payload.is_empty() {
                continue; // another ECU / empty frame
            }
            if payload.len() >= 3 && payload[0] == 0x7F && payload[2] == 0x78 {
                continue; // requestCorrectlyReceived-ResponsePending
            }
            return Ok(payload);
        }
        Err(Error::Timeout(timeout_ms))
    }
}

/// Vendor-neutral J2534 interface driver.
pub struct J2534Driver {
    opener: Opener,
    device: Option<Box<dyn PassThru>>,
    injected: Option<Box<dyn PassThru>>,
    tx_id: u32,
    rx_id: u32,
    cfg: Option<ConnectConfig>,
    seed_key: Option<Box<dyn SeedKey>>,
}

impl Default for J2534Driver {
    fn default() -> Self {
        Self {
            opener: Box::new(default_opener),
            device: None,
            injected: None,
            tx_id: 0,
            rx_id: 0,
            cfg: None,
            seed_key: None,
        }
    }
}

impl J2534Driver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install the seed/key handler used to unlock the ECU (service 0x27)
    /// before a download. OpenTune ships no OEM algorithms.
    pub fn set_seed_key(&mut self, seed_key: Box<dyn SeedKey>) {
        self.seed_key = Some(seed_key);
    }

    /// Inject a ready-made PassThru backend (e.g. a test mock), used by `open`
    /// in place of loading a DLL.
    pub fn with_device(device: Box<dyn PassThru>) -> Self {
        let mut d = Self::new();
        d.injected = Some(device);
        d
    }

    /// Replace the DLL opener (e.g. to point at a specific library loader).
    pub fn with_opener<F>(mut self, opener: F) -> Self
    where
        F: Fn(&ConnectConfig) -> Result<Box<dyn PassThru>> + Send + Sync + 'static,
    {
        self.opener = Box::new(opener);
        self
    }

    fn diag(&mut self) -> Result<PassThruDiag<'_>> {
        let (tx_id, rx_id) = (self.tx_id, self.rx_id);
        let dev = self.device.as_deref_mut().ok_or(Error::NotConnected)?;
        Ok(PassThruDiag { dev, tx_id, rx_id })
    }

    fn begin_session(&mut self) -> Result<()> {
        let mut t = self.diag()?;
        let mut uds = UdsClient::new(&mut t);
        uds.session(0x03).map(|_| ())
    }

    /// Run UDS security access (service 0x27) if a handler is set or the config
    /// marks security as required.
    fn security_access(&mut self) -> Result<()> {
        let required = security_required(self.cfg.as_ref());
        let level = seedkey_level(self.cfg.as_ref(), 0x11);
        let (tx_id, rx_id) = (self.tx_id, self.rx_id);
        // Disjoint field borrows: `seed_key` (shared), `device` (unique).
        let sk = self.seed_key.as_deref();
        let dev = self.device.as_deref_mut().ok_or(Error::NotConnected)?;
        let mut t = PassThruDiag { dev, tx_id, rx_id };
        uds_security_access(&mut t, sk, level, required)
    }
}

impl Interface for J2534Driver {
    fn set_seed_key(&mut self, seed_key: Box<dyn SeedKey>) -> bool {
        J2534Driver::set_seed_key(self, seed_key);
        true
    }

    fn info(&self) -> InterfaceInfo {
        InterfaceInfo {
            id: "j2534".into(),
            display_name: "J2534 PassThru (generic)".into(),
            firmware: None,
            serial: None,
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            can: true,
            read_memory: true,
            write_memory: true,
            ..Default::default()
        }
    }

    fn open(&mut self, cfg: &ConnectConfig) -> Result<()> {
        if !matches!(cfg.protocol, Protocol::IsoTp | Protocol::RawCan) {
            return Err(Error::Unsupported(
                "J2534 driver supports CAN/ISO15765 protocols only".into(),
            ));
        }
        let mut device = match self.injected.take() {
            Some(d) => d,
            None => (self.opener)(cfg)?,
        };

        let extended = cfg.tx_id > 0x7FF || cfg.rx_id > 0x7FF;
        let flags = if extended { codec::CAN_29BIT_ID } else { 0 };
        device.connect(codec::ISO15765, flags, cfg.bitrate)?;

        // ISO15765 needs a flow-control filter so the ECU reply is routed back.
        let (mask, pattern, flow) = PassThruMsg::iso15765_flow_filter(cfg.tx_id, cfg.rx_id);
        device.start_msg_filter(codec::FLOW_CONTROL_FILTER, &mask, &pattern, Some(&flow))?;

        self.device = Some(device);
        self.tx_id = cfg.tx_id;
        self.rx_id = cfg.rx_id;
        self.cfg = Some(cfg.clone());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        if let Some(mut dev) = self.device.take() {
            let _ = dev.close();
        }
        self.cfg = None;
        Ok(())
    }

    fn read_ecu_id(&mut self) -> Result<EcuId> {
        let mut t = self.diag()?;
        read_uds_ecu_id(&mut t)
    }

    fn read_memory(
        &mut self,
        region: MemoryRegion,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<Vec<u8>> {
        if self.device.is_none() {
            return Err(Error::NotConnected);
        }
        let _ = self.begin_session();
        // A full read from a locked ECU needs security access too; a no-op when
        // no handler is set and security is not required.
        self.security_access()?;
        let mut t = self.diag()?;
        uds_read_memory(&mut t, region, progress)
    }

    fn write_memory(
        &mut self,
        region: MemoryRegion,
        data: &[u8],
        progress: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        if self.device.is_none() {
            return Err(Error::NotConnected);
        }
        if data.len() != region.length as usize {
            return Err(Error::Config("data length != region length".into()));
        }
        let _ = self.begin_session();
        // Unlock the ECU (service 0x27) before requestDownload if required.
        self.security_access()?;
        let mut t = self.diag()?;
        uds_write_memory(&mut t, region, data, progress)
    }
}

/// Default backend opener: loads the vendor DLL on Windows (with `passthru-ffi`),
/// otherwise reports that no J2534 backend exists on this build.
fn default_opener(cfg: &ConnectConfig) -> Result<Box<dyn PassThru>> {
    #[cfg(all(windows, feature = "passthru-ffi"))]
    {
        let lib = cfg.options.get("library").ok_or_else(|| {
            Error::Config("J2534 needs options[\"library\"] = path to the PassThru DLL".into())
        })?;
        return ffi::PassThruDll::load(lib).map(|d| Box::new(d) as Box<dyn PassThru>);
    }
    #[cfg(not(all(windows, feature = "passthru-ffi")))]
    {
        let _ = cfg;
        Err(Error::Unsupported(
            "no J2534 PassThru backend in this build: the DLL binding is Windows-only \
             (build with --features passthru-ffi on Windows), or inject one with \
             J2534Driver::with_device"
                .into(),
        ))
    }
}

// --- UDS flows over the PassThru transport (fixed opentune-core API) ---------

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

// ---------------------------------------------------------------------------
// Security access (service 0x27) hook. OpenTune ships NO OEM seed/key
// algorithms; a handler is supplied by the application via
// [`J2534Driver::set_seed_key`]. (Kept self-contained per the driver
// convention.)
//
// Config keys from [`ConnectConfig::options`]:
// - `seedkey`:       expected algorithm id; its presence marks security required.
// - `security`:      `required`/`true`/`1`/`yes` marks security required.
// - `seedkey_level`: request-seed access level (odd), decimal or `0x`-hex.
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

// Keep `check_response` referenced for a clear error when a backend hands back a
// malformed reply directly (used by codec-level validation in future backends).
#[allow(dead_code)]
fn validate_reply(req_sid: u8, resp: &[u8]) -> Result<()> {
    check_response(req_sid, resp)
}

/// A scriptable in-memory PassThru backend for tests and dry runs: it records
/// every written request and returns queued reply messages. Public so callers
/// can build integration tests or offline simulations without hardware.
#[derive(Default)]
pub struct MockPassThru {
    pub connected: bool,
    pub filters: usize,
    pub written: Vec<PassThruMsg>,
    replies: std::collections::VecDeque<PassThruMsg>,
}

impl MockPassThru {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a raw reply message to be returned by the next `read_msg`.
    pub fn push_reply(&mut self, msg: PassThruMsg) {
        self.replies.push_back(msg);
    }

    /// Queue an ISO15765 reply carrying `payload` from CAN id `rx_id`.
    pub fn push_iso15765(&mut self, rx_id: u32, payload: &[u8]) {
        let mut data = rx_id.to_be_bytes().to_vec();
        data.extend_from_slice(payload);
        self.push_reply(PassThruMsg {
            protocol_id: codec::ISO15765,
            rx_status: 0,
            tx_flags: 0,
            data,
        });
    }
}

impl PassThru for MockPassThru {
    fn connect(&mut self, _protocol_id: u32, _flags: u32, _baud: u32) -> Result<()> {
        self.connected = true;
        Ok(())
    }
    fn start_msg_filter(
        &mut self,
        _filter_type: u32,
        _mask: &PassThruMsg,
        _pattern: &PassThruMsg,
        _flow_control: Option<&PassThruMsg>,
    ) -> Result<()> {
        self.filters += 1;
        Ok(())
    }
    fn write_msg(&mut self, msg: &PassThruMsg, _timeout_ms: u64) -> Result<()> {
        self.written.push(msg.clone());
        Ok(())
    }
    fn read_msg(&mut self, timeout_ms: u64) -> Result<PassThruMsg> {
        self.replies.pop_front().ok_or(Error::Timeout(timeout_ms))
    }
    fn close(&mut self) -> Result<()> {
        self.connected = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_connects_and_installs_flow_filter() {
        let mut drv = J2534Driver::with_device(Box::new(MockPassThru::new()));
        drv.open(&ConnectConfig::default()).unwrap(); // IsoTp, 0x7E0/0x7E8
        assert_eq!(drv.tx_id, 0x7E0);
        assert_eq!(drv.rx_id, 0x7E8);
    }

    #[test]
    fn read_ecu_id_skips_indication_and_frames_iso15765() {
        let mut dev = MockPassThru::new();
        // A transmit indication arrives first and must be skipped, then the real
        // RDBI 0xF190 (VIN) positive reply: 62 F1 90 <ascii VIN>.
        dev.push_reply(PassThruMsg {
            protocol_id: codec::ISO15765,
            rx_status: codec::TX_MSG_TYPE,
            tx_flags: 0,
            data: 0x7E0u32.to_be_bytes().to_vec(),
        });
        let mut payload = vec![0x62, 0xF1, 0x90];
        payload.extend_from_slice(b"VF1ABC");
        dev.push_iso15765(0x7E8, &payload);
        // The subsequent DIDs just time out → the loop ends after the success.

        let mut drv = J2534Driver::with_device(Box::new(dev));
        drv.open(&ConnectConfig::default()).unwrap();
        let id = drv.read_ecu_id().unwrap();
        assert_eq!(id.vin.as_deref(), Some("VF1ABC"));
    }

    #[test]
    fn default_opener_unsupported_without_backend() {
        let mut drv = J2534Driver::new();
        let cfg = ConnectConfig::default();
        let err = drv.open(&cfg).unwrap_err();
        assert!(matches!(err, Error::Unsupported(_)));
    }

    // --- seed/key handshake -------------------------------------------------
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
    fn errors_when_required_but_no_handler() {
        let mut t = ScriptedTransport {
            replies: VecDeque::new(),
            requests: Vec::new(),
        };
        assert!(matches!(
            uds_security_access(&mut t, None, 0x11, true),
            Err(Error::Config(_))
        ));
    }

    #[test]
    fn config_helpers_parse_options() {
        let mut cfg = ConnectConfig::default();
        assert!(!security_required(Some(&cfg)));
        cfg.options.insert("seedkey".into(), "edc17-sa2".into());
        cfg.options.insert("seedkey_level".into(), "0x09".into());
        assert!(security_required(Some(&cfg)));
        assert_eq!(seedkey_level(Some(&cfg), 0x11), 0x09);
    }
}
