//! The interface-driver abstraction. Every reader/flasher implements [`Interface`].
use crate::monitor::CanMonitor;
use crate::protocol::kline::KLineRaw;
use crate::protocol::seedkey::SeedKey;
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

    /// Dry run of [`read_memory`](Interface::read_memory) on the opened driver:
    /// the exact lines the read would write to the adapter, in order (adapter
    /// commands and request hex, as the driver defines them). **Sends nothing.**
    ///
    /// `Some(Err(_))` means the read itself would be refused before reaching
    /// the bus (bad options, a request format that does not fit, security
    /// required without a handler). The default returns `None`: the driver
    /// cannot plan, and a caller must say so rather than read for real.
    fn plan_read_memory(&mut self, region: MemoryRegion) -> Option<Result<Vec<String>>> {
        let _ = region;
        None
    }

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

    /// Borrow this driver's passive raw-bus [`CanMonitor`] for a full CAN dump,
    /// when the adapter can capture every frame on the bus.
    ///
    /// The default returns `None` (no raw capture). Unlike [`diag`](Interface::diag)
    /// this does **not** require [`open`](Interface::open): the monitor opens the
    /// hardware itself, in listen-only mode where possible, in
    /// [`CanMonitor::start`]. Do not call `open` first — a diagnostic session may
    /// transmit on the bus.
    fn can_monitor(&mut self) -> Option<&mut dyn CanMonitor> {
        None
    }

    /// Borrow this driver's K-line as an unframed byte link ([`KLineRaw`]),
    /// for protocols that frame their own packets (Subaru SSM2).
    ///
    /// The default returns `None`. A K-line driver that can offer it does so
    /// after [`open`](Interface::open); open it with `options["init"] =
    /// "none"` when the protocol needs no wake-up (SSM2 does not).
    fn kline(&mut self) -> Option<&mut dyn KLineRaw> {
        None
    }

    /// Hand this driver a [`SeedKey`] handler for its security-access unlock
    /// (block read/write behind 0x27 / KWP security). OpenTune ships no OEM
    /// algorithm; the caller (a plugin or the user) supplies it.
    ///
    /// Returns `true` when the driver accepted the handler. The default
    /// returns `false`: the driver has no unlock step, so the handler is
    /// dropped. Lets a registry-opened `Box<dyn Interface>` take a handler
    /// without knowing the concrete driver type.
    fn set_seed_key(&mut self, seed_key: Box<dyn SeedKey>) -> bool {
        let _ = seed_key;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Bare;
    impl Interface for Bare {
        fn info(&self) -> InterfaceInfo {
            InterfaceInfo {
                id: "bare".into(),
                display_name: "bare".into(),
                firmware: None,
                serial: None,
            }
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }
        fn open(&mut self, _: &ConnectConfig) -> Result<()> {
            Ok(())
        }
        fn close(&mut self) -> Result<()> {
            Ok(())
        }
        fn read_ecu_id(&mut self) -> Result<EcuId> {
            Ok(EcuId::default())
        }
        fn read_memory(&mut self, _: MemoryRegion, _: &mut dyn FnMut(Progress)) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }
        fn write_memory(
            &mut self,
            _: MemoryRegion,
            _: &[u8],
            _: &mut dyn FnMut(Progress),
        ) -> Result<()> {
            Ok(())
        }
    }

    struct Echo;
    impl SeedKey for Echo {
        fn id(&self) -> &str {
            "echo"
        }
        fn compute(&self, seed: &[u8], _level: u8) -> Result<Vec<u8>> {
            Ok(seed.to_vec())
        }
    }

    #[test]
    fn drivers_without_an_unlock_step_refuse_a_seed_key() {
        let mut d: Box<dyn Interface> = Box::new(Bare);
        assert!(!d.set_seed_key(Box::new(Echo)));
    }

    #[test]
    fn drivers_offer_no_raw_kline_by_default() {
        let mut d: Box<dyn Interface> = Box::new(Bare);
        assert!(d.kline().is_none());
    }

    #[test]
    fn drivers_cannot_plan_a_read_by_default() {
        let mut d: Box<dyn Interface> = Box::new(Bare);
        assert!(d.plan_read_memory(MemoryRegion::new(0, 16)).is_none());
    }
}
