//! [`DoIpSimStream`]: a loopback ISO 13400 DoIP **entity** wrapping a
//! [`UdsServer`].
//!
//! It implements `opentune_core`'s [`DoIpStream`] byte-stream abstraction, so a
//! real [`DoIpTransport`] client can run routing activation and diagnostics
//! against it with no network. The entity answers the routing-activation
//! handshake, optionally sends a diagnostic positive ack, and carries each UDS
//! response back as a diagnostic message — the full DoIP framing the client's
//! own code parses, including the NRC 0x78 (`responsePending`) drain loop, which
//! here is driven by real interposed diagnostic messages (not an internal
//! short-cut).
//!
//! Offline conformance only.
//!
//! [`DoIpStream`]: opentune_core::protocol::DoIpStream
//! [`DoIpTransport`]: opentune_core::protocol::DoIpTransport

use crate::fault::{self, Fault, FaultPlan};
use crate::server::UdsServer;
use crate::trace::{Direction, Trace};
use opentune_core::protocol::doip::{payload_type, routing_activation, DoIpMessage, HEADER_LEN};
use opentune_core::protocol::DoIpStream;
use opentune_core::{Error, Result};
use std::collections::VecDeque;

/// A loopback DoIP entity exposing a [`UdsServer`] over the [`DoIpStream`]
/// interface the real `DoIpTransport` client runs on.
pub struct DoIpSimStream {
    server: UdsServer,
    faults: FaultPlan,
    trace: Trace,
    ecu_address: u16,
    inbound: Vec<u8>,
    outbound: VecDeque<u8>,
    send_pos_ack: bool,
    request_count: usize,
}

impl DoIpSimStream {
    /// Build an entity at logical address `ecu_address` over `server`.
    pub fn new(server: UdsServer, ecu_address: u16) -> Self {
        Self {
            server,
            faults: FaultPlan::new(),
            trace: Trace::new(),
            ecu_address,
            inbound: Vec::new(),
            outbound: VecDeque::new(),
            send_pos_ack: true,
            request_count: 0,
        }
    }

    /// Whether to precede each diagnostic response with a positive ack (0x8002).
    /// On by default.
    pub fn set_pos_ack(&mut self, on: bool) {
        self.send_pos_ack = on;
    }

    /// The fault plan, to schedule misbehaviours.
    pub fn faults(&mut self) -> &mut FaultPlan {
        &mut self.faults
    }

    /// The simulated ECU.
    pub fn server(&self) -> &UdsServer {
        &self.server
    }

    /// The simulated ECU, mutably.
    pub fn server_mut(&mut self) -> &mut UdsServer {
        &mut self.server
    }

    /// The ordered UDS request/response trace (diagnostic payloads only).
    pub fn trace(&self) -> &Trace {
        &self.trace
    }

    /// How many diagnostic (UDS) requests the client issued.
    pub fn request_count(&self) -> usize {
        self.request_count
    }

    /// Enqueue a DoIP message for the client to read.
    fn enqueue(&mut self, msg: &DoIpMessage) {
        self.outbound.extend(msg.encode());
    }

    /// Build a diagnostic message from the entity to the tester carrying `uds`.
    fn diag_msg(&self, tester: u16, uds: &[u8]) -> DoIpMessage {
        let mut payload = Vec::with_capacity(4 + uds.len());
        payload.extend_from_slice(&self.ecu_address.to_be_bytes());
        payload.extend_from_slice(&tester.to_be_bytes());
        payload.extend_from_slice(uds);
        DoIpMessage::new(payload_type::DIAGNOSTIC_MESSAGE, payload)
    }

