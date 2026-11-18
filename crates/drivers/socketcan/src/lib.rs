//! SocketCAN driver for OpenTune.
//!
//! Uses the Linux SocketCAN stack (via the `socketcan` crate) as a raw CAN
//! link, then layers the fixed `opentune-core` transport/service stack on top:
//! [`IsoTp`] for ISO 15765-2 segmentation and [`UdsClient`] for the UDS read and
//! write flows. SocketCAN has no notion of battery voltage, so
//! `battery_millivolts` returns `None`.
//!
//! Config ([`ConnectConfig::options`]):
//! - `iface`: CAN interface name (default: option `port` unless it is a device
//!   path, else `can0`). Bring it up first, e.g.
//!   `ip link set can0 up type can bitrate 500000`.
//! - `transport=tp20` + `tp20_addr=<module>`: open a VW TP2.0 channel
//!   (KWP2000) instead of ISO-TP; see [`opentune_core::protocol::tp20channel`].
//!
//! This is Linux-only. On other targets the whole implementation is replaced by
//! a stub whose operations return [`Error::Unsupported`], so the crate still
//! compiles everywhere.

pub mod monitor;

#[cfg(target_os = "linux")]
mod imp {
    use opentune_core::protocol::diag::DiagTransport;
    use opentune_core::protocol::isotp::{CanLink, IsoTp};
    use opentune_core::protocol::seedkey::SeedKey;
    use opentune_core::protocol::tp20channel::{CanDiag, CanTransportKind, Tp20Options};
    use opentune_core::protocol::uds::UdsClient;
    use opentune_core::{
        Capabilities, ConnectConfig, EcuId, Error, Interface, InterfaceInfo, MemoryRegion, Phase,
        Progress, Protocol, Result,
    };
    use socketcan::{CanFrame, CanSocket, EmbeddedFrame, Id, Socket, StandardId};
    use std::time::Duration;

    /// [`CanLink`] over a bound SocketCAN socket.
    struct SocketCanLink {
        socket: CanSocket,
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
                Ok(frame) => {
                    let id = raw_id(frame.id());
                    Ok((id, frame.data().to_vec()))
                }
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

    /// Build a CAN [`Id`], choosing 11-bit vs 29-bit by magnitude.
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

    #[derive(Default)]
    pub struct SocketCanDriver {
        /// ISO-TP (default) or a VW TP2.0 channel, built on `open`.
        transport: Option<CanDiag>,
        cfg: Option<ConnectConfig>,
        /// Optional seed/key handler for UDS security access (service 0x27). No
        /// OEM algorithm ships with OpenTune; the application supplies one.
        seed_key: Option<Box<dyn SeedKey>>,
        /// Passive full-bus capture (independent of `open`).
        monitor: crate::monitor::SocketCanMonitor,
    }

    impl SocketCanDriver {
        pub fn new() -> Self {
            Self::default()
        }

        /// Install the seed/key handler used to unlock the ECU (service 0x27)
        /// before a download. Without it, writing a security-protected ECU
        /// fails with a clear error. OpenTune ships no OEM algorithms.
        pub fn set_seed_key(&mut self, seed_key: Box<dyn SeedKey>) {
            self.seed_key = Some(seed_key);
        }

        fn transport(&mut self) -> Result<&mut IsoTp> {
            self.transport
                .as_mut()
                .ok_or(Error::NotConnected)?
                .isotp("socketcan")
        }

        /// Open over a ready [`CanLink`] instead of a SocketCAN interface
        /// (tests, or another frame source). Same transport selection as
        /// [`Interface::open`]: ISO-TP by default, a VW TP2.0 channel with
        /// `transport=tp20`.
        pub fn open_with_link(
            &mut self,
            link: Box<dyn CanLink + Send>,
            cfg: &ConnectConfig,
        ) -> Result<()> {
            Tp20Options::from_config(cfg)?;
            self.close()?;
            let transport = CanDiag::open(link, cfg).map_err(|e| match e {
                Error::Protocol(m) => Error::Protocol(format!("socketcan: {m}")),
                other => other,
            })?;
            self.transport = Some(transport);
            self.cfg = Some(cfg.clone());
            Ok(())
        }

