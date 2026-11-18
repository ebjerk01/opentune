//! A scripted DQ250 02E responder for tests and the CLI's `--sim` mode: the
//! KWP2000 application behind a TP2.0 channel. **Synthetic only**: the part
//! number, texts, codes and cell values below are made up (no real unit's
//! identity) and the groups carry no meaning.
use crate::{TP20_LOGICAL_ADDRESS, TP20_TESTER_RX_ID};
use opentune_core::protocol::tp20channel::{Tp20Channel, Tp20Options};
use opentune_core::protocol::tp20sim::{Tp20Ecu, Tp20EcuConfig, Tp20SimLink};
use opentune_core::Result;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// CAN id the simulated unit assigns the tester for its data channel.
pub const SIM_TESTER_TX_ID: u32 = 0x740;

/// The simulated unit. Records every request it saw.
#[derive(Debug, Clone)]
pub struct SimDq250 {
    /// Identification records by option (the bytes after `5A <option>`);
    /// an option not in the map is refused with NRC 0x12.
    pub ids: BTreeMap<u8, Vec<u8>>,
    /// Stored codes `(code, status)`.
    pub dtcs: Vec<(u16, u8)>,
    /// Measuring groups: group -> record after the echoed group; others are
    /// refused with NRC 0x31.
    pub groups: BTreeMap<u8, Vec<u8>>,
    /// Session byte accepted by `0x10`; `None` refuses every session.
    pub session: Option<u8>,
    /// Refuse reads until a session is open (NRC 0x22).
    pub require_session: bool,
    /// Requests seen, in order.
    pub seen: Vec<Vec<u8>>,
    /// Whether a session has been opened.
    pub session_open: bool,
}

/// The synthetic `0x9B` record: part number, version, 10 bytes, text.
pub fn synthetic_vag_record() -> Vec<u8> {
    let mut r = b"02E000000X  0000".to_vec();
    r.extend_from_slice(&[0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09]);
    r.extend_from_slice(b"SIMULATED DSG       ");
    r
}

impl Default for SimDq250 {
    fn default() -> Self {
        let mut ids = BTreeMap::new();
        ids.insert(0x9B, synthetic_vag_record());
        ids.insert(0x91, b"02E000000Y  ".to_vec());
        // 0x97 deliberately absent: identification must tolerate a refusal.
        let mut groups = BTreeMap::new();
        // Arbitrary cells: known formulas and an unknown one, no meaning.
        groups.insert(
            1,
            vec![0x1A, 0, 60, 0x0E, 2, 150, 0x21, 100, 33, 0x25, 0x12, 0x34],
        );
        groups.insert(2, vec![0x06, 100, 140, 0x77, 0, 0, 0x18, 10, 50]);
        Self {
            ids,
            dtcs: Vec::new(),
            groups,
            session: Some(crate::KWP_SESSION),
            require_session: true,
            seen: Vec::new(),
            session_open: false,
        }
    }
}

impl SimDq250 {
    /// Service ids of every request seen.
    pub fn services(&self) -> Vec<u8> {
        self.seen
            .iter()
            .filter_map(|r| r.first().copied())
            .collect()
    }

    /// Answer one KWP2000 request telegram.
    pub fn answer(&mut self, req: &[u8]) -> Vec<u8> {
        self.seen.push(req.to_vec());
        let neg = |sid: u8, nrc: u8| vec![0x7F, sid, nrc];
        let Some(&sid) = req.first() else {
            return neg(0, 0x13);
        };
        match (sid, &req[1..]) {
            (0x10, [s]) if Some(*s) == self.session => {
                self.session_open = true;
                vec![0x50, *s]
            }
            (0x10, _) => neg(sid, 0x12),
            _ if self.require_session && !self.session_open => neg(sid, 0x22),
            (0x1A, [opt]) => match self.ids.get(opt) {
                Some(rec) => {
                    let mut r = vec![0x5A, *opt];
                    r.extend_from_slice(rec);
                    r
                }
                None => neg(sid, 0x12),
            },
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
            _ => neg(sid, 0x11),
        }
    }
}

/// The ECU side of a TP2.0 channel at the gearbox address around `unit`
/// (the core [`Tp20Ecu`] simulator, configured by `cfg` apart from the
/// address and assigned id).
pub fn sim_bus(unit: Arc<Mutex<SimDq250>>, cfg: Tp20EcuConfig) -> Arc<Mutex<Tp20Ecu>> {
    Tp20Ecu::new(
        Tp20EcuConfig {
            addr: TP20_LOGICAL_ADDRESS,
            tester_tx_id: SIM_TESTER_TX_ID,
            ..cfg
        },
        Box::new(move |req| unit.lock().unwrap_or_else(|e| e.into_inner()).answer(req)),
    )
    .shared()
}

/// Open the core [`Tp20Channel`] (setup, parameters, background keep-alive)
/// to a simulated bus.
pub fn open_sim_channel(ecu: Arc<Mutex<Tp20Ecu>>, keepalive_ms: u64) -> Result<Tp20Channel> {
    let mut o = Tp20Options::new(TP20_LOGICAL_ADDRESS);
    o.rx_id = TP20_TESTER_RX_ID;
    o.keepalive_ms = keepalive_ms;
    o.timeout_ms = 200;
    Tp20Channel::open(Box::new(Tp20SimLink::new(ecu)), &o)
}
