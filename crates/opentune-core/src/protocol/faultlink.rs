//! A **fault-injecting simulated CAN bus** for testing anything that runs
//! over a [`CanLink`] — [`IsoTp`](crate::protocol::IsoTp), and everything
//! above it (UDS polling, `0x2C` fast logging, OBD-II mode 01) — without
//! hardware and without wall-clock waits.
//!
//! [`FaultBus`] holds one or more [`SimNode`]s: simulated ECUs that speak
//! ISO-TP (single and multi-frame, both directions) and hand every complete
//! request to a [`Responder`] (any closure, or a [`RawDiag`] simulator via
//! [`raw_responder`]). [`FaultBus::link`] / [`FaultBus::reopen`] hand out
//! [`FaultLink`]s, the tester's side. The bus models the conditions a real
//! diagnostic bus throws at a live log:
//!
//! * **Quiet bus.** Nothing is ever sent unprompted: a receive with nothing
//!   queued times out, exactly like the diagnostic CAN behind a gateway,
//!   which is silent until queried. A node whose responder returns `None`
//!   stays silent (an unsupported OBD PID on CAN).
//! * **Foreign traffic.** [`FaultBus::with_noise`] puts unrelated frames
//!   ahead of every reply; [`FaultBus::with_idle_noise`] makes the bus
//!   *never* idle (a broadcast frame on every receive).
//! * **Several responders.** Nodes may share a functional request id
//!   ([`SimNode::obd`] listens on `0x7DF`); each answers on its own id, and a
//!   multi-frame reply waits for a flow control on the node's *physical* id,
//!   as ISO 15765-4 requires.
//! * **Slow replies.** [`SimNode::with_pending`] sends `7F <sid> 78`
//!   responsePending frames before the final reply.
//! * **Transport faults** ([`Fault`]): dead for the next N link calls, or
//!   dead until re-opened, failing as an I/O error, a timeout, or silently
//!   (frames vanish, receives time out); [`FaultBus::fail_after`] schedules
//!   one after N healthy calls, and [`FaultBus::refuse_opens`] makes the next
//!   re-opens fail (the adapter is still unplugged). A re-open makes every
//!   older [`FaultLink`] report I/O errors (that handle is closed).
//!
//! With [`FaultBus::with_clock`] a receive timeout advances a shared
//! [`VirtualClock`] by the requested timeout instead of waiting, so a test
//! of the [`resilient`](crate::resilient) loop runs in microseconds.
use crate::diag::RawDiag;
use crate::protocol::isotp::CanLink;
use crate::resilient::VirtualClock;
use crate::{Error, Result};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

/// A simulated ECU application: a complete request in, the reply out
/// (`None`, or an empty reply, = stay silent).
pub type Responder = Box<dyn FnMut(&[u8]) -> Option<Vec<u8>> + Send>;

/// Adapt a [`RawDiag`] simulator (e.g. `obd::sim::SimEcu`) as a
/// [`Responder`]: its errors (such as `NO DATA`) become silence.
pub fn raw_responder<D: RawDiag + Send + 'static>(mut d: D) -> Responder {
    Box::new(move |req| d.request(req).ok())
}

/// The OBD-II functional request id (ISO 15765-4, 11-bit).
pub const OBD_FUNCTIONAL_ID: u32 = 0x7DF;

/// Filler byte for frames the simulated nodes send.
const PAD: u8 = 0x55;

fn pad(mut f: Vec<u8>) -> Vec<u8> {
    f.resize(8.max(f.len()), PAD);
    f
}

/// A simulated ECU on a [`FaultBus`].
pub struct SimNode {
    /// Physical request id (e.g. `0x7E0`).
    pub phys_id: u32,
    /// Further (functional) request ids it answers single-frame requests on.
    pub functional_ids: Vec<u32>,
    /// Response id (e.g. `0x7E8`).
    pub resp_id: u32,
    /// responsePending (`7F <sid> 78`) frames sent before each final reply.
    pub pending: u32,
    /// Complete requests received.
    pub requests: u64,
    responder: Responder,
    /// Multi-frame request being received: (data, total length, next SN).
    rx: Option<(Vec<u8>, usize, u8)>,
    /// Consecutive frames of a reply, waiting for the tester's flow control.
    tx_cfs: Vec<Vec<u8>>,
}

