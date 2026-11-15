//! ISO 15765-2 (ISO-TP) transport over a raw CAN link, plus a [`DiagTransport`]
//! implementation on top of it.
use crate::protocol::diag::DiagTransport;
use crate::{Error, Result};

/// A raw CAN link: send/receive single CAN frames. Provided by a driver.
pub trait CanLink {
    fn send_frame(&mut self, id: u32, data: &[u8]) -> Result<()>;
    /// Receive one frame `(id, data)`, waiting up to `timeout_ms`.
    /// Returns [`Error::Timeout`] on expiry.
    fn recv_frame(&mut self, timeout_ms: u64) -> Result<(u32, Vec<u8>)>;
}

/// ISO-TP over an 11/29-bit CAN link. Owns the link so it can be used as a
/// [`DiagTransport`].
pub struct IsoTp {
    link: Box<dyn CanLink>,
    pub tx_id: u32,
    pub rx_id: u32,
    /// Pad every frame to 8 bytes with this byte, if set.
    pub padding: Option<u8>,
    pub block_size: u8,
    pub st_min: u8,
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

    fn recv_from_ecu(&mut self, timeout_ms: u64) -> Result<Vec<u8>> {
        loop {
            let (id, data) = self.link.recv_frame(timeout_ms)?;
            if id == self.rx_id {
                return Ok(data);
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
        ff.push(0x10 | ((len >> 8) & 0x0F) as u8);
        ff.push((len & 0xFF) as u8);
        ff.extend_from_slice(&payload[..6]);
        let f = self.pad(ff);
        self.link.send_frame(self.tx_id, &f)?;

        // Wait for a flow-control frame (CTS).
        let fc = self.recv_from_ecu(timeout_ms)?;
        if fc.first().map(|b| b & 0xF0) != Some(0x30) {
            return Err(Error::Protocol("expected flow control".into()));
        }

        let mut sn: u8 = 1;
        let mut idx = 6;
        while idx < len {
            let end = (idx + 7).min(len);
            let mut cf = Vec::with_capacity(8);
            cf.push(0x20 | (sn & 0x0F));
            cf.extend_from_slice(&payload[idx..end]);
            let f = self.pad(cf);
            self.link.send_frame(self.tx_id, &f)?;
            sn = sn.wrapping_add(1) & 0x0F;
            idx = end;
        }
        Ok(())
    }

    /// Receive a full ISO-TP payload (single or multi-frame, sending CTS).
    pub fn recv(&mut self, timeout_ms: u64) -> Result<Vec<u8>> {
        let first = self.recv_from_ecu(timeout_ms)?;
        if first.is_empty() {
            return Err(Error::Protocol("empty frame".into()));
        }
        match first[0] & 0xF0 {
            0x00 => {
                let len = (first[0] & 0x0F) as usize;
                let end = (1 + len).min(first.len());
                Ok(first[1..end].to_vec())
            }
            0x10 => {
                let len = (((first[0] & 0x0F) as usize) << 8) | first[1] as usize;
                let mut out = Vec::with_capacity(len);
                out.extend_from_slice(&first[2..]);
                let fc = self.pad(vec![0x30, self.block_size, self.st_min]);
                self.link.send_frame(self.tx_id, &fc)?;
                while out.len() < len {
                    let cf = self.recv_from_ecu(timeout_ms)?;
                    if cf.first().map(|b| b & 0xF0) != Some(0x20) {
                        return Err(Error::Protocol("expected consecutive frame".into()));
                    }
                    out.extend_from_slice(&cf[1..]);
                }
                out.truncate(len);
                Ok(out)
            }
            _ => Err(Error::Protocol("unexpected ISO-TP PCI".into())),
        }
    }
}

impl DiagTransport for IsoTp {
    fn request(&mut self, req: &[u8], timeout_ms: u64) -> Result<Vec<u8>> {
        self.send(req, timeout_ms)?;
        // Handle responsePending (0x78): the ECU asks us to keep waiting.
        for _ in 0..32 {
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
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// A scripted CAN endpoint: returns queued frames on recv, records sends.
    struct FakeEcu {
        to_tester: VecDeque<(u32, Vec<u8>)>,
        from_tester: Vec<(u32, Vec<u8>)>,
        rx_id: u32,
    }

    impl CanLink for FakeEcu {
        fn send_frame(&mut self, id: u32, data: &[u8]) -> Result<()> {
            self.from_tester.push((id, data.to_vec()));
            Ok(())
        }
        fn recv_frame(&mut self, _timeout_ms: u64) -> Result<(u32, Vec<u8>)> {
            self.to_tester
                .pop_front()
                .map(|(_, d)| (self.rx_id, d))
                .ok_or(Error::Timeout(0))
        }
    }

    #[test]
    fn single_frame_round_trip() {
        let ecu = FakeEcu {
            to_tester: VecDeque::from(vec![(0x7E8, vec![0x03, 0x62, 0x12, 0x34])]),
            from_tester: Vec::new(),
            rx_id: 0x7E8,
        };
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
        let ecu = FakeEcu {
            to_tester: VecDeque::from(vec![fc, resp_ff, resp_cf]),
            from_tester: Vec::new(),
            rx_id: 0x7E8,
        };
        let mut tp = IsoTp::new(Box::new(ecu), 0x7E0, 0x7E8);

        // Send a 12-byte payload -> first frame + consecutive frame(s).
        let payload: Vec<u8> = (0..12).collect();
        tp.send(&payload, 100).unwrap();

        // Receive the 10-byte multi-frame response.
        let resp = tp.recv(100).unwrap();
        assert_eq!(resp, (0u8..10).collect::<Vec<u8>>());
    }
}
