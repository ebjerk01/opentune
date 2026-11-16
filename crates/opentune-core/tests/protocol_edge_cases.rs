//! Additive edge-case coverage for the diagnostic transport layer, exercised
//! through the **public** API only (ISO-TP, VW TP2.0, KWP2000 and the NRC
//! helpers). The protocol modules already carry happy-path unit tests inline;
//! these integration tests pin the error/boundary behaviour a malformed or
//! hostile ECU can trigger, so a regression there fails CI instead of a car.
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use opentune_core::protocol::diag::{check_response, nrc_name, DiagTransport};
use opentune_core::protocol::isotp::{CanLink, IsoTp};
use opentune_core::protocol::kwp2000::Kwp2000Client;
use opentune_core::protocol::vwtp20::{VwTp20, BROADCAST_ID, PROTOCOL_DIAGNOSTIC};
use opentune_core::{Error, Result};

// ---------------------------------------------------------------------------
// Scripted CAN endpoint: hands back queued (id, data) frames on recv (with
// their real ids) and records every transmitted frame for inspection.
// ---------------------------------------------------------------------------
type Log = Rc<RefCell<Vec<(u32, Vec<u8>)>>>;

struct ScriptLink {
    to_tester: VecDeque<(u32, Vec<u8>)>,
    sent: Log,
}

impl ScriptLink {
    fn new(frames: Vec<(u32, Vec<u8>)>) -> (Self, Log) {
        let sent: Log = Rc::new(RefCell::new(Vec::new()));
        (
            Self {
                to_tester: VecDeque::from(frames),
                sent: sent.clone(),
            },
            sent,
        )
    }
}

impl CanLink for ScriptLink {
    fn send_frame(&mut self, id: u32, data: &[u8]) -> Result<()> {
        self.sent.borrow_mut().push((id, data.to_vec()));
        Ok(())
    }
    fn recv_frame(&mut self, _timeout_ms: u64) -> Result<(u32, Vec<u8>)> {
        self.to_tester.pop_front().ok_or(Error::Timeout(0))
    }
}

fn payloads(log: &Log) -> Vec<Vec<u8>> {
    log.borrow().iter().map(|(_, d)| d.clone()).collect()
}

const TX: u32 = 0x7E0;
const RX: u32 = 0x7E8;

fn isotp(frames: Vec<(u32, Vec<u8>)>) -> (IsoTp, Log) {
    let (link, log) = ScriptLink::new(frames);
    (IsoTp::new(Box::new(link), TX, RX), log)
}

// ---------------------------------------------------------------------------
// ISO-TP (ISO 15765-2)
// ---------------------------------------------------------------------------

#[test]
fn isotp_single_frame_zero_length_is_rejected() {
    // SF with length nibble 0 is reserved and must not decode to an empty body.
    let (mut tp, _l) = isotp(vec![(
        RX,
        vec![0x00, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA],
    )]);
    assert!(matches!(tp.recv(100), Err(Error::Protocol(_))));
}

#[test]
fn isotp_truncated_first_frame_is_rejected() {
    // A first frame must carry a full 8 data bytes; a short one is malformed.
    let (mut tp, _l) = isotp(vec![(RX, vec![0x10, 0x14, 0x01, 0x02, 0x03])]);
    assert!(matches!(tp.recv(100), Err(Error::Protocol(_))));
}

#[test]
fn isotp_first_frame_for_short_payload_is_rejected() {
    // FF announcing a length that fits a single frame (<=7) is a protocol abuse.
    let (mut tp, _l) = isotp(vec![(RX, vec![0x10, 0x05, 0, 0, 0, 0, 0, 0])]);
    assert!(matches!(tp.recv(100), Err(Error::Protocol(_))));
}

#[test]
fn isotp_unexpected_pci_on_recv_is_rejected() {
    // A flow-control frame where a SF/FF is expected is not a valid reply.
    let (mut tp, _l) = isotp(vec![(RX, vec![0x30, 0x00, 0x00])]);
    assert!(matches!(tp.recv(100), Err(Error::Protocol(_))));
}

#[test]
fn isotp_send_empty_payload_is_rejected() {
    let (mut tp, _l) = isotp(vec![]);
    assert!(matches!(tp.send(&[], 100), Err(Error::Protocol(_))));
}

#[test]
fn isotp_single_frame_send_is_padded_to_eight() {
    // Default padding (0xAA) must fill a short single frame to DLC 8.
    let (mut tp, log) = isotp(vec![]);
    tp.send(&[0x22, 0x12, 0x34], 100).unwrap();
    assert_eq!(
        payloads(&log),
        vec![vec![0x03, 0x22, 0x12, 0x34, 0xAA, 0xAA, 0xAA, 0xAA]]
    );
}

