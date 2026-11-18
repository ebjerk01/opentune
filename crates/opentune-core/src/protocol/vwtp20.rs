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
//! | `0x3`       | data, **last** frame, no ACK expected                 |
//! | `0x9`       | ACK — "block received, **not** ready for next" (a `0xB` follows) |
//! | `0xA`       | channel params: `A0` request / `A1` response / `A3` test / `A4` break / `A8` disconnect |
//! | `0xB`       | ACK — "block received, ready for next" (low nibble = next expected seq) |
//! | `0xC`       | broadcast channel-setup request (to CAN id `0x200`)   |
//! | `0xD`       | broadcast channel-setup response (positive = `0xD0`)  |
//!
//! The `0x3`/`0x9` opcodes and the `A3`/`A4`/`A8` frames follow the public
//! protocol description "VW Transport Protocol 2.0 (TP 2.0) for CAN bus"
//! (jazdw.net), which also documents the setup and parameter frames.
//!
//! The application telegram is `[len_hi, len_lo, payload…]` (length is the KWP
//! payload length, big-endian), split across the data frames' bytes `1..`.
//!
//! ## Recovery rules (see `docs/PROTOCOL-NOTES.md`, "TP2.0 hardening")
//!
//! * **Sequence numbers** run continuously per direction for the channel's
//!   life (4 bits, `0xF` wraps to `0x0`); the first data frame after a
//!   (re-)open synchronises the receive counter.
//! * **ACK loss on transmit.** After an ACK-requiring frame we wait the
//!   module's `T1` (from its `A1`, at least [`ACK_WAIT_FLOOR_MS`], at most the
//!   request timeout). Without the matching `0xB_` we send `A4` ("break:
//!   receiver discards all data since the last ACK") and re-send the whole
//!   block from its first frame with the same sequence numbers, at most
//!   [`VwTp20::max_block_retries`] times. A `0x9_` ("received, not ready")
//!   stops retransmission for that block. Module data arriving while we wait
//!   for the ACK of our *last* frame is taken as the implicit ACK.
//! * **Receive.** Frames with the expected sequence are accepted; an exact
//!   repeat of the previous frame and, right after a block we ACKed, a replay
//!   of that block (the module missed our ACK) are discarded (the block's ACK
//!   is re-sent). A gap (lost frame) stops acceptance until the module's `A4`,
//!   which rolls the telegram back to our last ACK; a gap in an
//!   unacknowledged (`0x3`) tail is an error.
//! * **After a failed exchange** the channel state is unknown (a late reply
//!   could be mistaken for the next one), so the channel is torn down (`A8`)
//!   and the next request sets up a new one. A module `A8` does the same.
//!   Either way the module's KWP session is gone: callers restart it.
//! * Every wait is bounded; no input makes this transport panic.
use crate::protocol::diag::DiagTransport;
use crate::protocol::isotp::CanLink;
use crate::{Error, Result};
use std::collections::VecDeque;

/// CAN id every TP2.0 channel-setup broadcast is sent to.
pub const BROADCAST_ID: u32 = 0x200;
/// Application protocol id for the KWP2000 diagnostic application.
pub const PROTOCOL_DIAGNOSTIC: u8 = 0x01;
/// Shortest wait for a block ACK before retransmitting, whatever the module's
/// `T1` says: USB CAN adapters add tens of ms of latency each way.
pub const ACK_WAIT_FLOOR_MS: u64 = 100;
/// Longest inter-frame separation we honour from a module's `T3`
/// (the timing byte can encode up to 6.3 s).
pub const MAX_SEPARATION_US: u64 = 100_000;
/// Largest KWP payload one TP2.0 telegram can carry (12-bit length).
pub const MAX_PAYLOAD: usize = 0x0FFF;
/// Module break (`A4`) resynchronisations tolerated within one telegram.
const MAX_RX_BREAKS: u32 = 8;

mod op {
    pub const MASK: u8 = 0xF0;
    pub const DATA_ACK_NOW: u8 = 0x00; // more data, ACK required now
    pub const DATA_LAST: u8 = 0x10; // last data frame, ACK required
    pub const DATA_MORE: u8 = 0x20; // more data, no ACK
    pub const DATA_LAST_NO_ACK: u8 = 0x30; // last data frame, no ACK expected
    pub const ACK_NOT_READY: u8 = 0x90; // block received, not ready for the next
    pub const PARAM: u8 = 0xA0; // A0/A1/A3/A4/A8 channel management
    pub const ACK: u8 = 0xB0; // block acknowledge
    pub const SETUP_RESP: u8 = 0xD0; // positive broadcast setup response
}

