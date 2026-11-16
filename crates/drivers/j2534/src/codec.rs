//! J2534 (SAE J2534-1) protocol constants and the PASSTHRU_MSG codec for the
//! ISO15765 (ISO-TP over CAN) protocol.
//!
//! A J2534 interface does the ISO-TP segmentation itself, so at the API level a
//! UDS request/response is one PASSTHRU_MSG whose `data` is the 4-byte CAN
//! identifier (big-endian) followed by the service bytes. These helpers build
//! and parse that framing; the FFI backend (Windows-only) marshals the resulting
//! [`PassThruMsg`] into the C `PASSTHRU_MSG` struct.

use opentune_core::{Error, Result};

// --- Protocol IDs (PassThruConnect) -----------------------------------------
pub const CAN: u32 = 5;
pub const ISO15765: u32 = 6;

// --- Connect flags ----------------------------------------------------------
/// 29-bit (extended) CAN identifiers.
pub const CAN_29BIT_ID: u32 = 0x0000_0100;

// --- Tx flags (PASSTHRU_MSG.TxFlags) ----------------------------------------
/// Pad ISO15765 frames to 8 bytes.
pub const ISO15765_FRAME_PAD: u32 = 0x0000_0040;
/// Message uses a 29-bit identifier.
pub const CAN_29BIT_ID_TX: u32 = 0x0000_0100;

// --- Rx status (PASSTHRU_MSG.RxStatus) --------------------------------------
/// This message is a transmit indication / loopback, not an ECU reply.
pub const TX_MSG_TYPE: u32 = 0x0000_0001;
/// First frame of a multi-frame ISO15765 message (indication only, no payload).
pub const START_OF_MESSAGE: u32 = 0x0000_0002;

// --- Filter types (PassThruStartMsgFilter) ----------------------------------
pub const PASS_FILTER: u32 = 1;
pub const BLOCK_FILTER: u32 = 2;
pub const FLOW_CONTROL_FILTER: u32 = 3;

// --- Common J2534 error codes (returned by the DLL) -------------------------
pub const STATUS_NOERROR: u32 = 0x00;
pub const ERR_TIMEOUT: u32 = 0x09;
pub const ERR_BUFFER_EMPTY: u32 = 0x10;

/// A J2534 message, abstracted to the fields OpenTune needs. The FFI backend
/// converts to/from the C `PASSTHRU_MSG` (which also carries timestamps and a
/// 4128-byte data buffer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassThruMsg {
    pub protocol_id: u32,
    pub rx_status: u32,
    pub tx_flags: u32,
    pub data: Vec<u8>,
}

impl PassThruMsg {
    /// Build an ISO15765 request message: 4-byte big-endian CAN id + payload.
    /// Sets the extended-id tx flag automatically for 29-bit ids, and always
    /// pads frames (`ISO15765_FRAME_PAD`), which the vast majority of ECUs
    /// expect.
    pub fn iso15765_request(can_id: u32, payload: &[u8]) -> Self {
        let mut data = Vec::with_capacity(4 + payload.len());
        data.extend_from_slice(&can_id.to_be_bytes());
        data.extend_from_slice(payload);
        let mut tx_flags = ISO15765_FRAME_PAD;
        if can_id > 0x7FF {
            tx_flags |= CAN_29BIT_ID_TX;
        }
        Self {
            protocol_id: ISO15765,
            rx_status: 0,
            tx_flags,
            data,
        }
    }

    /// Build the mask/pattern/flow-control triplet for the ISO15765 flow-control
    /// filter that routes an ECU reply (`rx_id`) back to a request (`tx_id`).
    pub fn iso15765_flow_filter(tx_id: u32, rx_id: u32) -> (Self, Self, Self) {
        let extended = tx_id > 0x7FF || rx_id > 0x7FF;
        let tx_flags = if extended { CAN_29BIT_ID_TX } else { 0 };
        let mask = Self {
            protocol_id: ISO15765,
            rx_status: 0,
            tx_flags,
            data: 0xFFFF_FFFFu32.to_be_bytes().to_vec(),
        };
        let pattern = Self {
            protocol_id: ISO15765,
            rx_status: 0,
            tx_flags,
            data: rx_id.to_be_bytes().to_vec(),
        };
        let flow = Self {
            protocol_id: ISO15765,
            rx_status: 0,
            tx_flags,
            data: tx_id.to_be_bytes().to_vec(),
        };
        (mask, pattern, flow)
    }

