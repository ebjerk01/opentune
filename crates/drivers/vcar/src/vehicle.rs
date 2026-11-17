//! The simulated vehicle: one [`UdsServer`] per module behind its own
//! server-side ISO-TP endpoint on a virtual 11-bit CAN bus, a functional OBD-II
//! responder on the OBD-capable module, and a small engine model shared by the
//! OBD live data, the battery voltage and the synthetic bus traffic.
//!
//! [`Vehicle::on_frame`] is the bus: the tester's CAN frame goes in, the frames
//! the modules answer with come out. A frame for an id no module listens on
//! produces nothing, so the tester's ISO-TP layer times out exactly as it would
//! for a module that is not fitted.
use crate::profile::{parse_dtc, Profile};
use crate::FUNCTIONAL_ID;
use opentune_core::obd::sim::SimEcu;
use opentune_core::RawDiag;
use opentune_udssim::{DtcRecord, ServerConfig, UdsServer, XorSecurity};
use std::time::Instant;

/// Padding byte used for the modules' CAN frames.
const PAD: u8 = 0xAA;

/// Live engine values at one instant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EngineState {
    /// Engine running.
    pub running: bool,
    /// Engine speed, rpm.
    pub rpm: f64,
    /// Vehicle speed, km/h.
    pub speed_kmh: f64,
    /// Coolant temperature, °C.
    pub coolant_c: f64,
    /// Throttle position, %.
    pub throttle_pct: f64,
    /// Calculated load, %.
    pub load_pct: f64,
    /// Battery / module supply voltage, mV.
    pub battery_mv: u32,
}

/// A slow 0..1..0 triangle wave with the given period in seconds.
fn tri(t_s: f64, period_s: f64) -> f64 {
    let p = (t_s / period_s).rem_euclid(1.0);
    if p < 0.5 {
        p * 2.0
    } else {
        2.0 - p * 2.0
    }
}

/// The engine model: a gentle drive cycle while running, everything at rest
/// (and the battery at its resting voltage) while off. Deterministic in `t_s`,
/// so the OBD responder and the bus monitor agree at any instant.
pub fn engine_state(running: bool, t_s: f64) -> EngineState {
    if !running {
        return EngineState {
            running,
            rpm: 0.0,
            speed_kmh: 0.0,
            coolant_c: 18.0,
            throttle_pct: 0.0,
            load_pct: 0.0,
            battery_mv: 12_600,
        };
    }
    let pedal = tri(t_s, 24.0);
    EngineState {
        running,
        rpm: 820.0 + 2_200.0 * pedal,
        speed_kmh: 110.0 * tri(t_s, 48.0),
        coolant_c: 90.0,
        throttle_pct: 6.0 + 60.0 * pedal,
        load_pct: 18.0 + 55.0 * pedal,
        battery_mv: 14_100,
    }
}

/// Server-side ISO-TP reassembly of a multi-frame request.
#[derive(Debug, Default)]
struct RxState {
    expected: usize,
    data: Vec<u8>,
    next_sn: u8,
}

/// The rest of a multi-frame response, waiting for the tester's flow control.
#[derive(Debug, Default)]
struct TxState {
    remaining: Vec<u8>,
    next_sn: u8,
}

/// One module on the virtual bus.
pub struct Module {
    /// Display name.
    pub name: String,
    /// Physical request id.
    pub req_id: u32,
    /// Response id.
    pub resp_id: u32,
    /// Answers OBD-II.
    pub obd: bool,
    server: UdsServer,
    rx: Option<RxState>,
    tx: Option<TxState>,
}

impl Module {
    /// The module's UDS server (DIDs, DTCs, IO state), for assertions.
    pub fn server(&self) -> &UdsServer {
        &self.server
    }

    /// The module's UDS server, mutably (to seed a scenario).
    pub fn server_mut(&mut self) -> &mut UdsServer {
        &mut self.server
    }
}

/// The whole simulated vehicle.
pub struct Vehicle {
    profile_id: String,
    name: String,
    vin: String,
    running: bool,
    epoch: Instant,
    modules: Vec<Module>,
}

