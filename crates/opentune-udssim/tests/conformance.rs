//! Conformance suite: drive OpenTune's real diagnostic client against the
//! independent in-memory ECU simulator and assert the client's validation and
//! timing behaviour.
//!
//! These are offline tests only — no hardware, no bus, no network. Each test
//! sets up a [`UdsServer`], optionally schedules one deterministic fault, drives
//! [`opentune_core::protocol::UdsClient`] (or the real `DoIpTransport` client)
//! against it, and asserts how the client handled the server's behaviour.
//!
//! Services the client has no typed method for (WriteDataByIdentifier 0x2E,
//! ReadDTCInformation 0x19) are exercised through the raw `DiagTransport` request
//! and the shared `opentune_core::protocol::check_response` validator, which the
//! client itself uses.

use opentune_core::protocol::doip::DoIpTransport;
use opentune_core::protocol::{check_response, DiagTransport, UdsClient};
use opentune_core::Error;
use opentune_udssim::{
    AlwaysReject, DoIpSimStream, DtcRecord, ServerConfig, SimTransport, UdsServer,
};

/// The XOR mask the default simulated ECU uses for SecurityAccess.
const SEC_MASK: u8 = 0xA5;

/// Compute the key the way a client with the correct algorithm would.
fn derive_key(seed: &[u8]) -> Vec<u8> {
    seed.iter().map(|b| b ^ SEC_MASK).collect()
}

/// Build a server pre-seeded with a VIN DID and a routine.
fn seeded_server() -> UdsServer {
    let mut server = UdsServer::new();
    server.set_did(0xF190, b"WVWZZZ3CZ9E000000".to_vec());
    server.set_routine(0xFF00, vec![0x00]); // startRoutine returns status 0x00
    server
}

// ---------------------------------------------------------------------------
// 1. Happy path: session -> security -> read/write -> routine ->
//    download/transfer/exit, all against the one server.
// ---------------------------------------------------------------------------

#[test]
fn happy_path_full_session_flow() {
    let mut t = SimTransport::new(seeded_server());

    // Session + security unlock + read, via the typed client.
    {
        let mut uds = UdsClient::new(&mut t);
        uds.session(0x03).expect("extended session");
        let seed = uds.request_seed(0x01).expect("seed");
        assert!(!seed.is_empty());
        let key = derive_key(&seed);
        uds.send_key(0x02, &key).expect("unlock");
        assert_eq!(
            uds.read_data_by_id(0xF190).unwrap(),
            b"WVWZZZ3CZ9E000000".to_vec()
        );
    }
    assert!(t.server().is_unlocked(), "security should be unlocked");

    // WriteDataByIdentifier (0x2E) via the raw transport + shared validator.
    {
        let resp = t.request(&[0x2E, 0xF1, 0x90, b'A', b'B'], 1000).unwrap();
        check_response(0x2E, &resp).expect("WDBI positive");
        assert_eq!(&resp, &[0x6E, 0xF1, 0x90]);
    }
    // Read the written value back, via the client.
    {
        let mut uds = UdsClient::new(&mut t);
        assert_eq!(uds.read_data_by_id(0xF190).unwrap(), b"AB".to_vec());
    }

    // RoutineControl (0x31).
    {
        let mut uds = UdsClient::new(&mut t);
        assert_eq!(uds.routine_control(0x01, 0xFF00, &[]).unwrap(), vec![0x00]);
    }

    // RequestDownload -> TransferData -> RequestTransferExit (0x34/0x36/0x37).
    {
        let mut uds = UdsClient::new(&mut t);
        let max_block = uds
            .request_download(0x00, 0x8000_0000, 4)
            .expect("download");
        assert_eq!(max_block, 0x0402);
        uds.transfer_data(0x01, &[0xDE, 0xAD, 0xBE, 0xEF])
            .expect("block 1");
        uds.transfer_exit().expect("exit");
    }
    assert_eq!(
        t.server().downloaded().unwrap(),
        &[0xDE, 0xAD, 0xBE, 0xEF],
        "the download bytes should be received exactly once"
    );
}

