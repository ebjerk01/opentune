//! Simulated Haldex units for tests and the CLI's `--sim` mode. Synthetic data
//! only: the identity, values and codes below are made up, not read from a
//! real unit.
use crate::gen4::{TP20_LOGICAL_ADDRESS, TP20_SETUP_ID, TP20_SETUP_RESPONSE_ID};
use crate::openhaldex::{Mode, DID_LOCK, DID_MODE, DID_SLIP};
use opentune_core::protocol::diag::DiagTransport;
use opentune_core::protocol::isotp::CanLink;
use opentune_core::{Error, RawDiag, Result};
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;

/// A simulated Gen4 controller speaking KWP2000 (the application layer that
/// rides on the TP2.0 channel). Records every request it saw.
#[derive(Debug, Clone)]
pub struct SimGen4 {
    /// Identification record returned after `5A 9B`.
    pub identity: Vec<u8>,
    /// Stored codes `(code, status)`.
    pub dtcs: Vec<(u16, u8)>,
    /// Measuring groups: group -> record after the echoed group.
    pub groups: BTreeMap<u8, Vec<u8>>,
    /// Requests seen, in order.
    pub seen: Vec<Vec<u8>>,
    session_open: bool,
}

impl Default for SimGen4 {
    fn default() -> Self {
        let mut identity = b"0AY907554C  9999".to_vec();
        identity.extend_from_slice(&[0; 10]);
        identity.extend_from_slice(b"Haldex SIMULATED    ");
        let mut groups = BTreeMap::new();
        // Group 1: oil 45 °C, plate 52 °C, supply 13.8 V (synthetic).
        groups.insert(1, vec![0x1A, 0, 45, 0x1A, 0, 52, 0x06, 100, 138]);
        // Group 3: 4.0 bar, 254 Nm, 21 % duty, 0.86 A (synthetic).
        groups.insert(
            3,
            vec![0x0E, 4, 200, 0x5E, 20, 255, 0x21, 100, 21, 0x18, 10, 86],
        );
        // Group 125: an unknown formula, reported raw.
        groups.insert(125, vec![0x25, 0x00, 0x01]);
        Self {
            identity,
            dtcs: Vec::new(),
            groups,
            seen: Vec::new(),
            session_open: false,
        }
    }
}

impl SimGen4 {
    /// Service ids of every request seen (for read-only assertions).
    pub fn services(&self) -> Vec<u8> {
        self.seen
            .iter()
            .filter_map(|r| r.first().copied())
            .collect()
    }

    fn answer(&mut self, req: &[u8]) -> Vec<u8> {
        let neg = |sid: u8, nrc: u8| vec![0x7F, sid, nrc];
        let Some(&sid) = req.first() else {
            return neg(0, 0x13);
        };
        match (sid, &req[1..]) {
            (0x10, [0x89]) => {
                self.session_open = true;
                vec![0x50, 0x89]
            }
            (0x10, _) => neg(sid, 0x12),
            _ if !self.session_open => neg(sid, 0x22),
            (0x1A, [0x9B]) => {
                let mut r = vec![0x5A, 0x9B];
                r.extend_from_slice(&self.identity);
                r
            }
            (0x18, [0x02, 0xFF, 0x00]) => {
                let mut r = vec![0x58, self.dtcs.len() as u8];
                for (c, s) in &self.dtcs {
                    r.extend_from_slice(&c.to_be_bytes());
                    r.push(*s);
                }
                r
            }
            (0x21, [g]) => match self.groups.get(g) {
                Some(rec) => {
                    let mut r = vec![0x61, *g];
                    r.extend_from_slice(rec);
                    r
                }
                None => neg(sid, 0x31),
            },
            (0x3E, _) => vec![0x7E],
            _ => neg(sid, 0x11),
        }
    }
}

impl DiagTransport for SimGen4 {
    fn request(&mut self, req: &[u8], _timeout_ms: u64) -> Result<Vec<u8>> {
        if req.is_empty() {
            return Err(Error::Protocol("empty request".into()));
        }
        self.seen.push(req.to_vec());
        Ok(self.answer(req))
    }
}

/// CAN id the simulated controller assigns the tester for its data channel
/// (as in the public capture).
pub const SIM_TESTER_TX_ID: u32 = 0x764;

/// The ECU side of a VW TP2.0 channel around a [`SimGen4`]: answers the
/// channel-setup broadcast for logical address `0x0A` only, the `A0`
/// parameter request and `A3` channel tests, ACKs the tester's blocks and
/// segments the KWP replies. Lets a real
/// [`VwTp20`](opentune_core::protocol::vwtp20::VwTp20) run end to end with no
/// hardware.
pub struct SimGen4Link {
    /// The simulated controller (shared, so a test can inspect it afterwards).
    pub unit: Rc<RefCell<SimGen4>>,
    /// Every CAN id the tester transmitted on, in order.
    pub tester_ids: Rc<RefCell<Vec<u32>>>,
    ecu_tx: u32,
    out: VecDeque<(u32, Vec<u8>)>,
    acc: Vec<u8>,
    ecu_seq: u8,
}

impl SimGen4Link {
    pub fn new(unit: SimGen4) -> Self {
        Self {
            unit: Rc::new(RefCell::new(unit)),
            tester_ids: Rc::new(RefCell::new(Vec::new())),
            ecu_tx: 0,
            out: VecDeque::new(),
            acc: Vec::new(),
            ecu_seq: 0,
        }
    }

