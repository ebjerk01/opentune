//! Offline file-backed driver. Pretends a `.bin` on disk is an ECU's flash, so
//! the whole read/verify/write pipeline is testable with no hardware.
//!
//! Config options (in [`ConnectConfig::options`]):
//! - `file`: path to the flash image to load (required).
//! - `out`:  path to write back to (defaults to `file`).
//! - `base`: hex base address mapped to file offset 0 (default `0`).
//! - `ecu`:  ecu name reported by `read_ecu_id` (default `mock-ecu`).
use opentune_core::{
    Capabilities, ConnectConfig, EcuId, Error, Interface, InterfaceInfo, MemoryRegion, Phase,
    Progress, Result,
};
use std::path::PathBuf;

#[derive(Default)]
pub struct MockDriver {
    path: Option<PathBuf>,
    out: Option<PathBuf>,
    base: u32,
    ecu: String,
    data: Vec<u8>,
    open: bool,
}

impl MockDriver {
    pub fn new() -> Self {
        Self { ecu: "mock-ecu".into(), ..Default::default() }
    }

    fn offset(&self, address: u32) -> Result<usize> {
        let off = address.checked_sub(self.base).ok_or_else(|| {
            Error::Config(format!("address 0x{address:08X} below base 0x{:08X}", self.base))
        })?;
        Ok(off as usize)
    }
}

impl Interface for MockDriver {
    fn info(&self) -> InterfaceInfo {
        InterfaceInfo {
            id: "mock".into(),
            display_name: "Mock / file driver".into(),
            firmware: Some("offline".into()),
            serial: None,
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            read_memory: true,
            write_memory: true,
            battery_voltage: true,
            ..Default::default()
        }
    }

    fn open(&mut self, cfg: &ConnectConfig) -> Result<()> {
        let file = cfg
            .options
            .get("file")
            .ok_or_else(|| Error::Config("mock driver needs options[\"file\"]".into()))?;
        self.path = Some(PathBuf::from(file));
        self.out = cfg.options.get("out").map(PathBuf::from);
        self.base = match cfg.options.get("base") {
            Some(s) => u32::from_str_radix(s.trim_start_matches("0x"), 16)
                .map_err(|e| Error::Config(format!("bad base: {e}")))?,
            None => 0,
        };
        if let Some(e) = cfg.options.get("ecu") {
            self.ecu = e.clone();
        }
        self.data = std::fs::read(self.path.as_ref().unwrap())?;
        self.open = true;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.open = false;
        Ok(())
    }

    fn battery_millivolts(&mut self) -> Result<Option<u32>> {
        Ok(Some(12_600))
    }

    fn read_ecu_id(&mut self) -> Result<EcuId> {
        if !self.open {
            return Err(Error::NotConnected);
        }
        Ok(EcuId {
            ecu_name: Some(self.ecu.clone()),
            sw_number: Some(format!("{} bytes", self.data.len())),
            ..Default::default()
        })
    }

    fn read_memory(
        &mut self,
        region: MemoryRegion,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<Vec<u8>> {
        if !self.open {
            return Err(Error::NotConnected);
        }
        let start = self.offset(region.address)?;
        let end = start
            .checked_add(region.length as usize)
            .filter(|&e| e <= self.data.len())
            .ok_or_else(|| Error::Config("read past end of image".into()))?;
        progress(Progress { phase: Phase::Read, done: 0, total: region.length as u64 });
        let out = self.data[start..end].to_vec();
        progress(Progress {
            phase: Phase::Read,
            done: region.length as u64,
            total: region.length as u64,
        });
        Ok(out)
    }

    fn write_memory(
        &mut self,
        region: MemoryRegion,
        data: &[u8],
        progress: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        if !self.open {
            return Err(Error::NotConnected);
        }
        if data.len() != region.length as usize {
            return Err(Error::Config("data length != region length".into()));
        }
        let start = self.offset(region.address)?;
        let end = start
            .checked_add(data.len())
            .filter(|&e| e <= self.data.len())
            .ok_or_else(|| Error::Config("write past end of image".into()))?;
        progress(Progress { phase: Phase::Write, done: 0, total: data.len() as u64 });
        self.data[start..end].copy_from_slice(data);
        let out = self.out.clone().or_else(|| self.path.clone()).unwrap();
        std::fs::write(out, &self.data)?;
        progress(Progress {
            phase: Phase::Write,
            done: data.len() as u64,
            total: data.len() as u64,
        });
        Ok(())
    }
}