impl Vehicle {
    /// Build a vehicle from a (validated) profile.
    pub fn from_profile(p: &Profile) -> opentune_core::Result<Self> {
        p.validate()?;
        let mut modules = Vec::with_capacity(p.modules.len());
        for m in &p.modules {
            let mut cfg = ServerConfig {
                require_security_for_write: m.require_security_for_write,
                ..ServerConfig::default()
            };
            if let Some(seed) = &m.seed {
                cfg.seed = seed.clone();
            }
            let mut server = UdsServer::with_config(cfg);
            // TEST-ONLY seed->key (see `SecurityProfile`).
            server.set_security_checker(Box::new(XorSecurity::new(p.security.xor_mask)));
            for d in &m.dids {
                // Validated above.
                let v = d.value(&p.vin).map_err(opentune_core::Error::Config)?;
                server.set_did(d.did, v);
            }
            for d in &m.dtcs {
                let [hi, mid] = parse_dtc(&d.code).map_err(opentune_core::Error::Config)?;
                server.add_dtc(DtcRecord::new([hi, mid, d.failure_type], d.status));
            }
            for r in &m.routines {
                server.set_routine(r.id, r.result.clone().unwrap_or_default());
            }
            for io in &m.io {
                server.set_io(io.did, io.default.clone());
            }
            modules.push(Module {
                name: m.name.clone(),
                req_id: m.req_id,
                resp_id: m.resp_id,
                obd: m.obd,
                server,
                rx: None,
                tx: None,
            });
        }
        Ok(Self {
            profile_id: p.id.clone(),
            name: p.name.clone(),
            vin: p.vin.clone(),
            running: p.engine_running,
            epoch: Instant::now(),
            modules,
        })
    }

    /// The profile id this vehicle was built from.
    pub fn profile_id(&self) -> &str {
        &self.profile_id
    }

    /// Display name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The vehicle's VIN.
    pub fn vin(&self) -> &str {
        &self.vin
    }

    /// The modules, in profile order.
    pub fn modules(&self) -> &[Module] {
        &self.modules
    }

    /// The module answering on request id `req_id`, if any.
    pub fn module(&self, req_id: u32) -> Option<&Module> {
        self.modules.iter().find(|m| m.req_id == req_id)
    }

    /// The module answering on request id `req_id`, mutably.
    pub fn module_mut(&mut self, req_id: u32) -> Option<&mut Module> {
        self.modules.iter_mut().find(|m| m.req_id == req_id)
    }

    /// Whether the engine is running.
    pub fn engine_running(&self) -> bool {
        self.running
    }

    /// Start or stop the engine.
    pub fn set_engine_running(&mut self, running: bool) {
        self.running = running;
    }

    /// When the vehicle's clock started (the engine model's `t = 0`).
    pub fn epoch(&self) -> Instant {
        self.epoch
    }

    /// The engine model now.
    pub fn engine_now(&self) -> EngineState {
        engine_state(self.running, self.epoch.elapsed().as_secs_f64())
    }