impl SimNode {
    /// A node answering physical requests on `phys_id` from `resp_id`.
    pub fn new(phys_id: u32, resp_id: u32, responder: Responder) -> Self {
        Self {
            phys_id,
            functional_ids: Vec::new(),
            resp_id,
            pending: 0,
            requests: 0,
            responder,
            rx: None,
            tx_cfs: Vec::new(),
        }
    }

    /// An OBD-II node: physical `phys_id` plus the functional `0x7DF`.
    pub fn obd(phys_id: u32, resp_id: u32, responder: Responder) -> Self {
        Self::new(phys_id, resp_id, responder).with_functional(OBD_FUNCTIONAL_ID)
    }

    /// Also answer single-frame requests on `id`.
    pub fn with_functional(mut self, id: u32) -> Self {
        self.functional_ids.push(id);
        self
    }

    /// Send `n` responsePending frames before every final reply.
    pub fn with_pending(mut self, n: u32) -> Self {
        self.pending = n;
        self
    }

    /// Handle one frame from the tester. `None` when it is not addressed to
    /// this node; otherwise the frames the node sends in reply (maybe none).
    fn on_frame(&mut self, id: u32, data: &[u8]) -> Option<Vec<(u32, Vec<u8>)>> {
        let phys = id == self.phys_id;
        if !phys && !self.functional_ids.contains(&id) {
            return None;
        }
        let Some(&pci) = data.first() else {
            return Some(Vec::new());
        };
        let out = match pci & 0xF0 {
            0x00 => {
                let len = usize::from(pci & 0x0F);
                if len == 0 || 1 + len > data.len() {
                    return Some(Vec::new());
                }
                self.rx = None;
                self.tx_cfs.clear();
                self.answer(&data[1..1 + len])
            }
            // Segmented requests and flow control are physical only.
            0x10 if phys && data.len() >= 8 => {
                let len = (usize::from(pci & 0x0F) << 8) | usize::from(data[1]);
                self.rx = Some((data[2..8].to_vec(), len, 1));
                self.tx_cfs.clear();
                vec![(self.resp_id, pad(vec![0x30, 0x00, 0x00]))]
            }
            0x20 if phys => {
                let mut done = None;
                if let Some((buf, len, sn)) = self.rx.as_mut() {
                    if pci & 0x0F != *sn {
                        self.rx = None; // sequence error: drop the request
                    } else {
                        *sn = (*sn + 1) & 0x0F;
                        let take = (*len - buf.len()).min(data.len() - 1);
                        buf.extend_from_slice(&data[1..1 + take]);
                        if buf.len() >= *len {
                            done = self.rx.take().map(|(b, _, _)| b);
                        }
                    }
                }
                match done {
                    Some(req) => self.answer(&req),
                    None => Vec::new(),
                }
            }
            0x30 if phys && pci & 0x0F == 0 => std::mem::take(&mut self.tx_cfs)
                .into_iter()
                .map(|cf| (self.resp_id, cf))
                .collect(),
            _ => Vec::new(),
        };
        Some(out)
    }

    /// Run the responder and encode its reply (pending frames first).
    fn answer(&mut self, req: &[u8]) -> Vec<(u32, Vec<u8>)> {
        self.requests += 1;
        let resp = match (self.responder)(req) {
            Some(r) if !r.is_empty() && r.len() <= 0x0FFF => r,
            _ => return Vec::new(),
        };
        let mut out: Vec<(u32, Vec<u8>)> = (0..self.pending)
            .map(|_| (self.resp_id, pad(vec![0x03, 0x7F, req[0], 0x78])))
            .collect();
        if resp.len() <= 7 {
            let mut sf = vec![resp.len() as u8];
            sf.extend_from_slice(&resp);
            out.push((self.resp_id, pad(sf)));
            return out;
        }
        let mut ff = vec![0x10 | (resp.len() >> 8) as u8, resp.len() as u8];
        ff.extend_from_slice(&resp[..6]);
        out.push((self.resp_id, ff));
        let mut sn = 1u8;
        self.tx_cfs = resp[6..]
            .chunks(7)
            .map(|c| {
                let mut cf = vec![0x20 | sn];
                cf.extend_from_slice(c);
                sn = (sn + 1) & 0x0F;
                pad(cf)
            })
            .collect();
        out
    }

