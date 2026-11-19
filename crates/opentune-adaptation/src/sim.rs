//! A **synthetic** VAG-style KWP2000 module for tests and offline demos: the
//! KWP application behind a TP2.0 channel (or a K-line). It answers the
//! services of the [`kwp`](crate::kwp) path with made-up values — no real
//! module's data, identity or login.
//!
//! Its SecurityAccess uses a **test-only** transform ([`SimSeedKey`], id
//! `kwpsim-test`: key = seed XOR [`SIM_KEY_MASK`]). It unlocks this simulator
//! and nothing else; it is not, and does not resemble, a manufacturer
//! algorithm.
//!
//! The module's KWP session lives with its TP2.0 channel:
//! [`open_sim_channel`] puts a link in front of the core simulator that resets
//! the session (and security, adaptation and basic-settings state) whenever
//! the tester sets up a channel, so a re-opened channel starts unauthenticated
//! exactly like a real module.
use opentune_core::protocol::isotp::CanLink;
use opentune_core::protocol::seedkey::SeedKey;
use opentune_core::protocol::tp20channel::{Tp20Channel, Tp20Options};
use opentune_core::protocol::tp20sim::{Tp20Ecu, Tp20EcuConfig, Tp20SimLink};
use opentune_core::protocol::vwtp20::BROADCAST_ID;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

/// TP2.0 logical address the simulated module answers (the engine, `0x01`).
pub const SIM_TP20_ADDR: u8 = 0x01;
/// CAN id the simulated module assigns the tester.
pub const SIM_TESTER_TX_ID: u32 = 0x740;
/// CAN id the tester asks the module to transmit on.
pub const SIM_TESTER_RX_ID: u32 = 0x300;
/// The test-only key mask of [`SimSeedKey`].
pub const SIM_KEY_MASK: [u8; 4] = [0x5A, 0xC3, 0x3C, 0xA5];
/// Id of [`SimSeedKey`].
pub const SIM_SEED_KEY_ID: &str = "kwpsim-test";

/// The simulator's test-only seed/key transform (see the module docs).
#[derive(Debug, Clone, Copy, Default)]
pub struct SimSeedKey;

impl SeedKey for SimSeedKey {
    fn id(&self) -> &str {
        SIM_SEED_KEY_ID
    }
    fn compute(&self, seed: &[u8], _level: u8) -> opentune_core::Result<Vec<u8>> {
        Ok(sim_key(seed))
    }
}

/// [`SimSeedKey`] for `id == "kwpsim-test"`, else `None` (a resolver).
pub fn test_seed_key(id: &str) -> Option<Box<dyn SeedKey>> {
    (id.trim() == SIM_SEED_KEY_ID).then(|| Box::new(SimSeedKey) as Box<dyn SeedKey>)
}

fn sim_key(seed: &[u8]) -> Vec<u8> {
    seed.iter()
        .zip(SIM_KEY_MASK.iter().cycle())
        .map(|(s, m)| s ^ m)
        .collect()
}

/// The simulated module. Records every request it answered.
#[derive(Debug, Clone)]
pub struct SimKwpModule {
    /// Session accepted by `0x10`.
    pub session: u8,
    /// requestSeed level (`level + 1` sends the key).
    pub security_level: u8,
    /// Seed handed out.
    pub seed: [u8; 4],
    /// Measuring groups (record after the echoed group); others: NRC 0x31.
    pub groups: BTreeMap<u8, Vec<u8>>,
    /// Basic-settings groups (cells after the status byte).
    pub basic_settings: BTreeMap<u8, Vec<u8>>,
    /// Stored adaptation values; channels not here: NRC 0x31.
    pub channels: BTreeMap<u8, u16>,
    /// The channel select (`B9 01 03 <ch>`) needs SecurityAccess too.
    pub select_needs_security: bool,
    /// Refuse every save with this NRC (e.g. 0x22).
    pub refuse_save: Option<u8>,
    /// Store this instead of the requested value on save (a verify failure).
    pub corrupt_save: Option<u16>,
    /// Requests answered, in order.
    pub seen: Vec<Vec<u8>>,
    /// Session resets (one per channel setup).
    pub resets: usize,
    /// Session state.
    pub session_open: bool,
    /// SecurityAccess state.
    pub unlocked: bool,
    /// Adaptation mode entered.
    pub adaptation_mode: bool,
    /// Selected adaptation channel.
    pub selected: Option<u8>,
    /// Test value applied (not stored).
    pub test_value: Option<u16>,
    /// Basic settings running for this group.
    pub basic_active: Option<u8>,
    /// Seed sent and awaiting a key.
    pub seed_sent: bool,
    /// Wrong keys in a row (three lock the module: NRC 0x36).
    pub bad_keys: u8,
}

