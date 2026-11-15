//! Request/response diagnostic transport abstraction and NRC helpers.
use crate::{Error, Result};

/// One UDS/KWP request → response exchange. Implemented over ISO-TP (CAN) or
/// K-line by the drivers. Negative responses surface as [`Error::NegativeResponse`].
pub trait DiagTransport {
    fn request(&mut self, req: &[u8], timeout_ms: u64) -> Result<Vec<u8>>;
}

/// Validate a response against the request service id. Returns Ok for a
/// positive response (`sid + 0x40`), else a descriptive error.
pub fn check_response(req_sid: u8, resp: &[u8]) -> Result<()> {
    if resp.is_empty() {
        return Err(Error::Protocol("empty response".into()));
    }
    if resp[0] == 0x7F {
        let code = *resp.get(2).unwrap_or(&0);
        return Err(Error::NegativeResponse {
            service: req_sid,
            code,
            name: nrc_name(code),
        });
    }
    if resp[0] != req_sid.wrapping_add(0x40) {
        return Err(Error::Protocol(format!(
            "unexpected response service 0x{:02X} to request 0x{:02X}",
            resp[0], req_sid
        )));
    }
    Ok(())
}

/// Human-readable name for a KWP/UDS negative-response code.
pub fn nrc_name(code: u8) -> &'static str {
    match code {
        0x10 => "generalReject",
        0x11 => "serviceNotSupported",
        0x12 => "subFunctionNotSupported",
        0x13 => "incorrectMessageLengthOrInvalidFormat",
        0x22 => "conditionsNotCorrect",
        0x24 => "requestSequenceError",
        0x31 => "requestOutOfRange",
        0x33 => "securityAccessDenied",
        0x35 => "invalidKey",
        0x36 => "exceededNumberOfAttempts",
        0x37 => "requiredTimeDelayNotExpired",
        0x70 => "uploadDownloadNotAccepted",
        0x71 => "transferDataSuspended",
        0x72 => "generalProgrammingFailure",
        0x73 => "wrongBlockSequenceCounter",
        0x78 => "requestCorrectlyReceivedResponsePending",
        0x7E => "subFunctionNotSupportedInActiveSession",
        0x7F => "serviceNotSupportedInActiveSession",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;

    #[test]
    fn positive_response_ok() {
        assert!(check_response(0x22, &[0x62, 0x01, 0x02]).is_ok());
    }

    #[test]
    fn negative_response_maps_code() {
        let e = check_response(0x27, &[0x7F, 0x27, 0x35]).unwrap_err();
        match e {
            Error::NegativeResponse { service: 0x27, code: 0x35, name } => {
                assert_eq!(name, "invalidKey");
            }
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn wrong_service_id_is_protocol_error() {
        assert!(matches!(
            check_response(0x22, &[0x50, 0x01]),
            Err(Error::Protocol(_))
        ));
    }
}
