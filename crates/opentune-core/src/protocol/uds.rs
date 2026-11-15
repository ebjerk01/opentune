//! UDS (ISO 14229) service client over a [`DiagTransport`].
//!
//! Each method sends a request and returns the response payload *after* the
//! positive-response service id. Negative responses become [`Error`]s.
use crate::protocol::diag::{check_response, DiagTransport};
use crate::{Error, Result};

pub mod sid {
    pub const SESSION_CONTROL: u8 = 0x10;
    pub const ECU_RESET: u8 = 0x11;
    pub const SECURITY_ACCESS: u8 = 0x27;
    pub const READ_DATA_BY_ID: u8 = 0x22;
    pub const WRITE_DATA_BY_ID: u8 = 0x2E;
    pub const ROUTINE_CONTROL: u8 = 0x31;
    pub const REQUEST_DOWNLOAD: u8 = 0x34;
    pub const REQUEST_UPLOAD: u8 = 0x35;
    pub const TRANSFER_DATA: u8 = 0x36;
    pub const TRANSFER_EXIT: u8 = 0x37;
    pub const TESTER_PRESENT: u8 = 0x3E;
}

pub struct UdsClient<'a> {
    pub transport: &'a mut dyn DiagTransport,
    pub timeout_ms: u64,
}

impl<'a> UdsClient<'a> {
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

    pub fn session(&mut self, session: u8) -> Result<Vec<u8>> {
        self.call(&[sid::SESSION_CONTROL, session])
    }

    pub fn ecu_reset(&mut self, kind: u8) -> Result<Vec<u8>> {
        self.call(&[sid::ECU_RESET, kind])
    }

    pub fn tester_present(&mut self) -> Result<()> {
        self.call(&[sid::TESTER_PRESENT, 0x00]).map(|_| ())
    }

    pub fn read_data_by_id(&mut self, did: u16) -> Result<Vec<u8>> {
        let resp = self.call(&[sid::READ_DATA_BY_ID, (did >> 8) as u8, did as u8])?;
        // resp echoes the 2-byte DID, then the data.
        if resp.len() < 2 {
            return Err(Error::Protocol("short RDBI response".into()));
        }
        Ok(resp[2..].to_vec())
    }

    /// Request a seed at an odd access level (e.g. 0x11).
    pub fn request_seed(&mut self, level: u8) -> Result<Vec<u8>> {
        let resp = self.call(&[sid::SECURITY_ACCESS, level])?;
        if resp.is_empty() {
            return Err(Error::Protocol("empty seed".into()));
        }
        Ok(resp[1..].to_vec()) // drop the echoed level
    }

    /// Send a key at an even access level (seed level + 1).
    pub fn send_key(&mut self, level: u8, key: &[u8]) -> Result<()> {
        let mut req = vec![sid::SECURITY_ACCESS, level];
        req.extend_from_slice(key);
        match self.call(&req) {
            Ok(_) => Ok(()),
            Err(Error::NegativeResponse { code: 0x35, .. }) => Err(Error::SecurityDenied),
            Err(e) => Err(e),
        }
    }

    pub fn routine_control(&mut self, kind: u8, routine: u16, data: &[u8]) -> Result<Vec<u8>> {
        let mut req = vec![
            sid::ROUTINE_CONTROL,
            kind,
            (routine >> 8) as u8,
            routine as u8,
        ];
        req.extend_from_slice(data);
        self.call(&req)
    }

    /// `requestDownload`: returns the ECU's maximum block length.
    pub fn request_download(&mut self, data_format: u8, address: u32, size: u32) -> Result<usize> {
        let mut req = vec![sid::REQUEST_DOWNLOAD, data_format, 0x44];
        req.extend_from_slice(&address.to_be_bytes());
        req.extend_from_slice(&size.to_be_bytes());
        let resp = self.call(&req)?;
        parse_max_block_len(&resp)
    }

    pub fn request_upload(&mut self, data_format: u8, address: u32, size: u32) -> Result<usize> {
        let mut req = vec![sid::REQUEST_UPLOAD, data_format, 0x44];
        req.extend_from_slice(&address.to_be_bytes());
        req.extend_from_slice(&size.to_be_bytes());
        let resp = self.call(&req)?;
        parse_max_block_len(&resp)
    }

    /// `transferData` with a block sequence counter.
    pub fn transfer_data(&mut self, seq: u8, block: &[u8]) -> Result<Vec<u8>> {
        let mut req = vec![sid::TRANSFER_DATA, seq];
        req.extend_from_slice(block);
        self.call(&req)
    }

    pub fn transfer_exit(&mut self) -> Result<Vec<u8>> {
        self.call(&[sid::TRANSFER_EXIT])
    }
}

fn parse_max_block_len(resp: &[u8]) -> Result<usize> {
    // lengthFormatIdentifier high nibble = number of maxNumberOfBlockLength bytes.
    if resp.is_empty() {
        return Err(Error::Protocol("short requestDownload response".into()));
    }
    let n = (resp[0] >> 4) as usize;
    if resp.len() < 1 + n || n == 0 {
        return Err(Error::Protocol("bad maxNumberOfBlockLength".into()));
    }
    let mut v = 0usize;
    for b in &resp[1..1 + n] {
        v = (v << 8) | *b as usize;
    }
    Ok(v)
}
