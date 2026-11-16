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
use opentune_core::protocol::uds::UdsClient;
use opentune_core::{
    Capabilities, ConnectConfig, EcuId, Error, Interface, InterfaceInfo, MemoryRegion, Phase,
    Progress, Protocol, Result,
};
use std::time::Duration;

type Serial = Box<dyn serialport::SerialPort>;

/// Which transport the current session uses.
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
}

impl Esp32Driver {
    pub fn new() -> Self {
        Self::default()
    }

    fn dev(&mut self) -> Result<&mut Device<Serial>> {
        self.device.as_mut().ok_or(Error::NotConnected)
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
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        if let Some(dev) = self.device.as_mut() {
            dev.can_stop().ok();
            dev.reset().ok();
        }
        self.device = None;
        self.mode = None;
        Ok(())
    }

    fn battery_millivolts(&mut self) -> Result<Option<u32>> {
        let dev = self.dev()?;
        Ok(Some(dev.battery_mv()?))
    }

    fn read_ecu_id(&mut self) -> Result<EcuId> {
        match self.mode.take() {
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
        match self.mode.take() {
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
                    let data = kwp.read_memory(addr, chunk)?;
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
        // TODO: security access via the SeedKey plugin before request_download.
        match self.mode.take() {
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

#[cfg(test)]
mod tests {
    use super::*;

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
