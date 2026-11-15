//! slcan (LAWICEL) USB-CAN driver — for cheap serial CAN adapters like CANable
//! / CANtact running slcan firmware, and the CH340 "USB-CAN" clones that speak
//! the slcan ASCII protocol. Presents as a serial port; this driver implements
//! the core `CanLink` over it and runs UDS/ISO-TP on top.
//!
//! slcan ASCII protocol: `Sn\r` set bitrate preset, `O\r` open, `C\r` close,
//! `t<iii><l><dd…>\r` standard frame, `T<iiiiiiii><l><dd…>\r` extended frame.
use opentune_core::protocol::isotp::{CanLink, IsoTp};
use opentune_core::protocol::uds::UdsClient;
use opentune_core::{
    Capabilities, ConnectConfig, EcuId, Error, Interface, InterfaceInfo, MemoryRegion, Phase,
    Progress, Result,
};
use std::io::{Read, Write};
use std::time::Duration;

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
}

impl SlcanDriver {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Interface for SlcanDriver {
    fn info(&self) -> InterfaceInfo {
        InterfaceInfo {
            id: "slcan".into(),
            display_name: "slcan USB-CAN (CANable/CANtact)".into(),
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
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.isotp = None;
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
        // TODO: security access via SeedKey plugin before requestDownload.
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
