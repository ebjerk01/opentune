//! VW Transport Protocol 2.0 (TP2.0): the VAG CAN diagnostic transport used by
//! older VW/Audi/Seat/Škoda ECUs (EDC15/EDC16, ME7, …) before ISO 15765-4 UDS.
//!
//! TP2.0 carries KWP2000 (ISO 14230) application data over CAN. Unlike ISO-TP it
//! is *connection oriented*: the tester first opens a logical channel with the
//! target ECU, the ECU hands back the pair of CAN identifiers to use, the two
//! sides negotiate timing/block-size parameters, and only then does segmented
//! data flow with per-block acknowledgements and an idle keep-alive.
//!
//! This is a clean synchronous reimplementation over a [`CanLink`], shaped like
//! [`IsoTp`](crate::protocol::isotp::IsoTp) so a
//! [`Kwp2000Client`](crate::protocol::kwp2000::Kwp2000Client) runs on top. The
//! wire behaviour (opcodes, the broadcast channel-setup handshake, the dynamic
//! CAN-id assignment, the `A0/A1` parameter exchange, the timing-byte encoding
//! and the data/ACK PCIs) was learned from the `ecu_diagnostics` crate
//! (`hardware/vwtp.rs`, GPL-3.0 — reuse-compatible with our GPL-3.0, but
//! reimplemented, not copied: that implementation is thread/`mpsc` based, this
//! one is a blocking request/response transport).
//!
//! ## Frame layout
//!
//! Byte 0 of every TP2.0 CAN frame is an opcode whose high nibble selects the
//! operation and whose low nibble (for data/ACK frames) is the sequence number:
//!
//! | High nibble | Meaning                                              |
//! |-------------|------------------------------------------------------|
//! | `0x0`       | data, more to follow, **ACK required now** (block end)|
//! | `0x1`       | data, **last** frame of the telegram, ACK required    |
//! | `0x2`       | data, more to follow, no ACK yet                      |
//! | `0xA`       | channel params: `A0` request / `A1` response / `A3` test / `A8` disconnect |
//! | `0xB`       | ACK — "block received, ready for next" (low nibble = next expected seq) |
//! | `0xC`       | broadcast channel-setup request (to CAN id `0x200`)   |
//! | `0xD`       | broadcast channel-setup response (positive = `0xD0`)  |
//!
//! The application telegram is `[len_hi, len_lo, payload…]` (length is the KWP
//! payload length, big-endian), split across the data frames' bytes `1..`.
use crate::protocol::diag::DiagTransport;
use crate::protocol::isotp::CanLink;
use crate::{Error, Result};

/// CAN id every TP2.0 channel-setup broadcast is sent to.
pub const BROADCAST_ID: u32 = 0x200;
/// Application protocol id for the KWP2000 diagnostic application.
pub const PROTOCOL_DIAGNOSTIC: u8 = 0x01;

mod op {
    pub const MASK: u8 = 0xF0;
    pub const DATA_ACK_NOW: u8 = 0x00; // more data, ACK required now
    pub const DATA_LAST: u8 = 0x10; // last data frame, ACK required
    pub const DATA_MORE: u8 = 0x20; // more data, no ACK
    pub const PARAM: u8 = 0xA0; // A0/A1/A3/A8 channel management
    pub const ACK: u8 = 0xB0; // block acknowledge
    pub const SETUP_RESP: u8 = 0xD0; // positive broadcast setup response
}

mod param {
    pub const SETUP_REQ: u8 = 0xC0; // byte1 of broadcast setup request
    pub const REQ: u8 = 0xA0; // our parameter request
    pub const RESP: u8 = 0xA1; // ECU parameter response
    pub const TEST: u8 = 0xA3; // channel test / keep-alive
    pub const DISCONNECT: u8 = 0xA8; // channel teardown
}

fn next_seq(seq: u8) -> u8 {
    (seq + 1) & 0x0F
}

/// Encode a duration (in microseconds) into a TP2.0 timing byte: the top two
/// bits select the unit (100 µs / 1 ms / 10 ms / 100 ms) and the low six bits
/// the count (0..63).
fn encode_timing(micros: u64) -> u8 {
    let (unit, value) = if micros < 6400 {
        (0u8, (micros / 100) as u8)
    } else if micros < 64_000 {
        (1u8, (micros / 1_000).clamp(1, 63) as u8)
    } else if micros < 640_000 {
        (2u8, (micros / 10_000).clamp(1, 63) as u8)
    } else {
        (3u8, (micros / 100_000).clamp(1, 63) as u8)
    };
    (unit << 6) | (value & 0x3F)
}

