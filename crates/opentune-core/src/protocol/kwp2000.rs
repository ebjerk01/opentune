//! KWP2000 (ISO 14230) service client over a [`DiagTransport`]. Used for older
//! ECUs (EDC15/EDC16 over K-line or VW TP2.0).
//!
//! ## Timing (ISO 14230-2, informed by freediag `diag_l2_iso14230.c`)
//!
//! KWP2000 is timing-sensitive; the numbers below are the transport's job, but
//! this client's `timeout_ms` (default 2000 ms) must stay ≥ P2max so a slow ECU
//! is not cut off:
//!
//! * **P1** ≤ 20 ms — inter-byte time within an ECU response (K-line only).
//! * **P2min 25 ms / P2max 50 ms** — gap between a request and the start of the
//!   ECU response. An ECU that needs longer must answer with NRC 0x78
//!   (`responsePending`), which [`DiagTransport`] implementations loop on.
//! * **P3min 55 ms / P3max 5000 ms** — gap between the end of a response and the
//!   next request. `TesterPresent` (0x3E) must be sent inside P3max to keep the
//!   session alive. P3max also bounds how long a channel may sit idle.
//! * **P4min 5 ms** — inter-byte time within a tester request (K-line only).
//!
//! On CAN (VW TP2.0) the byte-level P1/P4 do not apply — the transport frames
//! whole messages — but P2/P3 still gate request/response and keep-alive.
//! NRC 0x21 (`busyRepeatRequest`) means "retry the identical request later".
use crate::protocol::diag::{check_response, DiagTransport};
use crate::{Error, Result};

pub mod sid {
    pub const START_DIAG_SESSION: u8 = 0x10;
    pub const ECU_RESET: u8 = 0x11;
    pub const READ_ECU_ID: u8 = 0x1A;
    pub const SECURITY_ACCESS: u8 = 0x27;
    pub const READ_MEMORY_BY_ADDRESS: u8 = 0x23;
    pub const WRITE_MEMORY_BY_ADDRESS: u8 = 0x3D;
    pub const REQUEST_DOWNLOAD: u8 = 0x34;
    pub const REQUEST_UPLOAD: u8 = 0x35;
    pub const TRANSFER_DATA: u8 = 0x36;
    pub const TRANSFER_EXIT: u8 = 0x37;
    pub const START_ROUTINE: u8 = 0x31;
    pub const TESTER_PRESENT: u8 = 0x3E;
}

pub struct Kwp2000Client<'a> {
    pub transport: &'a mut dyn DiagTransport,
    pub timeout_ms: u64,
}

impl<'a> Kwp2000Client<'a> {
    pub fn new(transport: &'a mut dyn DiagTransport) -> Self {
        Self {
            transport,
            timeout_ms: 2000,
        }
    }

    fn call(&mut self, req: &[u8]) -> Result<Vec<u8>> {
        let resp = self.transport.request(req, self.timeout_ms)?;
        check_response(req[0], &resp)?;
        Ok(resp[1..].to_vec())
    }

    pub fn start_session(&mut self, session: u8) -> Result<Vec<u8>> {
        self.call(&[sid::START_DIAG_SESSION, session])
    }

    pub fn tester_present(&mut self) -> Result<()> {
        self.call(&[sid::TESTER_PRESENT, 0x01]).map(|_| ())
    }

    pub fn read_ecu_id(&mut self, option: u8) -> Result<Vec<u8>> {
        self.call(&[sid::READ_ECU_ID, option])
    }

    pub fn request_seed(&mut self, level: u8) -> Result<Vec<u8>> {
        let resp = self.call(&[sid::SECURITY_ACCESS, level])?;
        if resp.is_empty() {
            return Err(Error::Protocol("empty seed".into()));
        }
        Ok(resp[1..].to_vec())
    }

    pub fn send_key(&mut self, level: u8, key: &[u8]) -> Result<()> {
        let mut req = vec![sid::SECURITY_ACCESS, level];
        req.extend_from_slice(key);
        match self.call(&req) {
            Ok(_) => Ok(()),
            // invalidKey (0x35) or exceededNumberOfAttempts (0x36): failed unlock.
            Err(Error::NegativeResponse {
                code: 0x35 | 0x36, ..
            }) => Err(Error::SecurityDenied),
            Err(e) => Err(e),
        }
    }

    /// `readMemoryByAddress` (3-byte address, 1-byte length — common EDC15/16 form).
    pub fn read_memory(&mut self, address: u32, len: u8) -> Result<Vec<u8>> {
        let a = address.to_be_bytes();
        self.call(&[sid::READ_MEMORY_BY_ADDRESS, a[1], a[2], a[3], len])
    }

    pub fn request_download(&mut self, address: u32, size: u32, format: u8) -> Result<Vec<u8>> {
        let a = address.to_be_bytes();
        let s = size.to_be_bytes();
        self.call(&[
            sid::REQUEST_DOWNLOAD,
            a[1],
            a[2],
            a[3],
            format,
            s[1],
            s[2],
            s[3],
        ])
    }

    pub fn transfer_data(&mut self, block: &[u8]) -> Result<Vec<u8>> {
        let mut req = vec![sid::TRANSFER_DATA];
        req.extend_from_slice(block);
        self.call(&req)
    }

    pub fn transfer_exit(&mut self) -> Result<Vec<u8>> {
        self.call(&[sid::TRANSFER_EXIT])
    }

    pub fn start_routine(&mut self, routine: u8, data: &[u8]) -> Result<Vec<u8>> {
        let mut req = vec![sid::START_ROUTINE, routine];
        req.extend_from_slice(data);
        self.call(&req)
    }
}