mod param {
    pub const SETUP_REQ: u8 = 0xC0; // byte1 of broadcast setup request
    pub const REQ: u8 = 0xA0; // our parameter request
    pub const RESP: u8 = 0xA1; // ECU parameter response
    pub const TEST: u8 = 0xA3; // channel test / keep-alive
    pub const BREAK: u8 = 0xA4; // receiver discards all data since the last ACK
    pub const DISCONNECT: u8 = 0xA8; // channel teardown
}

fn next_seq(seq: u8) -> u8 {
    (seq + 1) & 0x0F
}

fn is_data(b0: u8) -> bool {
    b0 & op::MASK <= op::DATA_LAST_NO_ACK
}

/// Encode a duration (in microseconds) into a TP2.0 timing byte: the top two
/// bits select the unit (100 µs / 1 ms / 10 ms / 100 ms) and the low six bits
/// the count (0..63).
pub fn encode_timing(micros: u64) -> u8 {
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
pub fn decode_timing(byte: u8) -> u64 {
    let value = (byte & 0x3F) as u64;
    match (byte >> 6) & 0x3 {
        0 => value * 100,
        1 => value * 1_000,
        2 => value * 10_000,
        _ => value * 100_000,
    }
}

/// Counters of the recovery paths a channel took (for tests and logs).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tp20Stats {
    /// Channel set-ups after the first (re-opens after a break).
    pub reopens: u32,
    /// Blocks we re-sent after an ACK timeout (each preceded by an `A4`).
    pub retransmissions: u32,
    /// `A4` breaks the module sent us.
    pub breaks_received: u32,
    /// Received data frames discarded as duplicates / replays.
    pub duplicates: u32,
    /// Sequence gaps seen on receive.
    pub gaps: u32,
    /// ACKs ignored because their sequence did not match.
    pub stray_acks: u32,
    /// Replies taken as the implicit ACK of our last frame.
    pub implicit_acks: u32,
    /// `responsePending` (NRC 0x78) replies waited through.
    pub response_pending: u32,
}

/// Receive-side sequence state, continuous across telegrams.
#[derive(Debug, Default)]
struct Rx {
    /// Next sequence expected from the module; `None` = sync on the next frame.
    expected: Option<u8>,
    /// `expected` at our last ACK (the rollback point of an `A4`).
    ack_seq: Option<u8>,
    /// First sequence of the block being received.
    block_start: Option<u8>,
    /// The last block we ACKed: first sequence and frame count.
    last_block: Option<(u8, u8)>,
    /// The ACK we sent for `last_block` (re-sent when the block is replayed).
    last_ack: Option<u8>,
    /// Nothing accepted since the last ACK / telegram end / `A4`.
    at_boundary: bool,
    /// The last accepted frame (bus-duplicate detection).
    last_frame: Vec<u8>,
}

impl Rx {
    fn reset(&mut self) {
        *self = Rx {
            at_boundary: true,
            ..Rx::default()
        };
    }

    /// Whether `seq` belongs to the block we last ACKed.
    fn in_last_block(&self, seq: u8) -> bool {
        match self.last_block {
            Some((start, n)) => (seq.wrapping_sub(start) & 0x0F) < n,
            None => false,
        }
    }

    /// A frame that is a duplicate (bus repeat) or a replay of the block we
    /// last ACKed; returns `Some(re-ACK)` for it (the ACK to re-send, if any).
    fn duplicate(&self, f: &[u8]) -> Option<Option<u8>> {
        if !self.last_frame.is_empty() && f == self.last_frame.as_slice() {
            return Some(None);
        }
        let seq = f[0] & 0x0F;
        if self.at_boundary && self.in_last_block(seq) {
            let (start, n) = self.last_block?;
            let end = (start + n - 1) & 0x0F;
            let kind = f[0] & op::MASK;
            let reack = (seq == end && (kind == op::DATA_ACK_NOW || kind == op::DATA_LAST))
                .then_some(self.last_ack)
                .flatten();
            return Some(reack);
        }
        None
    }
}

/// Outcome of waiting for a block ACK.
enum AckWait {
    Acked,
    /// No ACK within the window; carries what was seen instead (for the error).
    Missing(String),
}

