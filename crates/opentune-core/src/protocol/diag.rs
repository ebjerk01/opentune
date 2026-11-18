//! Request/response diagnostic transport abstraction and NRC helpers.
use crate::{Error, Result};

/// One UDS/KWP request → response exchange. Implemented over ISO-TP (CAN) or
/// K-line by the drivers. Negative responses surface as [`Error::NegativeResponse`].
pub trait DiagTransport {
    fn request(&mut self, req: &[u8], timeout_ms: u64) -> Result<Vec<u8>>;
}

/// Whether `resp` is a responsePending (`7F <sid> 78`) for *this* request:
/// the transport should keep waiting (bounded) for the final reply. A 0x78
/// echoing another service belongs to a different exchange and is not a
/// reason to wait (the client reports it as mis-correlated).
pub fn is_response_pending(req: &[u8], resp: &[u8]) -> bool {
    resp.len() >= 3 && resp[0] == 0x7F && resp[2] == 0x78 && req.first() == Some(&resp[1])
}

/// Validate a response against the request service id. Returns Ok for a
/// positive response (`sid + 0x40`), else a descriptive error.
pub fn check_response(req_sid: u8, resp: &[u8]) -> Result<()> {
    if resp.is_empty() {
        return Err(Error::Protocol("empty response".into()));
    }
    if resp[0] == 0x7F {
        // A well-formed negative response is `7F <echoed sid> <nrc>`.
        // Anything shorter cannot carry the echoed service id and NRC, so it is
        // malformed rather than a placeholder/out-of-range read (python-udsoncan
        // treats a short 0x7F as an invalid response).
        if resp.len() < 3 {
            return Err(Error::Protocol("truncated negative response".into()));
        }
        // The second byte must echo the requested service id. A different id
        // means the response is correlated to another request, not an NRC for
        // this service, so surface it as a protocol error naming both ids.
        let echoed_sid = resp[1];
        if echoed_sid != req_sid {
            return Err(Error::Protocol(format!(
                "mis-correlated negative response: echoed service 0x{echoed_sid:02X} \
                 does not match request service 0x{req_sid:02X}"
            )));
        }
        let code = resp[2];
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
///
/// The table follows ISO 14229-1 Annex A (cross-checked against
/// `python-udsoncan`'s `ResponseCode`): the full common range (0x10–0x94)
/// including the manufacturer-conditions band (0x81–0x94) that an ECU uses to
/// refuse a routine or reset while the engine is running / voltage is out of
/// range. ISO reserved / vendor-specific codes map to a descriptive bucket.
pub fn nrc_name(code: u8) -> &'static str {
    match code {
        0x10 => "generalReject",
        0x11 => "serviceNotSupported",
        0x12 => "subFunctionNotSupported",
        0x13 => "incorrectMessageLengthOrInvalidFormat",
        0x14 => "responseTooLong",
        0x21 => "busyRepeatRequest",
        0x22 => "conditionsNotCorrect",
        0x24 => "requestSequenceError",
        0x25 => "noResponseFromSubnetComponent",
        0x26 => "failurePreventsExecutionOfRequestedAction",
        0x31 => "requestOutOfRange",
        0x33 => "securityAccessDenied",
        0x34 => "authenticationRequired",
        0x35 => "invalidKey",
        0x36 => "exceededNumberOfAttempts",
        0x37 => "requiredTimeDelayNotExpired",
        0x38 => "secureDataTransmissionRequired",
        0x39 => "secureDataTransmissionNotAllowed",
        0x3A => "secureDataVerificationFailed",
        0x50 => "certificateVerificationFailedInvalidTimePeriod",
        0x51 => "certificateVerificationFailedInvalidSignature",
        0x52 => "certificateVerificationFailedInvalidChainOfTrust",
        0x53 => "certificateVerificationFailedInvalidType",
        0x54 => "certificateVerificationFailedInvalidFormat",
        0x55 => "certificateVerificationFailedInvalidContent",
        0x56 => "certificateVerificationFailedInvalidScope",
        0x57 => "certificateVerificationFailedInvalidCertificate",
        0x58 => "ownershipVerificationFailed",
        0x59 => "challengeCalculationFailed",
        0x5A => "settingAccessRightsFailed",
        0x5B => "sessionKeyCreationDerivationFailed",
        0x5C => "configurationDataUsageFailed",
        0x5D => "deAuthenticationFailed",
        0x70 => "uploadDownloadNotAccepted",
        0x71 => "transferDataSuspended",
        0x72 => "generalProgrammingFailure",
        0x73 => "wrongBlockSequenceCounter",
        0x78 => "requestCorrectlyReceivedResponsePending",
        0x7E => "subFunctionNotSupportedInActiveSession",
        0x7F => "serviceNotSupportedInActiveSession",
        0x81 => "rpmTooHigh",
        0x82 => "rpmTooLow",
        0x83 => "engineIsRunning",
        0x84 => "engineIsNotRunning",
        0x85 => "engineRunTimeTooLow",
        0x86 => "temperatureTooHigh",
        0x87 => "temperatureTooLow",
        0x88 => "vehicleSpeedTooHigh",
        0x89 => "vehicleSpeedTooLow",
        0x8A => "throttlePedalTooHigh",
        0x8B => "throttlePedalTooLow",
        0x8C => "transmissionRangeNotInNeutral",
        0x8D => "transmissionRangeNotInGear",
        0x8F => "brakeSwitchNotClosed",
        0x90 => "shifterLeverNotInPark",
        0x91 => "torqueConverterClutchLocked",
        0x92 => "voltageTooHigh",
        0x93 => "voltageTooLow",
        0x94 => "resourceTemporarilyNotAvailable",
        // Codes outside the mapped points (ISO reserved 0x95–0xEF and
        // manufacturer-specific 0xF0–0xFE) stay "unknown" so callers can tell a
        // named ISO condition from a vendor/reserved code.
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
            Error::NegativeResponse {
                service: 0x27,
                code: 0x35,
                name,
            } => {
                assert_eq!(name, "invalidKey");
            }
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn truncated_negative_response_is_protocol_error() {
        // `7F` alone, and `7F <sid>` without an NRC byte, are both malformed:
        // they must not read an out-of-range/placeholder NRC.
        for resp in [&[0x7F][..], &[0x7F, 0x27][..]] {
            match check_response(0x27, resp) {
                Err(Error::Protocol(msg)) => {
                    assert!(
                        msg.contains("truncated negative response"),
                        "unexpected message: {msg}"
                    );
                }
                other => panic!("expected truncated protocol error, got {other:?}"),
            }
        }
    }

    #[test]
    fn mis_correlated_negative_response_is_protocol_error() {
        // 0x7F echoing a different service id (0x31) than requested (0x27) is a
        // mis-correlated response, not an NRC for service 0x27.
        match check_response(0x27, &[0x7F, 0x31, 0x35]) {
            Err(Error::Protocol(msg)) => {
                assert!(msg.contains("0x31"), "message should name echoed id: {msg}");
                assert!(
                    msg.contains("0x27"),
                    "message should name request id: {msg}"
                );
            }
            other => panic!("expected mis-correlation protocol error, got {other:?}"),
        }
    }

    #[test]
    fn wrong_positive_service_id_is_protocol_error() {
        // Positive response whose SID is not `req_sid + 0x40`.
        assert!(matches!(
            check_response(0x22, &[0x63, 0x01, 0x02]),
            Err(Error::Protocol(_))
        ));
    }

    #[test]
    fn response_pending_must_echo_the_service() {
        assert!(is_response_pending(&[0x31, 0x01], &[0x7F, 0x31, 0x78]));
        assert!(!is_response_pending(&[0x31, 0x01], &[0x7F, 0x22, 0x78]));
        assert!(!is_response_pending(&[0x31], &[0x7F, 0x31, 0x22]));
        assert!(!is_response_pending(&[0x31], &[0x7F, 0x31]));
        assert!(!is_response_pending(&[], &[0x7F, 0x31, 0x78]));
    }

    #[test]
    fn wrong_service_id_is_protocol_error() {
        assert!(matches!(
            check_response(0x22, &[0x50, 0x01]),
            Err(Error::Protocol(_))
        ));
    }

    #[test]
    fn nrc_table_matches_iso14229_additions() {
        // Codes udsoncan names that the original OpenTune table omitted.
        assert_eq!(nrc_name(0x26), "failurePreventsExecutionOfRequestedAction");
        assert_eq!(nrc_name(0x34), "authenticationRequired");
        assert_eq!(nrc_name(0x83), "engineIsRunning");
        assert_eq!(nrc_name(0x92), "voltageTooHigh");
        assert_eq!(nrc_name(0x93), "voltageTooLow");
        assert_eq!(nrc_name(0x94), "resourceTemporarilyNotAvailable");
        // Still named from before.
        assert_eq!(nrc_name(0x31), "requestOutOfRange");
        // ISO-reserved / vendor codes remain unnamed.
        assert_eq!(nrc_name(0x99), "unknown");
        assert_eq!(nrc_name(0x00), "unknown");
    }
}