impl Default for SimKwpModule {
    fn default() -> Self {
        SimKwpModule {
            session: 0x89,
            security_level: 0x03,
            seed: [0x12, 0x34, 0x56, 0x78],
            // Synthetic cells: formula 0x1A (°C: b - a) and 0x21 (%), and an
            // unknown formula 0x77 that stays raw.
            groups: BTreeMap::from([
                (
                    1,
                    vec![0x1A, 0x00, 0x5A, 0x21, 0x64, 0x32, 0x77, 0x01, 0x02],
                ),
                (2, vec![0x1A, 0x00, 0x14]),
            ]),
            basic_settings: BTreeMap::from([(60, vec![0x21, 0x64, 0x0A, 0x1A, 0x00, 0x50])]),
            channels: BTreeMap::from([(1, 0x0200), (5, 1000)]),
            select_needs_security: false,
            refuse_save: None,
            corrupt_save: None,
            seen: Vec::new(),
            resets: 0,
            session_open: false,
            unlocked: false,
            adaptation_mode: false,
            selected: None,
            test_value: None,
            basic_active: None,
            seed_sent: false,
            bad_keys: 0,
        }
    }
}

impl SimKwpModule {
    /// Service ids of every request answered.
    pub fn services(&self) -> Vec<u8> {
        self.seen
            .iter()
            .filter_map(|r| r.first().copied())
            .collect()
    }

    /// Forget the session (what a TP2.0 channel teardown does).
    pub fn reset_session(&mut self) {
        self.resets += 1;
        self.clear_state();
    }

    fn clear_state(&mut self) {
        self.session_open = false;
        self.unlocked = false;
        self.adaptation_mode = false;
        self.selected = None;
        self.test_value = None;
        self.basic_active = None;
        self.seed_sent = false;
    }

