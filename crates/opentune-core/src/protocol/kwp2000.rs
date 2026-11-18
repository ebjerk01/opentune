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
//!
//! ## responsePending and negative responses
//!
//! The `0x78` wait belongs to the transport ([`DiagTransport`]): each one
//! (ISO-TP, VW TP2.0, the K-line drivers) loops a bounded number of times
//! and only for a `7F` echoing the requested service
//! ([`is_response_pending`](crate::protocol::diag::is_response_pending)).
//! A `0x78` that still reaches this client surfaces as
//! [`Error::NegativeResponse`] with code 0x78, never as success.
//!
//! Negative-response codes are named from the ISO 14230-3 table
//! ([`kwp_nrc_name`]); where KWP2000 and UDS (ISO 14229) share a code the
//! UDS name is kept, where KWP2000 defines its own meaning (download/upload
//! and block-transfer codes 0x40–0x53 and 0x71–0x79, 0x80, the
//! manufacturer/supplier ranges) the KWP2000 name wins.
//!
//! ## Length checks
//!
//! Every positive response is checked for its service id (+0x40) and, where
//! the service echoes a parameter (session, identification option, security
//! level, local identifier), for that echo. `readMemoryByAddress` refuses an
//! empty or overlong block; [`Kwp2000Client::read_memory_exact`] also
//! refuses a short one.
use crate::protocol::diag::{check_response, nrc_name, DiagTransport};
use crate::{Error, Result};

