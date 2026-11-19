//! Raw K-line byte access: the driver capability behind protocols that frame
//! their own packets on an ISO 9141 physical layer, such as Subaru Select
//! Monitor 2 ([`ssm`](crate::protocol::ssm)).
//!
//! The K-line drivers normally hide the bus behind a KWP2000
//! [`DiagTransport`](crate::protocol::DiagTransport): they wake the ECU (fast
//! or 5-baud init), add the ISO 14230 header and checksum and remove the echo.
//! SSM2 needs none of that: no wake-up, its own `80 <dst> <src> <len>` header
//! and 4800 baud. A driver that can hand the bus out *unframed* implements
//! [`KLineRaw`] and returns it from
//! [`Interface::kline`](crate::Interface::kline).
//!
//! The contract is deliberately small:
//!
//! * [`write`](KLineRaw::write) puts bytes on the bus as one burst (no
//!   framing, no checksum added);
//! * [`read`](KLineRaw::read) returns the bytes that came back, waiting at most
//!   the given time for the first one;
//! * [`echoes`](KLineRaw::echoes) says whether those bytes still include the
//!   tester's own transmission. K-line is half duplex, so a dumb KKL cable
//!   hears itself and the protocol layer must read and check the echo; an
//!   adapter that strips it on board (the OpenTune K-line bridge) says `false`.
//!
//! The protocol layer owns everything else: framing, checksums, echo
//! verification and reply timing.
use crate::Result;

/// Unframed access to a K-line (ISO 9141 physical layer).
pub trait KLineRaw {
    /// Set the bus baud rate (8N1). SSM2 runs at 4800.
    fn set_baud(&mut self, baud: u32) -> Result<()>;

    /// Drop anything already received: stale replies, noise, a late echo.
    fn clear_input(&mut self) -> Result<()>;

    /// Transmit `bytes` as one burst. Nothing is added or removed.
    fn write(&mut self, bytes: &[u8]) -> Result<()>;

    /// Read up to `buf.len()` received bytes. Waits at most `timeout_ms` for
    /// the first byte and returns how many were stored (at least 1).
    /// [`Error::Timeout`](crate::Error::Timeout) when nothing arrived in time.
    fn read(&mut self, buf: &mut [u8], timeout_ms: u64) -> Result<usize>;

    /// `true` when [`read`](Self::read) also returns the tester's own
    /// transmitted bytes (the half-duplex echo), which the protocol layer must
    /// then read back and compare before the reply.
    fn echoes(&self) -> bool;

    /// Largest burst [`write`](Self::write) accepts, if the adapter has a
    /// limit (a USB frame buffer, say). `None` = no limit.
    fn max_write(&self) -> Option<usize> {
        None
    }
}

/// Read exactly `buf.len()` bytes from `link` before `deadline`.
///
/// Times out as [`Error::Timeout`](crate::Error::Timeout) carrying
/// `timeout_ms` when the bytes do not all arrive.
pub fn read_exact(
    link: &mut (impl KLineRaw + ?Sized),
    buf: &mut [u8],
    deadline: std::time::Instant,
    timeout_ms: u64,
) -> Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err(crate::Error::Timeout(timeout_ms));
        }
        let wait = left.as_millis().max(1) as u64;
        match link.read(&mut buf[filled..], wait) {
            Ok(n) => filled += n,
            Err(crate::Error::Timeout(_)) => return Err(crate::Error::Timeout(timeout_ms)),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
