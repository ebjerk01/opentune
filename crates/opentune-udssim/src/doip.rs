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
//! Entity-side ISO 13400-2 rules it enforces (checked against
//! python-doipclient in `tools/conformance/`): routing activation types
//! default / WWH-OBD / central security (others `0x06`), one tester per socket
//! (`0x02`), diagnostic messages only from the routed tester (diagnostic NACK
//! `0x02`) to this entity or the functional group (NACK `0x03`), generic header
//! NACKs for an incorrect pattern (`0x00`), an unknown payload type (`0x01`)
//! and a bad payload length (`0x04`), and alive checks in both directions. A
//! suppressed or lost UDS response sends no diagnostic message.
//!
//! Offline conformance only.
//!
//! [`DoIpStream`]: opentune_core::protocol::DoIpStream
//! [`DoIpTransport`]: opentune_core::protocol::DoIpTransport

use crate::fault::{self, Fault, FaultPlan};
use crate::server::UdsServer;
use crate::trace::{Direction, Trace};
use opentune_core::protocol::doip::{
    diag_nack, generic_nack, payload_type, routing_activation, DoIpMessage,
    FUNCTIONAL_GROUP_ADDRESS, HEADER_LEN,
};
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
    /// The tester address routing activation registered on this socket.
    routed_source: Option<u16>,
    /// Tester addresses received in alive-check responses (0x0008).
    alive_check_responses: Vec<u16>,
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
            routed_source: None,
            alive_check_responses: Vec::new(),
        }
    }

    /// The tester address routing activation registered, if any.
    pub fn routed_source(&self) -> Option<u16> {
        self.routed_source
    }

    /// Send an entity-initiated alive check request (0x0007) to the tester.
    /// Its answer (0x0008, the tester's address) is recorded in
    /// [`alive_check_responses`](Self::alive_check_responses).
    pub fn request_alive_check(&mut self) {
        self.enqueue(&DoIpMessage::new(
            payload_type::ALIVE_CHECK_REQUEST,
            Vec::new(),
        ));
    }

    /// Tester addresses received in alive check responses, in order.
    pub fn alive_check_responses(&self) -> &[u16] {
        &self.alive_check_responses
    }

    /// Take every byte queued for the tester (for a socket bridge that
    /// forwards whatever the entity has to say).
    pub fn take_outbound(&mut self) -> Vec<u8> {
        self.outbound.drain(..).collect()
    }

    /// Generic DoIP header negative acknowledge (0x0000) with `code`.
    fn generic_nack(&mut self, code: u8) {
        self.enqueue(&DoIpMessage::new(
            payload_type::GENERIC_NEGATIVE_ACK,
            vec![code],
        ));
    }

    /// Diagnostic message negative acknowledge (0x8003): `source` is the
    /// addressed (target) node, `target` the tester.
    fn diag_nack(&mut self, source: u16, target: u16, code: u8) {
        let mut p = Vec::with_capacity(5);
        p.extend_from_slice(&source.to_be_bytes());
        p.extend_from_slice(&target.to_be_bytes());
        p.push(code);
        self.enqueue(&DoIpMessage::new(
            payload_type::DIAGNOSTIC_MESSAGE_NEG_ACK,
            p,
        ));
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
                // source(2) + type(1) + reserved(4) [+ OEM-specific(4)].
                if payload.len() != 7 && payload.len() != 11 {
                    self.generic_nack(generic_nack::INVALID_PAYLOAD_LENGTH);
                    return;
                }
                let tester = u16::from_be_bytes([payload[0], payload[1]]);
                // Default, WWH-OBD and central security are supported.
                let supported = matches!(
                    payload[2],
                    routing_activation::TYPE_DEFAULT
                        | routing_activation::TYPE_WWH_OBD
                        | routing_activation::TYPE_CENTRAL_SECURITY
                );
                let code = if !supported {
                    routing_activation::RESPONSE_UNSUPPORTED_TYPE
                } else if self.routed_source.is_some_and(|r| r != tester) {
                    // This socket is already registered to another tester
                    // (ISO 13400-2 Table 25, code 0x02).
                    routing_activation::RESPONSE_SOURCE_IN_USE
                } else {
                    routing_activation::RESPONSE_SUCCESS
                };
                if code == routing_activation::RESPONSE_SUCCESS {
                    self.routed_source = Some(tester);
                }
                // response: tester(2) + entity(2) + code(1) + reserved(4).
                let mut resp = Vec::with_capacity(9);
                resp.extend_from_slice(&tester.to_be_bytes());
                resp.extend_from_slice(&self.ecu_address.to_be_bytes());
                resp.push(code);
                resp.extend_from_slice(&[0u8; 4]);
                let msg = DoIpMessage::new(payload_type::ROUTING_ACTIVATION_RESPONSE, resp);
                self.enqueue(&msg);
            }
            payload_type::ALIVE_CHECK_REQUEST => {
                // Tester-initiated alive check (python-doipclient's
                // `request_alive_check`): answer with this entity's address.
                let addr = self.ecu_address.to_be_bytes().to_vec();
                self.enqueue(&DoIpMessage::new(payload_type::ALIVE_CHECK_RESPONSE, addr));
            }
            payload_type::ALIVE_CHECK_RESPONSE => {
                // The tester's answer to an entity-initiated alive check.
                if payload.len() != 2 {
                    self.generic_nack(generic_nack::INVALID_PAYLOAD_LENGTH);
                    return;
                }
                self.alive_check_responses
                    .push(u16::from_be_bytes([payload[0], payload[1]]));
            }
            payload_type::DIAGNOSTIC_MESSAGE => {
                // source(2) + target(2) + uds (at least the SID).
                if payload.len() < 5 {
                    self.generic_nack(generic_nack::INVALID_PAYLOAD_LENGTH);
                    return;
                }
                let tester = u16::from_be_bytes([payload[0], payload[1]]);
                let target = u16::from_be_bytes([payload[2], payload[3]]);
                // ISO 13400-2 §7.3: the source must be the tester routing
                // activation registered on this socket ...
                if self.routed_source != Some(tester) {
                    self.diag_nack(target, tester, diag_nack::INVALID_SOURCE_ADDRESS);
                    return;
                }
                // ... and the target a node behind this entity.
                if target != self.ecu_address && target != FUNCTIONAL_GROUP_ADDRESS {
                    self.diag_nack(target, tester, diag_nack::UNKNOWN_TARGET_ADDRESS);
                    return;
                }
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
                    Some(Fault::NegativeResponse(code)) => Some(fault::negative(sid, code)),
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
                        self.server.respond(uds)
                    }
                    Some(other) => self
                        .server
                        .respond(uds)
                        .map(|canonical| fault::transform_response(sid, canonical, other)),
                    None => self.server.respond(uds),
                };

                // The ECU itself may answer responsePending first (a slow erase
                // in the flash model): interpose those as real diagnostic
                // messages too.
                let ecu_pending = self.server.take_pending();
                if ecu_pending > 0 {
                    let pending = fault::negative(sid, 0x78);
                    for _ in 0..ecu_pending {
                        self.trace.push(
                            Direction::ServerPending,
                            &pending,
                            Some("responsePending (ECU)".into()),
                        );
                        let msg = self.diag_msg(tester, &pending);
                        self.enqueue(&msg);
                    }
                }

                // No response at all (suppressPosRspMsgIndicationBit, a lost
                // response or a power loss in the flash model): send nothing.
                if let Some(resp) = resp {
                    self.trace.push(Direction::ServerResponse, &resp, None);
                    let msg = self.diag_msg(tester, &resp);
                    self.enqueue(&msg);
                }
            }
            // Vehicle identification (0x0001..=0x0003) belongs to the UDP
            // discovery path, which this TCP entity does not model: ignored, so
            // the client's read times out.
            payload_type::VEHICLE_ID_REQUEST
            | payload_type::VEHICLE_ID_REQUEST_EID
            | payload_type::VEHICLE_ID_REQUEST_VIN => {}
            // Any other payload type: generic header NACK unknownPayloadType.
            _ => self.generic_nack(generic_nack::UNKNOWN_PAYLOAD_TYPE),
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
                    // Incorrect pattern (version / inverse version): generic
                    // NACK 0x00, then drop the buffer (a real entity closes the
                    // socket) so we do not loop.
                    self.generic_nack(generic_nack::INCORRECT_PATTERN);
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

