//! ISO 15765-2 (ISO-TP) transport over a raw CAN link, plus a [`DiagTransport`]
//! implementation on top of it.
//!
//! Hardened against the `python-can-isotp` reference (MIT, reimplemented not
//! copied): flow-control Block Size / STmin are honored on transmit, `WAIT`
//! (0x31) and `OVFLW` (0x32) flow statuses are handled, consecutive-frame
//! sequence numbers are validated with correct 0x0F→0x00 wrap, 11-bit and
//! 29-bit CAN identifiers both work (the [`CanLink`] carries a full `u32` id and
//! nothing here masks it), and malformed frames are rejected instead of
//! panicking or reading out of bounds.
use crate::protocol::canfd::CanFrame;
use crate::protocol::diag::{is_response_pending, DiagTransport};
use crate::{Error, Result};

/// ISO-TP PCI (protocol control information) type, in the high nibble of byte 0.
///
/// Shared with the CAN-FD transport ([`crate::protocol::canfd`]); the PCI
/// encoding is identical for classic CAN and CAN-FD ISO-TP.
pub(crate) mod pci {
    pub const SINGLE: u8 = 0x00;
    pub const FIRST: u8 = 0x10;
    pub const CONSECUTIVE: u8 = 0x20;
    pub const FLOW_CONTROL: u8 = 0x30;
    pub const MASK: u8 = 0xF0;
}

/// Flow-control status, in the low nibble of a flow-control frame.
pub(crate) mod flow_status {
    pub const CONTINUE: u8 = 0x0; // ClearToSend
    pub const WAIT: u8 = 0x1;
    pub const OVERFLOW: u8 = 0x2;
}

/// Decode an ISO-TP STmin byte into microseconds (ISO 15765-2 §6.5.5.5).
///
/// * `0x00..=0x7F` → that many milliseconds.
/// * `0xF1..=0xF9` → 100–900 microseconds.
/// * anything else is reserved and, per spec, treated as the maximum (0x7F).
pub(crate) fn stmin_to_micros(stmin: u8) -> u64 {
    match stmin {
        0x00..=0x7F => (stmin as u64) * 1000,
        0xF1..=0xF9 => ((stmin - 0xF0) as u64) * 100,
        _ => 127 * 1000,
    }
}

/// A raw CAN link: send/receive single CAN frames. Provided by a driver.
///
/// Classic CAN drivers implement only [`send_frame`](CanLink::send_frame) and
/// [`recv_frame`](CanLink::recv_frame). CAN-FD metadata (payloads up to 64
/// bytes, the FD/BRS flags) travels through the defaulted
/// [`send_can_frame`](CanLink::send_can_frame) /
/// [`recv_can_frame`](CanLink::recv_can_frame) methods, which the CAN-FD
/// transport ([`crate::protocol::canfd::IsoTpFd`]) uses. The defaults route
/// through the classic byte interface and report classic frames, so existing
/// drivers keep working unchanged; an FD-capable driver overrides
/// [`supports_fd`](CanLink::supports_fd) and the two frame methods to carry the
/// flags natively.
pub trait CanLink {
    fn send_frame(&mut self, id: u32, data: &[u8]) -> Result<()>;
    /// Receive one frame `(id, data)`, waiting up to `timeout_ms`.
    /// Returns [`Error::Timeout`] on expiry.
    fn recv_frame(&mut self, timeout_ms: u64) -> Result<(u32, Vec<u8>)>;

    /// Whether this link can carry CAN-FD frames (data length > 8 and the
    /// bit-rate-switch flag). Classic links return `false`.
    fn supports_fd(&self) -> bool {
        false
    }

    /// Send a CAN frame carrying FD metadata. The default ignores the FD/BRS
    /// flags and sends the payload over the classic byte interface, which is
    /// correct for classic links and classic (≤8-byte) frames.
    fn send_can_frame(&mut self, frame: &CanFrame) -> Result<()> {
        self.send_frame(frame.id, &frame.data)
    }

    /// Receive a CAN frame with FD metadata. The default wraps the classic
    /// [`recv_frame`](CanLink::recv_frame) result as a non-FD frame.
    fn recv_can_frame(&mut self, timeout_ms: u64) -> Result<CanFrame> {
        let (id, data) = self.recv_frame(timeout_ms)?;
        Ok(CanFrame::classic(id, data))
    }
}