    /// Deliver one tester CAN frame to the bus and return the frames the
    /// modules send back (`(id, 8-byte payload)`), in order. Unknown ids, and
    /// requests a module chooses not to answer, return nothing.
    pub fn on_frame(&mut self, id: u32, data: &[u8]) -> Vec<(u32, Vec<u8>)> {
        if data.is_empty() {
            return Vec::new();
        }
        if id == FUNCTIONAL_ID {
            return self.on_functional(data);
        }
        let state = self.engine_now();
        let vin = self.vin.clone();
        let Some(m) = self.modules.iter_mut().find(|m| m.req_id == id) else {
            return Vec::new(); // nobody listens on this id
        };
        let mut out = Vec::new();
        match data[0] & 0xF0 {
            // Single frame.
            0x00 => {
                let len = (data[0] & 0x0F) as usize;
                if len == 0 || 1 + len > data.len() {
                    return out;
                }
                m.rx = None;
                let req = data[1..1 + len].to_vec();
                if let Some(resp) = process(m, &req, false, &state, &vin) {
                    respond(m, &resp, &mut out);
                }
            }
            // First frame: start reassembly and grant the rest (BS 0, STmin 0).
            0x10 => {
                if data.len() < 8 {
                    return out;
                }
                let expected = (((data[0] & 0x0F) as usize) << 8) | data[1] as usize;
                m.rx = Some(RxState {
                    expected,
                    data: data[2..8].to_vec(),
                    next_sn: 1,
                });
                out.push((m.resp_id, pad(vec![0x30, 0x00, 0x00])));
            }
            // Consecutive frame.
            0x20 => {
                let Some(rx) = m.rx.as_mut() else {
                    return out; // unexpected CF: ignore, like an ECU would
                };
                if data[0] & 0x0F != rx.next_sn {
                    m.rx = None; // sequence error: abort the reception
                    return out;
                }
                rx.next_sn = (rx.next_sn + 1) & 0x0F;
                let take = (rx.expected - rx.data.len()).min(data.len() - 1);
                rx.data.extend_from_slice(&data[1..1 + take]);
                if rx.data.len() >= rx.expected {
                    let req = std::mem::take(&mut rx.data);
                    m.rx = None;
                    if let Some(resp) = process(m, &req, false, &state, &vin) {
                        respond(m, &resp, &mut out);
                    }
                }
            }
            // Flow control from the tester for our pending multi-frame answer.
            0x30 => {
                if data[0] & 0x0F == 0x00 {
                    let bs = data.get(1).copied().unwrap_or(0) as usize;
                    send_consecutive(m, bs, &mut out);
                } else if data[0] & 0x0F == 0x02 {
                    m.tx = None; // overflow: abort
                }
                // WAIT: keep the pending answer until the next FC.
            }
            _ => {}
        }
        out
    }

    /// A functionally addressed (0x7DF) single frame: OBD services go to every
    /// OBD-capable module, other services to every module. Functional
    /// requests that fail are answered with silence, as on a real bus.
    fn on_functional(&mut self, data: &[u8]) -> Vec<(u32, Vec<u8>)> {
        let mut out = Vec::new();
        // A tester whose ISO-TP layer is bound to the functional id sends its
        // flow control there too: release any module's pending multi-frame
        // answer (e.g. the 20-byte mode 09 VIN).
        if data[0] & 0xF0 == 0x30 {
            if data[0] & 0x0F == 0x00 {
                let bs = data.get(1).copied().unwrap_or(0) as usize;
                for m in &mut self.modules {
                    send_consecutive(m, bs, &mut out);
                }
            }
            return out;
        }
        // Functional requests are single-frame only (ISO 15765-4).
        if data[0] & 0xF0 != 0x00 {
            return out;
        }
        let len = (data[0] & 0x0F) as usize;
        if len == 0 || 1 + len > data.len() {
            return out;
        }
        let req = data[1..1 + len].to_vec();
        let state = self.engine_now();
        let vin = self.vin.clone();
        for m in &mut self.modules {
            if is_obd_service(req[0]) && !m.obd {
                continue;
            }
            if let Some(resp) = process(m, &req, true, &state, &vin) {
                respond(m, &resp, &mut out);
            }
        }
        out
    }
}

/// OBD-II service ids (SAE J1979 modes 01..0A).
fn is_obd_service(sid: u8) -> bool {
    (0x01..=0x0A).contains(&sid)
}

/// Services whose sub-function byte carries the
/// suppressPosRspMsgIndicationBit (ISO 14229-1 §7.5).
fn has_spr_bit(sid: u8) -> bool {
    matches!(sid, 0x10 | 0x11 | 0x28 | 0x31 | 0x3E | 0x85)
}

/// Run one complete request on a module. `None` = no response.
fn process(
    m: &mut Module,
    req: &[u8],
    functional: bool,
    state: &EngineState,
    vin: &str,
) -> Option<Vec<u8>> {
    let sid = req[0];
    if is_obd_service(sid) {
        if !m.obd {
            // A non-OBD module refuses the OBD service.
            return Some(vec![0x7F, sid, 0x11]);
        }
        return obd(m, req, functional, state, vin);
    }
    let resp = m.server.handle(req);
    let positive = resp.first() == Some(&(sid.wrapping_add(0x40)));
    if positive && has_spr_bit(sid) && req.get(1).is_some_and(|s| s & 0x80 != 0) {
        return None; // positive response suppressed on request
    }
    if functional && !positive {
        return None; // functional requests are not answered negatively
    }
    Some(resp)
}