/// Decode a TP2.0 timing byte back into microseconds.
fn decode_timing(byte: u8) -> u64 {
    let value = (byte & 0x3F) as u64;
    match (byte >> 6) & 0x3 {
        0 => value * 100,
        1 => value * 1_000,
        2 => value * 10_000,
        _ => value * 100_000,
    }
}

/// VW TP2.0 transport over a [`CanLink`]. Built and used like
/// [`IsoTp`](crate::protocol::isotp::IsoTp) and implements [`DiagTransport`], so
/// a `Kwp2000Client` can drive it directly.
pub struct VwTp20 {
    link: Box<dyn CanLink>,
    /// Logical ECU address targeted by the channel-setup broadcast
    /// (0x01 = engine, 0x02 = gearbox, …).
    pub ecu_id: u8,
    /// Application protocol id placed in the broadcast (diagnostics = 0x01).
    pub app_protocol: u8,
    /// CAN id we ask the ECU to *send* responses on (chosen by the tester).
    pub rx_id: u32,
    /// CAN id we *send* to, assigned dynamically by the ECU during setup.
    pub tx_id: u32,
    /// Block size the ECU wants: ACK after this many data frames (0 = never
    /// until the last frame). Learned from the ECU's `A1` response.
    pub block_size: u8,
    /// ECU-requested inter-frame separation, as a raw timing byte.
    pub st_min: u8,
    /// Our outgoing sequence counter, continuous for the channel's life.
    tx_seq: u8,
    /// Whether the channel has been opened.
    opened: bool,
    /// How many response-pending (NRC 0x78) replies to tolerate.
    pub max_response_pending: u32,
}

impl VwTp20 {
    /// Create a TP2.0 transport. `ecu_id` is the logical target address and
    /// `rx_id` is the CAN id the tester asks the ECU to answer on. The channel
    /// is opened lazily on the first [`request`](DiagTransport::request), or
    /// eagerly via [`open`](Self::open).
    pub fn new(link: Box<dyn CanLink>, ecu_id: u8, rx_id: u32) -> Self {
        Self {
            link,
            ecu_id,
            app_protocol: PROTOCOL_DIAGNOSTIC,
            rx_id,
            tx_id: 0,
            block_size: 0x0F,
            st_min: 0,
            tx_seq: 0,
            opened: false,
            max_response_pending: 32,
        }
    }

    /// Open the channel: broadcast setup → dynamic id assignment → parameter
    /// handshake. Safe to call more than once (a no-op once open).
    pub fn open(&mut self, timeout_ms: u64) -> Result<()> {
        if self.opened {
            return Ok(());
        }
        self.setup_channel(timeout_ms)?;
        self.negotiate_params(timeout_ms)?;
        self.tx_seq = 0;
        self.opened = true;
        Ok(())
    }

    /// Step 1 — broadcast channel setup to CAN id 0x200 and read back the ECU's
    /// assigned transmit id.
    fn setup_channel(&mut self, timeout_ms: u64) -> Result<()> {
        // [ecu_id, 0xC0, 0x00, 0x10, rx_lo, rx_hi(&0x0F), app_protocol]
        let req = [
            self.ecu_id,
            param::SETUP_REQ,
            0x00,
            0x10,
            (self.rx_id & 0xFF) as u8,
            ((self.rx_id >> 8) & 0x0F) as u8,
            self.app_protocol,
        ];
        self.link.send_frame(BROADCAST_ID, &req)?;

        let resp_id = BROADCAST_ID + self.ecu_id as u32;
        let data = self.recv_on(resp_id, timeout_ms)?;
        if data.len() < 7 {
            return Err(Error::Protocol("short TP2.0 setup response".into()));
        }
        match data[1] {
            op::SETUP_RESP => {
                // tx id assigned by the ECU: data[4] low byte, data[5] low nibble high byte.
                self.tx_id = (((data[5] & 0x0F) as u32) << 8) | data[4] as u32;
                Ok(())
            }
            0xD6 => Err(Error::Protocol(
                "TP2.0 setup rejected: application not supported".into(),
            )),
            0xD7 => Err(Error::Protocol(
                "TP2.0 setup rejected: application temporarily unavailable".into(),
            )),
            0xD8 => Err(Error::Protocol(
                "TP2.0 setup rejected: no free resources".into(),
            )),
            other => Err(Error::Protocol(format!(
                "TP2.0 setup negative response 0x{other:02X}"
            ))),
        }
    }