/// VW TP2.0 transport over a [`CanLink`]. Built and used like
/// [`IsoTp`](crate::protocol::isotp::IsoTp) and implements [`DiagTransport`], so
/// a `Kwp2000Client` can drive it directly.
///
/// The link type defaults to `dyn CanLink`; a `VwTp20<dyn CanLink + Send>` is
/// `Send`, which is what [`Tp20Channel`](crate::protocol::tp20channel::Tp20Channel)
/// uses to keep the channel alive from a background thread.
pub struct VwTp20<L: CanLink + ?Sized = dyn CanLink> {
    link: Box<L>,
    /// Logical ECU address targeted by the channel-setup broadcast
    /// (0x01 = engine, 0x02 = gearbox, …).
    pub ecu_id: u8,
    /// Application protocol id placed in the broadcast (diagnostics = 0x01).
    pub app_protocol: u8,
    /// CAN id we ask the ECU to *send* responses on (chosen by the tester).
    pub rx_id: u32,
    /// CAN id we *send* to, assigned dynamically by the ECU during setup.
    pub tx_id: u32,
    /// Block size: before `open`, the one we *request* in our `A0` (the ECU
    /// should ACK-gate its transmissions to us every this many frames); after
    /// `open`, the one the ECU asked for in its `A1` (we wait for an ACK after
    /// this many frames, clamped to 15 so the 4-bit sequence stays
    /// unambiguous). 0 = only the last frame.
    pub block_size: u8,
    /// ECU-requested inter-frame separation (`T3`), as a raw timing byte.
    pub st_min: u8,
    /// ECU's ACK timeout (`T1`), as a raw timing byte: how long we wait for
    /// a block ACK before retransmitting (see [`ACK_WAIT_FLOOR_MS`]).
    pub t1: u8,
    /// Our outgoing sequence counter, continuous for the channel's life.
    tx_seq: u8,
    /// Whether the channel has been opened.
    opened: bool,
    /// How many response-pending (NRC 0x78) replies to tolerate per request.
    pub max_response_pending: u32,
    /// Wait for the next reply after a responsePending, ms (at least the
    /// request timeout). ISO 14230 / ISO 14229 use 5000 ms for this P2* wait.
    pub response_pending_timeout_ms: u64,
    /// Times an unacknowledged block is re-sent (after an `A4`) before the
    /// request fails. 0 = never retransmit.
    pub max_block_retries: u8,
    /// The block size we request in `A0`, kept across re-opens.
    requested_bs: Option<u8>,
    rx: Rx,
    /// Module frames that arrived while we waited for an ACK (implicit ACK).
    stash: VecDeque<Vec<u8>>,
    /// Last ACK value accepted from the module (duplicates are ignored).
    last_ack_in: Option<u8>,
    setups: u32,
    /// Why the channel was last torn down (module `A8`, failed exchange).
    last_break: Option<String>,
    /// Recovery counters.
    pub stats: Tp20Stats,
}

impl<L: CanLink + ?Sized> VwTp20<L> {
    /// Create a TP2.0 transport. `ecu_id` is the logical target address and
    /// `rx_id` is the CAN id the tester asks the ECU to answer on. The channel
    /// is opened lazily on the first [`request`](DiagTransport::request), or
    /// eagerly via [`open`](Self::open).
    pub fn new(link: Box<L>, ecu_id: u8, rx_id: u32) -> Self {
        Self {
            link,
            ecu_id,
            app_protocol: PROTOCOL_DIAGNOSTIC,
            rx_id,
            tx_id: 0,
            block_size: 0x0F,
            st_min: 0,
            t1: encode_timing(100_000),
            tx_seq: 0,
            opened: false,
            max_response_pending: 32,
            response_pending_timeout_ms: 5000,
            max_block_retries: 2,
            requested_bs: None,
            rx: Rx {
                at_boundary: true,
                ..Rx::default()
            },
            stash: VecDeque::new(),
            last_ack_in: None,
            setups: 0,
            last_break: None,
            stats: Tp20Stats::default(),
        }
    }

    /// Whether the channel is open (set up, parameters exchanged, not torn
    /// down by either side).
    pub fn is_open(&self) -> bool {
        self.opened
    }

    /// Why the channel was last torn down (module disconnect or a failed
    /// exchange), if it was.
    pub fn last_break(&self) -> Option<&str> {
        self.last_break.as_deref()
    }

