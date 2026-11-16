//! gs_usb / candleLight USB-CAN driver for OpenTune.
//!
//! [candleLight]-class adapters (and anything running the `gs_usb` firmware:
//! CANable in gs_usb mode, cantact-pro, many clones) speak the Geschwister
//! Schneider USB protocol. There are two host-side ways to reach them:
//!
//! * **Kernel path (used here on Linux):** the in-tree `gs_usb` driver binds the
//!   device and exposes it as a normal SocketCAN interface (`canX`). We layer a
//!   [`CanLink`] over that socket and run the fixed opentune-core ISO-TP + UDS
//!   stack, exactly like [`opentune-driver-socketcan`]. Bring the interface up
//!   first, e.g. `ip link set can0 up type can bitrate 500000`.
//! * **Raw libusb path (codec provided):** for platforms without the kernel
//!   driver one talks the gs_usb *host-frame* USB protocol directly. The wire
//!   format lives in [`codec`] (encode/decode of `gs_host_frame`), ready for a
//!   libusb backend; that backend is not implemented here.
//!
//! Config ([`ConnectConfig::options`]):
//! - `iface`: SocketCAN interface name the kernel gs_usb driver created
//!   (default `can0`).
//!
//! Nothing here has been run against real hardware; the [`CanLink`] and the
//! host-frame codec are mock/round-trip tested.

pub mod codec;

use opentune_core::protocol::diag::DiagTransport;
use opentune_core::protocol::isotp::{CanLink, IsoTp};
use opentune_core::protocol::seedkey::SeedKey;
use opentune_core::protocol::uds::UdsClient;
use opentune_core::{
    Capabilities, ConnectConfig, EcuId, Error, Interface, InterfaceInfo, MemoryRegion, Phase,
    Progress, Protocol, Result,
};

/// gs_usb / candleLight USB-CAN interface driver.
#[derive(Default)]
pub struct GsUsbDriver {
    transport: Option<IsoTp>,
    cfg: Option<ConnectConfig>,
    /// Optional seed/key handler for UDS security access (service 0x27). No OEM
    /// algorithm ships with OpenTune; the application supplies one.
    seed_key: Option<Box<dyn SeedKey>>,
}

impl GsUsbDriver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install the seed/key handler used to unlock the ECU (service 0x27)
    /// before a download. Without it, writing a security-protected ECU fails
    /// with a clear error. OpenTune ships no OEM algorithms.
    pub fn set_seed_key(&mut self, seed_key: Box<dyn SeedKey>) {
        self.seed_key = Some(seed_key);
    }

    /// Build the driver around a pre-made [`CanLink`] (e.g. a test mock or a
    /// future libusb backend), bypassing the SocketCAN opener.
    pub fn with_link(link: Box<dyn CanLink>, tx_id: u32, rx_id: u32) -> Self {
        let mut d = Self::new();
        d.transport = Some(IsoTp::new(link, tx_id, rx_id));
        d
    }

    fn transport(&mut self) -> Result<&mut IsoTp> {
        self.transport.as_mut().ok_or(Error::NotConnected)
    }

    fn begin_session(&mut self) -> Result<()> {
        let t = self.transport()?;
        let mut uds = UdsClient::new(t);
        uds.session(0x03).map(|_| ())
    }

    /// Run UDS security access (service 0x27) if a seed/key handler is set or
    /// the config marks security as required.
    fn security_access(&mut self) -> Result<()> {
        let required = security_required(self.cfg.as_ref());
        let level = seedkey_level(self.cfg.as_ref(), 0x11);
        // Disjoint field borrows: `seed_key` (shared), `transport` (unique).
        let sk = self.seed_key.as_deref();
        let transport = self.transport.as_mut().ok_or(Error::NotConnected)?;
        uds_security_access(transport, sk, level, required)
    }

    /// Open the SocketCAN link the kernel `gs_usb` driver exposes.
    fn open_link(&self, cfg: &ConnectConfig) -> Result<Box<dyn CanLink>> {
        socketcan_link(cfg)
    }
}

impl Interface for GsUsbDriver {
    fn info(&self) -> InterfaceInfo {
        InterfaceInfo {
            id: "gsusb".into(),
            display_name: "gs_usb / candleLight USB-CAN".into(),
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
                "gs_usb driver supports CAN/ISO-TP protocols only".into(),
            ));
        }
        let link = self.open_link(cfg)?;
        self.transport = Some(IsoTp::new(link, cfg.tx_id, cfg.rx_id));
        self.cfg = Some(cfg.clone());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.transport = None;
        self.cfg = None;
        Ok(())
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
        // A full read from a locked ECU needs security access too; a no-op when
        // no handler is set and security is not required.
        self.security_access()?;
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
        // Unlock the ECU (service 0x27) before requestDownload if required.
        self.security_access()?;
        uds_write_memory(self.transport()?, region, data, progress)
    }
}