    /// Step 2 — parameter handshake (`A0` → `A1`). We request our desired
    /// timing/block-size; the ECU answers with the parameters *we* must honor
    /// when transmitting, which we store.
    fn negotiate_params(&mut self, timeout_ms: u64) -> Result<()> {
        // [0xA0, BS, T1(ack timeout), 0xFF, T3(separation), 0xFF]
        let req = [
            param::REQ,
            self.block_size,
            encode_timing(100_000), // T1: 100 ms ACK timeout
            0xFF,
            self.st_min,
            0xFF,
        ];
        self.link.send_frame(self.tx_id, &req)?;

        let data = self.recv_on(self.tx_id_or_rx(), timeout_ms)?;
        if data.first() != Some(&param::RESP) || data.len() < 5 {
            return Err(Error::Protocol(
                "expected TP2.0 parameter response (A1)".into(),
            ));
        }
        // Honor the ECU's block size and separation time for our transmits.
        self.block_size = data[1];
        self.st_min = data[4];
        Ok(())
    }

    fn tx_id_or_rx(&self) -> u32 {
        self.rx_id
    }

    /// Send a channel-test / keep-alive (`A3`) and wait for the ECU's `A1`
    /// reply. Call within P3max while the channel is idle to keep it alive.
    pub fn keep_alive(&mut self, timeout_ms: u64) -> Result<()> {
        self.link.send_frame(self.tx_id, &[param::TEST])?;
        let data = self.recv_on(self.rx_id, timeout_ms)?;
        if data.first() == Some(&param::RESP) {
            if data.len() >= 5 {
                self.block_size = data[1];
                self.st_min = data[4];
            }
            Ok(())
        } else {
            Err(Error::Protocol("no TP2.0 keep-alive response".into()))
        }
    }

    /// Tear the channel down (`A8`). Best-effort; ignores link errors.
    pub fn disconnect(&mut self) {
        if self.opened {
            let _ = self.link.send_frame(self.tx_id, &[param::DISCONNECT]);
            self.opened = false;
        }
    }

    /// Receive the next frame whose CAN id equals `want`, skipping others.
    fn recv_on(&mut self, want: u32, timeout_ms: u64) -> Result<Vec<u8>> {
        loop {
            let (id, data) = self.link.recv_frame(timeout_ms)?;
            if id == want {
                if data.is_empty() {
                    return Err(Error::Protocol("empty TP2.0 frame".into()));
                }
                return Ok(data);
            }
        }
    }

    /// Receive the next *data or ACK* frame on `rx_id`, transparently answering
    /// a channel-test (`A3`) with an `A1`, absorbing stray `A1`s, and erroring
    /// on an ECU disconnect (`A8`).
    fn recv_channel_frame(&mut self, timeout_ms: u64) -> Result<Vec<u8>> {
        loop {
            let data = self.recv_on(self.rx_id, timeout_ms)?;
            match data[0] & op::MASK {
                op::DATA_ACK_NOW | op::DATA_LAST | op::DATA_MORE | op::ACK => return Ok(data),
                op::PARAM => match data[0] {
                    param::TEST => {
                        // ECU is testing the channel: answer with our params.
                        let reply = [
                            param::RESP,
                            self.block_size,
                            encode_timing(100_000),
                            0xFF,
                            self.st_min,
                            0xFF,
                        ];
                        self.link.send_frame(self.tx_id, &reply)?;
                    }
                    param::DISCONNECT => {
                        self.opened = false;
                        return Err(Error::Protocol("ECU closed the TP2.0 channel".into()));
                    }
                    // A1 or other management: ignore and keep reading.
                    _ => {}
                },
                _ => return Err(Error::Protocol("unexpected TP2.0 PCI".into())),
            }
        }
    }

    /// Wait for an ACK (`0xB_`) acknowledging a transmitted block.
    fn await_ack(&mut self, timeout_ms: u64) -> Result<()> {
        let data = self.recv_channel_frame(timeout_ms)?;
        if data[0] & op::MASK == op::ACK {
            Ok(())
        } else {
            Err(Error::Protocol("expected TP2.0 block ACK".into()))
        }
    }