/// Name of a KWP2000 negative-response code (ISO 14230-3). Codes KWP2000
/// shares with UDS keep the UDS name ([`nrc_name`]).
pub fn kwp_nrc_name(code: u8) -> &'static str {
    match code {
        0x12 => "subFunctionNotSupportedOrInvalidFormat",
        0x22 => "conditionsNotCorrect",
        0x23 => "routineNotComplete",
        0x40 => "downloadNotAccepted",
        0x41 => "improperDownloadType",
        0x42 => "cantDownloadToSpecifiedAddress",
        0x43 => "cantDownloadNumberOfBytesRequested",
        0x50 => "uploadNotAccepted",
        0x51 => "improperUploadType",
        0x52 => "cantUploadFromSpecifiedAddress",
        0x53 => "cantUploadNumberOfBytesRequested",
        0x71 => "transferSuspended",
        0x72 => "transferAborted",
        0x74 => "illegalAddressInBlockTransfer",
        0x75 => "illegalByteCountInBlockTransfer",
        0x76 => "illegalBlockTransferType",
        0x77 => "blockTransferDataChecksumError",
        0x79 => "incorrectByteCountDuringBlockTransfer",
        0x80 => "serviceNotSupportedInActiveDiagnosticSession",
        0x81..=0x8F | 0xFF => "reservedByDocument",
        0x90..=0xF9 => "manufacturerSpecific",
        0xFA..=0xFE => "systemSupplierSpecific",
        c => nrc_name(c),
    }
}

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
        match check_response(req[0], &resp) {
            Ok(()) => Ok(resp[1..].to_vec()),
            Err(Error::NegativeResponse { service, code, .. }) => Err(Error::NegativeResponse {
                service,
                code,
                name: kwp_nrc_name(code),
            }),
            Err(e) => Err(e),
        }
    }

    /// A positive response whose first byte must echo `want` (`what` names
    /// it in the error). Returns the response after the service id.
    fn call_echo(&mut self, req: &[u8], want: u8, what: &str) -> Result<Vec<u8>> {
        let resp = self.call(req)?;
        match resp.first() {
            Some(&b) if b == want => Ok(resp),
            Some(&b) => Err(Error::Protocol(format!(
                "KWP2000 service 0x{:02X}: response echoes {what} 0x{b:02X}, expected 0x{want:02X}",
                req[0]
            ))),
            None => Err(Error::Protocol(format!(
                "KWP2000 service 0x{:02X}: response without the {what} echo",
                req[0]
            ))),
        }
    }

    /// `startDiagnosticSession`; the response must echo `session` when it
    /// carries any parameter (an echo-less `50` is tolerated).
    pub fn start_session(&mut self, session: u8) -> Result<Vec<u8>> {
        let resp = self.call(&[sid::START_DIAG_SESSION, session])?;
        match resp.first() {
            Some(&b) if b != session => Err(Error::Protocol(format!(
                "startDiagnosticSession: response echoes session 0x{b:02X}, expected \
                 0x{session:02X}"
            ))),
            _ => Ok(resp),
        }
    }

    pub fn tester_present(&mut self) -> Result<()> {
        self.call(&[sid::TESTER_PRESENT, 0x01]).map(|_| ())
    }

    /// `readEcuIdentification`; returns the record **including** the echoed
    /// option byte (callers parse from it), which must match `option`.
    pub fn read_ecu_id(&mut self, option: u8) -> Result<Vec<u8>> {
        self.call_echo(&[sid::READ_ECU_ID, option], option, "identification option")
    }

    /// `securityAccess` requestSeed; the response must echo `level` and
    /// carry a seed.
    pub fn request_seed(&mut self, level: u8) -> Result<Vec<u8>> {
        let resp = self.call_echo(&[sid::SECURITY_ACCESS, level], level, "security level")?;
        if resp.len() < 2 {
            return Err(Error::Protocol("empty seed".into()));
        }
        Ok(resp[1..].to_vec())
    }

    pub fn send_key(&mut self, level: u8, key: &[u8]) -> Result<()> {
        let mut req = vec![sid::SECURITY_ACCESS, level];
        req.extend_from_slice(key);
        match self.call(&req) {
            // `67 <level> [status]`: an echo of another level is not our unlock.
            Ok(resp) if resp.first().is_some_and(|&b| b != level) => Err(Error::Protocol(format!(
                "securityAccess sendKey: response echoes level 0x{:02X}, expected 0x{level:02X}",
                resp[0]
            ))),
            Ok(_) => Ok(()),
            // invalidKey (0x35) or exceededNumberOfAttempts (0x36): failed unlock.
            Err(Error::NegativeResponse {
                code: 0x35 | 0x36, ..
            }) => Err(Error::SecurityDenied),
            Err(e) => Err(e),
        }
    }

    /// `readMemoryByAddress` (3-byte address, 1-byte length — common EDC15/16
    /// form). An empty block, or one longer than `len`, is a protocol error;
    /// a shorter one is returned (callers advance by its length) — use
    /// [`read_memory_exact`](Self::read_memory_exact) to refuse it too.
    pub fn read_memory(&mut self, address: u32, len: u8) -> Result<Vec<u8>> {
        if address > 0x00FF_FFFF {
            return Err(Error::Protocol(format!(
                "readMemoryByAddress: address 0x{address:X} does not fit 3 bytes"
            )));
        }
        let a = address.to_be_bytes();
        let data = self.call(&[sid::READ_MEMORY_BY_ADDRESS, a[1], a[2], a[3], len])?;
        if data.is_empty() {
            return Err(Error::Protocol(format!(
                "readMemoryByAddress 0x{address:06X}: empty block"
            )));
        }
        if len != 0 && data.len() > len as usize {
            return Err(Error::Protocol(format!(
                "readMemoryByAddress 0x{address:06X}: asked for {len} byte(s), got {}",
                data.len()
            )));
        }
        Ok(data)
    }

    /// [`read_memory`](Self::read_memory) that also refuses a short block.
    pub fn read_memory_exact(&mut self, address: u32, len: u8) -> Result<Vec<u8>> {
        let data = self.read_memory(address, len)?;
        if len != 0 && data.len() != len as usize {
            return Err(Error::Protocol(format!(
                "readMemoryByAddress 0x{address:06X}: asked for {len} byte(s), got {}",
                data.len()
            )));
        }
        Ok(data)
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
    fn kwp_nrc_table_covers_iso14230() {
        // Every code ISO 14230-3 defines has a name.
        for c in [
            0x10u8, 0x11, 0x12, 0x21, 0x22, 0x23, 0x31, 0x33, 0x35, 0x36, 0x37, 0x40, 0x41, 0x42,
            0x43, 0x50, 0x51, 0x52, 0x53, 0x71, 0x72, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x80,
        ] {
            assert_ne!(kwp_nrc_name(c), "unknown", "0x{c:02X}");
        }
        // KWP meanings win where UDS reuses the code differently.
        assert_eq!(kwp_nrc_name(0x50), "uploadNotAccepted");
        assert_eq!(kwp_nrc_name(0x71), "transferSuspended");
        assert_eq!(kwp_nrc_name(0x83), "reservedByDocument");
        assert_eq!(kwp_nrc_name(0x90), "manufacturerSpecific");
        assert_eq!(kwp_nrc_name(0xF9), "manufacturerSpecific");
        assert_eq!(kwp_nrc_name(0xFA), "systemSupplierSpecific");
        assert_eq!(kwp_nrc_name(0xFE), "systemSupplierSpecific");
        // Shared codes keep the UDS name.
        assert_eq!(kwp_nrc_name(0x31), "requestOutOfRange");
        assert_eq!(
            kwp_nrc_name(0x78),
            "requestCorrectlyReceivedResponsePending"
        );
        // Every byte maps to something (no panic, no gap in the match).
        for c in 0..=255u8 {
            assert!(!kwp_nrc_name(c).is_empty());
        }
    }

    #[test]
    fn negative_responses_carry_kwp_names_and_leaked_pending() {
        let mut t = Script {
            replies: vec![
                (
                    vec![0x35, 0x01, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00],
                    vec![0x7F, 0x35, 0x50],
                ),
                (vec![0x36, 0xAA], vec![0x7F, 0x36, 0x77]),
                (vec![0x31, 0x05], vec![0x7F, 0x31, 0x78]),
                (vec![0x21, 0x04], vec![0x7F, 0x21, 0x92]),
            ],
        };
        let mut k = Kwp2000Client::new(&mut t);
        let name = |r: Result<Vec<u8>>| match r {
            Err(Error::NegativeResponse { name, .. }) => name,
            other => panic!("{other:?}"),
        };
        let up = [
            sid::REQUEST_UPLOAD,
            0x01,
            0x00,
            0x00,
            0x00,
            0x00,
            0x10,
            0x00,
        ];
        assert_eq!(name(k.call(&up)), "uploadNotAccepted");
        assert_eq!(
            name(k.transfer_data(&[0xAA])),
            "blockTransferDataChecksumError"
        );
        // A responsePending the transport did not wait through is an error,
        // never success.
        assert_eq!(
            name(k.start_routine(0x05, &[])),
            "requestCorrectlyReceivedResponsePending"
        );
        assert_eq!(name(k.read_local_id(0x04)), "manufacturerSpecific");
    }

    #[test]
    fn echoes_are_checked() {
        let mut t = Script {
            replies: vec![
                (vec![0x10, 0x89], vec![0x50, 0x85]),
                (vec![0x10, 0x85], vec![0x50]),
                (vec![0x1A, 0x9B], vec![0x5A, 0x9A, 0x30]),
                (vec![0x1A, 0x91], vec![0x5A]),
                (vec![0x27, 0x01], vec![0x67, 0x03, 0x12, 0x34]),
                (vec![0x27, 0x03], vec![0x67, 0x03]),
                (vec![0x27, 0x05], vec![0x67, 0x05, 0xBE, 0xEF]),
                (vec![0x27, 0x06, 0x01, 0x02], vec![0x67, 0x02]),
                (vec![0x27, 0x02, 0x01, 0x02], vec![0x67, 0x02, 0x34]),
            ],
        };
        let mut k = Kwp2000Client::new(&mut t);
        let proto = |r: Result<Vec<u8>>| matches!(r, Err(Error::Protocol(_)));
        assert!(proto(k.start_session(0x89)), "wrong session echo");
        assert!(
            k.start_session(0x85).is_ok(),
            "an echo-less 50 is tolerated"
        );
        assert!(proto(k.read_ecu_id(0x9B)), "wrong option echo");
        assert!(proto(k.read_ecu_id(0x91)), "missing option echo");
        assert!(proto(k.request_seed(0x01)), "seed for another level");
        assert!(proto(k.request_seed(0x03)), "echo without a seed");
        assert_eq!(k.request_seed(0x05).unwrap(), vec![0xBE, 0xEF]);
        assert!(matches!(k.send_key(0x06, &[1, 2]), Err(Error::Protocol(_))));
        k.send_key(0x02, &[1, 2]).unwrap();
    }

    #[test]
    fn read_memory_length_checks() {
        let mut t = Script {
            replies: vec![
                (
                    vec![0x23, 0x01, 0x00, 0x00, 0x04],
                    vec![0x63, 1, 2, 3, 4, 5],
                ),
                (vec![0x23, 0x01, 0x00, 0x10, 0x04], vec![0x63]),
                (vec![0x23, 0x01, 0x00, 0x20, 0x04], vec![0x63, 1, 2]),
                (vec![0x23, 0x01, 0x00, 0x30, 0x04], vec![0x63, 1, 2]),
                (vec![0x23, 0x01, 0x00, 0x40, 0x04], vec![0x63, 1, 2, 3, 4]),
            ],
        };
        let mut k = Kwp2000Client::new(&mut t);
        let proto = |r: Result<Vec<u8>>| matches!(r, Err(Error::Protocol(_)));
        assert!(proto(k.read_memory(0x01_0000, 4)), "overlong");
        assert!(proto(k.read_memory(0x01_0010, 4)), "empty");
        assert_eq!(
            k.read_memory(0x01_0020, 4).unwrap(),
            vec![1, 2],
            "short is returned"
        );
        assert!(proto(k.read_memory_exact(0x01_0030, 4)), "short refused");
        assert_eq!(k.read_memory_exact(0x01_0040, 4).unwrap(), vec![1, 2, 3, 4]);
        assert!(
            proto(k.read_memory(0x0100_0000, 4)),
            "address beyond 24 bits"
        );
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