        /// The transport `open` built, if any.
        pub fn transport_kind(&self) -> Option<CanTransportKind> {
            self.transport.as_ref().map(CanDiag::kind)
        }

        fn begin_session(&mut self) -> Result<()> {
            let t = self.transport()?;
            let mut uds = UdsClient::new(t);
            uds.session(0x03).map(|_| ())
        }

        /// Run UDS security access (service 0x27) if a seed/key handler is set
        /// or the config marks security as required. A no-op on an open ECU
        /// with no handler configured, so unlocked sessions keep working.
        fn security_access(&mut self) -> Result<()> {
            let required = security_required(self.cfg.as_ref());
            let level = seedkey_level(self.cfg.as_ref(), 0x11);
            // Disjoint field borrows: `seed_key` (shared), `transport` (unique).
            let sk = self.seed_key.as_deref();
            let transport = self
                .transport
                .as_mut()
                .ok_or(Error::NotConnected)?
                .isotp("socketcan")?;
            uds_security_access(transport, sk, level, required)
        }
    }

    impl Interface for SocketCanDriver {
        fn set_seed_key(&mut self, seed_key: Box<dyn SeedKey>) -> bool {
            SocketCanDriver::set_seed_key(self, seed_key);
            true
        }

        fn info(&self) -> InterfaceInfo {
            let display_name = match &self.transport {
                Some(t @ CanDiag::Tp20(_)) => format!("SocketCAN, {}", t.describe()),
                _ => "SocketCAN".into(),
            };
            InterfaceInfo {
                id: "socketcan".into(),
                display_name,
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
                vwtp20: true,
                ..Default::default()
            }
        }

        /// Expose the transport over the SocketCAN interface (host-side ISO-TP,
        /// or the VW TP2.0 channel with `transport=tp20`) as a
        /// [`DiagTransport`] so the coding/adaptation/auto-scan/service/logging
        /// engines (and the raw-request console) can drive this adapter.
        /// `None` until `open()` succeeds.
        fn diag(&mut self) -> Option<&mut dyn DiagTransport> {
            self.transport.as_mut().map(|t| t as &mut dyn DiagTransport)
        }

        fn open(&mut self, cfg: &ConnectConfig) -> Result<()> {
            let tp20 = Tp20Options::from_config(cfg)?;
            if tp20.is_none() && !matches!(cfg.protocol, Protocol::IsoTp | Protocol::RawCan) {
                return Err(Error::Unsupported(
                    "SocketCAN driver supports CAN/ISO-TP protocols, and VW TP2.0 with \
                     transport=tp20"
                        .into(),
                ));
            }
            // `port` (the CLI's --port, the desktop's port field) names the
            // interface too, as for the monitor; a device path cannot be one.
            let iface = cfg
                .options
                .get("iface")
                .or_else(|| cfg.options.get("port").filter(|p| !p.contains('/')))
                .map(String::as_str)
                .unwrap_or("can0");
            let socket = CanSocket::open(iface)
                .map_err(|e| Error::Config(format!("cannot open CAN interface {iface}: {e}")))?;
            self.open_with_link(Box::new(SocketCanLink { socket }), cfg)
        }