    fn sleep_st_min(&self) {
        let micros = decode_timing(self.st_min);
        if micros > 0 {
            std::thread::sleep(std::time::Duration::from_micros(micros));
        }
    }

    /// Send one KWP application telegram as a sequence of TP2.0 data frames,
    /// honoring the negotiated block size (ACK every `block_size` frames) and
    /// separation time.
    fn send_telegram(&mut self, payload: &[u8], timeout_ms: u64) -> Result<()> {
        if payload.is_empty() {
            return Err(Error::Protocol("empty payload".into()));
        }
        if payload.len() > 0x0FFF {
            return Err(Error::Protocol("payload too large for TP2.0".into()));
        }
        // Application layer: 2-byte big-endian length prefix, then the payload.
        let mut telegram = Vec::with_capacity(payload.len() + 2);
        telegram.push((payload.len() >> 8) as u8);
        telegram.push((payload.len() & 0xFF) as u8);
        telegram.extend_from_slice(payload);

        let mut idx = 0usize;
        let mut frames_in_block: u32 = 0;
        while idx < telegram.len() {
            let end = (idx + 7).min(telegram.len());
            let is_last = end == telegram.len();
            let (op_byte, ack_required) = if is_last {
                (op::DATA_LAST | self.tx_seq, true)
            } else {
                frames_in_block += 1;
                if self.block_size != 0 && frames_in_block >= self.block_size as u32 {
                    frames_in_block = 0;
                    (op::DATA_ACK_NOW | self.tx_seq, true)
                } else {
                    (op::DATA_MORE | self.tx_seq, false)
                }
            };
            let mut frame = Vec::with_capacity(1 + (end - idx));
            frame.push(op_byte);
            frame.extend_from_slice(&telegram[idx..end]);
            self.link.send_frame(self.tx_id, &frame)?;
            self.tx_seq = next_seq(self.tx_seq);
            idx = end;

            if ack_required {
                self.await_ack(timeout_ms)?;
            } else {
                self.sleep_st_min();
            }
        }
        Ok(())
    }

    /// Receive one full KWP application telegram, ACKing blocks as required.
    fn recv_telegram(&mut self, timeout_ms: u64) -> Result<Vec<u8>> {
        let mut buf: Vec<u8> = Vec::new();
        let mut expected_seq: Option<u8> = None;
        loop {
            let frame = self.recv_channel_frame(timeout_ms)?;
            let kind = frame[0] & op::MASK;
            if kind == op::ACK {
                return Err(Error::Protocol("unexpected ACK while receiving".into()));
            }
            let seq = frame[0] & 0x0F;
            match expected_seq {
                None => expected_seq = Some(next_seq(seq)),
                Some(exp) => {
                    if seq != exp {
                        return Err(Error::Protocol(format!(
                            "TP2.0 sequence error: expected {exp}, got {seq}"
                        )));
                    }
                    expected_seq = Some(next_seq(seq));
                }
            }
            buf.extend_from_slice(&frame[1..]);

            match kind {
                op::DATA_MORE => {
                    // More data, no ACK; keep reading.
                }
                op::DATA_ACK_NOW => {
                    // Block boundary: ACK then keep reading.
                    self.link
                        .send_frame(self.tx_id, &[op::ACK | next_seq(seq)])?;
                }
                op::DATA_LAST => {
                    self.link
                        .send_frame(self.tx_id, &[op::ACK | next_seq(seq)])?;
                    break;
                }
                _ => return Err(Error::Protocol("unexpected TP2.0 data PCI".into())),
            }
        }
        decode_telegram(&buf)
    }
}

/// Strip and validate the 2-byte application length prefix.
fn decode_telegram(buf: &[u8]) -> Result<Vec<u8>> {
    if buf.len() < 2 {
        return Err(Error::Protocol("TP2.0 telegram too short".into()));
    }
    // Length is 12 bits wide in the VAG diagnostic application encoding.
    let declared = (((buf[0] as usize) << 8) | buf[1] as usize) & 0x0FFF;
    let available = buf.len() - 2;
    if available < declared {
        return Err(Error::Protocol(format!(
            "TP2.0 telegram truncated: declared {declared}, have {available}"
        )));
    }
    // Tolerate trailing padding from links that pad to DLC 8.
    Ok(buf[2..2 + declared].to_vec())
}