    fn reset(&mut self) {
        self.rx = None;
        self.tx_cfs.clear();
    }
}

/// How a dead link fails a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailKind {
    /// Sends and receives fail with an I/O error (adapter unplugged).
    Io,
    /// Sends and receives fail with a timeout (a wedged transmitter).
    Timeout,
    /// Sends "succeed" but nothing reaches the bus; receives time out (the
    /// bus or ECU went to sleep).
    Silent,
}

/// A transport fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// The next `n` link calls (sends and receives) fail, then it recovers on
    /// its own.
    Calls(u64, FailKind),
    /// Every call fails until [`FaultBus::reopen`].
    UntilReopen(FailKind),
}

/// Counters a test can assert on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FaultStats {
    /// Frames the tester sent that reached the bus.
    pub frames_sent: u64,
    /// Frames the tester received.
    pub frames_received: u64,
    /// Receives that timed out (quiet bus or a fault).
    pub timeouts: u64,
    /// Link calls failed by a [`Fault`] (or on a closed handle).
    pub failed_calls: u64,
    /// Successful [`FaultBus::reopen`]s.
    pub opens: u64,
    /// Re-opens refused ([`FaultBus::refuse_opens`]).
    pub refused_opens: u64,
    /// Complete requests the nodes received.
    pub requests: u64,
}

struct Bus {
    nodes: Vec<SimNode>,
    queue: VecDeque<(u32, Vec<u8>)>,
    noise: Vec<(u32, Vec<u8>)>,
    idle_noise: Option<(u32, Vec<u8>)>,
    generation: u64,
    dead_calls: u64,
    dead_kind: FailKind,
    until_reopen: Option<FailKind>,
    scheduled: Option<(u64, Fault)>,
    refuse_opens: u32,
    clock: Option<VirtualClock>,
    stats: FaultStats,
}

impl Bus {
    fn apply(&mut self, f: Fault) {
        match f {
            Fault::Calls(n, k) => {
                self.dead_calls = n;
                self.dead_kind = k;
            }
            Fault::UntilReopen(k) => self.until_reopen = Some(k),
        }
    }

    /// The fault (if any) this call of a handle from `generation` hits.
    fn fault(&mut self, generation: u64) -> Option<FailKind> {
        let hit = if generation != self.generation {
            Some(FailKind::Io) // a handle from before a reopen is closed
        } else {
            if let Some((left, f)) = self.scheduled {
                if left == 0 {
                    self.scheduled = None;
                    self.apply(f);
                } else {
                    self.scheduled = Some((left - 1, f));
                }
            }
            if let Some(k) = self.until_reopen {
                Some(k)
            } else if self.dead_calls > 0 {
                self.dead_calls -= 1;
                Some(self.dead_kind)
            } else {
                None
            }
        };
        if hit.is_some() {
            self.stats.failed_calls += 1;
        }
        hit
    }

    fn timeout(&mut self, ms: u64) -> Error {
        self.stats.timeouts += 1;
        if let Some(c) = &self.clock {
            c.advance(ms);
        }
        Error::Timeout(ms)
    }
}

fn io_error(what: &str) -> Error {
    Error::Io(std::io::Error::new(
        std::io::ErrorKind::BrokenPipe,
        format!("simulated: {what}"),
    ))
}

/// The simulated bus. Clones share it.
#[derive(Clone)]
pub struct FaultBus {
    st: Arc<Mutex<Bus>>,
}

impl Default for FaultBus {
    fn default() -> Self {
        Self::new()
    }
}