#[test]
fn already_unlocked_seed_is_all_zero() {
    let mut t = SimTransport::new(UdsServer::new());
    let mut uds = UdsClient::new(&mut t);
    let seed = uds.request_seed(0x01).unwrap();
    let key = derive_key(&seed);
    uds.send_key(0x02, &key).unwrap();
    // A second seed request while unlocked returns the all-zero "already
    // unlocked" seed the client recognises via `seed_is_unlocked`.
    let seed2 = uds.request_seed(0x01).unwrap();
    assert!(opentune_core::protocol::uds::seed_is_unlocked(&seed2));
}

// ---------------------------------------------------------------------------
// 2. responsePending drained, write issued exactly once (no resend).
// ---------------------------------------------------------------------------

#[test]
fn response_pending_drained_without_resending_write() {
    let mut t = SimTransport::new(UdsServer::new());
    t.faults().response_pending(0x36, 3); // 3x 0x78 before the transferData answer

    {
        let mut uds = UdsClient::new(&mut t);
        uds.request_download(0x00, 0x1000, 4).unwrap();
        uds.transfer_data(0x01, &[1, 2, 3, 4])
            .expect("drained to success");
    }

    // One request per service; the transferData was NOT re-sent despite 3x 0x78.
    assert_eq!(t.request_count(), 2, "download + transferData, no resend");
    assert_eq!(
        t.trace().pending_count(),
        3,
        "three pending frames interposed"
    );
    assert_eq!(
        t.server().downloaded().unwrap(),
        &[1, 2, 3, 4],
        "the write landed exactly once"
    );
}

#[test]
fn response_pending_surfaced_to_client_is_not_resent() {
    // A transport that surfaces a bare 0x78 (did not drain it) must NOT make the
    // client re-send the write — the client surfaces it as a negative response.
    let mut t = SimTransport::new(UdsServer::new());
    {
        let mut uds = UdsClient::new(&mut t);
        uds.request_download(0x00, 0x1000, 4).unwrap();
    }
    t.faults().negative_response(0x36, 0x78);
    let result = {
        let mut uds = UdsClient::new(&mut t);
        uds.transfer_data(0x01, &[1, 2, 3, 4])
    };
    assert!(
        matches!(result, Err(Error::NegativeResponse { code: 0x78, .. })),
        "0x78 at the client surfaces, got {result:?}"
    );
    assert_eq!(t.request_count(), 2, "transferData issued exactly once");
    assert!(
        t.server().downloaded().unwrap().is_empty(),
        "the refused block was not applied"
    );
}

// ---------------------------------------------------------------------------
// 3. Wrong correlation echoes rejected (block counter / DID / sub-function).
// ---------------------------------------------------------------------------

#[test]
fn wrong_block_counter_echo_rejected() {
    let mut t = SimTransport::new(UdsServer::new());
    {
        let mut uds = UdsClient::new(&mut t);
        uds.request_download(0x00, 0x1000, 4).unwrap();
    }
    t.faults().wrong_echo(0x36);
    let mut uds = UdsClient::new(&mut t);
    assert!(
        matches!(
            uds.transfer_data(0x01, &[1, 2, 3, 4]),
            Err(Error::Protocol(_))
        ),
        "a corrupted blockSequenceCounter echo must be rejected"
    );
}

#[test]
fn wrong_did_echo_rejected() {
    let mut t = SimTransport::new(seeded_server());
    t.faults().wrong_echo(0x22);
    let mut uds = UdsClient::new(&mut t);
    assert!(
        matches!(uds.read_data_by_id(0xF190), Err(Error::Protocol(_))),
        "a corrupted DID echo must not be returned as data"
    );
}

#[test]
fn wrong_subfunction_echo_rejected() {
    let mut t = SimTransport::new(UdsServer::new());
    t.faults().wrong_echo(0x10);
    let mut uds = UdsClient::new(&mut t);
    assert!(
        matches!(uds.session(0x03), Err(Error::Protocol(_))),
        "a corrupted session sub-function echo must be rejected"
    );
}

