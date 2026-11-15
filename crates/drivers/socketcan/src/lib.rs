//! SocketCAN driver for OpenTune.
//!
//! Uses the Linux SocketCAN stack (via the `socketcan` crate) as a raw CAN
//! link, then layers the fixed `opentune-core` transport/service stack on top:
//! [`IsoTp`] for ISO 15765-2 segmentation and [`UdsClient`] for the UDS read and
//! write flows. SocketCAN has no notion of battery voltage, so
//! `battery_millivolts` returns `None`.
//!
//! Config ([`ConnectConfig::options`]):
//! - `iface`: CAN interface name (default `can0`). Bring it up first, e.g.
//!   `ip link set can0 up type can bitrate 500000`.
//!
//! This is Linux-only. On other targets the whole implementation is replaced by
//! a stub whose operations return [`Error::Unsupported`], so the crate still
//! compiles everywhere.

#[cfg(target_os = "linux")]
mod imp {
    use opentune_core::protocol::isotp::{CanLink, IsoTp};
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
        transport: Option<IsoTp>,
    }

    impl SocketCanDriver {
        pub fn new() -> Self {
            Self::default()
        }

        fn transport(&mut self) -> Result<&mut IsoTp> {
            self.transport.as_mut().ok_or(Error::NotConnected)
        }

        fn begin_session(&mut self) -> Result<()> {
            let t = self.transport()?;
            let mut uds = UdsClient::new(t);
            uds.session(0x03).map(|_| ())
        }
    }

    impl Interface for SocketCanDriver {
        fn info(&self) -> InterfaceInfo {
            InterfaceInfo {
                id: "socketcan".into(),
                display_name: "SocketCAN".into(),
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
                    "SocketCAN driver supports CAN/ISO-TP protocols only".into(),
                ));
            }
            let iface = cfg
                .options
                .get("iface")
                .map(String::as_str)
                .unwrap_or("can0");
            let socket = CanSocket::open(iface)
                .map_err(|e| Error::Config(format!("cannot open CAN interface {iface}: {e}")))?;
            let link = SocketCanLink { socket };
            self.transport = Some(IsoTp::new(Box::new(link), cfg.tx_id, cfg.rx_id));
            Ok(())
        }

        fn close(&mut self) -> Result<()> {
            self.transport = None;
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

    // TODO: security access via SeedKey plugin (service 0x27) before
    // requestDownload for ECUs that are still locked.
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
}

#[cfg(target_os = "linux")]
pub use imp::SocketCanDriver;

// --- Non-Linux stub ---------------------------------------------------------

#[cfg(not(target_os = "linux"))]
mod stub {
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

        fn unsupported<T>() -> Result<T> {
            Err(Error::Unsupported(
                "SocketCAN is only available on Linux".into(),
            ))
        }
    }

    impl Interface for SocketCanDriver {
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
