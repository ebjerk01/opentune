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

/// Map a core transport result to the [`RawDiag`] contract: a negative
/// response a transport surfaced as [`Error::NegativeResponse`](crate::Error)
/// comes back as its raw `7F <sid> <nrc>` bytes (the ECU *answered*), an
/// error that means the link is gone ([`is_link_lost`](crate::resilient::is_link_lost),
/// e.g. an adapter reset) as its message behind
/// [`LINK_LOST_PREFIX`](crate::resilient::LINK_LOST_PREFIX), every other
/// error as its message.
fn raw_result(r: crate::Result<Vec<u8>>) -> std::result::Result<Vec<u8>, String> {
    match r {
        Ok(v) => Ok(v),
        Err(crate::Error::NegativeResponse { service, code, .. }) => Ok(vec![0x7F, service, code]),
        Err(e) if crate::resilient::is_link_lost(&e) => {
            Err(format!("{}{e}", crate::resilient::LINK_LOST_PREFIX))
        }
        Err(e) => Err(e.to_string()),
    }
}

/// An **owning** [`RawDiag`] over a boxed core
/// [`DiagTransport`](crate::protocol::DiagTransport) — e.g. an
/// [`IsoTp`](crate::protocol::IsoTp) over an embedder's own
/// [`CanLink`](crate::protocol::CanLink). Unlike [`CoreDiag`] it does not
/// borrow, so a reopen hook can hand out a fresh one
/// ([`run_resilient`](crate::resilient::run_resilient)). Negative responses
/// are returned as raw `7F` bytes, as the [`RawDiag`] contract says.
pub struct OwnedDiag {
    inner: Box<dyn crate::protocol::DiagTransport>,
    timeout_ms: u64,
}

impl OwnedDiag {
    /// Own `inner`, issuing every request with `timeout_ms`.
    pub fn new(inner: Box<dyn crate::protocol::DiagTransport>, timeout_ms: u64) -> Self {
        Self { inner, timeout_ms }
    }
}

impl RawDiag for OwnedDiag {
    fn request(&mut self, req: &[u8]) -> std::result::Result<Vec<u8>, String> {
        raw_result(self.inner.request(req, self.timeout_ms))
    }
}

/// An **owning** [`RawDiag`] over an opened [`Interface`](crate::Interface)
/// driver's [`diag`](crate::Interface::diag) transport. Dropping it closes
/// the driver, so a resilient log that drops a lost link releases the
/// hardware before re-opening it. Negative responses are returned as raw
/// `7F` bytes, as the [`RawDiag`] contract says.
pub struct InterfaceDiag {
    drv: Box<dyn crate::Interface>,
    timeout_ms: u64,
}

impl InterfaceDiag {
    /// Own the opened driver `drv`. Errors (and closes it) when the driver
    /// does not expose a diagnostic transport.
    pub fn new(mut drv: Box<dyn crate::Interface>, timeout_ms: u64) -> crate::Result<Self> {
        if drv.diag().is_none() {
            let _ = drv.close();
            return Err(crate::Error::Unsupported(
                "driver does not expose a diagnostic transport".into(),
            ));
        }
        Ok(Self { drv, timeout_ms })
    }

    /// The driver (e.g. for its battery voltage).
    pub fn interface(&mut self) -> &mut dyn crate::Interface {
        self.drv.as_mut()
    }
}

impl RawDiag for InterfaceDiag {
    fn request(&mut self, req: &[u8]) -> std::result::Result<Vec<u8>, String> {
        match self.drv.diag() {
            Some(t) => raw_result(t.request(req, self.timeout_ms)),
            None => Err("driver no longer exposes a diagnostic transport".into()),
        }
    }
}

impl Drop for InterfaceDiag {
    fn drop(&mut self) {
        let _ = self.drv.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Nrc;
    impl crate::protocol::DiagTransport for Nrc {
        fn request(&mut self, req: &[u8], _t: u64) -> crate::Result<Vec<u8>> {
            match req[0] {
                0x22 => Err(crate::Error::NegativeResponse {
                    service: 0x22,
                    code: 0x31,
                    name: "requestOutOfRange",
                }),
                0x3E => Ok(vec![0x7E, 0x00]),
                _ => Err(crate::Error::Timeout(5)),
            }
        }
    }

    #[test]
    fn owned_diag_keeps_negative_responses_as_bytes() {
        let mut d = OwnedDiag::new(Box::new(Nrc), 5);
        assert_eq!(d.request(&[0x22, 0xF1, 0x90]).unwrap(), [0x7F, 0x22, 0x31]);
        assert_eq!(d.request(&[0x3E, 0x00]).unwrap(), [0x7E, 0x00]);
        assert!(d.request(&[0x10]).unwrap_err().contains("timed out"));
    }
}
