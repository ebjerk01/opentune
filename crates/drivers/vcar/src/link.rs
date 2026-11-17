//! [`VcarLink`]: the tester's side of the virtual CAN bus, as a core
//! [`CanLink`]. The driver stacks the core's own [`IsoTp`](opentune_core::protocol::IsoTp)
//! on top of it, so every request crosses the same ISO-TP framing (single /
//! first / consecutive / flow-control frames) a real adapter would carry.
use crate::SharedVehicle;
use opentune_core::protocol::CanLink;
use opentune_core::{Error, Result};
use std::collections::VecDeque;

/// A tester attached to the virtual bus. Frames the modules answer with are
/// queued here; an empty queue is an immediate timeout (the simulation never
/// sleeps), which is how an absent module looks.
pub struct VcarLink {
    vehicle: SharedVehicle,
    inbox: VecDeque<(u32, Vec<u8>)>,
}

impl VcarLink {
    /// Attach a tester to `vehicle`'s bus.
    pub fn new(vehicle: SharedVehicle) -> Self {
        Self {
            vehicle,
            inbox: VecDeque::new(),
        }
    }
}

impl CanLink for VcarLink {
    fn send_frame(&mut self, id: u32, data: &[u8]) -> Result<()> {
        if data.len() > 8 {
            return Err(Error::Protocol(format!(
                "classic CAN frame of {} bytes",
                data.len()
            )));
        }
        let out = self
            .vehicle
            .lock()
            .map_err(|_| Error::Protocol("virtual car state poisoned".into()))?
            .on_frame(id, data);
        self.inbox.extend(out);
        Ok(())
    }

    fn recv_frame(&mut self, timeout_ms: u64) -> Result<(u32, Vec<u8>)> {
        self.inbox.pop_front().ok_or(Error::Timeout(timeout_ms))
    }
}