/// The OBD-II responder of an OBD-capable module. Mode 01 / 09 come from
/// [`SimEcu`] fed by the engine model; mode 03 / 07 / 04 work on the module's
/// own UDS DTC store, so the OBD and UDS views of the codes agree.
fn obd(
    m: &mut Module,
    req: &[u8],
    functional: bool,
    st: &EngineState,
    vin: &str,
) -> Option<Vec<u8>> {
    let sid = req[0];
    match req {
        // Mode 03 confirmed / mode 07 pending emission DTCs: 4x/47 <count> <pairs>.
        [0x03] | [0x07] => {
            let bit = if sid == 0x03 { 0x08 } else { 0x04 };
            let codes: Vec<[u8; 2]> = m
                .server
                .dtcs()
                .iter()
                .filter(|d| d.status & bit != 0 && d.dtc[0] >> 6 == 0)
                .map(|d| [d.dtc[0], d.dtc[1]])
                .collect();
            let mut r = vec![sid + 0x40, codes.len() as u8];
            for c in codes {
                r.extend_from_slice(&c);
            }
            Some(r)
        }
        // Mode 04: clear emission-related diagnostic information.
        [0x04] => {
            m.server.clear_dtcs();
            Some(vec![0x44])
        }
        _ => {
            let mut ecu = sim_ecu(st, vin);
            match ecu.request(req) {
                Ok(r) if functional && r.first() == Some(&0x7F) => None,
                Ok(r) => Some(r),
                // SimEcu answers nothing for a request whose PIDs it lacks.
                Err(_) if functional => None,
                Err(_) => Some(vec![0x7F, sid, 0x31]),
            }
        }
    }
}

/// The mode-01 / mode-09 responder: the stock turbo-diesel demo with the
/// engine-model channels (rpm, speed, coolant, throttle, load, voltage) and the
/// profile VIN substituted.
fn sim_ecu(st: &EngineState, vin: &str) -> SimEcu {
    let mut ecu = SimEcu::demo((st.rpm as u64) / 10);
    ecu.vin = Some(vin.to_string());
    let pct = |v: f64| ((v * 255.0 / 100.0).round().clamp(0.0, 255.0)) as u8;
    let rpm = ((st.rpm * 4.0).round().clamp(0.0, 65535.0) as u16).to_be_bytes();
    let mv = (st.battery_mv.min(65_535) as u16).to_be_bytes();
    ecu.set(0x04, &[pct(st.load_pct)])
        .set(
            0x05,
            &[(st.coolant_c + 40.0).round().clamp(0.0, 255.0) as u8],
        )
        .set(0x0C, &rpm)
        .set(0x0D, &[st.speed_kmh.round().clamp(0.0, 255.0) as u8])
        .set(0x11, &[pct(st.throttle_pct)])
        .set(0x42, &mv);
    if !st.running {
        ecu.set(0x1F, &[0, 0]).set(0x5E, &[0, 0]);
    }
    ecu
}

fn pad(mut f: Vec<u8>) -> Vec<u8> {
    f.resize(8, PAD);
    f
}

/// Frame `resp` from module `m`: a single frame, or a first frame with the rest
/// held until the tester's flow control.
fn respond(m: &mut Module, resp: &[u8], out: &mut Vec<(u32, Vec<u8>)>) {
    m.tx = None;
    if resp.len() <= 7 {
        let mut f = vec![resp.len() as u8];
        f.extend_from_slice(resp);
        out.push((m.resp_id, pad(f)));
        return;
    }
    // Classic ISO-TP caps a message at 4095 bytes; the simulator's own answers
    // never get near it.
    let len = resp.len().min(0x0FFF);
    let mut ff = vec![0x10 | ((len >> 8) as u8 & 0x0F), (len & 0xFF) as u8];
    ff.extend_from_slice(&resp[..6]);
    out.push((m.resp_id, ff));
    m.tx = Some(TxState {
        remaining: resp[6..len].to_vec(),
        next_sn: 1,
    });
}

