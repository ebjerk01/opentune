//! CAN-FD support: a richer [`CanFrame`] (up to 64 data bytes, FD/BRS flags),
//! the CAN-FD DLC↔length table, and [`IsoTpFd`] — ISO 15765-2:2016 ISO-TP in
//! its CAN-FD mode.
//!
//! The classic-CAN transport ([`crate::protocol::isotp`]) is unchanged. This
//! module adds an FD-capable sibling that runs over the *same* [`CanLink`]
//! trait: FD metadata flows through `CanLink::send_can_frame` /
//! `recv_can_frame` (both defaulted on the trait), so an existing classic
//! driver can be driven in classic mode and an FD-capable driver carries the
//! 64-byte payloads and the BRS flag natively.
//!
//! [`IsoTpFd`] handles both framings from one implementation:
//!
//! * **classic mode** ([`IsoTpFd::new_classic`]) — 8-byte frames, identical on
//!   the wire to [`crate::protocol::isotp::IsoTp`]; and
//! * **FD mode** ([`IsoTpFd::new_fd`]) — frames up to 64 bytes, the escape
//!   SingleFrame (2-byte length) for payloads of 8..=62 bytes, FirstFrame /
//!   ConsecutiveFrame using the full FD payload, the 32-bit escape FirstFrame
//!   for payloads over 4095 bytes, and TX padding to the nearest valid CAN-FD
//!   data length.
//!
//! PCI encoding, flow-control handling (Block Size / STmin / WAIT / OVERFLOW)
//! and sequence-number validation are the same rules as the classic transport;
//! the shared constants and the STmin decoder are reused from
//! [`crate::protocol::isotp`].
use crate::protocol::diag::DiagTransport;
use crate::protocol::isotp::{flow_status, pci, stmin_to_micros, CanLink};
use crate::{Error, Result};

/// The valid CAN-FD frame data lengths, indexed by DLC (0..=15).
///
/// A classic CAN DLC maps 0..=8 to 0..=8 bytes; CAN-FD reuses 0..=8 for 0..=8
/// bytes and then jumps 12/16/20/24/32/48/64 for DLC 9..=15 (ISO 11898-1).
pub const FD_DATA_LENGTHS: [usize; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 12, 16, 20, 24, 32, 48, 64];

/// The largest CAN-FD frame payload, in bytes.
pub const FD_MAX_DLEN: usize = 64;

/// True if `n` is a valid CAN-FD frame data length.
pub fn is_valid_fd_len(n: usize) -> bool {
    FD_DATA_LENGTHS.contains(&n)
}

/// The smallest valid CAN-FD data length ≥ `n` — the TX padding target for a
/// frame carrying `n` bytes. `None` if `n` exceeds [`FD_MAX_DLEN`].
pub fn fd_pad_len(n: usize) -> Option<usize> {
    FD_DATA_LENGTHS.iter().copied().find(|&l| l >= n)
}

/// The DLC (0..=15) encoding a given valid CAN-FD data length.
pub fn len_to_fd_dlc(n: usize) -> Option<u8> {
    FD_DATA_LENGTHS
        .iter()
        .position(|&l| l == n)
        .map(|i| i as u8)
}

/// The CAN-FD data length for a DLC (0..=15).
pub fn fd_dlc_to_len(dlc: u8) -> Option<usize> {
    FD_DATA_LENGTHS.get(dlc as usize).copied()
}

/// A CAN frame that can carry either classic (≤8 byte) or CAN-FD (≤64 byte)
/// payloads, with the FD-format and bit-rate-switch flags.
///
/// Carries a full `u32` identifier (11-bit or 29-bit); nothing here masks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanFrame {
    /// CAN identifier (11-bit or 29-bit; the caller tracks which).
    pub id: u32,
    /// Frame payload (0..=8 for classic, up to 64 for CAN-FD).
    pub data: Vec<u8>,
    /// FD-format frame (EDL/FDF set). `false` is a classic CAN frame.
    pub fd: bool,
    /// Bit-rate switch (BRS): the data phase ran at the faster FD bitrate.
    /// Only meaningful when [`fd`](CanFrame::fd) is set.
    pub brs: bool,
}