    /// Answer one KWP2000 request telegram.
    pub fn answer(&mut self, req: &[u8]) -> Vec<u8> {
        self.seen.push(req.to_vec());
        let neg = |sid: u8, nrc: u8| vec![0x7F, sid, nrc];
        let Some((&sid, args)) = req.split_first() else {
            return neg(0x00, 0x13);
        };
        if sid == 0x10 {
            return match args {
                [s] if *s == self.session => {
                    // A new session starts locked, outside any mode.
                    self.clear_state();
                    self.session_open = true;
                    vec![0x50, *s]
                }
                _ => neg(sid, 0x12),
            };
        }
        if !self.session_open {
            return neg(sid, 0x80);
        }
        let lvl = self.security_level;
        match (sid, args) {
            (0x21, [g]) => match self.groups.get(g) {
                Some(rec) => [&[0x61, *g][..], rec.as_slice()].concat(),
                None => neg(sid, 0x31),
            },
            (0x27, [l]) if *l == lvl => {
                self.seed_sent = true;
                if self.unlocked {
                    vec![0x67, lvl, 0, 0, 0, 0]
                } else {
                    [&[0x67, lvl][..], &self.seed[..]].concat()
                }
            }
            (0x27, [l, key @ ..]) if *l == lvl.wrapping_add(1) && !key.is_empty() => {
                if !self.seed_sent {
                    return neg(sid, 0x22);
                }
                self.seed_sent = false;
                if self.bad_keys >= 3 {
                    return neg(sid, 0x36);
                }
                if key == sim_key(&self.seed).as_slice() {
                    self.unlocked = true;
                    self.bad_keys = 0;
                    vec![0x67, *l, 0x34]
                } else {
                    self.bad_keys += 1;
                    neg(sid, 0x35)
                }
            }
            (0x27, _) => neg(sid, 0x12),
            (0x31, [0xB8, 0x00, 0x00]) => vec![0x71, 0xB8, 0x00, 0x00],
            (0x31, [0xB8, 0x01, 0x03]) => {
                self.adaptation_mode = true;
                self.selected = None;
                self.test_value = None;
                vec![0x71, 0xB8, 0x01, 0x03]
            }
            (0x31, [0xB8, 0x00, g]) => {
                if !self.unlocked {
                    return neg(sid, 0x33);
                }
                if !self.basic_settings.contains_key(g) {
                    return neg(sid, 0x31);
                }
                self.basic_active = Some(*g);
                vec![0x71, 0xB8, 0x00, *g]
            }
            (0x31, [0xB9, 0x01, 0x03, c]) => {
                if !self.adaptation_mode {
                    return neg(sid, 0x22);
                }
                if self.select_needs_security && !self.unlocked {
                    return neg(sid, 0x33);
                }
                if !self.channels.contains_key(c) {
                    return neg(sid, 0x31);
                }
                self.selected = Some(*c);
                self.test_value = None;
                vec![0x71, 0xB9, 0x01, 0x03]
            }
            (0x31, [0xB9, 0x01, 0x03, hi, lo]) => {
                if self.selected.is_none() {
                    return neg(sid, 0x22);
                }
                if !self.unlocked {
                    return neg(sid, 0x33);
                }
                self.test_value = Some(u16::from_be_bytes([*hi, *lo]));
                vec![0x71, 0xB9, 0x01, 0x03]
            }
            (0x31, [0xBA, 0x01, 0x03]) => {
                let Some(c) = self.selected else {
                    return neg(sid, 0x22);
                };
                let v = self
                    .test_value
                    .unwrap_or_else(|| self.channels.get(&c).copied().unwrap_or(0));
                let [hi, lo] = v.to_be_bytes();
                // status, value format, value, measured cell (0x1A: 30 °C)
                vec![
                    0x71, 0xBA, 0x01, 0x03, 0x00, 0x03, hi, lo, 0x04, 0x1A, 0x00, 0x1E,
                ]
            }
            (0x31, [0xBA, 0x00, g]) => match (self.basic_active, self.basic_settings.get(g)) {
                (Some(a), Some(cells)) if a == *g => {
                    [&[0x71, 0xBA, 0x00, *g, 0x00][..], cells.as_slice()].concat()
                }
                _ => neg(sid, 0x22),
            },
            (0x31, [0xBB, 0x01, 0x03, hi, lo, wsc @ ..]) if wsc.len() == 6 => {
                if !self.unlocked {
                    return neg(sid, 0x33);
                }
                if let Some(nrc) = self.refuse_save {
                    return neg(sid, nrc);
                }
                let v = u16::from_be_bytes([*hi, *lo]);
                match self.selected {
                    Some(c) if self.test_value == Some(v) => {
                        let stored = self.corrupt_save.unwrap_or(v);
                        self.channels.insert(c, stored);
                        self.test_value = None;
                        vec![0x71, 0xBB, 0x01, 0x03]
                    }
                    _ => neg(sid, 0x22),
                }
            }
            (0x31, [0xBB, ..]) => neg(sid, 0x12),
            (0x32, [0xB8, 0x01, 0x03]) => {
                self.adaptation_mode = false;
                self.selected = None;
                self.test_value = None;
                vec![0x72, 0xB8]
            }
            (0x32, [0xB8, 0x00, g]) if self.basic_active == Some(*g) => {
                self.basic_active = None;
                vec![0x72, 0xB8]
            }
            (0x32, _) => neg(sid, 0x22),
            (0x31, _) => neg(sid, 0x12),
            _ => neg(sid, 0x11),
        }
    }
}

