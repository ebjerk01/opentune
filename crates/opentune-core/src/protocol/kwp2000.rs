//! KWP2000 (ISO 14230) service client over a [`DiagTransport`]. Used for older
//! ECUs (EDC15/EDC16 over K-line or VW TP2.0).
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
            Err(Error::NegativeResponse { code: 0x35, .. }) => Err(Error::SecurityDenied),
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