    fn params(&self) -> Vec<u8> {
        // A1: block size 15, T1 100 ms, no separation time (tests never sleep).
        vec![0xA1, 0x0F, 0x8A, 0xFF, 0x00, 0xFF]
    }

    fn reply(&mut self, req: &[u8]) {
        let resp = self
            .unit
            .borrow_mut()
            .request(req, 0)
            .unwrap_or_else(|_| vec![0x7F, req.first().copied().unwrap_or(0), 0x10]);
        let mut telegram = vec![(resp.len() >> 8) as u8 & 0x0F, resp.len() as u8];
        telegram.extend_from_slice(&resp);
        let n = telegram.len().div_ceil(7);
        for (i, chunk) in telegram.chunks(7).enumerate() {
            let op = if i + 1 == n { 0x10 } else { 0x20 };
            let mut f = vec![op | self.ecu_seq];
            f.extend_from_slice(chunk);
            self.ecu_seq = (self.ecu_seq + 1) & 0x0F;
            self.out.push_back((self.ecu_tx, f));
        }
    }
}

impl CanLink for SimGen4Link {
    fn send_frame(&mut self, id: u32, data: &[u8]) -> Result<()> {
        self.tester_ids.borrow_mut().push(id);
        if id == TP20_SETUP_ID {
            // [dest, C0, 00, 10, rx_lo, rx_hi, app]; only address 0x0A answers.
            if data.len() >= 7 && data[0] == TP20_LOGICAL_ADDRESS && data[1] == 0xC0 {
                self.ecu_tx = data[4] as u32 | ((data[5] as u32 & 0x0F) << 8);
                let [lo, hi] = [
                    (SIM_TESTER_TX_ID & 0xFF) as u8,
                    ((SIM_TESTER_TX_ID >> 8) & 0x0F) as u8,
                ];
                self.out.push_back((
                    TP20_SETUP_RESPONSE_ID,
                    vec![0x00, 0xD0, data[4], data[5], lo, hi, data[6]],
                ));
            }
            return Ok(());
        }
        if id != SIM_TESTER_TX_ID || data.is_empty() {
            return Ok(());
        }
        match data[0] {
            0xA0 | 0xA3 => self.out.push_back((self.ecu_tx, self.params())),
            0xA8 => {}
            b if b & 0xF0 == 0xB0 => {} // the tester's ACK of our frames
            b => {
                let (kind, seq) = (b & 0xF0, b & 0x0F);
                self.acc.extend_from_slice(&data[1..]);
                if kind == 0x00 || kind == 0x10 {
                    self.out
                        .push_back((self.ecu_tx, vec![0xB0 | ((seq + 1) & 0x0F)]));
                }
                if kind == 0x10 && self.acc.len() >= 2 {
                    let len = ((self.acc[0] as usize) << 8 | self.acc[1] as usize) & 0x0FFF;
                    let req: Vec<u8> = self.acc.iter().skip(2).take(len).copied().collect();
                    self.acc.clear();
                    self.reply(&req);
                }
            }
        }
        Ok(())
    }

    fn recv_frame(&mut self, timeout_ms: u64) -> Result<(u32, Vec<u8>)> {
        self.out.pop_front().ok_or(Error::Timeout(timeout_ms))
    }
}

/// A simulated OpenHaldex answering its supplier DIDs on `0x70F`/`0x779`,
/// with the documented behaviour: a mode change is rejected (mode unchanged)
/// while the controller is disabled, and an index above 5 is out of range.
#[derive(Debug, Clone)]
pub struct SimOpenHaldex {
    pub mode: Mode,
    pub controller_enabled: bool,
    pub commanded_pct: u8,
    pub engagement_raw: u8,
    pub slip: [i8; 4],
    /// Requests seen.
    pub seen: Vec<Vec<u8>>,
}

impl Default for SimOpenHaldex {
    fn default() -> Self {
        Self {
            mode: Mode::Stock,
            controller_enabled: true,
            commanded_pct: 0,
            engagement_raw: 0x80,
            slip: [0, 1, -128, -128],
            seen: Vec::new(),
        }
    }
}

impl RawDiag for SimOpenHaldex {
    fn request(&mut self, req: &[u8]) -> std::result::Result<Vec<u8>, String> {
        self.seen.push(req.to_vec());
        let [h_lock, l_lock] = DID_LOCK.to_be_bytes();
        let [h_slip, l_slip] = DID_SLIP.to_be_bytes();
        let [h_mode, l_mode] = DID_MODE.to_be_bytes();
        Ok(match req {
            [0x22, h, l] if (*h, *l) == (h_lock, l_lock) => vec![
                0x62,
                *h,
                *l,
                self.commanded_pct,
                self.engagement_raw,
                self.mode.index(),
            ],
            [0x22, h, l] if (*h, *l) == (h_slip, l_slip) => {
                let mut r = vec![0x62, *h, *l];
                r.extend(self.slip.iter().map(|&s| s as u8));
                r
            }
            [0x2E, h, l, m] if (*h, *l) == (h_mode, l_mode) => match Mode::from_index(*m) {
                Some(mode) => {
                    if self.controller_enabled {
                        self.mode = mode;
                        self.commanded_pct = mode.nominal_lock_pct().unwrap_or(0);
                    }
                    vec![0x6E, *h, *l]
                }
                None => vec![0x7F, 0x2E, 0x31],
            },
            [sid, ..] => vec![0x7F, *sid, 0x31],
            [] => return Err("empty request".into()),
        })
    }
}