    /// Open the channel: broadcast setup → dynamic id assignment → parameter
    /// handshake. Safe to call more than once (a no-op once open); after a
    /// break it sets up a fresh channel. Each step waits at most `timeout_ms`;
    /// a failure names the step and the CAN ids involved.
    pub fn open(&mut self, timeout_ms: u64) -> Result<()> {
        if self.opened {
            return Ok(());
        }
        let requested = *self.requested_bs.get_or_insert(self.block_size);
        self.block_size = requested;
        self.tx_seq = 0;
        self.rx.reset();
        self.stash.clear();
        self.last_ack_in = None;
        self.setup_channel(timeout_ms)?;
        self.negotiate_params(timeout_ms)?;
        if self.setups > 0 {
            self.stats.reopens += 1;
        }
        self.setups += 1;
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
        let deadline = Deadline::new(timeout_ms);
        let data = loop {
            match self.recv_on_until(resp_id, &deadline) {
                Ok(d) => break d,
                Err(Error::Timeout(_)) => {
                    return Err(Error::Protocol(format!(
                        "TP2.0 channel setup (step 1 of 2): logical address 0x{:02X} did not \
                         answer on CAN id 0x{resp_id:03X} within {timeout_ms} ms (setup request \
                         sent on 0x{BROADCAST_ID:03X}, asking the module to transmit on \
                         0x{:03X}). Check the module address, ignition, that the adapter sits on \
                         a bus the module is reachable from, and the bitrate",
                        self.ecu_id, self.rx_id
                    )))
                }
                Err(e) => return Err(e),
            }
        };
        if data.len() < 7 {
            return Err(Error::Protocol(format!(
                "TP2.0 channel setup: short response on 0x{resp_id:03X} ({} bytes)",
                data.len()
            )));
        }
        let why = match data[1] {
            op::SETUP_RESP => {
                // tx id assigned by the ECU: data[4] low byte, data[5] low nibble high byte.
                let tx = (((data[5] & 0x0F) as u32) << 8) | data[4] as u32;
                if tx == self.rx_id || (BROADCAST_ID..=BROADCAST_ID + 0xFF).contains(&tx) {
                    return Err(Error::Protocol(format!(
                        "TP2.0 channel setup: logical address 0x{:02X} assigned an unusable \
                         transmit id 0x{tx:03X}",
                        self.ecu_id
                    )));
                }
                self.tx_id = tx;
                return Ok(());
            }
            0xD6 => "application not supported".to_string(),
            0xD7 => "application temporarily unavailable".to_string(),
            0xD8 => "no free resources".to_string(),
            other => format!("negative response 0x{other:02X}"),
        };
        Err(Error::Protocol(format!(
            "TP2.0 setup rejected by logical address 0x{:02X} (on 0x{resp_id:03X}): {why}",
            self.ecu_id
        )))
    }

    /// Our `A0`/`A1` parameter frame body (block size, T1, T3).
    fn our_params(&self, op_byte: u8) -> [u8; 6] {
        [
            op_byte,
            self.requested_bs.unwrap_or(self.block_size),
            encode_timing(100_000), // T1: 100 ms ACK timeout
            0xFF,
            0x00, // T3: we need no separation between the module's frames
            0xFF,
        ]
    }

    /// Store the module's `A1` parameters (the ones we honour when sending).
    fn take_params(&mut self, data: &[u8]) {
        if data.len() >= 5 {
            self.block_size = data[1].min(0x0F);
            self.t1 = data[2];
            self.st_min = data[4];
        }
    }

    /// Step 2 — parameter handshake (`A0` → `A1`). We request our desired
    /// timing/block-size; the ECU answers with the parameters *we* must honor
    /// when transmitting, which we store.
    fn negotiate_params(&mut self, timeout_ms: u64) -> Result<()> {
        // [0xA0, BS, T1(ack timeout), 0xFF, T3(separation), 0xFF]
        let req = self.our_params(param::REQ);
        self.link.send_frame(self.tx_id, &req)?;

        // Skip anything else on our receive id (a stale ACK or disconnect
        // from an earlier channel) until the A1 arrives.
        let deadline = Deadline::new(timeout_ms);
        loop {
            let data = match self.recv_on_until(self.rx_id, &deadline) {
                Ok(d) => d,
                Err(Error::Timeout(_)) => {
                    return Err(Error::Protocol(format!(
                        "TP2.0 parameter exchange (step 2 of 2): logical address 0x{:02X} \
                         accepted the channel (tester transmits on 0x{:03X}) but sent no A1 \
                         parameter response on 0x{:03X} within {timeout_ms} ms",
                        self.ecu_id, self.tx_id, self.rx_id
                    )))
                }
                Err(e) => return Err(e),
            };
            if data[0] == param::RESP && data.len() >= 5 {
                self.take_params(&data);
                return Ok(());
            }
        }
    }

