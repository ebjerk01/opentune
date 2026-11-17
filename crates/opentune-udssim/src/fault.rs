//! Programmable fault injection.
//!
//! A [`FaultPlan`] is an ordered list of scheduled faults. Each one is matched
//! against outgoing requests (by service id, after an optional number of
//! matching requests have passed) and fires **once**. The transport consults the
//! plan per request and applies the first unfired, matching fault, so a test can
//! deterministically make the simulated ECU misbehave in exactly one defined
//! way at exactly one point in a flow.
//!
//! The faults model the failure modes OpenTune's client validation and deadline
//! logic must survive:
//!
//! - [`Fault::ResponsePending`] — the ECU answers NRC 0x78 (`responsePending`)
//!   `count` times before the real answer.
//! - [`Fault::NegativeResponse`] — the ECU refuses with a specific NRC instead of
//!   doing the work.
//! - [`Fault::WrongEcho`] — the ECU corrupts the field the client correlates on
//!   (the echoed DID / sub-function / block-sequence-counter).
//! - [`Fault::Truncate`] — the ECU returns a response shorter than the service
//!   requires.
//! - [`Fault::MisCorrelate`] — the ECU answers a *different* service's negative
//!   response (a reply correlated to the wrong request).

/// A single injectable misbehaviour. See the module docs for the failure mode
/// each one reproduces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// Interpose `count` NRC 0x78 (`responsePending`) frames before the real,
    /// computed answer. The request is still processed exactly once.
    ResponsePending {
        /// How many `responsePending` frames to interpose.
        count: usize,
    },
    /// Replace the response with the negative response `7F <sid> <nrc>`. The
    /// request is **not** processed (the ECU refused it).
    NegativeResponse(u8),
    /// Corrupt the byte the client correlates on in the positive response (the
    /// echoed DID / sub-function / routine-id / block counter).
    WrongEcho,
    /// Truncate the positive response to `len` bytes.
    Truncate(usize),
    /// Answer with a negative response whose echoed service id does **not** match
    /// the request, i.e. a reply correlated to another request.
    MisCorrelate,
}

/// One scheduled fault: which service it waits for, how many matching requests
/// to let pass first, and whether it has already fired.
#[derive(Debug, Clone)]
struct Scheduled {
    on_service: Option<u8>,
    skip: usize,
    fired: bool,
    fault: Fault,
}

/// An ordered set of scheduled faults consulted by the transports.
#[derive(Debug, Clone, Default)]
pub struct FaultPlan {
    scheduled: Vec<Scheduled>,
}

impl FaultPlan {
    /// An empty plan (the ECU behaves correctly).
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the plan holds no faults.
    pub fn is_empty(&self) -> bool {
        self.scheduled.is_empty()
    }

    /// Schedule `fault` to fire on the first request for `service`
    /// (`None` = any service), after `skip` earlier matching requests.
    pub fn schedule(&mut self, service: Option<u8>, skip: usize, fault: Fault) -> &mut Self {
        self.scheduled.push(Scheduled {
            on_service: service,
            skip,
            fired: false,
            fault,
        });
        self
    }

    /// Emit `count` NRC 0x78 `responsePending` frames before the real answer to
    /// the next request for `service`.
    pub fn response_pending(&mut self, service: u8, count: usize) -> &mut Self {
        self.schedule(Some(service), 0, Fault::ResponsePending { count })
    }

    /// Refuse the next request for `service` with NRC `nrc`.
    pub fn negative_response(&mut self, service: u8, nrc: u8) -> &mut Self {
        self.schedule(Some(service), 0, Fault::NegativeResponse(nrc))
    }

    /// Corrupt the correlation echo in the next response for `service`.
    pub fn wrong_echo(&mut self, service: u8) -> &mut Self {
        self.schedule(Some(service), 0, Fault::WrongEcho)
    }

    /// Truncate the next response for `service` to `len` bytes.
    pub fn truncate(&mut self, service: u8, len: usize) -> &mut Self {
        self.schedule(Some(service), 0, Fault::Truncate(len))
    }

    /// Answer the next request for `service` with a mis-correlated negative
    /// response (echoing a different service id).
    pub fn mis_correlate(&mut self, service: u8) -> &mut Self {
        self.schedule(Some(service), 0, Fault::MisCorrelate)
    }

    /// Return the fault to apply to a request for `req_sid`, if any, marking it
    /// fired. Faults are considered in schedule order; the first matching,
    /// unfired one whose `skip` has elapsed fires.
    pub(crate) fn take(&mut self, req_sid: u8) -> Option<Fault> {
        for s in &mut self.scheduled {
            if s.fired {
                continue;
            }
            if let Some(svc) = s.on_service {
                if svc != req_sid {
                    continue;
                }
            }
            if s.skip > 0 {
                s.skip -= 1;
                continue;
            }
            s.fired = true;
            return Some(s.fault);
        }
        None
    }
}

/// The byte index of the correlation echo in a positive response for `sid`
/// (the first byte after the service id that the client checks). Used by
/// [`Fault::WrongEcho`].
fn echo_index(sid: u8) -> usize {
    match sid {
        // routineControl: 71 <type> <rid hi> <rid lo> — corrupt the routine id.
        0x31 => 3,
        // Everything else the client correlates on the byte right after the SID
        // (session/reset/security level echo, DID high byte, block counter).
        _ => 1,
    }
}

/// Apply a *post-computation* fault that transforms the ECU's canonical
/// response. `ResponsePending` and `NegativeResponse` are handled by the
/// transport before the server runs, so only the output-shaping faults reach
/// here. Returns the mutated bytes.
pub(crate) fn transform_response(req_sid: u8, mut resp: Vec<u8>, fault: Fault) -> Vec<u8> {
    match fault {
        Fault::WrongEcho => {
            let i = echo_index(req_sid);
            if let Some(b) = resp.get_mut(i) {
                *b ^= 0xFF;
            }
            resp
        }
        Fault::Truncate(len) => {
            resp.truncate(len);
            resp
        }
        Fault::MisCorrelate => {
            // A negative response echoing a *different* service id than the one
            // requested. The client's check_response has a dedicated branch for
            // this (echoed sid != request sid).
            vec![0x7F, req_sid ^ 0x01, 0x10]
        }
        // Handled earlier by the transport; never reaches here.
        Fault::ResponsePending { .. } | Fault::NegativeResponse(_) => resp,
    }
}

/// Build the negative response `7F <sid> <nrc>`.
pub(crate) fn negative(sid: u8, nrc: u8) -> Vec<u8> {
    vec![0x7F, sid, nrc]
}