impl DiagTransport for VwTp20 {
    fn request(&mut self, req: &[u8], timeout_ms: u64) -> Result<Vec<u8>> {
        self.open(timeout_ms)?;
        self.send_telegram(req, timeout_ms)?;
        for _ in 0..self.max_response_pending {
            let resp = self.recv_telegram(timeout_ms)?;
            // KWP/UDS responsePending (NRC 0x78): keep waiting.
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

    /// Scripted CAN endpoint: returns queued `(id, data)` frames on recv with
    /// their real ids, and records every sent frame for inspection.
    struct FakeEcu {
        to_tester: VecDeque<(u32, Vec<u8>)>,
        sent: Rc<RefCell<Vec<(u32, Vec<u8>)>>>,
    }

    impl FakeEcu {
        fn new(frames: Vec<(u32, Vec<u8>)>) -> (Self, Rc<RefCell<Vec<(u32, Vec<u8>)>>>) {
            let sent = Rc::new(RefCell::new(Vec::new()));
            (
                Self {
                    to_tester: VecDeque::from(frames),
                    sent: sent.clone(),
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
            self.to_tester.pop_front().ok_or(Error::Timeout(0))
        }
    }

    const ECU: u8 = 0x01;
    const RX: u32 = 0x300; // id the ECU answers on (tester-chosen)
    const TX: u32 = 0x740; // id assigned back to the tester
    const SETUP_RESP_ID: u32 = BROADCAST_ID + ECU as u32; // 0x201

    // A positive broadcast setup response assigning TX (0x740) to the tester.
    fn setup_resp() -> (u32, Vec<u8>) {
        (
            SETUP_RESP_ID,
            vec![
                ECU,
                op::SETUP_RESP,
                0x00,
                0x10,
                (TX & 0xFF) as u8,
                ((TX >> 8) & 0x0F) as u8,
                PROTOCOL_DIAGNOSTIC,
            ],
        )
    }

    // ECU parameter response: BS=2, zero separation (so tests don't sleep).
    fn param_resp(bs: u8) -> (u32, Vec<u8>) {
        (RX, vec![param::RESP, bs, 0x00, 0xFF, 0x00, 0xFF])
    }

    #[test]
    fn timing_byte_roundtrips() {
        assert_eq!(decode_timing(encode_timing(100_000)), 100_000);
        assert_eq!(decode_timing(0x00), 0);
        // 1 ms encodes in the 1 ms unit.
        assert_eq!(decode_timing(encode_timing(1_000)), 1_000);
    }

    #[test]
    fn channel_setup_assigns_tx_id() {
        let (ecu, sent) = FakeEcu::new(vec![setup_resp(), param_resp(0x0F)]);
        let mut tp = VwTp20::new(Box::new(ecu), ECU, RX);
        tp.open(100).unwrap();
        assert_eq!(tp.tx_id, TX);
        // First frame went to the broadcast id and carried our rx id + proto.
        let log = sent.borrow();
        assert_eq!(log[0].0, BROADCAST_ID);
        assert_eq!(log[0].1[1], param::SETUP_REQ);
        assert_eq!(log[0].1[4], (RX & 0xFF) as u8);
        assert_eq!(log[0].1[6], PROTOCOL_DIAGNOSTIC);
        // Second frame was the A0 parameter request on the assigned tx id.
        assert_eq!(log[1].0, TX);
        assert_eq!(log[1].1[0], param::REQ);
    }

    #[test]
    fn single_frame_request_response() {
        // After setup/params: ECU ACKs our request, then answers [0x50,0x89].
        let ack = (RX, vec![op::ACK | 1]);
        // telegram [0x00,0x02,0x50,0x89] in one last-frame.
        let resp = (RX, vec![op::DATA_LAST, 0x00, 0x02, 0x50, 0x89]);
        let (ecu, sent) = FakeEcu::new(vec![setup_resp(), param_resp(0x0F), ack, resp]);
        let mut tp = VwTp20::new(Box::new(ecu), ECU, RX);

        let out = tp.request(&[0x10, 0x89], 100).unwrap();
        assert_eq!(out, vec![0x50, 0x89]);

        let log = sent.borrow();
        // Our request frame: last-frame PCI, seq 0, telegram [00 02 10 89].
        let req_frame = &log[2];
        assert_eq!(req_frame.0, TX);
        assert_eq!(req_frame.1, vec![op::DATA_LAST, 0x00, 0x02, 0x10, 0x89]);
        // We ACKed the ECU's last data frame (seq 0 -> ack next = 1).
        assert_eq!(log.last().unwrap().1, vec![op::ACK | 1]);
    }

    #[test]
    fn multiframe_send_honors_block_size() {
        // 20-byte payload -> telegram 22 bytes -> 4 frames (7,7,7,1). BS=2.
        // Expected tester PCIs: MORE|0, ACK_NOW|1, MORE|2, LAST|3.
        let ack_block = (RX, vec![op::ACK | 2]); // ack of frame seq 1
        let ack_last = (RX, vec![op::ACK | 4]); // ack of last frame seq 3
        let resp = (RX, vec![op::DATA_LAST, 0x00, 0x01, 0x7F]); // 1-byte reply
        let (ecu, sent) =
            FakeEcu::new(vec![setup_resp(), param_resp(2), ack_block, ack_last, resp]);
        let mut tp = VwTp20::new(Box::new(ecu), ECU, RX);

        let payload: Vec<u8> = (0..20).collect();
        let out = tp.request(&payload, 100).unwrap();
        assert_eq!(out, vec![0x7F]);

        let log = sent.borrow();
        // frames[2..6] are the four data frames.
        assert_eq!(log[2].1[0], op::DATA_MORE);
        assert_eq!(log[3].1[0], op::DATA_ACK_NOW | 1);
        assert_eq!(log[4].1[0], op::DATA_MORE | 2);
        assert_eq!(log[5].1[0], op::DATA_LAST | 3);
    }

    #[test]
    fn multiframe_receive_acks_blocks() {
        // ECU response spans three frames: MORE|0, ACK_NOW|1 (we ACK), LAST|2.
        // telegram declares 16 bytes.
        let ack = (RX, vec![op::ACK | 1]); // ack of our one-frame request
        let f0 = (RX, {
            let mut v = vec![op::DATA_MORE, 0x00, 0x10];
            v.extend(0u8..5); // 5 payload bytes
            v
        });
        let f1 = (RX, {
            let mut v = vec![op::DATA_ACK_NOW | 1];
            v.extend(5u8..12); // 7 payload bytes
            v
        });
        let f2 = (RX, {
            let mut v = vec![op::DATA_LAST | 2];
            v.extend(12u8..16); // 4 payload bytes -> total 16
            v
        });
        let (ecu, sent) = FakeEcu::new(vec![setup_resp(), param_resp(0x0F), ack, f0, f1, f2]);
        let mut tp = VwTp20::new(Box::new(ecu), ECU, RX);

        let out = tp.request(&[0x3E, 0x01], 100).unwrap();
        assert_eq!(out, (0u8..16).collect::<Vec<u8>>());

        // We ACKed the mid-stream block (seq1 -> next 2) and the last frame
        // (seq2 -> next 3).
        let log = sent.borrow();
        let acks: Vec<u8> = log
            .iter()
            .filter(|(_, d)| d.len() == 1 && d[0] & op::MASK == op::ACK)
            .map(|(_, d)| d[0])
            .collect();
        assert!(acks.contains(&(op::ACK | 2)));
        assert!(acks.contains(&(op::ACK | 3)));
    }

    #[test]
    fn setup_rejection_is_error() {
        let reject = (
            SETUP_RESP_ID,
            vec![ECU, 0xD6, 0x00, 0x10, 0x00, 0x00, PROTOCOL_DIAGNOSTIC],
        );
        let (ecu, _sent) = FakeEcu::new(vec![reject]);
        let mut tp = VwTp20::new(Box::new(ecu), ECU, RX);
        assert!(matches!(tp.open(100), Err(Error::Protocol(_))));
    }

    #[test]
    fn response_pending_is_retried() {
        let ack = (RX, vec![op::ACK | 1]);
        // First response is NRC 0x78 (pending), then the real answer.
        let pending = (RX, vec![op::DATA_LAST, 0x00, 0x03, 0x7F, 0x10, 0x78]);
        let real = (RX, vec![op::DATA_LAST | 1, 0x00, 0x02, 0x50, 0x01]);
        let (ecu, _sent) = FakeEcu::new(vec![setup_resp(), param_resp(0x0F), ack, pending, real]);
        let mut tp = VwTp20::new(Box::new(ecu), ECU, RX);
        let out = tp.request(&[0x10, 0x01], 100).unwrap();
        assert_eq!(out, vec![0x50, 0x01]);
    }
}