impl CanFrame {
    /// A classic (non-FD) CAN frame.
    pub fn classic(id: u32, data: Vec<u8>) -> Self {
        Self {
            id,
            data,
            fd: false,
            brs: false,
        }
    }

    /// A CAN-FD frame, with the bit-rate-switch flag.
    pub fn fd(id: u32, data: Vec<u8>, brs: bool) -> Self {
        Self {
            id,
            data,
            fd: true,
            brs,
        }
    }

    /// The DLC (0..=15) for this frame's data length, if the length is a valid
    /// CAN(-FD) length. A classic frame only has DLCs 0..=8.
    pub fn dlc(&self) -> Option<u8> {
        if self.fd {
            len_to_fd_dlc(self.data.len())
        } else if self.data.len() <= 8 {
            Some(self.data.len() as u8)
        } else {
            None
        }
    }
}

/// Flow-control parameters received from the ECU during a segmented transmit.
struct FlowControl {
    block_size: u8,
    st_min: u8,
}

/// ISO 15765-2 transport over a [`CanLink`], supporting classic CAN and CAN-FD
/// framing. Owns the link so it can serve as a [`DiagTransport`].
pub struct IsoTpFd {
    link: Box<dyn CanLink>,
    pub tx_id: u32,
    pub rx_id: u32,
    /// Frame data length used for transmitted frames: 8 in classic mode, up to
    /// 64 in FD mode. Transmitted frames are padded up to a valid length ≤ this.
    pub tx_dl: usize,
    /// Whether transmitted frames are marked as CAN-FD (and padded to a valid
    /// CAN-FD length). Reception handles either framing regardless.
    pub fd: bool,
    /// Bit-rate-switch flag set on transmitted FD frames.
    pub brs: bool,
    /// Padding byte. Classic frames are padded to 8 bytes; FD frames to the
    /// nearest valid CAN-FD length. ISO-TP over CAN-FD requires padding so that
    /// every frame lands on a valid DLC, so this transport always pads.
    pub padding: u8,
    /// Block size advertised to the ECU when *receiving* (0 = no limit).
    pub block_size: u8,
    /// STmin advertised to the ECU when *receiving*.
    pub st_min: u8,
    /// Maximum consecutive `WAIT` (0x31) flow-control frames tolerated on
    /// transmit before giving up (ISO-TP `N_WFTmax`).
    pub wft_max: u8,
    /// Response-pending (NRC 0x78) replies tolerated before
    /// [`DiagTransport::request`] gives up.
    pub max_response_pending: u32,
}

impl IsoTpFd {
    /// Classic-CAN ISO-TP over an FD-capable (or any) link: 8-byte frames.
    pub fn new_classic(link: Box<dyn CanLink>, tx_id: u32, rx_id: u32) -> Self {
        Self {
            link,
            tx_id,
            rx_id,
            tx_dl: 8,
            fd: false,
            brs: false,
            padding: 0xAA,
            block_size: 0,
            st_min: 0,
            wft_max: 16,
            max_response_pending: 32,
        }
    }

    /// CAN-FD ISO-TP: frames up to 64 bytes, BRS set on transmit.
    pub fn new_fd(link: Box<dyn CanLink>, tx_id: u32, rx_id: u32) -> Self {
        Self {
            link,
            tx_id,
            rx_id,
            tx_dl: FD_MAX_DLEN,
            fd: true,
            brs: true,
            padding: 0xCC,
            block_size: 0,
            st_min: 0,
            wft_max: 16,
            max_response_pending: 32,
        }
    }

    /// Build the frame for `data` (already containing its PCI bytes): pad to a
    /// valid length and send it with the configured FD/BRS flags.
    fn send_padded(&mut self, mut data: Vec<u8>) -> Result<()> {
        if self.fd {
            let target = fd_pad_len(data.len())
                .ok_or_else(|| Error::Protocol("frame exceeds CAN-FD maximum length".into()))?;
            while data.len() < target {
                data.push(self.padding);
            }
            let frame = CanFrame::fd(self.tx_id, data, self.brs);
            self.link.send_can_frame(&frame)
        } else {
            while data.len() < 8 {
                data.push(self.padding);
            }
            let frame = CanFrame::classic(self.tx_id, data);
            self.link.send_can_frame(&frame)
        }
    }