// --- SocketCAN link (kernel gs_usb path) ------------------------------------

#[cfg(target_os = "linux")]
fn socketcan_link(cfg: &ConnectConfig) -> Result<Box<dyn CanLink>> {
    use socketcan::Socket;
    let iface = cfg
        .options
        .get("iface")
        .map(String::as_str)
        .unwrap_or("can0");
    let socket = socketcan::CanSocket::open(iface).map_err(|e| {
        Error::Config(format!(
            "cannot open gs_usb SocketCAN interface {iface}: {e} \
             (is the gs_usb kernel driver bound and `ip link set {iface} up` done?)"
        ))
    })?;
    Ok(Box::new(linux_link::SocketCanLink { socket }))
}

#[cfg(not(target_os = "linux"))]
fn socketcan_link(_cfg: &ConnectConfig) -> Result<Box<dyn CanLink>> {
    Err(Error::Unsupported(
        "the gs_usb kernel/SocketCAN path is Linux-only; a libusb backend using \
         the gs_usb host-frame codec is needed on this platform"
            .into(),
    ))
}

#[cfg(target_os = "linux")]
mod linux_link {
    use super::*;
    use socketcan::{CanFrame, EmbeddedFrame, Id, Socket, StandardId};
    use std::time::Duration;

    pub struct SocketCanLink {
        pub socket: socketcan::CanSocket,
    }

    impl CanLink for SocketCanLink {
        fn send_frame(&mut self, id: u32, data: &[u8]) -> Result<()> {
            let can_id = make_id(id);
            let frame = CanFrame::new(can_id, data)
                .ok_or_else(|| Error::Protocol("CAN frame too long".into()))?;
            self.socket.write_frame(&frame).map_err(Error::Io)
        }

        fn recv_frame(&mut self, timeout_ms: u64) -> Result<(u32, Vec<u8>)> {
            self.socket
                .set_read_timeout(Duration::from_millis(timeout_ms))
                .map_err(Error::Io)?;
            match self.socket.read_frame() {
                Ok(frame) => Ok((raw_id(frame.id()), frame.data().to_vec())),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    Err(Error::Timeout(timeout_ms))
                }
                Err(e) => Err(Error::Io(e)),
            }
        }
    }

    fn make_id(id: u32) -> Id {
        if id <= 0x7FF {
            Id::Standard(StandardId::new(id as u16).unwrap_or(StandardId::ZERO))
        } else {
            Id::Extended(
                socketcan::ExtendedId::new(id & 0x1FFF_FFFF).unwrap_or(socketcan::ExtendedId::ZERO),
            )
        }
    }

    fn raw_id(id: Id) -> u32 {
        match id {
            Id::Standard(s) => s.as_raw() as u32,
            Id::Extended(e) => e.as_raw(),
        }
    }
}

// --- UDS flows over the ISO-TP transport (fixed opentune-core API) -----------

fn read_uds_ecu_id(transport: &mut IsoTp) -> Result<EcuId> {
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
    transport: &mut IsoTp,
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
    transport: &mut IsoTp,
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
// [`GsUsbDriver::set_seed_key`], keyed by the `seedkey` id in an ECU
// definition. (Kept self-contained per the convention of the other drivers.)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// A mocked raw CAN link: returns queued `(id, data)` frames to `recv_frame`
    /// and records everything sent, so a CanLink→IsoTp→UDS exchange can be
    /// driven without hardware.
    struct MockCanLink {
        rx: VecDeque<(u32, Vec<u8>)>,
        pub tx: Vec<(u32, Vec<u8>)>,
    }

    impl CanLink for MockCanLink {
        fn send_frame(&mut self, id: u32, data: &[u8]) -> Result<()> {
            self.tx.push((id, data.to_vec()));
            Ok(())
        }
        fn recv_frame(&mut self, timeout_ms: u64) -> Result<(u32, Vec<u8>)> {
            self.rx.pop_front().ok_or(Error::Timeout(timeout_ms))
        }
    }

    #[test]
    fn canlink_drives_a_single_frame_uds_exchange() {
        // ECU answers TesterPresent (3E 00) with a positive 7E 00, as one ISO-TP
        // single frame on the rx id.
        let link = MockCanLink {
            rx: VecDeque::from(vec![(0x7E8, vec![0x02, 0x7E, 0x00, 0, 0, 0, 0, 0])]),
            tx: Vec::new(),
        };
        let mut tp = IsoTp::new(Box::new(link), 0x7E0, 0x7E8);
        let mut uds = UdsClient::new(&mut tp);
        uds.tester_present().unwrap();
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
        cfg.options.insert("security".into(), "required".into());
        cfg.options.insert("seedkey_level".into(), "9".into());
        assert!(security_required(Some(&cfg)));
        assert_eq!(seedkey_level(Some(&cfg), 0x11), 9);
    }
}
