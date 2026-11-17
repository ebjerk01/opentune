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
//!   (0x10), ECUReset (0x11), ReadDTCInformation (0x19), ReadDataByIdentifier
//!   (0x22), SecurityAccess (0x27, with an injected [`security::SecurityChecker`]),
//!   WriteDataByIdentifier (0x2E), RoutineControl (0x31), RequestDownload /
//!   RequestUpload (0x34 / 0x35), TransferData (0x36, with block-counter
//!   tracking), RequestTransferExit (0x37) and TesterPresent (0x3E).
//! - [`transport::SimTransport`] — wraps the server as an `opentune_core`
//!   [`DiagTransport`], so [`UdsClient`] drives it directly.
//! - [`doip::DoIpSimStream`] — a loopback ISO 13400 DoIP entity, so the real
//!   [`DoIpTransport`] client (routing activation, acks, the 0x78 drain loop)
//!   runs against the same server end to end.
//! - [`fault::FaultPlan`] — programmable fault injection: response-pending
//!   storms, injected NRCs, wrong correlation echoes, truncated responses and
//!   mis-correlated replies, each firing deterministically at one point in a
//!   flow.
//! - [`trace::Trace`] — an ordered request/response/pending log for replay and
//!   golden-trace assertions.
//!
//! [`UdsClient`]: opentune_core::protocol::UdsClient
//! [`UdsServer::handle`]: server::UdsServer::handle
//! [`DiagTransport`]: opentune_core::protocol::DiagTransport
//! [`DoIpTransport`]: opentune_core::protocol::DoIpTransport
//! [`pylessard/python-udsoncan`]: https://github.com/pylessard/python-udsoncan
//! [`driftregion/iso14229`]: https://github.com/driftregion/iso14229

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod doip;
pub mod fault;
pub mod security;
pub mod server;
pub mod trace;
pub mod transport;

pub use doip::DoIpSimStream;
pub use fault::{Fault, FaultPlan};
pub use security::{AlwaysReject, SecurityChecker, XorSecurity};
pub use server::{DtcRecord, ServerConfig, UdsServer};
pub use trace::{Direction, Trace, TraceEntry};
pub use transport::SimTransport;