    /// Receive the next frame addressed to us, skipping other ids.
    fn recv_from_ecu(&mut self, timeout_ms: u64) -> Result<Vec<u8>> {
        loop {
            let frame = self.link.recv_can_frame(timeout_ms)?;
            if frame.id == self.rx_id {
                if frame.data.is_empty() {
                    return Err(Error::Protocol("empty CAN frame".into()));
                }
                return Ok(frame.data);
            }
        }
    }

    fn sleep_st_min(st_min: u8) {
        let micros = stmin_to_micros(st_min);
        if micros > 0 {
            std::thread::sleep(std::time::Duration::from_micros(micros));
        }
    }

    /// Wait for a flow-control frame, honoring `WAIT` and `OVERFLOW`.
    fn await_flow_control(&mut self, timeout_ms: u64) -> Result<FlowControl> {
        let mut waits = 0u8;
        loop {
            let fc = self.recv_from_ecu(timeout_ms)?;
            if fc[0] & pci::MASK != pci::FLOW_CONTROL {
                return Err(Error::Protocol("expected flow control frame".into()));
            }
            match fc[0] & 0x0F {
                flow_status::CONTINUE => {
                    if fc.len() < 3 {
                        return Err(Error::Protocol("truncated flow control frame".into()));
                    }
                    return Ok(FlowControl {
                        block_size: fc[1],
                        st_min: fc[2],
                    });
                }
                flow_status::WAIT => {
                    waits += 1;
                    if waits > self.wft_max {
                        return Err(Error::Protocol("too many flow-control WAIT frames".into()));
                    }
                }
                flow_status::OVERFLOW => {
                    return Err(Error::Protocol(
                        "flow control OVERFLOW: receiver buffer too small".into(),
                    ));
                }
                other => {
                    return Err(Error::Protocol(format!("reserved flow status 0x{other:X}")));
                }
            }
        }
    }

    /// Send an ISO-TP payload (single frame, or first frame + consecutive
    /// frames), choosing classic or FD framing per [`tx_dl`](IsoTpFd::tx_dl).
    pub fn send(&mut self, payload: &[u8], timeout_ms: u64) -> Result<()> {
        if payload.is_empty() {
            return Err(Error::Protocol("empty payload".into()));
        }
        let len = payload.len();

        // SingleFrame with a one-byte PCI (length in the low nibble): ≤7 bytes.
        // Valid on classic and FD alike.
        if len <= 7 {
            let mut d = Vec::with_capacity(self.tx_dl);
            d.push(len as u8);
            d.extend_from_slice(payload);
            return self.send_padded(d);
        }

        // Escape SingleFrame (CAN-FD): a zero low-nibble PCI byte, then a
        // one-byte length. Usable up to tx_dl - 2 data bytes.
        if self.fd && len <= self.tx_dl - 2 {
            let mut d = Vec::with_capacity(self.tx_dl);
            d.push(pci::SINGLE); // high nibble SF, low nibble 0 = escape
            d.push(len as u8);
            d.extend_from_slice(payload);
            return self.send_padded(d);
        }

        // Multi-frame: FirstFrame + ConsecutiveFrames.
        let mut ff = Vec::with_capacity(self.tx_dl);
        let first_data = if len <= 0x0FFF {
            ff.push(pci::FIRST | ((len >> 8) & 0x0F) as u8);
            ff.push((len & 0xFF) as u8);
            self.tx_dl - 2
        } else {
            // Escape FirstFrame: FF_DL = 0 in the 12-bit field, then a 32-bit
            // length (ISO 15765-2:2016 §9.6.3.2).
            ff.push(pci::FIRST);
            ff.push(0x00);
            ff.extend_from_slice(&(len as u32).to_be_bytes());
            self.tx_dl - 6
        };
        let n0 = first_data.min(len);
        ff.extend_from_slice(&payload[..n0]);
        self.send_padded(ff)?;

        let mut fc = self.await_flow_control(timeout_ms)?;
        let cf_data = self.tx_dl - 1;
        let mut sn: u8 = 1;
        let mut idx = n0;
        let mut cfs_in_block: u32 = 0;
        while idx < len {
            let end = (idx + cf_data).min(len);
            let mut cf = Vec::with_capacity(self.tx_dl);
            cf.push(pci::CONSECUTIVE | (sn & 0x0F));
            cf.extend_from_slice(&payload[idx..end]);
            self.send_padded(cf)?;
            sn = sn.wrapping_add(1) & 0x0F;
            idx = end;
            cfs_in_block += 1;

            if idx >= len {
                break;
            }
            if fc.block_size != 0 && cfs_in_block >= fc.block_size as u32 {
                fc = self.await_flow_control(timeout_ms)?;
                cfs_in_block = 0;
            } else {
                Self::sleep_st_min(fc.st_min);
            }
        }
        Ok(())
    }