#[cfg(test)]
mod tests {
    use super::*;

    const TESTER: u16 = 0x0E00;
    const ECU: u16 = 0x0010;

    /// Send one DoIP message and return every message the entity answered.
    fn exchange(s: &mut DoIpSimStream, ptype: u16, payload: Vec<u8>) -> Vec<(u16, Vec<u8>)> {
        s.write_all(&DoIpMessage::new(ptype, payload).encode())
            .unwrap();
        let out = s.take_outbound();
        let mut msgs = Vec::new();
        let mut i = 0;
        while i < out.len() {
            let (t, len) = DoIpMessage::parse_header(&out[i..i + HEADER_LEN]).unwrap();
            msgs.push((t, out[i + HEADER_LEN..i + HEADER_LEN + len].to_vec()));
            i += HEADER_LEN + len;
        }
        msgs
    }

    fn activation(tester: u16, kind: u8) -> Vec<u8> {
        let mut p = tester.to_be_bytes().to_vec();
        p.push(kind);
        p.extend_from_slice(&[0; 4]);
        p
    }

    fn diag(tester: u16, target: u16, uds: &[u8]) -> Vec<u8> {
        let mut p = tester.to_be_bytes().to_vec();
        p.extend_from_slice(&target.to_be_bytes());
        p.extend_from_slice(uds);
        p
    }