    /// Send a channel-test / keep-alive (`A3`) and wait for the ECU's `A1`
    /// reply. Call while the channel is idle to keep it alive. Replayed data
    /// from the module (it missed our last ACK) is re-ACKed meanwhile.
    pub fn keep_alive(&mut self, timeout_ms: u64) -> Result<()> {
        if !self.opened {
            return Err(Error::Protocol(format!(
                "TP2.0 keep-alive: the channel to logical address 0x{:02X} is not open",
                self.ecu_id
            )));
        }
        self.link.send_frame(self.tx_id, &[param::TEST])?;
        let deadline = Deadline::new(timeout_ms);
        loop {
            let data = match self.recv_on_until(self.rx_id, &deadline) {
                Ok(d) => d,
                Err(Error::Timeout(_)) => {
                    return Err(Error::Protocol(format!(
                        "no TP2.0 keep-alive response (A1) from logical address 0x{:02X} on \
                         0x{:03X} within {timeout_ms} ms",
                        self.ecu_id, self.rx_id
                    )))
                }
                Err(e) => return Err(e),
            };
            match data[0] {
                param::RESP => {
                    self.take_params(&data);
                    return Ok(());
                }
                param::DISCONNECT => return Err(self.module_disconnected()),
                b if b & op::MASK == op::PARAM => self.on_management(b)?,
                b if is_data(b) => {
                    self.absorb_idle_data(&data)?;
                }
                // A stale ACK or other channel frame: keep waiting for the A1.
                _ => {}
            }
        }
    }

    /// Tear the channel down (`A8`). Best-effort; ignores link errors.
    pub fn disconnect(&mut self) {
        if self.opened {
            let _ = self.link.send_frame(self.tx_id, &[param::DISCONNECT]);
            self.opened = false;
        }
    }

    /// The module sent `A8`: the channel is gone.
    fn module_disconnected(&mut self) -> Error {
        self.opened = false;
        let why = format!(
            "logical address 0x{:02X} closed the TP2.0 channel (A8); the next request sets \
             up a new channel and the module's diagnostic session must be restarted",
            self.ecu_id
        );
        self.last_break = Some(why.clone());
        Error::Protocol(why)
    }

    /// Channel-management frames other than `A1`/`A8` seen while waiting:
    /// answer a module channel test, note a module break.
    fn on_management(&mut self, b: u8) -> Result<()> {
        match b {
            param::TEST => {
                // ECU is testing the channel: answer with our params.
                let reply = self.our_params(param::RESP);
                self.link.send_frame(self.tx_id, &reply)
            }
            param::BREAK => {
                // Outside a telegram there is nothing to discard; the block
                // that follows replays the one we last ACKed.
                self.stats.breaks_received += 1;
                self.rx.at_boundary = true;
                self.rx.expected = self.rx.ack_seq.or(self.rx.expected);
                Ok(())
            }
            // A1 or other management: ignore.
            _ => Ok(()),
        }
    }