        /// Close: a TP2.0 channel stops its keep-alive and sends the
        /// disconnect (`A8`) first.
        fn close(&mut self) -> Result<()> {
            if let Some(mut t) = self.transport.take() {
                t.close();
            }
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
            // A full read from a locked ECU needs security access too; a no-op
            // when no handler is set and security is not required.
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

    // --- UDS flows over the ISO-TP transport (fixed opentune-core API) --------

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

    // Security access (service 0x27) runs in `SocketCanDriver::write_memory`
    // via `uds_security_access` before this function. OpenTune ships no OEM
    // seed/key algorithms; the application supplies one with `set_seed_key`.
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

    // -----------------------------------------------------------------------
    // Security access (service 0x27) hook.
    //
    // OpenTune ships NO OEM seed/key algorithms. A handler is supplied by the
    // application via `SocketCanDriver::set_seed_key` (or a plugin), keyed by
    // the `seedkey` id in an ECU definition. This runs request-seed → compute
    // → send-key over the existing `UdsClient` before a download.
    //
    // Config keys from `ConnectConfig::options`:
    // - `seedkey`:       expected algorithm id; its presence marks security required.
    // - `security`:      `required`/`true`/`1`/`yes` marks security required.
    // - `seedkey_level`: request-seed access level (odd), decimal or `0x`-hex.
    // -----------------------------------------------------------------------

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

    /// Request-seed access level from config, or `default` (send-key is +1).
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

    /// Perform UDS service 0x27 if a handler is set; otherwise error only when
    /// security is required.
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
            let mut d = SocketCanDriver::new();
            assert!(d.diag().is_none(), "no transport before open");
            d.transport = Some(CanDiag::IsoTp(IsoTp::new(Box::new(IdleLink), 0x7E0, 0x7E8)));
            assert!(
                d.diag().is_some(),
                "diag() must expose the ISO-TP transport"
            );
        }
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
            let err = uds_security_access(&mut t, None, 0x11, true).unwrap_err();
            assert!(matches!(err, Error::Config(_)));
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
}

#[cfg(target_os = "linux")]
pub use imp::SocketCanDriver;

// --- Non-Linux stub ---------------------------------------------------------

#[cfg(not(target_os = "linux"))]
mod stub {
    use opentune_core::protocol::seedkey::SeedKey;
    use opentune_core::{
        Capabilities, ConnectConfig, EcuId, Error, Interface, InterfaceInfo, MemoryRegion,
        Progress, Result,
    };

    /// SocketCAN is Linux-only; this stub keeps the crate compiling elsewhere.
    #[derive(Default)]
    pub struct SocketCanDriver;

    impl SocketCanDriver {
        pub fn new() -> Self {
            Self
        }

        /// API parity with the Linux driver; the stub never flashes, so the
        /// handler is accepted and dropped.
        pub fn set_seed_key(&mut self, _seed_key: Box<dyn SeedKey>) {}

        fn unsupported<T>() -> Result<T> {
            Err(Error::Unsupported(
                "SocketCAN is only available on Linux".into(),
            ))
        }
    }

    impl Interface for SocketCanDriver {
        fn set_seed_key(&mut self, seed_key: Box<dyn SeedKey>) -> bool {
            // The stub never unlocks anything: report the handler as unused.
            SocketCanDriver::set_seed_key(self, seed_key);
            false
        }

        fn info(&self) -> InterfaceInfo {
            InterfaceInfo {
                id: "socketcan".into(),
                display_name: "SocketCAN (unavailable: not Linux)".into(),
                firmware: None,
                serial: None,
            }
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }

        fn open(&mut self, _cfg: &ConnectConfig) -> Result<()> {
            Self::unsupported()
        }

        fn close(&mut self) -> Result<()> {
            Ok(())
        }

        fn read_ecu_id(&mut self) -> Result<EcuId> {
            Self::unsupported()
        }

        fn read_memory(
            &mut self,
            _region: MemoryRegion,
            _p: &mut dyn FnMut(Progress),
        ) -> Result<Vec<u8>> {
            Self::unsupported()
        }

        fn write_memory(
            &mut self,
            _region: MemoryRegion,
            _data: &[u8],
            _p: &mut dyn FnMut(Progress),
        ) -> Result<()> {
            Self::unsupported()
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub use stub::SocketCanDriver;