    fn entity() -> DoIpSimStream {
        DoIpSimStream::new(UdsServer::new(), ECU)
    }

    #[test]
    fn routing_activation_types_and_socket_ownership() {
        let mut s = entity();
        let r = exchange(
            &mut s,
            payload_type::ROUTING_ACTIVATION_REQUEST,
            activation(TESTER, 0x02),
        );
        assert_eq!(r[0].1[4], routing_activation::RESPONSE_UNSUPPORTED_TYPE);
        assert_eq!(s.routed_source(), None);
        let r = exchange(
            &mut s,
            payload_type::ROUTING_ACTIVATION_REQUEST,
            activation(TESTER, 0x00),
        );
        assert_eq!(r[0].1[4], routing_activation::RESPONSE_SUCCESS);
        // Re-activation by the same tester is fine; another tester is refused.
        let r = exchange(
            &mut s,
            payload_type::ROUTING_ACTIVATION_REQUEST,
            activation(TESTER, 0x01),
        );
        assert_eq!(r[0].1[4], routing_activation::RESPONSE_SUCCESS);
        let r = exchange(
            &mut s,
            payload_type::ROUTING_ACTIVATION_REQUEST,
            activation(0x0E01, 0x00),
        );
        assert_eq!(r[0].1[4], routing_activation::RESPONSE_SOURCE_IN_USE);
        assert_eq!(s.routed_source(), Some(TESTER));
        // A malformed request length is a generic NACK.
        let r = exchange(
            &mut s,
            payload_type::ROUTING_ACTIVATION_REQUEST,
            vec![0x0E, 0x00],
        );
        assert_eq!(
            r,
            vec![(
                payload_type::GENERIC_NEGATIVE_ACK,
                vec![generic_nack::INVALID_PAYLOAD_LENGTH]
            )]
        );
    }