/// Send consecutive frames of the pending answer: all of them for block size
/// 0, else one block (the rest waits for the next flow control).
fn send_consecutive(m: &mut Module, block_size: usize, out: &mut Vec<(u32, Vec<u8>)>) {
    let Some(tx) = m.tx.as_mut() else {
        return;
    };
    let mut sent = 0usize;
    while !tx.remaining.is_empty() && (block_size == 0 || sent < block_size) {
        let take = tx.remaining.len().min(7);
        let mut cf = vec![0x20 | tx.next_sn];
        cf.extend(tx.remaining.drain(..take));
        out.push((m.resp_id, pad(cf)));
        tx.next_sn = (tx.next_sn + 1) & 0x0F;
        sent += 1;
    }
    if tx.remaining.is_empty() {
        m.tx = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo() -> Vehicle {
        Vehicle::from_profile(&Profile::demo()).unwrap()
    }

    #[test]
    fn unknown_id_is_silent() {
        let mut v = demo();
        assert!(v.on_frame(0x7A0, &[0x02, 0x3E, 0x00]).is_empty());
    }

    #[test]
    fn single_frame_request_gets_single_frame_answer() {
        let mut v = demo();
        let out = v.on_frame(0x7E1, &[0x02, 0x3E, 0x00, 0, 0, 0, 0, 0]);
        assert_eq!(
            out,
            vec![(0x7E9, vec![0x02, 0x7E, 0x00, PAD, PAD, PAD, PAD, PAD])]
        );
        // Suppressed positive response.
        assert!(v.on_frame(0x7E1, &[0x02, 0x3E, 0x80]).is_empty());
    }

    #[test]
    fn multi_frame_answer_waits_for_flow_control() {
        let mut v = demo();
        // 22 F190 -> 62 F1 90 + 17-byte VIN = 20 bytes.
        let ff = v.on_frame(0x7E0, &[0x03, 0x22, 0xF1, 0x90]);
        assert_eq!(ff.len(), 1);
        assert_eq!(&ff[0].1[..5], &[0x10, 20, 0x62, 0xF1, 0x90]);
        let cfs = v.on_frame(0x7E0, &[0x30, 0x00, 0x00]);
        assert_eq!(cfs.len(), 2);
        assert_eq!(cfs[0].1[0], 0x21);
        assert_eq!(cfs[1].1[0], 0x22);
        let mut vin = ff[0].1[5..].to_vec();
        vin.extend_from_slice(&cfs[0].1[1..]);
        vin.extend_from_slice(&cfs[1].1[1..]);
        vin.truncate(17);
        assert_eq!(vin, b"TESTVCAR0D0000001");
    }

    #[test]
    fn functional_obd_reaches_only_the_engine() {
        let mut v = demo();
        let out = v.on_frame(FUNCTIONAL_ID, &[0x02, 0x01, 0x00]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, 0x7E8);
        assert_eq!(&out[0].1[1..3], &[0x41, 0x00]);
        // Mode 03: P0101 and P0420 are confirmed, P0171 only pending.
        let out = v.on_frame(FUNCTIONAL_ID, &[0x01, 0x03]);
        assert_eq!(&out[0].1[..7], &[0x06, 0x43, 0x02, 0x01, 0x01, 0x04, 0x20]);
        let out = v.on_frame(FUNCTIONAL_ID, &[0x01, 0x07]);
        assert_eq!(&out[0].1[..7], &[0x06, 0x47, 0x02, 0x01, 0x01, 0x01, 0x71]);
    }

    #[test]
    fn engine_model_rests_when_off() {
        let on = engine_state(true, 3.0);
        assert!(on.rpm > 800.0 && on.battery_mv == 14_100);
        let off = engine_state(false, 3.0);
        assert_eq!((off.rpm, off.speed_kmh, off.battery_mv), (0.0, 0.0, 12_600));
    }
}
