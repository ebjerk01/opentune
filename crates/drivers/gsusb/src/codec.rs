//! gs_usb USB host-frame codec (`struct gs_host_frame`).
//!
//! This is the wire format a gs_usb device exchanges over its bulk USB
//! endpoints, independent of any OS CAN stack. It is provided so a raw-libusb
//! backend (for platforms without the kernel `gs_usb` driver) can be built on
//! top; the kernel/SocketCAN path in the parent module does not use it.
//!
//! Classic-CAN frame layout (little-endian, 20 bytes; a 4-byte hardware
//! timestamp may follow when `GS_CAN_FEATURE_HW_TIMESTAMP` is negotiated, and is
//! not encoded here):
//!
//! ```text
//! offset  size  field
//!   0      4    echo_id     (0xFFFFFFFF for a device-received frame)
//!   4      4    can_id      (SocketCAN-style, with EFF/RTR/ERR flag bits)
//!   8      1    can_dlc
//!   9      1    channel
//!  10      1    flags
//!  11      1    reserved
//!  12      8    data[8]
//! ```

use opentune_core::{Error, Result};

/// Length of a classic `gs_host_frame` without the optional timestamp.
pub const HOST_FRAME_LEN: usize = 20;

/// `can_id` flag bits (identical to Linux `<linux/can.h>`).
pub const CAN_EFF_FLAG: u32 = 0x8000_0000;
pub const CAN_RTR_FLAG: u32 = 0x4000_0000;
pub const CAN_ERR_FLAG: u32 = 0x2000_0000;
/// Mask for the 29-bit extended identifier.
pub const CAN_EFF_MASK: u32 = 0x1FFF_FFFF;
/// Mask for the 11-bit standard identifier.
pub const CAN_SFF_MASK: u32 = 0x0000_07FF;

/// `echo_id` value a device uses for frames it received from the bus (as
/// opposed to the echo of a frame the host transmitted).
pub const ECHO_ID_RX: u32 = 0xFFFF_FFFF;

/// A decoded/decodable gs_usb classic-CAN host frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostFrame {
    pub echo_id: u32,
    /// Raw `can_id` including any EFF/RTR/ERR flag bits.
    pub can_id: u32,
    pub can_dlc: u8,
    pub channel: u8,
    pub flags: u8,
    /// Payload, trimmed to `can_dlc` (≤ 8 for classic CAN).
    pub data: Vec<u8>,
}

impl HostFrame {
    /// Build a host frame to transmit `data` with CAN identifier `id` on
    /// `channel`. The extended-frame flag is set automatically for ids that do
    /// not fit 11 bits.
    pub fn tx(id: u32, data: &[u8], channel: u8) -> Self {
        let extended = id > CAN_SFF_MASK;
        let can_id = if extended {
            (id & CAN_EFF_MASK) | CAN_EFF_FLAG
        } else {
            id & CAN_SFF_MASK
        };
        let len = data.len().min(8);
        Self {
            echo_id: 0,
            can_id,
            can_dlc: len as u8,
            channel,
            flags: 0,
            data: data[..len].to_vec(),
        }
    }

    /// The bus identifier with the flag bits removed.
    pub fn id(&self) -> u32 {
        if self.is_extended() {
            self.can_id & CAN_EFF_MASK
        } else {
            self.can_id & CAN_SFF_MASK
        }
    }

    pub fn is_extended(&self) -> bool {
        self.can_id & CAN_EFF_FLAG != 0
    }

    pub fn is_remote(&self) -> bool {
        self.can_id & CAN_RTR_FLAG != 0
    }

    /// True if this frame is one the device received from the bus.
    pub fn is_rx(&self) -> bool {
        self.echo_id == ECHO_ID_RX
    }

    /// Encode to the 20-byte classic host-frame wire layout.
    pub fn encode(&self) -> [u8; HOST_FRAME_LEN] {
        let mut buf = [0u8; HOST_FRAME_LEN];
        buf[0..4].copy_from_slice(&self.echo_id.to_le_bytes());
        buf[4..8].copy_from_slice(&self.can_id.to_le_bytes());
        buf[8] = self.can_dlc;
        buf[9] = self.channel;
        buf[10] = self.flags;
        buf[11] = 0; // reserved
        let n = (self.can_dlc as usize).min(8).min(self.data.len());
        buf[12..12 + n].copy_from_slice(&self.data[..n]);
        buf
    }

    /// Decode a classic host frame from the first [`HOST_FRAME_LEN`] bytes of
    /// `buf`. Extra trailing bytes (e.g. a hardware timestamp) are ignored.
    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < HOST_FRAME_LEN {
            return Err(Error::Protocol(format!(
                "gs_usb host frame too short: {} < {HOST_FRAME_LEN}",
                buf.len()
            )));
        }
        let echo_id = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let can_id = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let can_dlc = buf[8];
        let channel = buf[9];
        let flags = buf[10];
        let n = (can_dlc as usize).min(8);
        Ok(Self {
            echo_id,
            can_id,
            can_dlc,
            channel,
            flags,
            data: buf[12..12 + n].to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_frame_round_trips() {
        let f = HostFrame::tx(0x7E0, &[0x02, 0x3E, 0x00], 0);
        assert!(!f.is_extended());
        assert_eq!(f.id(), 0x7E0);
        let bytes = f.encode();
        assert_eq!(bytes.len(), HOST_FRAME_LEN);
        // can_id little-endian, no EFF flag.
        assert_eq!(&bytes[4..8], &0x0000_07E0u32.to_le_bytes());
        assert_eq!(bytes[8], 3); // dlc
        assert_eq!(&bytes[12..15], &[0x02, 0x3E, 0x00]);
        let back = HostFrame::decode(&bytes).unwrap();
        assert_eq!(back, f);
        assert_eq!(back.id(), 0x7E0);
    }

    #[test]
    fn extended_frame_sets_eff_flag() {
        let f = HostFrame::tx(0x18DA_F110, &[0xAA], 1);
        assert!(f.is_extended());
        assert_eq!(f.id(), 0x18DA_F110);
        assert_eq!(f.can_id & CAN_EFF_FLAG, CAN_EFF_FLAG);
        let back = HostFrame::decode(&f.encode()).unwrap();
        assert_eq!(back.id(), 0x18DA_F110);
        assert!(back.is_extended());
        assert_eq!(back.channel, 1);
    }

    #[test]
    fn payload_is_clamped_to_eight() {
        let f = HostFrame::tx(0x123, &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10], 0);
        assert_eq!(f.can_dlc, 8);
        assert_eq!(f.data.len(), 8);
    }

    #[test]
    fn rx_echo_id_detected() {
        let mut bytes = HostFrame::tx(0x7E8, &[0x01], 0).encode();
        bytes[0..4].copy_from_slice(&ECHO_ID_RX.to_le_bytes());
        let f = HostFrame::decode(&bytes).unwrap();
        assert!(f.is_rx());
    }

    #[test]
    fn decode_rejects_short_buffer() {
        assert!(HostFrame::decode(&[0u8; 10]).is_err());
    }
}