    /// True if this received message is a transmit indication / start-of-message
    /// marker rather than an actual ECU payload.
    pub fn is_indication(&self) -> bool {
        self.rx_status & (TX_MSG_TYPE | START_OF_MESSAGE) != 0
    }

    /// Extract `(can_id, payload)` from a received ISO15765 message.
    pub fn parse_iso15765(&self) -> Result<(u32, Vec<u8>)> {
        if self.data.len() < 4 {
            return Err(Error::Protocol(
                "J2534 ISO15765 message shorter than the 4-byte CAN id".into(),
            ));
        }
        let can_id = u32::from_be_bytes([self.data[0], self.data[1], self.data[2], self.data[3]]);
        Ok((can_id, self.data[4..].to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_prefixes_big_endian_can_id() {
        let m = PassThruMsg::iso15765_request(0x7E0, &[0x22, 0xF1, 0x90]);
        assert_eq!(m.protocol_id, ISO15765);
        assert_eq!(&m.data, &[0x00, 0x00, 0x07, 0xE0, 0x22, 0xF1, 0x90]);
        assert_eq!(m.tx_flags & ISO15765_FRAME_PAD, ISO15765_FRAME_PAD);
        assert_eq!(m.tx_flags & CAN_29BIT_ID_TX, 0);
    }

    #[test]
    fn extended_request_sets_29bit_flag() {
        let m = PassThruMsg::iso15765_request(0x18DA_10F1, &[0x3E, 0x00]);
        assert_eq!(m.tx_flags & CAN_29BIT_ID_TX, CAN_29BIT_ID_TX);
        assert_eq!(&m.data[..4], &0x18DA_10F1u32.to_be_bytes());
    }

    #[test]
    fn parse_round_trips_the_request() {
        let m = PassThruMsg::iso15765_request(0x7E8, &[0x62, 0xF1, 0x90, 0x41]);
        let (id, payload) = m.parse_iso15765().unwrap();
        assert_eq!(id, 0x7E8);
        assert_eq!(payload, vec![0x62, 0xF1, 0x90, 0x41]);
    }

    #[test]
    fn parse_rejects_truncated() {
        let m = PassThruMsg {
            protocol_id: ISO15765,
            rx_status: 0,
            tx_flags: 0,
            data: vec![0x00, 0x00],
        };
        assert!(m.parse_iso15765().is_err());
    }

    #[test]
    fn indication_messages_are_flagged() {
        let ind = PassThruMsg {
            protocol_id: ISO15765,
            rx_status: TX_MSG_TYPE,
            tx_flags: 0,
            data: 0x7E8u32.to_be_bytes().to_vec(),
        };
        assert!(ind.is_indication());
        let real = PassThruMsg {
            protocol_id: ISO15765,
            rx_status: 0,
            tx_flags: 0,
            data: [0x7E8u32.to_be_bytes().as_slice(), &[0x50, 0x03]].concat(),
        };
        assert!(!real.is_indication());
    }

    #[test]
    fn flow_filter_triplet_has_rx_pattern_and_tx_flow() {
        let (mask, pattern, flow) = PassThruMsg::iso15765_flow_filter(0x7E0, 0x7E8);
        assert_eq!(mask.data, 0xFFFF_FFFFu32.to_be_bytes());
        assert_eq!(pattern.data, 0x7E8u32.to_be_bytes());
        assert_eq!(flow.data, 0x7E0u32.to_be_bytes());
    }
}