    /// Send a Clear-To-Send flow-control frame advertising our BS/STmin.
    fn send_flow_control(&mut self) -> Result<()> {
        self.send_padded(vec![
            pci::FLOW_CONTROL | flow_status::CONTINUE,
            self.block_size,
            self.st_min,
        ])
    }

    /// Receive a full ISO-TP payload, handling classic and CAN-FD framing
    /// (one-byte SF, escape SF, standard FF, escape FF, and CFs).
    pub fn recv(&mut self, timeout_ms: u64) -> Result<Vec<u8>> {
        let first = self.recv_from_ecu(timeout_ms)?;
        match first[0] & pci::MASK {
            pci::SINGLE => {
                let low = first[0] & 0x0F;
                if low != 0 {
                    // One-byte PCI SingleFrame.
                    let len = low as usize;
                    if 1 + len > first.len() {
                        return Err(Error::Protocol("single frame length exceeds data".into()));
                    }
                    Ok(first[1..1 + len].to_vec())
                } else {
                    // Escape SingleFrame: length in byte 1 (CAN-FD).
                    if first.len() < 2 {
                        return Err(Error::Protocol("truncated escape single frame".into()));
                    }
                    let len = first[1] as usize;
                    if len == 0 {
                        return Err(Error::Protocol("single frame with zero length".into()));
                    }
                    if 2 + len > first.len() {
                        return Err(Error::Protocol("single frame length exceeds data".into()));
                    }
                    Ok(first[2..2 + len].to_vec())
                }
            }
            pci::FIRST => {
                // Escape FirstFrame carries FF_DL == 0 in the 12-bit field, then
                // a 32-bit length; standard FirstFrame never has length 0.
                let (len, mut idx) = if first[0] == pci::FIRST && first.get(1) == Some(&0x00) {
                    if first.len() < 6 {
                        return Err(Error::Protocol("truncated escape first frame".into()));
                    }
                    let len = u32::from_be_bytes([first[2], first[3], first[4], first[5]]) as usize;
                    (len, 6)
                } else {
                    if first.len() < 2 {
                        return Err(Error::Protocol("truncated first frame".into()));
                    }
                    let len = (((first[0] & 0x0F) as usize) << 8) | first[1] as usize;
                    (len, 2)
                };
                if len <= 7 {
                    return Err(Error::Protocol(
                        "first frame used for a payload that fits a single frame".into(),
                    ));
                }
                let mut out = Vec::with_capacity(len);
                let take = (first.len() - idx).min(len);
                out.extend_from_slice(&first[idx..idx + take]);
                idx += take;
                let _ = idx;

                self.send_flow_control()?;

                let mut expected_sn: u8 = 1;
                let mut cfs_in_block: u32 = 0;
                while out.len() < len {
                    let cf = self.recv_from_ecu(timeout_ms)?;
                    if cf[0] & pci::MASK != pci::CONSECUTIVE {
                        return Err(Error::Protocol("expected consecutive frame".into()));
                    }
                    let sn = cf[0] & 0x0F;
                    if sn != expected_sn {
                        return Err(Error::Protocol(format!(
                            "consecutive-frame sequence error: expected {expected_sn}, got {sn}"
                        )));
                    }
                    expected_sn = expected_sn.wrapping_add(1) & 0x0F;

                    let remaining = len - out.len();
                    let take = remaining.min(cf.len().saturating_sub(1));
                    out.extend_from_slice(&cf[1..1 + take]);

                    cfs_in_block += 1;
                    if out.len() < len
                        && self.block_size != 0
                        && cfs_in_block >= self.block_size as u32
                    {
                        self.send_flow_control()?;
                        cfs_in_block = 0;
                    }
                }
                Ok(out)
            }
            _ => Err(Error::Protocol("unexpected ISO-TP PCI".into())),
        }
    }
}