#[test]
fn isotp_reserved_flow_status_aborts_send() {
    // A multi-frame send whose flow-control carries a reserved status (0x3) must
    // abort rather than stream consecutive frames blindly.
    let (mut tp, _l) = isotp(vec![(RX, vec![0x33, 0x00, 0x00])]);
    let payload: Vec<u8> = (0..12).collect();
    assert!(matches!(tp.send(&payload, 100), Err(Error::Protocol(_))));
}

#[test]
fn isotp_too_many_wait_frames_aborts_send() {
    // WAIT (0x31) is honoured only up to wft_max; beyond it the send gives up.
    let (mut tp, _l) = isotp(vec![
        (RX, vec![0x31, 0x00, 0x00]),
        (RX, vec![0x31, 0x00, 0x00]),
        (RX, vec![0x31, 0x00, 0x00]),
    ]);
    tp.wft_max = 2;
    let payload: Vec<u8> = (0..12).collect();
    assert!(matches!(tp.send(&payload, 100), Err(Error::Protocol(_))));
}

#[test]
fn isotp_truncated_cts_flow_control_is_rejected() {
    // A CTS flow-control frame missing its BS/STmin bytes is malformed.
    let (mut tp, _l) = isotp(vec![(RX, vec![0x30])]);
    let payload: Vec<u8> = (0..12).collect();
    assert!(matches!(tp.send(&payload, 100), Err(Error::Protocol(_))));
}

#[test]
fn isotp_request_retries_through_response_pending() {
    // DiagTransport::request must loop over NRC 0x78 (responsePending) and then
    // return the real answer.
    let (mut tp, _l) = isotp(vec![
        (RX, vec![0x03, 0x7F, 0x22, 0x78]), // responsePending
        (RX, vec![0x03, 0x62, 0xF1, 0x90]), // real positive reply
    ]);
    let resp = tp.request(&[0x22, 0xF1, 0x90], 100).unwrap();
    assert_eq!(resp, vec![0x62, 0xF1, 0x90]);
}

// ---------------------------------------------------------------------------
// KWP2000 (ISO 14230) over a scripted DiagTransport
// ---------------------------------------------------------------------------

/// A scripted request/response transport: returns queued responses in order and
/// records each request so a test can assert the exact bytes on the wire.
struct ScriptTransport {
    responses: VecDeque<Vec<u8>>,
    requests: Rc<RefCell<Vec<Vec<u8>>>>,
}

impl ScriptTransport {
    fn new(responses: Vec<Vec<u8>>) -> (Self, Rc<RefCell<Vec<Vec<u8>>>>) {
        let requests = Rc::new(RefCell::new(Vec::new()));
        (
            Self {
                responses: VecDeque::from(responses),
                requests: requests.clone(),
            },
            requests,
        )
    }
}

impl DiagTransport for ScriptTransport {
    fn request(&mut self, req: &[u8], timeout_ms: u64) -> Result<Vec<u8>> {
        self.requests.borrow_mut().push(req.to_vec());
        self.responses.pop_front().ok_or(Error::Timeout(timeout_ms))
    }
}

#[test]
fn kwp_negative_response_surfaces_service_and_code() {
    let (mut t, _r) = ScriptTransport::new(vec![vec![0x7F, 0x10, 0x22]]);
    let mut kwp = Kwp2000Client::new(&mut t);
    match kwp.start_session(0x87) {
        Err(Error::NegativeResponse {
            service: 0x10,
            code: 0x22,
            name,
        }) => assert_eq!(name, "conditionsNotCorrect"),
        other => panic!("expected NegativeResponse, got {other:?}"),
    }
}

#[test]
fn kwp_invalid_key_maps_to_security_denied() {
    // invalidKey (0x35) during securityAccess must collapse to SecurityDenied.
    let (mut t, _r) = ScriptTransport::new(vec![vec![0x7F, 0x27, 0x35]]);
    let mut kwp = Kwp2000Client::new(&mut t);
    assert!(matches!(
        kwp.send_key(0x02, &[0xDE, 0xAD, 0xBE, 0xEF]),
        Err(Error::SecurityDenied)
    ));
}

#[test]
fn kwp_read_memory_encodes_three_byte_address() {
    // readMemoryByAddress uses the low 3 bytes of the address + a length byte;
    // the positive reply strips the service id.
    let (mut t, reqs) = ScriptTransport::new(vec![vec![0x63, 0xDE, 0xAD]]);
    let mut kwp = Kwp2000Client::new(&mut t);
    let data = kwp.read_memory(0x0012_3456, 0x04).unwrap();
    assert_eq!(data, vec![0xDE, 0xAD]);
    assert_eq!(
        reqs.borrow()[0],
        vec![0x23, 0x12, 0x34, 0x56, 0x04],
        "3-byte address + length must be laid out big-endian"
    );
}

#[test]
fn kwp_tester_present_accepts_positive_reply() {
    let (mut t, reqs) = ScriptTransport::new(vec![vec![0x7E, 0x01]]);
    let mut kwp = Kwp2000Client::new(&mut t);
    kwp.tester_present().unwrap();
    assert_eq!(reqs.borrow()[0], vec![0x3E, 0x01]);
}