/// ISO-TP over an 11/29-bit CAN link. Owns the link so it can be used as a
/// [`DiagTransport`].
pub struct IsoTp {
    link: Box<dyn CanLink>,
    pub tx_id: u32,
    pub rx_id: u32,
    /// Pad every frame to 8 bytes with this byte, if set.
    pub padding: Option<u8>,
    /// Block size we advertise to the ECU when *receiving* (0 = send all CFs
    /// without further flow control).
    pub block_size: u8,
    /// STmin we advertise to the ECU when *receiving*.
    pub st_min: u8,
    /// Maximum number of consecutive `WAIT` (0x31) flow-control frames to
    /// tolerate on transmit before giving up (ISO-TP `N_WFTmax`).
    pub wft_max: u8,
    /// How many response-pending (NRC 0x78) replies to tolerate before the
    /// [`DiagTransport::request`] loop gives up.
    pub max_response_pending: u32,
}

/// Flow-control parameters received from the ECU during a segmented transmit.
struct FlowControl {
    block_size: u8,
    st_min: u8,
}

impl IsoTp {
    pub fn new(link: Box<dyn CanLink>, tx_id: u32, rx_id: u32) -> Self {
        Self {
            link,
            tx_id,
            rx_id,
            padding: Some(0xAA),
            block_size: 0,
            st_min: 0,
            wft_max: 16,
            max_response_pending: 32,
        }
    }

    fn pad(&self, mut f: Vec<u8>) -> Vec<u8> {
        if let Some(p) = self.padding {
            while f.len() < 8 {
                f.push(p);
            }
        }
        f
    }

    /// Receive the next frame addressed to us, skipping frames with other ids.
    fn recv_from_ecu(&mut self, timeout_ms: u64) -> Result<Vec<u8>> {
        loop {
            let (id, data) = self.link.recv_frame(timeout_ms)?;
            if id == self.rx_id {
                if data.is_empty() {
                    return Err(Error::Protocol("empty CAN frame".into()));
                }
                return Ok(data);
            }
        }
    }

    /// Sleep for the STmin separation time requested by the ECU. A zero/sub-
    /// millisecond separation is skipped so unit tests and fast links are not
    /// penalized; real hardware links should still see the gap from scheduling.
    fn sleep_st_min(st_min: u8) {
        let micros = stmin_to_micros(st_min);
        if micros > 0 {
            std::thread::sleep(std::time::Duration::from_micros(micros));
        }
    }