    /// Process one fully-received DoIP message from the client.
    fn handle_message(&mut self, ptype: u16, payload: &[u8]) {
        match ptype {
            payload_type::ROUTING_ACTIVATION_REQUEST => {
                // source(2) + type(1) + reserved(4).
                let tester = if payload.len() >= 2 {
                    u16::from_be_bytes([payload[0], payload[1]])
                } else {
                    0
                };
                // response: tester(2) + entity(2) + code(1) + reserved(4).
                let mut resp = Vec::with_capacity(9);
                resp.extend_from_slice(&tester.to_be_bytes());
                resp.extend_from_slice(&self.ecu_address.to_be_bytes());
                resp.push(routing_activation::RESPONSE_SUCCESS);
                resp.extend_from_slice(&[0u8; 4]);
                let msg = DoIpMessage::new(payload_type::ROUTING_ACTIVATION_RESPONSE, resp);
                self.enqueue(&msg);
            }
            payload_type::DIAGNOSTIC_MESSAGE => {
                // source(2) + target(2) + uds.
                if payload.len() < 4 {
                    return;
                }
                let tester = u16::from_be_bytes([payload[0], payload[1]]);
                let uds = &payload[4..];
                self.request_count += 1;
                self.trace.push(Direction::ClientRequest, uds, None);

                // DoIP positive ack (optional).
                if self.send_pos_ack {
                    let mut ack = Vec::with_capacity(5);
                    ack.extend_from_slice(&self.ecu_address.to_be_bytes());
                    ack.extend_from_slice(&tester.to_be_bytes());
                    ack.push(0x00);
                    let msg = DoIpMessage::new(payload_type::DIAGNOSTIC_MESSAGE_POS_ACK, ack);
                    self.enqueue(&msg);
                }

                let sid = uds.first().copied().unwrap_or(0);
                let resp = match self.faults.take(sid) {
                    Some(Fault::NegativeResponse(code)) => fault::negative(sid, code),
                    Some(Fault::ResponsePending { count }) => {
                        // Interpose real 0x78 diagnostic messages; the client's
                        // DoIP transport drains them without re-sending.
                        let pending = fault::negative(sid, 0x78);
                        for _ in 0..count {
                            self.trace.push(
                                Direction::ServerPending,
                                &pending,
                                Some("responsePending".into()),
                            );
                            let msg = self.diag_msg(tester, &pending);
                            self.enqueue(&msg);
                        }
                        self.server.handle(uds)
                    }
                    Some(other) => {
                        let canonical = self.server.handle(uds);
                        fault::transform_response(sid, canonical, other)
                    }
                    None => self.server.handle(uds),
                };

                self.trace.push(Direction::ServerResponse, &resp, None);
                let msg = self.diag_msg(tester, &resp);
                self.enqueue(&msg);
            }
            // Any other payload type (e.g. vehicle identification) is ignored by
            // this minimal entity; the client's read will time out, which is the
            // correct signal that this entity does not implement it.
            _ => {}
        }
    }

    /// Drain every complete DoIP message currently buffered.
    fn drain_inbound(&mut self) {
        loop {
            if self.inbound.len() < HEADER_LEN {
                return;
            }
            let (ptype, plen) = match DoIpMessage::parse_header(&self.inbound[..HEADER_LEN]) {
                Ok(v) => v,
                Err(_) => {
                    // Unparseable header: drop the buffer so we do not loop.
                    self.inbound.clear();
                    return;
                }
            };
            let total = HEADER_LEN + plen;
            if self.inbound.len() < total {
                return; // wait for the rest of the payload
            }
            let payload = self.inbound[HEADER_LEN..total].to_vec();
            self.inbound.drain(..total);
            self.handle_message(ptype, &payload);
        }
    }
}

impl DoIpStream for DoIpSimStream {
    fn write_all(&mut self, data: &[u8]) -> Result<()> {
        self.inbound.extend_from_slice(data);
        self.drain_inbound();
        Ok(())
    }

    fn read_exact(&mut self, buf: &mut [u8], timeout_ms: u64) -> Result<()> {
        if self.outbound.len() < buf.len() {
            // Nothing (more) to say: model a read timeout.
            return Err(Error::Timeout(timeout_ms));
        }
        for b in buf.iter_mut() {
            *b = self.outbound.pop_front().expect("length checked above");
        }
        Ok(())
    }
}
