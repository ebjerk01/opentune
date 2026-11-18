//! Diagnostic protocol layer used by CAN/K-line/Ethernet drivers.
//!
//! - [`isotp`]: ISO 15765-2 transport over a raw CAN link (classic CAN).
//! - [`canfd`]: CAN-FD frame abstraction + ISO 15765-2 in its CAN-FD mode.
//! - [`vwtp20`]: VW TP2.0 (VAG) transport over a raw CAN link.
//! - [`tp20channel`]: ISO-TP / TP2.0 transport selection for raw-CAN drivers,
//!   with the TP2.0 keep-alive.
//! - [`tp20sim`]: a simulated TP2.0 module + in-memory CAN link (tests).
//! - [`doip`]: ISO 13400 Diagnostics-over-IP transport (UDS over TCP).
//! - [`doip_discovery`]: DoIP vehicle discovery, entity status and power mode
//!   over UDP, and discovered entity → activated [`DoIpTransport`].
//! - [`elm`]: ELM327 reply parsing + read loop shared by the ELM drivers.
//! - [`diag`]: the request/response [`DiagTransport`] trait + NRC helpers.
//! - [`uds`] / [`kwp2000`]: service clients over a `DiagTransport`.
//! - [`seedkey`]: the seed→key plugin interface for security access.
//! - [`vagmeas`]: VAG measuring-block cells and their shared formula table.
pub mod canfd;
pub mod diag;
pub mod doip;
pub mod doip_discovery;
pub mod elm;
pub mod isotp;
pub mod kwp2000;
pub mod seedkey;
pub mod tp20channel;
pub mod tp20sim;
pub mod uds;
pub mod vagmeas;
pub mod vwtp20;

pub use canfd::{CanFrame, IsoTpFd};
pub use diag::{check_response, nrc_name, DiagTransport};
pub use doip::{DoIpStream, DoIpTransport};
pub use doip_discovery::{DiscoveredEntity, Discovery, DoIpUdpClient, VehicleIdRequest};
pub use isotp::{CanLink, IsoTp};
pub use kwp2000::Kwp2000Client;
pub use seedkey::SeedKey;
pub use tp20channel::{CanDiag, CanTransportKind, Tp20Channel, Tp20Options};
pub use uds::UdsClient;
pub use vwtp20::VwTp20;