    /// Wait for a flow-control frame, honoring `WAIT` (keep waiting, bounded by
    /// `wft_max`) and `OVERFLOW` (abort).
    fn await_flow_control(&mut self, timeout_ms: u64) -> Result<FlowControl> {
        let mut waits = 0u8;
        loop {
            let fc = self.recv_from_ecu(timeout_ms)?;
            if fc[0] & pci::MASK != pci::FLOW_CONTROL {
                return Err(Error::Protocol("expected flow control frame".into()));
            }
            match fc[0] & 0x0F {
                flow_status::CONTINUE => {
                    // A valid FC.CTS must carry BS and STmin bytes.
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
                    // Loop and wait for the next flow-control frame.
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

    /// Send a payload (single frame, or first frame + consecutive frames).
    pub fn send(&mut self, payload: &[u8], timeout_ms: u64) -> Result<()> {
        if payload.is_empty() {
            return Err(Error::Protocol("empty payload".into()));
        }
        if payload.len() <= 7 {
            let mut d = Vec::with_capacity(8);
            d.push(payload.len() as u8);
            d.extend_from_slice(payload);
            let f = self.pad(d);
            return self.link.send_frame(self.tx_id, &f);
        }
        let len = payload.len();
        if len > 0x0FFF {
            return Err(Error::Protocol(
                "payload too large for classic ISO-TP".into(),
            ));
        }
        let mut ff = Vec::with_capacity(8);
        ff.push(pci::FIRST | ((len >> 8) & 0x0F) as u8);
        ff.push((len & 0xFF) as u8);
        ff.extend_from_slice(&payload[..6]);
        let f = self.pad(ff);
        self.link.send_frame(self.tx_id, &f)?;

        // Wait for the first flow-control frame (honors WAIT/OVERFLOW).
        let mut fc = self.await_flow_control(timeout_ms)?;

        let mut sn: u8 = 1;
        let mut idx = 6;
        let mut cfs_in_block: u32 = 0;
        while idx < len {
            let end = (idx + 7).min(len);
            let mut cf = Vec::with_capacity(8);
            cf.push(pci::CONSECUTIVE | (sn & 0x0F));
            cf.extend_from_slice(&payload[idx..end]);
            let f = self.pad(cf);
            self.link.send_frame(self.tx_id, &f)?;
            sn = sn.wrapping_add(1) & 0x0F;
            idx = end;
            cfs_in_block += 1;

            if idx >= len {
                break;
            }
            // Honor the ECU's block size: after `block_size` CFs, wait for the
            // next flow-control frame before continuing (BS == 0 => no limit).
            if fc.block_size != 0 && cfs_in_block >= fc.block_size as u32 {
                fc = self.await_flow_control(timeout_ms)?;
                cfs_in_block = 0;
            } else {
                Self::sleep_st_min(fc.st_min);
            }
        }
        Ok(())
    }

    /// Receive a full ISO-TP payload (single or multi-frame, sending CTS and,
    /// when `block_size` is set, further flow-control frames per block).
    pub fn recv(&mut self, timeout_ms: u64) -> Result<Vec<u8>> {
        let first = self.recv_from_ecu(timeout_ms)?;
        match first[0] & pci::MASK {
            pci::SINGLE => {
                let len = (first[0] & 0x0F) as usize;
                if len == 0 {
                    return Err(Error::Protocol("single frame with zero length".into()));
                }
                if 1 + len > first.len() {
                    return Err(Error::Protocol("single frame length exceeds data".into()));
                }
                Ok(first[1..1 + len].to_vec())
            }
            pci::FIRST => {
                if first.len() < 8 {
                    return Err(Error::Protocol("truncated first frame".into()));
                }
                let len = (((first[0] & 0x0F) as usize) << 8) | first[1] as usize;
                if len <= 7 {
                    return Err(Error::Protocol(
                        "first frame used for a payload that fits a single frame".into(),
                    ));
                }
                let mut out = Vec::with_capacity(len);
                out.extend_from_slice(&first[2..8]);

                self.send_flow_control()?;

                // CF sequence numbers start at 1 after the FF and wrap 0xF→0x0.
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

    /// Send a Clear-To-Send flow-control frame advertising our BS/STmin.
    fn send_flow_control(&mut self) -> Result<()> {
        let fc = self.pad(vec![
            pci::FLOW_CONTROL | flow_status::CONTINUE,
            self.block_size,
            self.st_min,
        ]);
        self.link.send_frame(self.tx_id, &fc)
    }
}

impl DiagTransport for IsoTp {
    fn request(&mut self, req: &[u8], timeout_ms: u64) -> Result<Vec<u8>> {
        self.send(req, timeout_ms)?;
        // Handle responsePending (0x78) for this request's service: the ECU
        // asks us to keep waiting. A 0x78 echoing another service is returned
        // (the client reports it as mis-correlated), not waited on.
        for _ in 0..self.max_response_pending {
            let resp = self.recv(timeout_ms)?;
            if is_response_pending(req, &resp) {
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

    /// A scripted CAN endpoint: returns queued frames on recv, records sends
    /// into a shared log the test can inspect after the exchange.
    struct FakeEcu {
        to_tester: VecDeque<(u32, Vec<u8>)>,
        sent: Rc<RefCell<Vec<(u32, Vec<u8>)>>>,
        rx_id: u32,
    }

    impl FakeEcu {
        fn new(
            rx_id: u32,
            frames: Vec<(u32, Vec<u8>)>,
        ) -> (Self, Rc<RefCell<Vec<(u32, Vec<u8>)>>>) {
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

    impl CanLink for FakeEcu {
        fn send_frame(&mut self, id: u32, data: &[u8]) -> Result<()> {
            self.sent.borrow_mut().push((id, data.to_vec()));
            Ok(())
        }
        fn recv_frame(&mut self, _timeout_ms: u64) -> Result<(u32, Vec<u8>)> {
            self.to_tester
                .pop_front()
                .map(|(_, d)| (self.rx_id, d))
                .ok_or(Error::Timeout(0))
        }
    }

    /// The tester-transmitted frames' data payloads, in order.
    fn sent_frames(log: &Rc<RefCell<Vec<(u32, Vec<u8>)>>>) -> Vec<Vec<u8>> {
        log.borrow().iter().map(|(_, d)| d.clone()).collect()
    }

    #[test]
    fn response_pending_is_echo_checked() {
        // 0x78 for this RDBI is waited out ...
        let (ecu, _log) = FakeEcu::new(
            0x7E8,
            vec![
                (0x7E8, vec![0x03, 0x7F, 0x22, 0x78]),
                (0x7E8, vec![0x03, 0x62, 0x12, 0x34]),
            ],
        );
        let mut tp = IsoTp::new(Box::new(ecu), 0x7E0, 0x7E8);
        assert_eq!(
            tp.request(&[0x22, 0x12, 0x34], 100).unwrap(),
            vec![0x62, 0x12, 0x34]
        );
        // ... but a 0x78 for another service (0x10) is not treated as pending.
        let (ecu, _log) = FakeEcu::new(
            0x7E8,
            vec![
                (0x7E8, vec![0x03, 0x7F, 0x10, 0x78]),
                (0x7E8, vec![0x03, 0x62, 0x12, 0x34]),
            ],
        );
        let mut tp = IsoTp::new(Box::new(ecu), 0x7E0, 0x7E8);
        assert_eq!(
            tp.request(&[0x22, 0x12, 0x34], 100).unwrap(),
            vec![0x7F, 0x10, 0x78]
        );
    }

    #[test]
    fn single_frame_round_trip() {
        let (ecu, _log) = FakeEcu::new(0x7E8, vec![(0x7E8, vec![0x03, 0x62, 0x12, 0x34])]);
        let mut tp = IsoTp::new(Box::new(ecu), 0x7E0, 0x7E8);
        tp.send(&[0x22, 0x12, 0x34], 100).unwrap();
        let resp = tp.recv(100).unwrap();
        assert_eq!(resp, vec![0x62, 0x12, 0x34]);
    }

    #[test]
    fn multiframe_send_and_recv() {
        // ECU will: 1) send flow-control after our first frame,
        //           2) answer a 10-byte response as FF + one CF.
        let fc = (0x7E8, vec![0x30, 0x00, 0x00]);
        let resp_ff = (0x7E8, vec![0x10, 0x0A, 0, 1, 2, 3, 4, 5]);
        let resp_cf = (0x7E8, vec![0x21, 6, 7, 8, 9, 0xAA, 0xAA, 0xAA]);
        let (ecu, _log) = FakeEcu::new(0x7E8, vec![fc, resp_ff, resp_cf]);
        let mut tp = IsoTp::new(Box::new(ecu), 0x7E0, 0x7E8);

        // Send a 12-byte payload -> first frame + consecutive frame(s).
        let payload: Vec<u8> = (0..12).collect();
        tp.send(&payload, 100).unwrap();

        // Receive the 10-byte multi-frame response.
        let resp = tp.recv(100).unwrap();
        assert_eq!(resp, (0u8..10).collect::<Vec<u8>>());
    }

    #[test]
    fn stmin_decode_matches_spec() {
        assert_eq!(stmin_to_micros(0x00), 0);
        assert_eq!(stmin_to_micros(0x0A), 10_000); // 10 ms
        assert_eq!(stmin_to_micros(0x7F), 127_000); // 127 ms
        assert_eq!(stmin_to_micros(0xF1), 100); // 100 µs
        assert_eq!(stmin_to_micros(0xF9), 900); // 900 µs
        assert_eq!(stmin_to_micros(0xFF), 127_000); // reserved -> max
    }

    #[test]
    fn send_honors_block_size_and_waits_for_second_fc() {
        // 20-byte payload: FF (6 bytes) + 2 CFs (7 each). With BS=1 the ECU
        // sends one FC, then another FC after the first CF.
        let fc1 = (0x7E8, vec![0x30, 0x01, 0x00]); // BS=1, STmin=0
        let fc2 = (0x7E8, vec![0x30, 0x01, 0x00]);
        let (ecu, log) = FakeEcu::new(0x7E8, vec![fc1, fc2]);
        let mut tp = IsoTp::new(Box::new(ecu), 0x7E0, 0x7E8);
        let payload: Vec<u8> = (0..20).collect();
        tp.send(&payload, 100).unwrap();

        // 3 frames were sent by the tester: FF, CF#1, CF#2.
        let sent = sent_frames(&log);
        assert_eq!(sent.len(), 3);
        assert_eq!(sent[0][0] & pci::MASK, pci::FIRST);
        assert_eq!(sent[1][0], pci::CONSECUTIVE | 1);
        assert_eq!(sent[2][0], pci::CONSECUTIVE | 2);
    }

    #[test]
    fn send_overflow_flow_control_errors() {
        let fc = (0x7E8, vec![0x32, 0x00, 0x00]); // OVERFLOW
        let (ecu, _log) = FakeEcu::new(0x7E8, vec![fc]);
        let mut tp = IsoTp::new(Box::new(ecu), 0x7E0, 0x7E8);
        let payload: Vec<u8> = (0..12).collect();
        let err = tp.send(&payload, 100).unwrap_err();
        assert!(matches!(err, Error::Protocol(_)));
    }

    #[test]
    fn send_wait_flow_control_then_continue() {
        // ECU asks us to wait once (0x31) before clearing us to send.
        let wait = (0x7E8, vec![0x31, 0x00, 0x00]);
        let cts = (0x7E8, vec![0x30, 0x00, 0x00]);
        let (ecu, log) = FakeEcu::new(0x7E8, vec![wait, cts]);
        let mut tp = IsoTp::new(Box::new(ecu), 0x7E0, 0x7E8);
        let payload: Vec<u8> = (0..12).collect();
        tp.send(&payload, 100).unwrap();
        // FF + one CF for a 12-byte payload.
        assert_eq!(sent_frames(&log).len(), 2);
    }

    #[test]
    fn recv_sequence_number_wrap() {
        // A long response forces the CF sequence number past 0x0F and back to
        // 0x00. Length picked so the last CF carries SN == 0.
        // FF holds 6 bytes; each CF holds 7. After 15 CFs (SN 1..=0xF) we have
        // 6 + 15*7 = 111 bytes; a 16th CF (SN 0) is needed for anything longer.
        let total = 6 + 16 * 7; // 118 bytes -> 16 CFs, last SN wraps to 0
        let mut frames: Vec<(u32, Vec<u8>)> = Vec::new();
        let mut ff = vec![0x10 | ((total >> 8) & 0x0F) as u8, (total & 0xFF) as u8];
        ff.extend(0u8..6);
        frames.push((0x7E8, ff));
        let mut produced = 6usize;
        let mut sn = 1u8;
        while produced < total {
            let n = (total - produced).min(7);
            let mut cf = vec![pci::CONSECUTIVE | (sn & 0x0F)];
            cf.extend((0..n).map(|i| (produced + i) as u8));
            while cf.len() < 8 {
                cf.push(0xAA);
            }
            frames.push((0x7E8, cf));
            produced += n;
            sn = sn.wrapping_add(1) & 0x0F;
        }
        let (ecu, _log) = FakeEcu::new(0x7E8, frames);
        let mut tp = IsoTp::new(Box::new(ecu), 0x7E0, 0x7E8);
        let resp = tp.recv(100).unwrap();
        assert_eq!(resp.len(), total);
        let expected: Vec<u8> = (0..total).map(|i| i as u8).collect();
        assert_eq!(resp, expected);
    }

    #[test]
    fn recv_sequence_number_gap_is_error() {
        // FF announces 10 bytes but the CF uses SN 2 instead of 1.
        let ff = (0x7E8, vec![0x10, 0x0A, 0, 1, 2, 3, 4, 5]);
        let bad_cf = (0x7E8, vec![0x22, 6, 7, 8, 9, 0xAA, 0xAA, 0xAA]);
        let (ecu, _log) = FakeEcu::new(0x7E8, vec![ff, bad_cf]);
        let mut tp = IsoTp::new(Box::new(ecu), 0x7E0, 0x7E8);
        let err = tp.recv(100).unwrap_err();
        assert!(matches!(err, Error::Protocol(_)));
    }

    #[test]
    fn extended_29bit_ids_work() {
        let (ecu, log) = FakeEcu::new(0x18DAF110, vec![(0x18DAF110, vec![0x03, 0x62, 0x12, 0x34])]);
        let mut tp = IsoTp::new(Box::new(ecu), 0x18DA10F1, 0x18DAF110);
        tp.send(&[0x22, 0x12, 0x34], 100).unwrap();
        let resp = tp.recv(100).unwrap();
        assert_eq!(resp, vec![0x62, 0x12, 0x34]);
        // The tester sent on the full 29-bit id, not a masked 11-bit value.
        assert_eq!(log.borrow()[0].0, 0x18DA10F1);
    }
}