/// Lock a shared module (poison-tolerant).
pub fn lock(unit: &Arc<Mutex<SimKwpModule>>) -> MutexGuard<'_, SimKwpModule> {
    unit.lock().unwrap_or_else(|e| e.into_inner())
}

/// A [`CanLink`] in front of [`Tp20SimLink`] that resets the module's session
/// whenever the tester sets up a channel (a frame on the setup id `0x200`).
pub struct SessionResetLink {
    inner: Tp20SimLink,
    unit: Arc<Mutex<SimKwpModule>>,
}

impl SessionResetLink {
    /// Wrap the link onto `ecu`, resetting `unit` on every channel setup.
    pub fn new(ecu: Arc<Mutex<Tp20Ecu>>, unit: Arc<Mutex<SimKwpModule>>) -> Self {
        SessionResetLink {
            inner: Tp20SimLink::new(ecu),
            unit,
        }
    }
}

impl CanLink for SessionResetLink {
    fn send_frame(&mut self, id: u32, data: &[u8]) -> opentune_core::Result<()> {
        if id == BROADCAST_ID {
            lock(&self.unit).reset_session();
        }
        self.inner.send_frame(id, data)
    }
    fn recv_frame(&mut self, timeout_ms: u64) -> opentune_core::Result<(u32, Vec<u8>)> {
        self.inner.recv_frame(timeout_ms)
    }
}

/// The ECU side of a TP2.0 channel at [`SIM_TP20_ADDR`] around `unit` (the
/// core [`Tp20Ecu`] simulator, configured by `cfg` apart from the address and
/// the assigned id).
pub fn sim_bus(unit: Arc<Mutex<SimKwpModule>>, cfg: Tp20EcuConfig) -> Arc<Mutex<Tp20Ecu>> {
    Tp20Ecu::new(
        Tp20EcuConfig {
            addr: SIM_TP20_ADDR,
            tester_tx_id: SIM_TESTER_TX_ID,
            ..cfg
        },
        Box::new(move |req| lock(&unit).answer(req)),
    )
    .shared()
}

/// Open the core [`Tp20Channel`] (setup, parameters, keep-alive every
/// `keepalive_ms`, 0 = none) to a simulated bus, through a
/// [`SessionResetLink`].
pub fn open_sim_channel(
    ecu: Arc<Mutex<Tp20Ecu>>,
    unit: Arc<Mutex<SimKwpModule>>,
    keepalive_ms: u64,
) -> opentune_core::Result<Tp20Channel> {
    let mut o = Tp20Options::new(SIM_TP20_ADDR);
    o.rx_id = SIM_TESTER_RX_ID;
    o.keepalive_ms = keepalive_ms;
    o.timeout_ms = 200;
    Tp20Channel::open(Box::new(SessionResetLink::new(ecu, unit)), &o)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sim_key_is_a_reversible_test_mask() {
        let k = SimSeedKey.compute(&[0, 0, 0, 0], 3).unwrap();
        assert_eq!(k, SIM_KEY_MASK.to_vec());
        assert!(test_seed_key("kwpsim-test").is_some());
        assert!(test_seed_key("vag").is_none());
    }

    #[test]
    fn everything_needs_a_session_first() {
        let mut m = SimKwpModule::default();
        assert_eq!(m.answer(&[0x21, 0x01]), vec![0x7F, 0x21, 0x80]);
        assert_eq!(m.answer(&[0x10, 0x89]), vec![0x50, 0x89]);
        assert_eq!(m.answer(&[0x21, 0x01])[..2], [0x61, 0x01]);
        m.reset_session();
        assert_eq!(m.answer(&[0x21, 0x01]), vec![0x7F, 0x21, 0x80]);
        assert_eq!(m.resets, 1);
    }
}