impl DiagTransport for IsoTpFd {
    fn request(&mut self, req: &[u8], timeout_ms: u64) -> Result<Vec<u8>> {
        self.send(req, timeout_ms)?;
        for _ in 0..self.max_response_pending {
            let resp = self.recv(timeout_ms)?;
            if resp.len() >= 3 && resp[0] == 0x7F && resp[2] == 0x78 {
                continue;
            }
            return Ok(resp);
        }
        Err(Error::Timeout(timeout_ms))
    }
}

#[cfg(test)]
#[allow(clippy::type_complexity)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    /// A scripted CAN-FD endpoint: returns queued frames on recv, records sent
    /// frames (with their FD/BRS flags) for the test to inspect.
    struct FakeFdEcu {
        to_tester: VecDeque<CanFrame>,
        sent: Rc<RefCell<Vec<CanFrame>>>,
        rx_id: u32,
    }

    impl FakeFdEcu {
        fn new(rx_id: u32, frames: Vec<CanFrame>) -> (Self, Rc<RefCell<Vec<CanFrame>>>) {
            let sent = Rc::new(RefCell::new(Vec::new()));
            (
                Self {
                    to_tester: VecDeque::from(frames),
                    sent: sent.clone(),
                    rx_id,
                },
                sent,
            )
        }
    }

    impl CanLink for FakeFdEcu {
        fn send_frame(&mut self, id: u32, data: &[u8]) -> Result<()> {
            // Classic path still works; record as a classic frame.
            self.sent
                .borrow_mut()
                .push(CanFrame::classic(id, data.to_vec()));
            Ok(())
        }
        fn recv_frame(&mut self, timeout_ms: u64) -> Result<(u32, Vec<u8>)> {
            self.recv_can_frame(timeout_ms).map(|f| (f.id, f.data))
        }
        fn supports_fd(&self) -> bool {
            true
        }
        fn send_can_frame(&mut self, frame: &CanFrame) -> Result<()> {
            self.sent.borrow_mut().push(frame.clone());
            Ok(())
        }
        fn recv_can_frame(&mut self, _timeout_ms: u64) -> Result<CanFrame> {
            self.to_tester
                .pop_front()
                // The ECU answers on the tester's rx id.
                .map(|mut f| {
                    f.id = self.rx_id;
                    f
                })
                .ok_or(Error::Timeout(0))
        }
    }

    fn cf_fd(data: Vec<u8>) -> CanFrame {
        CanFrame::fd(0, data, true)
    }

    #[test]
    fn fd_dlc_table_round_trips() {
        for (dlc, &len) in FD_DATA_LENGTHS.iter().enumerate() {
            assert_eq!(fd_dlc_to_len(dlc as u8), Some(len));
            assert_eq!(len_to_fd_dlc(len), Some(dlc as u8));
            assert!(is_valid_fd_len(len));
        }
        // Values between table entries are not valid lengths.
        assert!(!is_valid_fd_len(9));
        assert!(!is_valid_fd_len(63));
        // Padding rounds up to the next valid length.
        assert_eq!(fd_pad_len(9), Some(12));
        assert_eq!(fd_pad_len(33), Some(48));
        assert_eq!(fd_pad_len(64), Some(64));
        assert_eq!(fd_pad_len(65), None);
    }

    #[test]
    fn fd_single_frame_round_trip() {
        // 3-byte request, 3-byte response, both one-byte-PCI single frames.
        let resp = cf_fd(vec![0x03, 0x62, 0x12, 0x34]);
        let (ecu, log) = FakeFdEcu::new(0x7E8, vec![resp]);
        let mut tp = IsoTpFd::new_fd(Box::new(ecu), 0x7E0, 0x7E8);
        tp.send(&[0x22, 0x12, 0x34], 100).unwrap();
        let got = tp.recv(100).unwrap();
        assert_eq!(got, vec![0x62, 0x12, 0x34]);

        // The transmitted frame was FD, BRS set, padded to a valid FD length.
        let sent = log.borrow();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].fd && sent[0].brs);
        assert!(is_valid_fd_len(sent[0].data.len()));
        assert_eq!(sent[0].data[0], 0x03); // one-byte PCI SF, length 3
    }

    #[test]
    fn fd_escape_single_frame_for_large_payload() {
        // A 40-byte payload fits one CAN-FD escape single frame (no FC needed).
        let payload: Vec<u8> = (0..40).collect();
        // ECU replies with a 20-byte escape SF.
        let mut resp = vec![0x00, 20];
        resp.extend(100u8..120);
        while resp.len() < 24 {
            resp.push(0xCC);
        }
        let (ecu, log) = FakeFdEcu::new(0x7E8, vec![cf_fd(resp)]);
        let mut tp = IsoTpFd::new_fd(Box::new(ecu), 0x7E0, 0x7E8);
        tp.send(&payload, 100).unwrap();
        let got = tp.recv(100).unwrap();
        assert_eq!(got, (100u8..120).collect::<Vec<u8>>());

        let sent = log.borrow();
        assert_eq!(sent.len(), 1, "40 bytes must fit one escape single frame");
        assert_eq!(sent[0].data[0], 0x00); // escape SF PCI
        assert_eq!(sent[0].data[1], 40); // length
        assert_eq!(fd_pad_len(42), Some(48));
        assert_eq!(sent[0].data.len(), 48); // padded to a valid FD length
    }

    #[test]
    fn fd_multiframe_send_and_recv() {
        // Payload of 100 bytes: one escape SF would not fit (>62), so it is
        // FF (62 data bytes) + one CF (38 bytes). The ECU sends FC, then a
        // 100-byte response as FF + CF.
        let payload: Vec<u8> = (0..100).map(|i| i as u8).collect();

        let fc = cf_fd(vec![0x30, 0x00, 0x00]);
        // Response FF: standard FirstFrame for a 100-byte payload, 62 data bytes.
        let total = 100usize;
        let mut resp_ff = vec![
            pci::FIRST | ((total >> 8) & 0x0F) as u8,
            (total & 0xFF) as u8,
        ];
        resp_ff.extend((0u8..62).map(|i| 200u8.wrapping_add(i)));
        // Response CF: SN 1, remaining 38 bytes.
        let mut resp_cf = vec![pci::CONSECUTIVE | 1];
        resp_cf.extend((62u8..100).map(|i| 200u8.wrapping_add(i)));
        while resp_cf.len() < 48 {
            resp_cf.push(0xCC);
        }

        let (ecu, log) = FakeFdEcu::new(0x7E8, vec![fc, cf_fd(resp_ff), cf_fd(resp_cf)]);
        let mut tp = IsoTpFd::new_fd(Box::new(ecu), 0x7E0, 0x7E8);
        tp.send(&payload, 100).unwrap();

        // Snapshot the request-side frames before recv() adds a flow-control
        // frame of its own: the tester sent FF + one CF (2 frames), both FD.
        let request_frames = log.borrow().clone();
        assert_eq!(request_frames.len(), 2);
        assert_eq!(request_frames[0].data[0] & pci::MASK, pci::FIRST);
        assert_eq!(request_frames[0].data[1], 100); // low byte of length
        assert_eq!(request_frames[1].data[0], pci::CONSECUTIVE | 1);
        assert!(request_frames.iter().all(|f| f.fd));

        let got = tp.recv(100).unwrap();
        let expected: Vec<u8> = (0..100).map(|i| 200u8.wrapping_add(i)).collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn fd_escape_first_frame_for_huge_payload() {
        // 5000 bytes forces the 32-bit escape FirstFrame (len > 0x0FFF).
        let payload: Vec<u8> = (0..5000).map(|i| i as u8).collect();
        let fc = cf_fd(vec![0x30, 0x00, 0x00]);
        // Minimal valid response so recv() has something; a 3-byte SF.
        let resp = cf_fd(vec![0x03, 0x62, 0x00, 0x00]);
        let (ecu, log) = FakeFdEcu::new(0x7E8, vec![fc, resp]);
        let mut tp = IsoTpFd::new_fd(Box::new(ecu), 0x7E0, 0x7E8);
        tp.send(&payload, 100).unwrap();
        let _ = tp.recv(100).unwrap();

        let sent = log.borrow();
        // First frame is the escape form: 0x10 0x00 then the 32-bit length.
        assert_eq!(sent[0].data[0], pci::FIRST);
        assert_eq!(sent[0].data[1], 0x00);
        assert_eq!(
            u32::from_be_bytes([
                sent[0].data[2],
                sent[0].data[3],
                sent[0].data[4],
                sent[0].data[5]
            ]),
            5000
        );
        // FF carries 64-6 = 58 bytes; each CF 63. Total frames = 1 + ceil((5000-58)/63).
        let cf_count = (5000usize - 58).div_ceil(63);
        assert_eq!(sent.len(), 1 + cf_count);
    }

    #[test]
    fn classic_mode_over_fd_link_round_trip() {
        // IsoTpFd in classic mode: 8-byte frames, FF(6)+CF for a 12-byte payload.
        let fc = cf_fd(vec![0x30, 0x00, 0x00]);
        let resp_ff = cf_fd(vec![0x10, 0x0A, 0, 1, 2, 3, 4, 5]);
        let resp_cf = cf_fd(vec![0x21, 6, 7, 8, 9, 0xAA, 0xAA, 0xAA]);
        let (ecu, log) = FakeFdEcu::new(0x7E8, vec![fc, resp_ff, resp_cf]);
        let mut tp = IsoTpFd::new_classic(Box::new(ecu), 0x7E0, 0x7E8);

        let payload: Vec<u8> = (0..12).collect();
        tp.send(&payload, 100).unwrap();
        let got = tp.recv(100).unwrap();
        assert_eq!(got, (0u8..10).collect::<Vec<u8>>());

        // Classic mode: every transmitted frame is exactly 8 bytes and not FD.
        let sent = log.borrow();
        assert!(sent.iter().all(|f| f.data.len() == 8 && !f.fd));
        assert_eq!(sent[0].data[0] & pci::MASK, pci::FIRST);
    }

    #[test]
    fn recv_sequence_number_gap_is_error() {
        // FD FirstFrame announces 80 bytes but the CF uses SN 2 instead of 1.
        let mut ff = vec![0x10, 80];
        ff.extend(0u8..62);
        let mut bad_cf = vec![pci::CONSECUTIVE | 2];
        bad_cf.extend(62u8..80);
        let (ecu, _log) = FakeFdEcu::new(0x7E8, vec![cf_fd(ff), cf_fd(bad_cf)]);
        let mut tp = IsoTpFd::new_fd(Box::new(ecu), 0x7E0, 0x7E8);
        let err = tp.recv(100).unwrap_err();
        assert!(matches!(err, Error::Protocol(_)));
    }

    #[test]
    fn diag_transport_handles_response_pending() {
        // ECU answers 0x7F .. 0x78 (response pending) once, then the real reply.
        let pending = cf_fd(vec![0x03, 0x7F, 0x22, 0x78]);
        let real = cf_fd(vec![0x03, 0x62, 0x12, 0x34]);
        let (ecu, _log) = FakeFdEcu::new(0x7E8, vec![pending, real]);
        let mut tp = IsoTpFd::new_fd(Box::new(ecu), 0x7E0, 0x7E8);
        let resp = tp.request(&[0x22, 0x12, 0x34], 100).unwrap();
        assert_eq!(resp, vec![0x62, 0x12, 0x34]);
    }
}
