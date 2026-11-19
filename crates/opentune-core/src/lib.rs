//! OpenTune core: the driver abstraction, diagnostic protocols, ECU definitions
//! and the safety contract shared by every app and interface driver.
//!
//! An *interface driver* (OpenPort 2.0, SocketCAN, K-line, or the offline mock)
//! implements [`Interface`]. Everything above — CLI, desktop, Android — talks only
//! to that trait, so a new reader/flasher is a new driver, nothing else.
pub mod capabilities;
pub mod catalog;
pub mod diag;
pub mod ecudef;
pub mod error;
pub mod interface;
pub mod monitor;
pub mod obd;
pub mod privacy;
pub mod redact;
pub mod protocol;
pub mod provenance;
pub mod registry;
pub mod resilient;
pub mod types;
pub mod validate;

pub use capabilities::Capabilities;
pub use diag::{CoreDiag, InterfaceDiag, OwnedDiag, RawDiag};
pub use error::{Error, Result};
pub use interface::{Interface, InterfaceInfo};
pub use monitor::{CanMonitor, ListenOnly, MonitorOpts, TimedFrame};
pub use protocol::{CanFrame, DoIpTransport, IsoTpFd};
pub use registry::{DriverEntry, Registry};
pub use types::{ConnectConfig, DiagLayer, EcuId, MemoryRegion, Phase, Progress, Protocol};
pub use validate::{Severity, ValidationIssue};
