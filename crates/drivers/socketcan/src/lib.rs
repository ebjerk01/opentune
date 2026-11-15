//! SocketCAN driver for OpenTune.
//!
//! STATUS: SKELETON. Implements the core Interface trait against the fixed core API; the
//! transport/protocol wiring is filled in during ROADMAP M2. Until then the
//! operations return Error::Unsupported.
use opentune_core::{
    Capabilities, ConnectConfig, EcuId, Error, Interface, InterfaceInfo, MemoryRegion, Progress,
    Result,
};

#[derive(Default)]
pub struct SocketCanDriver {
    _open: bool,
}

impl SocketCanDriver {
    pub fn new() -> Self {
        Self::default()
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
        Capabilities::default()
    }

    fn open(&mut self, _cfg: &ConnectConfig) -> Result<()> {
        Err(Error::Unsupported("SocketCAN driver not yet implemented (ROADMAP M2)".into()))
    }

    fn close(&mut self) -> Result<()> {
        Ok(())
    }

    fn read_ecu_id(&mut self) -> Result<EcuId> {
        Err(Error::NotConnected)
    }

    fn read_memory(&mut self, _region: MemoryRegion, _p: &mut dyn FnMut(Progress)) -> Result<Vec<u8>> {
        Err(Error::NotConnected)
    }

    fn write_memory(&mut self, _region: MemoryRegion, _data: &[u8], _p: &mut dyn FnMut(Progress)) -> Result<()> {
        Err(Error::NotConnected)
    }
}