impl FaultBus {
    /// An empty, healthy, quiet bus.
    pub fn new() -> Self {
        Self {
            st: Arc::new(Mutex::new(Bus {
                nodes: Vec::new(),
                queue: VecDeque::new(),
                noise: Vec::new(),
                idle_noise: None,
                generation: 0,
                dead_calls: 0,
                dead_kind: FailKind::Io,
                until_reopen: None,
                scheduled: None,
                refuse_opens: 0,
                clock: None,
                stats: FaultStats::default(),
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Bus> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Add a node.
    pub fn with_node(self, node: SimNode) -> Self {
        self.lock().nodes.push(node);
        self
    }

    /// Advance `clock` by each receive timeout instead of waiting.
    pub fn with_clock(self, clock: VirtualClock) -> Self {
        self.lock().clock = Some(clock);
        self
    }

    /// Put this unrelated frame on the bus ahead of every reply.
    pub fn with_noise(self, id: u32, data: &[u8]) -> Self {
        self.lock().noise.push((id, data.to_vec()));
        self
    }

    /// Never idle: a receive with nothing queued returns this frame.
    pub fn with_idle_noise(self, id: u32, data: &[u8]) -> Self {
        self.lock().idle_noise = Some((id, data.to_vec()));
        self
    }

    /// The tester's link on the current connection (no reopen).
    pub fn link(&self) -> FaultLink {
        FaultLink {
            st: Arc::clone(&self.st),
            generation: self.lock().generation,
        }
    }

    /// Re-open the adapter: closes every older [`FaultLink`], ends a
    /// [`Fault::UntilReopen`] and drops anything queued. Fails while
    /// [`refuse_opens`](Self::refuse_opens) has refusals left.
    pub fn reopen(&self) -> Result<FaultLink> {
        let mut b = self.lock();
        if b.refuse_opens > 0 {
            b.refuse_opens -= 1;
            b.stats.refused_opens += 1;
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "simulated: adapter not present",
            )));
        }
        b.generation += 1;
        b.until_reopen = None;
        b.queue.clear();
        b.nodes.iter_mut().for_each(SimNode::reset);
        b.stats.opens += 1;
        Ok(FaultLink {
            st: Arc::clone(&self.st),
            generation: b.generation,
        })
    }

    /// Inject `fault` now.
    pub fn fail(&self, fault: Fault) {
        self.lock().apply(fault);
    }

    /// Inject `fault` after `calls` more healthy link calls.
    pub fn fail_after(&self, calls: u64, fault: Fault) {
        self.lock().scheduled = Some((calls, fault));
    }

    /// Refuse the next `n` [`reopen`](Self::reopen)s.
    pub fn refuse_opens(&self, n: u32) {
        self.lock().refuse_opens = n;
    }

    /// Clear every fault (scheduled ones too); the current link works again.
    pub fn heal(&self) {
        let mut b = self.lock();
        b.dead_calls = 0;
        b.until_reopen = None;
        b.scheduled = None;
    }

    /// Counters so far.
    pub fn stats(&self) -> FaultStats {
        let b = self.lock();
        let mut s = b.stats.clone();
        s.requests = b.nodes.iter().map(|n| n.requests).sum();
        s
    }
}

/// The tester's [`CanLink`] onto a [`FaultBus`].
pub struct FaultLink {
    st: Arc<Mutex<Bus>>,
    generation: u64,
}

impl FaultLink {
    fn lock(&self) -> MutexGuard<'_, Bus> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl CanLink for FaultLink {
    fn send_frame(&mut self, id: u32, data: &[u8]) -> Result<()> {
        let mut b = self.lock();
        match b.fault(self.generation) {
            Some(FailKind::Io) => return Err(io_error("adapter unplugged")),
            Some(FailKind::Timeout) => return Err(Error::Timeout(0)),
            Some(FailKind::Silent) => return Ok(()), // never reaches the bus
            None => {}
        }
        b.stats.frames_sent += 1;
        let mut out = Vec::new();
        for n in b.nodes.iter_mut() {
            if let Some(frames) = n.on_frame(id, data) {
                out.extend(frames);
            }
        }
        if !out.is_empty() {
            let noise = b.noise.clone();
            b.queue.extend(noise);
        }
        b.queue.extend(out);
        Ok(())
    }

    fn recv_frame(&mut self, timeout_ms: u64) -> Result<(u32, Vec<u8>)> {
        let mut b = self.lock();
        match b.fault(self.generation) {
            Some(FailKind::Io) => {
                b.queue.clear();
                return Err(io_error("adapter unplugged"));
            }
            Some(_) => {
                b.queue.clear();
                return Err(b.timeout(timeout_ms));
            }
            None => {}
        }
        if let Some(f) = b.queue.pop_front().or_else(|| b.idle_noise.clone()) {
            b.stats.frames_received += 1;
            return Ok(f);
        }
        Err(b.timeout(timeout_ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obd::sim::{MultiMode, SimEcu};
    use crate::obd::ObdSession;
    use crate::protocol::{DiagTransport, IsoTp};
    use crate::resilient::Clock;
    use crate::OwnedDiag;

    /// A UDS-ish responder: `22 F1 90` → a 17-byte record, `22 xx xx` → 2
    /// bytes, `09 02` → a multi-frame VIN, everything else NRC 0x11.
    fn uds(tag: u8) -> Responder {
        Box::new(move |req| {
            Some(match req {
                [0x22, 0xF1, 0x90] => {
                    let mut r = vec![0x62, 0xF1, 0x90];
                    r.extend_from_slice(b"ZZZTEST0000000001");
                    r
                }
                [0x22, a, b] => vec![0x62, *a, *b, tag, 0x10],
                [0x09, 0x02] => {
                    let mut r = vec![0x49, 0x02, 0x01];
                    r.extend_from_slice(b"ZZZTEST0000000002");
                    r
                }
                [sid, ..] => vec![0x7F, *sid, 0x11],
                [] => return None,
            })
        })
    }

    fn isotp(link: FaultLink, tx: u32, rx: u32) -> IsoTp {
        IsoTp::new(Box::new(link), tx, rx)
    }

    #[test]
    fn quiet_bus_times_out_without_error_until_queried() {
        let bus = FaultBus::new().with_node(SimNode::new(0x7E0, 0x7E8, uds(1)));
        let mut link = bus.link();
        // Nothing arrives unprompted: a timeout, not an I/O error.
        for _ in 0..3 {
            assert!(matches!(link.recv_frame(5), Err(Error::Timeout(5))));
        }
        let mut tp = isotp(link, 0x7E0, 0x7E8);
        assert_eq!(tp.request(&[0x22, 0x10, 0x00], 5).unwrap(), [0x62, 0x10, 0x00, 1, 0x10]);
        // Multi-frame reply over the quiet bus.
        let vin = tp.request(&[0x22, 0xF1, 0x90], 5).unwrap();
        assert_eq!(&vin[3..], b"ZZZTEST0000000001");
        // A silent node (responder None) is a timeout, not a hang.
        let bus = FaultBus::new().with_node(SimNode::new(0x7E0, 0x7E8, Box::new(|_| None)));
        let mut tp = isotp(bus.link(), 0x7E0, 0x7E8);
        assert!(matches!(tp.request(&[0x3E, 0x00], 5), Err(Error::Timeout(_))));
        assert_eq!(bus.stats().requests, 1);
    }

    #[test]
    fn foreign_ids_are_skipped_and_a_busy_bus_still_times_out() {
        let bus = FaultBus::new()
            .with_node(SimNode::new(0x7E0, 0x7E8, uds(2)))
            .with_noise(0x280, &[1, 2, 3, 4, 5, 6, 7, 8])
            .with_noise(0x7E9, &[0x02, 0x7E, 0x00])
            .with_noise(0x1A0, &[0x00; 8]);
        let mut tp = isotp(bus.link(), 0x7E0, 0x7E8);
        for _ in 0..3 {
            assert_eq!(tp.request(&[0x22, 0x20, 0x01], 50).unwrap()[3], 2);
            let vin = tp.request(&[0x22, 0xF1, 0x90], 50).unwrap();
            assert_eq!(vin.len(), 20);
        }
        // A bus that is never idle and an ECU that never answers: the wait is
        // bounded by the request timeout, not restarted per foreign frame.
        let bus = FaultBus::new()
            .with_node(SimNode::new(0x7E0, 0x7E8, Box::new(|_| None)))
            .with_idle_noise(0x280, &[0; 8]);
        let mut tp = isotp(bus.link(), 0x7E0, 0x7E8);
        let t = std::time::Instant::now();
        assert!(matches!(tp.request(&[0x3E, 0x00], 20), Err(Error::Timeout(20))));
        assert!(t.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn functional_request_with_two_responders() {
        let bus = FaultBus::new()
            .with_node(SimNode::obd(0x7E0, 0x7E8, uds(0xA)))
            .with_node(SimNode::obd(0x7E1, 0x7E9, uds(0xB)));
        // Each tester instance takes its rx id's answer, skips the other.
        let mut a = isotp(bus.link(), OBD_FUNCTIONAL_ID, 0x7E8);
        let mut b = isotp(bus.link(), OBD_FUNCTIONAL_ID, 0x7E9);
        for _ in 0..2 {
            assert_eq!(a.request(&[0x22, 0x00, 0x01], 5).unwrap()[3], 0xA);
            assert_eq!(b.request(&[0x22, 0x00, 0x01], 5).unwrap()[3], 0xB);
        }
        // A multi-frame reply to a functional request needs the flow control
        // sent physically (ISO 15765-4); one on 0x7DF is ignored.
        let mut a = isotp(bus.reopen().unwrap(), OBD_FUNCTIONAL_ID, 0x7E8);
        assert!(matches!(a.request(&[0x09, 0x02], 5), Err(Error::Timeout(_))));
        a.fc_tx_id = Some(0x7E0);
        let vin = a.request(&[0x09, 0x02], 5).unwrap();
        assert_eq!(&vin[3..], b"ZZZTEST0000000002");
    }

    #[test]
    fn response_pending_is_waited_out_within_the_limit() {
        let bus = FaultBus::new().with_node(SimNode::new(0x7E0, 0x7E8, uds(3)).with_pending(5));
        let mut tp = isotp(bus.link(), 0x7E0, 0x7E8);
        assert_eq!(tp.request(&[0x22, 0x00, 0x02], 5).unwrap()[3], 3);
        let vin = tp.request(&[0x22, 0xF1, 0x90], 5).unwrap();
        assert_eq!(vin.len(), 20);
        tp.max_response_pending = 3;
        assert!(matches!(tp.request(&[0x22, 0x00, 0x02], 5), Err(Error::Timeout(_))));
    }

    #[test]
    fn dead_for_n_calls_recovers_on_its_own() {
        for kind in [FailKind::Io, FailKind::Timeout, FailKind::Silent] {
            let bus = FaultBus::new().with_node(SimNode::new(0x7E0, 0x7E8, uds(4)));
            let mut tp = isotp(bus.link(), 0x7E0, 0x7E8);
            bus.fail(Fault::Calls(4, kind));
            let mut errors = 0;
            for _ in 0..6 {
                if tp.request(&[0x22, 0x00, 0x03], 5).is_err() {
                    errors += 1;
                }
            }
            assert!(errors >= 2, "{kind:?}: {errors}");
            assert_eq!(tp.request(&[0x22, 0x00, 0x03], 5).unwrap()[3], 4);
            assert_eq!(bus.stats().failed_calls, 4);
        }
    }

    #[test]
    fn dead_until_reopen_closes_old_handles() {
        let clock = VirtualClock::new(0);
        let bus = FaultBus::new()
            .with_node(SimNode::new(0x7E0, 0x7E8, uds(5)))
            .with_clock(clock.clone());
        let mut tp = isotp(bus.link(), 0x7E0, 0x7E8);
        bus.fail_after(2, Fault::UntilReopen(FailKind::Silent));
        assert!(tp.request(&[0x22, 0, 1], 100).is_ok()); // 2 calls: send + recv
        for _ in 0..5 {
            assert!(matches!(tp.request(&[0x22, 0, 1], 100), Err(Error::Timeout(100))));
        }
        assert_eq!(clock.now_ms(), 500, "timeouts advance the virtual clock");
        assert!(clock.sleeps().is_empty());
        bus.refuse_opens(2);
        assert!(bus.reopen().is_err() && bus.reopen().is_err());
        let mut fresh = isotp(bus.reopen().unwrap(), 0x7E0, 0x7E8);
        assert_eq!(fresh.request(&[0x22, 0, 1], 100).unwrap()[3], 5);
        // The old handle is closed: I/O error.
        assert!(matches!(tp.request(&[0x22, 0, 1], 100), Err(Error::Io(_))));
        let s = bus.stats();
        assert_eq!((s.opens, s.refused_opens), (1, 2));
    }

    fn obd_bus() -> FaultBus {
        FaultBus::new().with_node(SimNode::obd(0x7E0, 0x7E8, raw_responder(SimEcu::demo(0))))
    }

    fn obd_diag(link: FaultLink) -> OwnedDiag {
        OwnedDiag::new(Box::new(isotp(link, 0x7E0, 0x7E8)), 20)
    }

    #[test]
    fn obd_session_discovers_and_reads_over_isotp_on_a_quiet_bus() {
        let bus = obd_bus().with_noise(0x7E9, &[0x03, 0x41, 0x0D, 0x50]);
        let mut d = obd_diag(bus.link());
        let mut s = ObdSession::new();
        let set = s.discover(&mut d).unwrap();
        assert!(set.contains(&0x0C) && set.contains(&0x87));
        // 7 PIDs incl. unsupported 0x79 (silent on CAN): 6 + 1 requests.
        let ro = s.read(&mut d, &[0x0C, 0x05, 0x0F, 0x79, 0x10, 0x78, 0x42]);
        assert_eq!(ro.values["rpm"].as_ref().unwrap().first(), Some(1800.0));
        assert_eq!(ro.errors["egt_b2"], "not supported by ECU");
        assert_eq!(s.multi_pid(), Some(true));

        // Functional addressing reaches the same ECU (single-frame replies).
        let mut f = OwnedDiag::new(Box::new(isotp(bus.link(), OBD_FUNCTIONAL_ID, 0x7E8)), 20);
        let ro = ObdSession::new().read(&mut f, &[0x0C, 0x0D]);
        assert!(ro.values["speed"].is_some() && ro.errors.is_empty(), "{ro:?}");
    }

    #[test]
    fn a_dead_link_does_not_poison_the_learned_pids() {
        let bus = obd_bus();
        let mut d = obd_diag(bus.link());
        let mut s = ObdSession::new();
        let pids = [0x0C, 0x05, 0x0F];
        // Unplugged before anything was learned: every request fails.
        bus.fail(Fault::Calls(1000, FailKind::Io));
        let ro = s.read(&mut d, &pids);
        assert!(ro.values.values().all(Option::is_none));
        assert_eq!(s.multi_pid(), None, "nothing learned from a dead tick");
        bus.heal();
        let ro = s.read(&mut d, &pids);
        assert!(ro.values.values().all(Option::is_some), "{ro:?}");
        assert_eq!(s.multi_pid(), Some(true));

        // Single-PID ECU: the same with the multi request rejected.
        let ecu = {
            let mut e = SimEcu::demo(0);
            e.multi = MultiMode::Reject;
            e
        };
        let bus = FaultBus::new().with_node(SimNode::obd(0x7E0, 0x7E8, raw_responder(ecu)));
        let mut d = obd_diag(bus.link());
        let mut s = ObdSession::new();
        bus.fail(Fault::UntilReopen(FailKind::Silent));
        assert!(s.read(&mut d, &pids).values.values().all(Option::is_none));
        let mut d = obd_diag(bus.reopen().unwrap());
        let ro = s.read(&mut d, &pids);
        assert!(ro.values.values().all(Option::is_some), "{ro:?}");
        assert_eq!(s.multi_pid(), Some(false));
    }
}
