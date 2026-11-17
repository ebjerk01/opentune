//! The interface-driver abstraction. Every reader/flasher implements [`Interface`].
use crate::protocol::DiagTransport;
use crate::{Capabilities, ConnectConfig, EcuId, MemoryRegion, Progress, Result};

/// Static description of a connected interface.
#[derive(Debug, Clone)]
pub struct InterfaceInfo {
    pub id: String,
    pub display_name: String,
    pub firmware: Option<String>,
    pub serial: Option<String>,
}

/// A reader/flasher. Drivers handle their own transport and protocol; callers
/// use only these ECU-level operations.
pub trait Interface {
    fn info(&self) -> InterfaceInfo;
    fn capabilities(&self) -> Capabilities;

    /// Open a diagnostic session to the ECU with the given configuration.
    fn open(&mut self, cfg: &ConnectConfig) -> Result<()>;
    /// Close the session and release the hardware.
    fn close(&mut self) -> Result<()>;

    /// Battery voltage in millivolts, if the interface can measure it.
    fn battery_millivolts(&mut self) -> Result<Option<u32>> {
        Ok(None)
    }

    /// Read ECU identity (part/software number, VIN, …).
    fn read_ecu_id(&mut self) -> Result<EcuId>;

    /// Read a memory region, reporting progress.
    fn read_memory(
        &mut self,
        region: MemoryRegion,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<Vec<u8>>;

    /// Write a memory region, reporting progress.
    fn write_memory(
        &mut self,
        region: MemoryRegion,
        data: &[u8],
        progress: &mut dyn FnMut(Progress),
    ) -> Result<()>;

    /// Borrow this driver's request/response [`DiagTransport`] for raw UDS/KWP
    /// exchanges (module coding, routines, DIDs), when the driver can expose one.
    ///
    /// The default returns `None`: a driver that only implements the block
    /// read/write pipeline does not offer a generic diagnostic transport, and a
    /// caller that needs one (e.g. the coding engine) must handle its absence.
    /// Drivers that own a `DiagTransport` override this to hand it out, so an
    /// application can obtain a transport generically from any registry driver.
    fn diag(&mut self) -> Option<&mut dyn DiagTransport> {
        None
    }
}
