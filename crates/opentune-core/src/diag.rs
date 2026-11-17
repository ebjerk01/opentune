//! The one tiny request→response seam every service engine (coding, adaptation,
//! auto-scan, service functions, …) runs on.
//!
//! An engine stays standalone over [`RawDiag`] and the application wraps a real
//! driver's [`DiagTransport`](crate::protocol::DiagTransport) in [`CoreDiag`] to
//! drive it. Keeping this trait here — rather than one copy per engine — means a
//! single adapter satisfies every engine at once.

/// One UDS request → response exchange.
///
/// Implementations return the raw response bytes **including** a `0x7F` negative
/// response, which each engine classifies itself. Hard transport failures
/// (timeout, I/O) are returned as the `String` error.
pub trait RawDiag {
    /// Send `req` and return the ECU's response bytes.
    fn request(&mut self, req: &[u8]) -> std::result::Result<Vec<u8>, String>;
}

/// Blanket impl so a plain closure can be used as a [`RawDiag`].
impl<F> RawDiag for F
where
    F: FnMut(&[u8]) -> std::result::Result<Vec<u8>, String>,
{
    fn request(&mut self, req: &[u8]) -> std::result::Result<Vec<u8>, String> {
        self(req)
    }
}

/// Adapter that runs a [`RawDiag`] request over a core
/// [`DiagTransport`](crate::protocol::DiagTransport).
///
/// This is the integration seam: the engine stays standalone over the tiny
/// [`RawDiag`] abstraction, and the application wraps a real driver's transport
/// in `CoreDiag` to drive it. The core transport already handles ISO-TP framing
/// and the UDS `responsePending` (0x78) loop, so each `request` returns the final
/// raw response bytes — including a `0x7F` negative response. Hard transport
/// failures are mapped from the core [`Error`](crate::Error) to the `String`
/// that [`RawDiag`] uses.
pub struct CoreDiag<'a> {
    inner: &'a mut dyn crate::protocol::DiagTransport,
    timeout_ms: u64,
}

impl<'a> CoreDiag<'a> {
    /// Wrap a core [`DiagTransport`](crate::protocol::DiagTransport), issuing
    /// every request with `timeout_ms`.
    pub fn new(inner: &'a mut dyn crate::protocol::DiagTransport, timeout_ms: u64) -> Self {
        CoreDiag { inner, timeout_ms }
    }
}

impl RawDiag for CoreDiag<'_> {
    fn request(&mut self, req: &[u8]) -> std::result::Result<Vec<u8>, String> {
        self.inner
            .request(req, self.timeout_ms)
            .map_err(|e| e.to_string())
    }
}