    #[test]
    fn diagnostic_messages_need_routing_and_a_known_target() {
        let mut s = entity();
        let r = exchange(
            &mut s,
            payload_type::DIAGNOSTIC_MESSAGE,
            diag(TESTER, ECU, &[0x3E, 0x00]),
        );
        assert_eq!(
            r,
            vec![(
                payload_type::DIAGNOSTIC_MESSAGE_NEG_ACK,
                vec![0x00, 0x10, 0x0E, 0x00, diag_nack::INVALID_SOURCE_ADDRESS]
            )]
        );
        exchange(
            &mut s,
            payload_type::ROUTING_ACTIVATION_REQUEST,
            activation(TESTER, 0x00),
        );
        let r = exchange(
            &mut s,
            payload_type::DIAGNOSTIC_MESSAGE,
            diag(TESTER, 0x0099, &[0x3E, 0x00]),
        );
        assert_eq!(r[0].0, payload_type::DIAGNOSTIC_MESSAGE_NEG_ACK);
        assert_eq!(r[0].1[4], diag_nack::UNKNOWN_TARGET_ADDRESS);
        let r = exchange(
            &mut s,
            payload_type::DIAGNOSTIC_MESSAGE,
            diag(TESTER, ECU, &[0x3E, 0x00]),
        );
        assert_eq!(r[0].0, payload_type::DIAGNOSTIC_MESSAGE_POS_ACK);
        assert_eq!(
            r[1],
            (
                payload_type::DIAGNOSTIC_MESSAGE,
                vec![0x00, 0x10, 0x0E, 0x00, 0x7E, 0x00]
            )
        );
        // Functional addressing reaches the entity too.
        let r = exchange(
            &mut s,
            payload_type::DIAGNOSTIC_MESSAGE,
            diag(TESTER, FUNCTIONAL_GROUP_ADDRESS, &[0x3E, 0x00]),
        );
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn suppressed_response_sends_only_the_ack() {
        let mut s = entity();
        exchange(
            &mut s,
            payload_type::ROUTING_ACTIVATION_REQUEST,
            activation(TESTER, 0x00),
        );
        let r = exchange(
            &mut s,
            payload_type::DIAGNOSTIC_MESSAGE,
            diag(TESTER, ECU, &[0x3E, 0x80]),
        );
        assert_eq!(r.len(), 1, "pos ack only: {r:?}");
        assert_eq!(r[0].0, payload_type::DIAGNOSTIC_MESSAGE_POS_ACK);
    }

    #[test]
    fn unknown_payload_type_and_bad_pattern_are_generic_nacks() {
        let mut s = entity();
        let r = exchange(&mut s, 0x1234, vec![]);
        assert_eq!(
            r,
            vec![(
                payload_type::GENERIC_NEGATIVE_ACK,
                vec![generic_nack::UNKNOWN_PAYLOAD_TYPE]
            )]
        );
        s.write_all(&[0x02, 0x02, 0x80, 0x01, 0, 0, 0, 0]).unwrap();
        let out = s.take_outbound();
        assert_eq!(
            &out[2..4],
            &payload_type::GENERIC_NEGATIVE_ACK.to_be_bytes()
        );
        assert_eq!(out[8], generic_nack::INCORRECT_PATTERN);
    }

    #[test]
    fn alive_check_both_directions() {
        let mut s = entity();
        let r = exchange(&mut s, payload_type::ALIVE_CHECK_REQUEST, vec![]);
        assert_eq!(
            r,
            vec![(
                payload_type::ALIVE_CHECK_RESPONSE,
                ECU.to_be_bytes().to_vec()
            )]
        );
        s.request_alive_check();
        assert_eq!(
            &s.take_outbound()[2..4],
            &payload_type::ALIVE_CHECK_REQUEST.to_be_bytes()
        );
        let r = exchange(
            &mut s,
            payload_type::ALIVE_CHECK_RESPONSE,
            TESTER.to_be_bytes().to_vec(),
        );
        assert!(r.is_empty());
        assert_eq!(s.alive_check_responses(), &[TESTER]);
    }

    #[test]
    fn flash_model_pending_is_interposed() {
        let mut server = UdsServer::new();
        server.enable_flash(crate::FlashConfig {
            erase_pending: 2,
            require_extended_session: false,
            ..crate::FlashConfig::default()
        });
        let mut s = DoIpSimStream::new(server, ECU);
        exchange(
            &mut s,
            payload_type::ROUTING_ACTIVATION_REQUEST,
            activation(TESTER, 0x00),
        );
        exchange(
            &mut s,
            payload_type::DIAGNOSTIC_MESSAGE,
            diag(TESTER, ECU, &[0x10, 0x02]),
        );
        let seed = exchange(
            &mut s,
            payload_type::DIAGNOSTIC_MESSAGE,
            diag(TESTER, ECU, &[0x27, 0x11]),
        );
        let mut key = vec![0x27, 0x12];
        key.extend(seed[1].1[6..].iter().map(|b| b ^ 0xA5));
        exchange(
            &mut s,
            payload_type::DIAGNOSTIC_MESSAGE,
            diag(TESTER, ECU, &key),
        );
        let erase = [0x31, 0x01, 0xFF, 0x00, 0x44, 0, 1, 0, 0, 0, 0, 0x10, 0];
        let r = exchange(
            &mut s,
            payload_type::DIAGNOSTIC_MESSAGE,
            diag(TESTER, ECU, &erase),
        );
        let uds: Vec<&[u8]> = r.iter().skip(1).map(|(_, p)| &p[4..]).collect();
        assert_eq!(
            uds,
            vec![
                &[0x7F, 0x31, 0x78][..],
                &[0x7F, 0x31, 0x78],
                &[0x71, 0x01, 0xFF, 0x00, 0x00]
            ]
        );
    }
}
