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
    /// `readDataByLocalIdentifier` — on VAG KWP-on-TP2.0 ECUs this reads a
    /// *measuring block* ("Messwertblock") group.
    pub const READ_DATA_BY_LOCAL_ID: u8 = 0x21;
    /// `readDiagnosticTroubleCodesByStatus`.
    pub const READ_DTC_BY_STATUS: u8 = 0x18;
    /// `clearDiagnosticInformation`.
    pub const CLEAR_DIAGNOSTIC_INFO: u8 = 0x14;
}

/// One stored trouble code from [`Kwp2000Client::read_dtcs_by_status`]: the
/// 16-bit code as the ECU sends it (VAG ECUs send their 5-digit fault number
/// here; decoding it to text is out of scope) and its status byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KwpDtc {
    pub code: u16,
    pub status: u8,
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

    /// `readDataByLocalIdentifier` (`0x21 <id>`): on VAG KWP2000-over-TP2.0
    /// ECUs `id` is a measuring-block group. Returns the record **after** the
    /// echoed id (for a VAG group: 3-byte `formula, a, b` cells). An echo for a
    /// different id is a protocol error, never silently accepted.
    pub fn read_local_id(&mut self, id: u8) -> Result<Vec<u8>> {
        let resp = self.call(&[sid::READ_DATA_BY_LOCAL_ID, id])?;
        match resp.first() {
            Some(&echo) if echo == id => Ok(resp[1..].to_vec()),
            Some(&echo) => Err(Error::Protocol(format!(
                "readDataByLocalIdentifier echoed id 0x{echo:02X}, expected 0x{id:02X}"
            ))),
            None => Err(Error::Protocol(
                "empty readDataByLocalIdentifier response".into(),
            )),
        }
    }

    /// `readDiagnosticTroubleCodesByStatus` (`0x18 <status> <group hi> <group lo>`).
    /// `status` selects which codes are reported (`0x02` = all stored on VAG
    /// ECUs); `group` `0xFF00` = all groups. Response: `58 <count> (hi lo status)*`.
    /// A count that disagrees with the payload length is a protocol error.
    pub fn read_dtcs_by_status(&mut self, status: u8, group: u16) -> Result<Vec<KwpDtc>> {
        let g = group.to_be_bytes();
        let resp = self.call(&[sid::READ_DTC_BY_STATUS, status, g[0], g[1]])?;
        let Some((&count, body)) = resp.split_first() else {
            return Err(Error::Protocol("empty readDTCByStatus response".into()));
        };
        if body.len() != count as usize * 3 {
            return Err(Error::Protocol(format!(
                "readDTCByStatus reports {count} code(s) but carries {} byte(s)",
                body.len()
            )));
        }
        Ok(body
            .chunks_exact(3)
            .map(|c| KwpDtc {
                code: u16::from_be_bytes([c[0], c[1]]),
                status: c[2],
            })
            .collect())
    }

    /// `clearDiagnosticInformation` (`0x14 <group hi> <group lo>`); `0xFF00`
    /// clears all groups. A **write**: callers must gate it behind an explicit
    /// user confirmation.
    pub fn clear_dtcs(&mut self, group: u16) -> Result<()> {
        let g = group.to_be_bytes();
        self.call(&[sid::CLEAR_DIAGNOSTIC_INFO, g[0], g[1]])
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Answers each request from a fixed script, recording what was sent.
    struct Script {
        replies: Vec<(Vec<u8>, Vec<u8>)>,
    }

    impl DiagTransport for Script {
        fn request(&mut self, req: &[u8], _timeout_ms: u64) -> Result<Vec<u8>> {
            let pos = self
                .replies
                .iter()
                .position(|(r, _)| r == req)
                .ok_or_else(|| Error::Protocol(format!("unscripted request {req:02X?}")))?;
            Ok(self.replies.remove(pos).1)
        }
    }

    #[test]
    fn measuring_block_strips_echo() {
        let mut t = Script {
            replies: vec![(vec![0x21, 0x01], vec![0x61, 0x01, 0x1A, 0x00, 0x18])],
        };
        let mut k = Kwp2000Client::new(&mut t);
        assert_eq!(k.read_local_id(0x01).unwrap(), vec![0x1A, 0x00, 0x18]);
    }

    #[test]
    fn measuring_block_rejects_wrong_echo() {
        let mut t = Script {
            replies: vec![(vec![0x21, 0x03], vec![0x61, 0x01, 0x1A, 0x00, 0x18])],
        };
        let mut k = Kwp2000Client::new(&mut t);
        assert!(matches!(k.read_local_id(0x03), Err(Error::Protocol(_))));
    }

    #[test]
    fn dtcs_by_status_parse_and_validate_count() {
        let mut t = Script {
            replies: vec![
                (vec![0x18, 0x02, 0xFF, 0x00], vec![0x58, 0x00]),
                (
                    vec![0x18, 0x02, 0xFF, 0x01],
                    vec![0x58, 0x02, 0x12, 0x34, 0x24, 0x00, 0x01, 0x60],
                ),
                (
                    vec![0x18, 0x02, 0xFF, 0x02],
                    vec![0x58, 0x02, 0x12, 0x34, 0x24],
                ),
            ],
        };
        let mut k = Kwp2000Client::new(&mut t);
        assert!(k.read_dtcs_by_status(0x02, 0xFF00).unwrap().is_empty());
        assert_eq!(
            k.read_dtcs_by_status(0x02, 0xFF01).unwrap(),
            vec![
                KwpDtc {
                    code: 0x1234,
                    status: 0x24
                },
                KwpDtc {
                    code: 0x0001,
                    status: 0x60
                }
            ]
        );
        assert!(matches!(
            k.read_dtcs_by_status(0x02, 0xFF02),
            Err(Error::Protocol(_))
        ));
    }

    #[test]
    fn clear_dtcs_and_negative_response() {
        let mut t = Script {
            replies: vec![
                (vec![0x14, 0xFF, 0x00], vec![0x54, 0xFF, 0x00]),
                (vec![0x21, 0x7D], vec![0x7F, 0x21, 0x31]),
            ],
        };
        let mut k = Kwp2000Client::new(&mut t);
        k.clear_dtcs(0xFF00).unwrap();
        assert!(matches!(
            k.read_local_id(0x7D),
            Err(Error::NegativeResponse { code: 0x31, .. })
        ));
    }
}
