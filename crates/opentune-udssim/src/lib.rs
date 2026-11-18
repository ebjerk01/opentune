//! # opentune-udssim
//!
//! An **in-memory UDS (ISO 14229) ECU simulator and conformance harness** for
//! OpenTune.
//!
//! OpenTune's diagnostic client ([`opentune_core::protocol::UdsClient`]) is
//! normally tested against mocks derived from the client's own expectations. A
//! mock that answers exactly what the client assumes cannot catch a client that
//! assumes the wrong thing. This crate provides an **independent peer**: a
//! clean-room implementation of the *server* side of the services the client
//! speaks, reimplemented from the ISO 14229-1 / ISO 13400-2 service catalogue
//! and cross-checked against two independent MIT references
//! ([`pylessard/python-udsoncan`] and [`driftregion/iso14229`]) without copying
//! code from either. The client is then driven against this peer and its
//! validation and timing behaviour asserted.
//!
//! This is **offline conformance only**. Nothing here touches a vehicle bus,
//! hardware interface, or network; the simulator runs entirely in memory and is
//! not a tool for interacting with a real ECU.
//!
//! ## Pieces
//!
//! - [`server::UdsServer`] — the simulated ECU. [`UdsServer::handle`] turns one
//!   UDS request into one UDS response. It implements DiagnosticSessionControl
//!   (0x10), ECUReset (0x11), ClearDiagnosticInformation (0x14),
//!   ReadDTCInformation (0x19), ReadDataByIdentifier (0x22),
//!   ReadMemoryByAddress (0x23, over regions mapped with
//!   [`UdsServer::set_memory`]), SecurityAccess (0x27, with an injected
//!   [`security::SecurityChecker`]), DynamicallyDefineDataIdentifier (0x2C:
//!   define by identifier / by memory address, clear; dynamic DIDs in
//!   `0xF200..=0xF3FF`, cleared on session change and reset — periodic
//!   `0x2A` is not modelled), WriteDataByIdentifier (0x2E), InputOutputControlByIdentifier (0x2F),
//!   RoutineControl (0x31), RequestDownload /
//!   RequestUpload (0x34 / 0x35), TransferData (0x36, with block-counter
//!   tracking), RequestTransferExit (0x37) and TesterPresent (0x3E).
//! - [`flash::FlashEcu`] — an optional **flashable ECU** model
//!   ([`UdsServer::enable_flash`]): programming session with preconditions,
//!   programming-level SecurityAccess, eraseMemory (0xFF00) / checksum (0x0202,
//!   configurable) / checkProgrammingDependencies (0xFF01) routines, download
//!   into a persistent flash memory with ISO block-counter rules, NRC 0x78 on
//!   slow erase, and fault injection (power loss mid-transfer, lost response,
//!   erase refused, corrupt write, failing checksum).
//! - [`driver::UdssimDriver`] — the `udssim` registry driver: an in-process
//!   flashable ECU behind the `Interface` trait (diag, UDS block read/write,
//!   a documented test seed-key provider), so front ends run offline.
//! - [`transport::SimTransport`] — wraps the server as an `opentune_core`
//!   [`DiagTransport`], so [`UdsClient`] drives it directly.
//! - [`doip::DoIpSimStream`] — a loopback ISO 13400 DoIP entity, so the real
//!   [`DoIpTransport`] client (routing activation, acks, the 0x78 drain loop)
//!   runs against the same server end to end.
//!   The `doip_tcp_entity` example puts it on a loopback TCP port, and
//!   `tools/conformance/` drives that with python-udsoncan / python-doipclient
//!   as independent third-party clients.
//! - [`fault::FaultPlan`] — programmable fault injection: response-pending
//!   storms, injected NRCs, wrong correlation echoes, truncated responses and
//!   mis-correlated replies, each firing deterministically at one point in a
//!   flow.
//! - [`trace::Trace`] — an ordered request/response/pending log for replay and
//!   golden-trace assertions.
//!
//! [`UdsClient`]: opentune_core::protocol::UdsClient
//! [`UdsServer::handle`]: server::UdsServer::handle
//! [`UdsServer::set_memory`]: server::UdsServer::set_memory
//! [`UdsServer::enable_flash`]: server::UdsServer::enable_flash
//! [`DiagTransport`]: opentune_core::protocol::DiagTransport
//! [`DoIpTransport`]: opentune_core::protocol::DoIpTransport
//! [`pylessard/python-udsoncan`]: https://github.com/pylessard/python-udsoncan
//! [`driftregion/iso14229`]: https://github.com/driftregion/iso14229

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod doip;
pub mod driver;
pub mod fault;
pub mod flash;
pub mod security;
pub mod server;
pub mod trace;
pub mod transport;

pub use doip::DoIpSimStream;
pub use driver::{TestSeedKey, UdssimDriver};
pub use fault::{Fault, FaultPlan};
pub use flash::{FlashConfig, FlashEcu, FlashFaults, FlashRegion, FlashStats};
pub use security::{AlwaysReject, SecurityChecker, XorSecurity};
pub use server::{DddidConfig, DtcRecord, ServerConfig, UdsServer, DYNAMIC_DID_RANGE};
pub use trace::{Direction, Trace, TraceEntry};
pub use transport::SimTransport;
