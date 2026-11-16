//! Shared value types.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Transport/protocol an ECU speaks over the diagnostic connector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    /// ISO 15765-4: UDS/KWP over CAN with ISO-TP.
    IsoTp,
    /// VW TP2.0: KWP2000 over CAN (VAG).
    Vwtp20,
    /// ISO 14230-4: KWP2000 over K-line.
    Iso14230Kline,
    /// ISO 9141-2: K-line.
    Iso9141,
    /// Raw CAN frames (no transport).
    RawCan,
    // NOTE: CAN-FD and DoIP are implemented at the transport layer
    // ([`crate::protocol::canfd::IsoTpFd`], [`crate::protocol::doip::DoIpTransport`]).
    // Adding `CanFd`/`DoIp` variants here is deferred: several interface drivers
    // match `Protocol` exhaustively, so the variants must land together with
    // those drivers' handling (a wildcard arm or per-driver support) rather than
    // break them from opentune-core alone.
}

/// Diagnostic service layer spoken on top of the transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagLayer {
    Uds,
    Kwp2000,
}

/// A contiguous region of ECU memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRegion {
    pub address: u32,
    pub length: u32,
}

impl MemoryRegion {
    pub fn new(address: u32, length: u32) -> Self {
        Self { address, length }
    }
    pub fn end(&self) -> u32 {
        self.address.wrapping_add(self.length)
    }
}

/// Identity read from an ECU.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EcuId {
    pub vin: Option<String>,
    pub hw_number: Option<String>,
    pub sw_number: Option<String>,
    pub supplier: Option<String>,
    pub ecu_name: Option<String>,
    pub raw: BTreeMap<String, String>,
}

/// Phase of a long-running operation, for progress reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Connect,
    Identify,
    SecurityAccess,
    Erase,
    Read,
    Write,
    Verify,
}

/// Progress callback payload.
#[derive(Debug, Clone, Copy)]
pub struct Progress {
    pub phase: Phase,
    pub done: u64,
    pub total: u64,
}

/// Connection configuration passed to a driver on `open`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectConfig {
    pub protocol: Protocol,
    pub diag_layer: DiagLayer,
    /// CAN bitrate or K-line baud.
    pub bitrate: u32,
    /// Diagnostic tx/rx CAN ids (or K-line ECU address in `tx_id`).
    pub tx_id: u32,
    pub rx_id: u32,
    /// Extra driver-specific options.
    #[serde(default)]
    pub options: BTreeMap<String, String>,
}

impl Default for ConnectConfig {
    fn default() -> Self {
        Self {
            protocol: Protocol::IsoTp,
            diag_layer: DiagLayer::Uds,
            bitrate: 500_000,
            tx_id: 0x7E0,
            rx_id: 0x7E8,
            options: BTreeMap::new(),
        }
    }
}