#[test]
fn wrong_routine_echo_rejected() {
    let mut t = SimTransport::new(seeded_server());
    t.faults().wrong_echo(0x31); // corrupts the echoed routine id
    let mut uds = UdsClient::new(&mut t);
    assert!(
        matches!(
            uds.routine_control(0x01, 0xFF00, &[]),
            Err(Error::Protocol(_))
        ),
        "a corrupted routine-id echo must be rejected"
    );
}

// ---------------------------------------------------------------------------
// 4. Truncated and mis-correlated responses rejected.
// ---------------------------------------------------------------------------

#[test]
fn truncated_download_response_rejected() {
    let mut t = SimTransport::new(UdsServer::new());
    // Keep only `74 20`, dropping the maxNumberOfBlockLength bytes.
    t.faults().truncate(0x34, 2);
    let mut uds = UdsClient::new(&mut t);
    assert!(
        matches!(
            uds.request_download(0x00, 0x1000, 4),
            Err(Error::Protocol(_))
        ),
        "a truncated requestDownload response must be rejected"
    );
}

#[test]
fn mis_correlated_response_rejected() {
    let mut t = SimTransport::new(seeded_server());
    // The ECU answers a negative response echoing a different service id.
    t.faults().mis_correlate(0x22);
    let mut uds = UdsClient::new(&mut t);
    assert!(
        matches!(uds.read_data_by_id(0xF190), Err(Error::Protocol(_))),
        "a reply correlated to another request must be rejected"
    );
}

// ---------------------------------------------------------------------------
// 5. NRCs surfaced (request out of range, security denied).
// ---------------------------------------------------------------------------

#[test]
fn unknown_did_surfaces_request_out_of_range() {
    let mut t = SimTransport::new(UdsServer::new());
    let mut uds = UdsClient::new(&mut t);
    match uds.read_data_by_id(0xDEAD) {
        Err(Error::NegativeResponse {
            service: 0x22,
            code: 0x31,
            ..
        }) => {}
        other => panic!("expected requestOutOfRange NRC, got {other:?}"),
    }
}

#[test]
fn invalid_key_surfaces_security_denied() {
    let mut server = UdsServer::new();
    server.set_security_checker(Box::new(AlwaysReject));
    let mut t = SimTransport::new(server);
    let mut uds = UdsClient::new(&mut t);
    let seed = uds.request_seed(0x01).unwrap();
    let key = derive_key(&seed); // wrong: the ECU rejects every key
    assert!(
        matches!(uds.send_key(0x02, &key), Err(Error::SecurityDenied)),
        "an invalid key must surface as SecurityDenied"
    );
}

#[test]
fn exceeded_attempts_surfaces_security_denied() {
    let mut server = UdsServer::with_config(ServerConfig {
        max_sec_attempts: 1,
        ..ServerConfig::default()
    });
    server.set_security_checker(Box::new(AlwaysReject));
    let mut t = SimTransport::new(server);
    let mut uds = UdsClient::new(&mut t);
    let seed = uds.request_seed(0x01).unwrap();
    // The single allowed attempt fails and the ECU answers 0x36; both 0x35 and
    // 0x36 map to SecurityDenied at the client.
    assert!(matches!(
        uds.send_key(0x02, &derive_key(&seed)),
        Err(Error::SecurityDenied)
    ));
}