    /// A module data frame while no telegram is being received: a replay of
    /// the block we last ACKed is re-ACKed and dropped. Returns `true` when
    /// it was such a duplicate.
    fn absorb_idle_data(&mut self, f: &[u8]) -> Result<bool> {
        match self.rx.duplicate(f) {
            Some(reack) => {
                self.stats.duplicates += 1;
                if let Some(a) = reack {
                    self.link.send_frame(self.tx_id, &[op::ACK | a])?;
                }
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Receive the next frame whose CAN id equals `want`, skipping others.
    /// The whole wait is bounded by the deadline, also on a busy bus where
    /// unrelated frames keep arriving.
    fn recv_on_until(&mut self, want: u32, deadline: &Deadline) -> Result<Vec<u8>> {
        loop {
            let left = deadline
                .left_ms()
                .ok_or(Error::Timeout(deadline.total_ms))?;
            let (id, data) = match self.link.recv_frame(left) {
                Ok(f) => f,
                Err(Error::Timeout(_)) => return Err(Error::Timeout(deadline.total_ms)),
                Err(e) => return Err(e),
            };
            if id == want && !data.is_empty() {
                return Ok(data);
            }
        }
    }

    /// The next frame for the receive path: stashed frames first.
    fn next_rx(&mut self, deadline: &Deadline) -> Result<Vec<u8>> {
        if let Some(f) = self.stash.pop_front() {
            return Ok(f);
        }
        self.recv_on_until(self.rx_id, deadline)
    }

    /// How long to wait for a block ACK before retransmitting.
    fn ack_wait_ms(&self, timeout_ms: u64) -> u64 {
        (decode_timing(self.t1) / 1000)
            .max(ACK_WAIT_FLOOR_MS)
            .min(timeout_ms.max(1))
    }

    /// Wait for the ACK (`0xB<want>`) of a transmitted block. A
    /// "not ready" ACK (`0x9<want>`) means the receiver got the block but
    /// needs time: keep waiting (up to the full timeout) for the `0xB_`.
    fn await_ack(&mut self, want: u8, last: bool, timeout_ms: u64) -> Result<AckWait> {
        let mut deadline = Deadline::new(self.ack_wait_ms(timeout_ms));
        let mut received = false;
        let mut seen = String::from("nothing");
        loop {
            let data = match self.recv_on_until(self.rx_id, &deadline) {
                Ok(d) => d,
                Err(Error::Timeout(_)) if received => return Err(Error::Timeout(timeout_ms)),
                Err(Error::Timeout(_)) => return Ok(AckWait::Missing(seen)),
                Err(e) => return Err(e),
            };
            let b = data[0];
            match b & op::MASK {
                op::ACK | op::ACK_NOT_READY => {
                    let seq = b & 0x0F;
                    if seq != want {
                        // A duplicate of an earlier ACK, or a wrong one.
                        if Some(seq) != self.last_ack_in {
                            seen = format!("ACK 0x{b:02X}");
                        }
                        self.stats.stray_acks += 1;
                        continue;
                    }
                    if b & op::MASK == op::ACK {
                        self.last_ack_in = Some(want);
                        return Ok(AckWait::Acked);
                    }
                    if !received {
                        received = true;
                        deadline = Deadline::new(timeout_ms);
                    }
                }
                op::PARAM => match b {
                    param::DISCONNECT => return Err(self.module_disconnected()),
                    other => self.on_management(other)?,
                },
                k if k <= op::DATA_LAST_NO_ACK => {
                    if self.absorb_idle_data(&data)? {
                        continue;
                    }
                    if self.rx.expected.is_some_and(|e| e != b & 0x0F) {
                        // A stale frame of an earlier telegram: not ours to take.
                        self.stats.stray_acks += 1;
                        continue;
                    }
                    if last {
                        // The reply started: the module has our telegram.
                        self.stats.implicit_acks += 1;
                        self.last_ack_in = Some(want);
                        self.stash.push_back(data);
                        return Ok(AckWait::Acked);
                    }
                    return Err(Error::Protocol(format!(
                        "TP2.0: logical address 0x{:02X} sent data (0x{b:02X}) while we were \
                         still transmitting a telegram",
                        self.ecu_id
                    )));
                }
                _ => {}
            }
        }
    }

    fn sleep_st_min(&self) {
        let micros = decode_timing(self.st_min).min(MAX_SEPARATION_US);
        if micros > 0 {
            std::thread::sleep(std::time::Duration::from_micros(micros));
        }
    }

    /// Send one KWP application telegram as a sequence of TP2.0 data frames,
    /// honoring the negotiated block size (ACK every `block_size` frames) and
    /// separation time, retransmitting a block whose ACK does not arrive.
    fn send_telegram(&mut self, payload: &[u8], timeout_ms: u64) -> Result<()> {
        check_payload(payload)?;
        // Application layer: 2-byte big-endian length prefix, then the payload.
        let mut telegram = Vec::with_capacity(payload.len() + 2);
        telegram.push((payload.len() >> 8) as u8);
        telegram.push((payload.len() & 0xFF) as u8);
        telegram.extend_from_slice(payload);

        let bs = self.block_size.min(0x0F) as u32;
        let mut idx = 0usize;
        let mut frames_in_block: u32 = 0;
        let (mut block_idx, mut block_seq) = (0usize, self.tx_seq);
        let mut retries = 0u8;
        while idx < telegram.len() {
            let end = (idx + 7).min(telegram.len());
            let is_last = end == telegram.len();
            let (op_byte, ack_required) = if is_last {
                (op::DATA_LAST | self.tx_seq, true)
            } else {
                frames_in_block += 1;
                if bs != 0 && frames_in_block >= bs {
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

            if !ack_required {
                self.sleep_st_min();
                continue;
            }
            match self.await_ack(self.tx_seq, is_last, timeout_ms)? {
                AckWait::Acked => {
                    frames_in_block = 0;
                    retries = 0;
                    (block_idx, block_seq) = (idx, self.tx_seq);
                }
                AckWait::Missing(seen) => {
                    if retries >= self.max_block_retries {
                        return Err(Error::Protocol(format!(
                            "TP2.0: no ACK 0x{:02X} from logical address 0x{:02X} for the block \
                             ending with frame 0x{op_byte:02X} (saw {seen}) after {retries} \
                             retransmission(s)",
                            op::ACK | self.tx_seq,
                            self.ecu_id
                        )));
                    }
                    // A4: the module discards what it got since its last ACK;
                    // then the block again from its first frame.
                    retries += 1;
                    self.stats.retransmissions += 1;
                    self.link.send_frame(self.tx_id, &[param::BREAK])?;
                    (idx, self.tx_seq, frames_in_block) = (block_idx, block_seq, 0);
                }
            }
        }
        Ok(())
    }

    /// ACK a received block (`ack` = the next sequence we expect).
    fn ack_block(&mut self, ack: u8, buf_len: usize, acked_len: &mut usize) -> Result<()> {
        self.link.send_frame(self.tx_id, &[op::ACK | ack])?;
        let start = self.rx.block_start.take().unwrap_or(ack);
        let n = (ack.wrapping_sub(start) & 0x0F).max(1);
        self.rx.last_block = Some((start, n));
        self.rx.last_ack = Some(ack);
        self.rx.ack_seq = Some(ack);
        self.rx.at_boundary = true;
        *acked_len = buf_len;
        Ok(())
    }

    /// Receive one full KWP application telegram, ACKing blocks as required
    /// and recovering from lost/duplicated frames per the module docs.
    fn recv_telegram(&mut self, timeout_ms: u64) -> Result<Vec<u8>> {
        let mut buf: Vec<u8> = Vec::new();
        let mut acked_len = 0usize;
        let mut gap: Option<(u8, u8)> = None;
        let mut breaks = 0u32;
        self.rx.ack_seq = self.rx.expected;
        self.rx.block_start = None;
        let mut deadline = Deadline::new(timeout_ms);
        loop {
            let frame = match self.next_rx(&deadline) {
                Ok(f) => f,
                Err(Error::Timeout(t)) => {
                    return Err(match gap {
                        Some((exp, got)) => Error::Protocol(format!(
                            "TP2.0 sequence error: expected {exp}, got {got} (a frame from \
                             logical address 0x{:02X} was lost) and the module did not resend \
                             (A4) within {t} ms",
                            self.ecu_id
                        )),
                        None => Error::Timeout(t),
                    })
                }
                Err(e) => return Err(e),
            };
            let b = frame[0];
            let kind = b & op::MASK;
            if kind == op::PARAM {
                match b {
                    param::DISCONNECT => return Err(self.module_disconnected()),
                    param::BREAK => {
                        // Discard everything since our last ACK and resync.
                        breaks += 1;
                        self.stats.breaks_received += 1;
                        if breaks > MAX_RX_BREAKS {
                            return Err(Error::Protocol(format!(
                                "TP2.0: logical address 0x{:02X} broke its transmission (A4) \
                                 {breaks} times in one telegram",
                                self.ecu_id
                            )));
                        }
                        buf.truncate(acked_len);
                        self.rx.expected = self.rx.ack_seq;
                        self.rx.block_start = None;
                        self.rx.at_boundary = true;
                        self.rx.last_frame.clear();
                        gap = None;
                        deadline = Deadline::new(timeout_ms);
                    }
                    other => self.on_management(other)?,
                }
                continue;
            }
            if !is_data(b) {
                // A duplicated/stray ACK or an unknown opcode on our id.
                self.stats.stray_acks += 1;
                continue;
            }
            let seq = b & 0x0F;
            let expected = *self.rx.expected.get_or_insert(seq);
            if self.rx.ack_seq.is_none() {
                self.rx.ack_seq = Some(seq);
            }
            if gap.is_none() && seq == expected {
                // Accept.
                if buf.len() + frame.len() - 1 > MAX_PAYLOAD + 2 + 7 {
                    return Err(Error::Protocol(
                        "TP2.0 telegram longer than 4095 bytes".into(),
                    ));
                }
                buf.extend_from_slice(&frame[1..]);
                self.rx.block_start.get_or_insert(seq);
                self.rx.expected = Some(next_seq(seq));
                self.rx.at_boundary = false;
                self.rx.last_frame = frame.clone();
                deadline = Deadline::new(timeout_ms);
                match kind {
                    op::DATA_MORE => {}
                    op::DATA_ACK_NOW => self.ack_block(next_seq(seq), buf.len(), &mut acked_len)?,
                    op::DATA_LAST => {
                        self.ack_block(next_seq(seq), buf.len(), &mut acked_len)?;
                        break;
                    }
                    _ => {
                        // 0x3: last frame, no ACK. Nothing to replay later.
                        self.rx.block_start = None;
                        self.rx.last_block = None;
                        self.rx.last_ack = None;
                        self.rx.ack_seq = self.rx.expected;
                        self.rx.at_boundary = true;
                        break;
                    }
                }
                continue;
            }
            if let Some(reack) = self.rx.duplicate(&frame) {
                self.stats.duplicates += 1;
                if let Some(a) = reack {
                    self.link.send_frame(self.tx_id, &[op::ACK | a])?;
                }
                continue;
            }
            // A gap: a frame was lost. Accept nothing until the module's A4.
            if gap.is_none() {
                self.stats.gaps += 1;
                gap = Some((expected, seq));
            }
            if kind == op::DATA_LAST_NO_ACK {
                let (exp, got) = gap.unwrap_or((expected, seq));
                return Err(Error::Protocol(format!(
                    "TP2.0 sequence error: expected {exp}, got {got} in an unacknowledged \
                     telegram from logical address 0x{:02X} (a frame was lost)",
                    self.ecu_id
                )));
            }
        }
        decode_telegram(&buf)
    }

    /// One request/response exchange on an open channel, waiting through
    /// responsePending replies.
    fn exchange(&mut self, req: &[u8], timeout_ms: u64) -> Result<Vec<u8>> {
        self.send_telegram(req, timeout_ms)?;
        let mut wait = timeout_ms;
        let mut pending = 0u32;
        loop {
            let resp = self.recv_telegram(wait)?;
            // KWP/UDS responsePending (NRC 0x78) for *this* service: keep
            // waiting, with the extended P2* timeout. A 0x78 echoing another
            // service is returned for the client to flag as mis-correlated.
            if resp.len() >= 3 && resp[0] == 0x7F && resp[2] == 0x78 && resp[1] == req[0] {
                pending += 1;
                self.stats.response_pending += 1;
                if pending > self.max_response_pending {
                    return Err(Error::Protocol(format!(
                        "logical address 0x{:02X} answered service 0x{:02X} with \
                         responsePending (0x78) {pending} times without a final reply \
                         (limit {})",
                        self.ecu_id, req[0], self.max_response_pending
                    )));
                }
                wait = self.response_pending_timeout_ms.max(timeout_ms);
                continue;
            }
            return Ok(resp);
        }
    }
}

/// A wall-clock bound for a multi-frame wait.
struct Deadline {
    end: std::time::Instant,
    total_ms: u64,
}

impl Deadline {
    fn new(timeout_ms: u64) -> Self {
        Self {
            end: std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms),
            total_ms: timeout_ms,
        }
    }

    /// Milliseconds left (at least 1 while any time remains), or `None` once
    /// the deadline has passed.
    fn left_ms(&self) -> Option<u64> {
        let left = self.end.checked_duration_since(std::time::Instant::now())?;
        if left.is_zero() {
            None
        } else {
            Some((left.as_millis() as u64).max(1))
        }
    }
}

/// A request payload one telegram can carry.
fn check_payload(payload: &[u8]) -> Result<()> {
    if payload.is_empty() {
        return Err(Error::Protocol("empty payload".into()));
    }
    if payload.len() > MAX_PAYLOAD {
        return Err(Error::Protocol(format!(
            "payload of {} bytes too large for TP2.0 (max {MAX_PAYLOAD})",
            payload.len()
        )));
    }
    Ok(())
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
    if declared == 0 {
        return Err(Error::Protocol("TP2.0 telegram declares no payload".into()));
    }
    // Tolerate trailing padding from links that pad to DLC 8 (less than a frame).
    if available - declared >= 7 {
        return Err(Error::Protocol(format!(
            "TP2.0 telegram declares {declared} bytes but carries {available}"
        )));
    }
    Ok(buf[2..2 + declared].to_vec())
}

impl<L: CanLink + ?Sized> DiagTransport for VwTp20<L> {
    fn request(&mut self, req: &[u8], timeout_ms: u64) -> Result<Vec<u8>> {
        check_payload(req)?;
        self.open(timeout_ms)?;
        match self.exchange(req, timeout_ms) {
            Ok(r) => Ok(r),
            Err(e) => {
                // The channel state is unknown (a late reply could be taken
                // for the next one): tear it down; the next request re-opens.
                if self.opened {
                    self.last_break = Some(format!("exchange failed: {e}"));
                    self.disconnect();
                }
                self.stash.clear();
                Err(e)
            }
        }
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