// ---------------------------------------------------------------------------
// NRC / response-validation helpers
// ---------------------------------------------------------------------------

#[test]
fn check_response_rejects_empty_and_wrong_service() {
    assert!(matches!(check_response(0x22, &[]), Err(Error::Protocol(_))));
    assert!(matches!(
        check_response(0x22, &[0x50, 0x01]),
        Err(Error::Protocol(_))
    ));
    assert!(check_response(0x22, &[0x62, 0x00]).is_ok());
}

#[test]
fn nrc_name_covers_known_and_unknown_codes() {
    assert_eq!(nrc_name(0x78), "requestCorrectlyReceivedResponsePending");
    assert_eq!(nrc_name(0x35), "invalidKey");
    assert_eq!(nrc_name(0x99), "unknown");
}

// ---------------------------------------------------------------------------
// VW TP2.0 (VAG connection-oriented transport)
// ---------------------------------------------------------------------------

const VW_ECU: u8 = 0x01;
const VW_RX: u32 = 0x300; // id the ECU answers on (tester-chosen)
const VW_TX: u32 = 0x740; // id the ECU assigns back to the tester

// TP2.0 opcodes (the module keeps these private; mirror the documented wire
// values here so the test speaks the same protocol).
const OP_DATA_LAST: u8 = 0x10;
const OP_DATA_MORE: u8 = 0x20;
const OP_ACK: u8 = 0xB0;
const OP_SETUP_RESP: u8 = 0xD0;
const PARAM_RESP: u8 = 0xA1;
const PARAM_DISCONNECT: u8 = 0xA8;

fn vw_setup_resp() -> (u32, Vec<u8>) {
    (
        BROADCAST_ID + VW_ECU as u32,
        vec![
            VW_ECU,
            OP_SETUP_RESP,
            0x00,
            0x10,
            (VW_TX & 0xFF) as u8,
            ((VW_TX >> 8) & 0x0F) as u8,
            PROTOCOL_DIAGNOSTIC,
        ],
    )
}

// Parameter response with the given block size and zero separation (no sleep).
fn vw_param_resp(bs: u8) -> (u32, Vec<u8>) {
    (VW_RX, vec![PARAM_RESP, bs, 0x00, 0xFF, 0x00, 0xFF])
}

fn vwtp(frames: Vec<(u32, Vec<u8>)>) -> VwTp20 {
    let (link, _log) = ScriptLink::new(frames);
    VwTp20::new(Box::new(link), VW_ECU, VW_RX)
}

#[test]
fn vwtp20_setup_rejection_no_resources_is_error() {
    // 0xD8 = "no free resources": channel setup must fail, not proceed.
    let mut tp = vwtp(vec![(
        BROADCAST_ID + VW_ECU as u32,
        vec![VW_ECU, 0xD8, 0x00, 0x10, 0x00, 0x00, PROTOCOL_DIAGNOSTIC],
    )]);
    assert!(matches!(tp.open(100), Err(Error::Protocol(_))));
}

#[test]
fn vwtp20_receive_sequence_error_is_rejected() {
    // After a clean open + request ACK, the ECU's data frames carry a bad
    // sequence number on the second frame.
    let ack = (VW_RX, vec![OP_ACK | 1]);
    let f0 = (VW_RX, vec![OP_DATA_MORE, 0x00, 0x10, 0, 1, 2, 3, 4]); // seq 0, len=16
    let f1_bad = (VW_RX, vec![OP_DATA_LAST | 5, 9, 9, 9]); // seq 5, expected 1
    let mut tp = vwtp(vec![vw_setup_resp(), vw_param_resp(0x0F), ack, f0, f1_bad]);
    assert!(matches!(
        tp.request(&[0x3E, 0x01], 100),
        Err(Error::Protocol(_))
    ));
}

#[test]
fn vwtp20_ecu_disconnect_during_receive_is_error() {
    // An A8 (disconnect) arriving while we wait for the response tears down.
    let ack = (VW_RX, vec![OP_ACK | 1]);
    let bye = (VW_RX, vec![PARAM_DISCONNECT]);
    let mut tp = vwtp(vec![vw_setup_resp(), vw_param_resp(0x0F), ack, bye]);
    assert!(matches!(
        tp.request(&[0x3E, 0x01], 100),
        Err(Error::Protocol(_))
    ));
}

#[test]
fn vwtp20_keep_alive_roundtrip() {
    // open() then a channel test (A3) answered by the ECU's A1 parameter frame.
    let mut tp = vwtp(vec![
        vw_setup_resp(),
        vw_param_resp(0x0F),
        vw_param_resp(0x0F),
    ]);
    tp.open(100).unwrap();
    tp.keep_alive(100).unwrap();
}