#[test]
fn injected_nrc_surfaces_named() {
    // Inject conditionsNotCorrect (0x22) on a routine the client starts.
    let mut t = SimTransport::new(seeded_server());
    t.faults().negative_response(0x31, 0x22);
    let mut uds = UdsClient::new(&mut t);
    match uds.routine_control(0x01, 0xFF00, &[]) {
        Err(Error::NegativeResponse {
            code: 0x22, name, ..
        }) => assert_eq!(name, "conditionsNotCorrect"),
        other => panic!("expected conditionsNotCorrect NRC, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 6. ReadDTCInformation (0x19) via the raw transport + shared validator.
// ---------------------------------------------------------------------------

#[test]
fn read_dtc_by_status_mask() {
    let mut server = UdsServer::new();
    server.add_dtc(DtcRecord::new([0x12, 0x34, 0x56], 0x2F));
    server.add_dtc(DtcRecord::new([0xAB, 0xCD, 0xEF], 0x08));
    let mut t = SimTransport::new(server);

    // reportDTCByStatusMask (0x02) with mask 0xFF -> both DTCs.
    let resp = t.request(&[0x19, 0x02, 0xFF], 1000).unwrap();
    check_response(0x19, &resp).expect("0x19 positive");
    assert_eq!(resp[0], 0x59);
    assert_eq!(resp[1], 0x02);
    assert_eq!(resp[2], 0x2F, "availability mask = OR of statuses");
    // header(3) + 2 records of 4 bytes each.
    assert_eq!(resp.len(), 3 + 2 * 4);
    assert_eq!(&resp[3..7], &[0x12, 0x34, 0x56, 0x2F]);

    // reportNumberOfDTCByStatusMask (0x01) counts only the matching DTCs.
    let count = t.request(&[0x19, 0x01, 0x08], 1000).unwrap();
    check_response(0x19, &count).unwrap();
    assert_eq!(count, vec![0x59, 0x01, 0x2F, 0x01, 0x00, 0x02]);
}

// ---------------------------------------------------------------------------
// 7. The DoIP client, end to end against the loopback DoIP entity.
// ---------------------------------------------------------------------------

const TESTER: u16 = 0x0E80;
const ECU: u16 = 0x0010;

#[test]
fn doip_happy_path_session_and_read() {
    let stream = DoIpSimStream::new(seeded_server(), ECU);
    let mut doip = DoIpTransport::with_stream(stream, TESTER, ECU);
    doip.activate(1000).expect("routing activation");
    assert!(doip.is_activated());

    let mut uds = UdsClient::new(&mut doip);
    uds.session(0x03).expect("session over DoIP");
    assert_eq!(
        uds.read_data_by_id(0xF190).unwrap(),
        b"WVWZZZ3CZ9E000000".to_vec()
    );
}

#[test]
fn doip_response_pending_drained_by_client_transport() {
    // Schedule a 2x responsePending storm on the ReadDataByIdentifier the client
    // issues; the real DoIP transport must drain the interposed 0x78 diagnostic
    // messages and return the correct value without re-sending.
    let mut stream = DoIpSimStream::new(seeded_server(), ECU);
    stream.faults().response_pending(0x22, 2);
    let mut doip = DoIpTransport::with_stream(stream, TESTER, ECU);
    doip.activate(1000).unwrap();

    let mut uds = UdsClient::new(&mut doip);
    assert_eq!(
        uds.read_data_by_id(0xF190).unwrap(),
        b"WVWZZZ3CZ9E000000".to_vec(),
        "the DoIP 0x78 drain loop should yield the real answer"
    );
}

#[test]
fn doip_nrc_surfaced() {
    let mut stream = DoIpSimStream::new(UdsServer::new(), ECU);
    stream.faults().negative_response(0x22, 0x31);
    let mut doip = DoIpTransport::with_stream(stream, TESTER, ECU);
    doip.activate(1000).unwrap();

    let mut uds = UdsClient::new(&mut doip);
    assert!(matches!(
        uds.read_data_by_id(0xF190),
        Err(Error::NegativeResponse { code: 0x31, .. })
    ));
}

// ---------------------------------------------------------------------------
// 8. The trace records the full ordered exchange.
// ---------------------------------------------------------------------------

#[test]
fn trace_records_ordered_exchange_with_pending() {
    let mut t = SimTransport::new(seeded_server());
    t.faults().response_pending(0x22, 1);
    {
        let mut uds = UdsClient::new(&mut t);
        uds.read_data_by_id(0xF190).unwrap();
    }
    let lines = t.trace().to_lines();
    assert_eq!(t.trace().requests().len(), 1);
    assert_eq!(t.trace().responses().len(), 1);
    assert_eq!(t.trace().pending_count(), 1);
    // Order: request, pending, response.
    assert!(lines[0].starts_with("-> 22 F1 90"));
    assert!(lines[1].contains("7F 22 78"));
    assert!(lines[2].starts_with("<- 62 F1 90"));
}
