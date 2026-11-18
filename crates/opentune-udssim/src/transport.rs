//! [`SimTransport`]: drives [`UdsServer`] as an `opentune_core` [`DiagTransport`].
//!
//! This is the plug the client talks to. Its [`request`](DiagTransport::request)
//! records the request, consults the [`FaultPlan`], runs the ECU, applies any
//! output-shaping fault, records the response, and returns it. It is the UDS
//! equivalent of a real ISO-TP / K-line transport, so `opentune_core`'s
//! [`UdsClient`] works against it unmodified.
//!
//! Draining of NRC 0x78 (`responsePending`) is a transport responsibility in
//! OpenTune (the client never re-sends — see the `UdsClient` module docs). So a
//! [`Fault::ResponsePending`] here records the interposed 0x78 frames in the
//! trace and still returns the one real answer within the single `request` call,
//! modelling a correct transport that drained them. The per-request count the
//! client issued is observable via [`SimTransport::request_count`], which stays
//! at 1 for such an exchange — pinning that the client did not duplicate the
//! request.
//!
//! [`DiagTransport`]: opentune_core::protocol::DiagTransport
//! [`UdsClient`]: opentune_core::protocol::UdsClient
//! [`Fault::ResponsePending`]: crate::fault::Fault::ResponsePending

use crate::fault::{self, Fault, FaultPlan};
use crate::server::UdsServer;
use crate::trace::{Direction, Trace};
use opentune_core::protocol::DiagTransport;
use opentune_core::Result;

/// A [`DiagTransport`] backed by an in-memory [`UdsServer`], with fault injection
/// and trace recording.
pub struct SimTransport {
    server: UdsServer,
    faults: FaultPlan,
    trace: Trace,
    request_count: usize,
}

impl SimTransport {
    /// Wrap a server as a transport (no faults).
    pub fn new(server: UdsServer) -> Self {
        Self {
            server,
            faults: FaultPlan::new(),
            trace: Trace::new(),
            request_count: 0,
        }
    }

    /// The fault plan, to schedule misbehaviours before driving the client.
    pub fn faults(&mut self) -> &mut FaultPlan {
        &mut self.faults
    }

    /// The simulated ECU (for seeding DIDs / routines / DTCs and assertions).
    pub fn server(&self) -> &UdsServer {
        &self.server
    }

    /// The simulated ECU, mutably.
    pub fn server_mut(&mut self) -> &mut UdsServer {
        &mut self.server
    }

    /// The ordered request/response trace.
    pub fn trace(&self) -> &Trace {
        &self.trace
    }

    /// How many requests the client issued (one `DiagTransport::request` call
    /// each). A `responsePending` drain does **not** increment this: the request
    /// is still issued exactly once.
    pub fn request_count(&self) -> usize {
        self.request_count
    }
}

impl DiagTransport for SimTransport {
    fn request(&mut self, req: &[u8], timeout_ms: u64) -> Result<Vec<u8>> {
        self.request_count += 1;
        self.trace.push(Direction::ClientRequest, req, None);
        let sid = req.first().copied().unwrap_or(0);
        let pending_frame = fault::negative(sid, 0x78);

        let resp = match self.faults.take(sid) {
            Some(Fault::NegativeResponse(code)) => {
                // The ECU refused: do not run the service.
                Some(fault::negative(sid, code))
            }
            Some(Fault::ResponsePending { count }) => {
                // Interpose `count` 0x78 frames (a correct transport drains them
                // without re-sending), then return the one real answer.
                for _ in 0..count {
                    self.trace.push(
                        Direction::ServerPending,
                        &pending_frame,
                        Some("responsePending".into()),
                    );
                }
                self.server.respond(req)
            }
            Some(other) => {
                // WrongEcho / Truncate / MisCorrelate: shape the real response.
                self.server
                    .respond(req)
                    .map(|canonical| fault::transform_response(sid, canonical, other))
            }
            None => self.server.respond(req),
        };

        // The ECU itself may answer responsePending first (a slow erase in the
        // flash model); a correct transport drains those too.
        for _ in 0..self.server.take_pending() {
            self.trace.push(
                Direction::ServerPending,
                &pending_frame,
                Some("responsePending (ECU)".into()),
            );
        }

        match resp {
            Some(resp) => {
                self.trace.push(Direction::ServerResponse, &resp, None);
                Ok(resp)
            }
            // No response at all (lost frame / power loss): the tester times out.
            None => Err(opentune_core::Error::Timeout(timeout_ms)),
        }
    }
}
