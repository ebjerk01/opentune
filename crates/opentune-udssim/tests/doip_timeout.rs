//! `DoIpTransport::request` honours the caller's timeout against the
//! simulator's TCP entity when the target ECU never answers (a silent target),
//! instead of waiting out the 60 s exchange cap. Loopback only.

use opentune_core::protocol::diag::DiagTransport;
use opentune_core::protocol::doip::{DoIpTransport, DIAG_ACK_TIMEOUT_MS, TOTAL_DIAG_TIMEOUT_MS};
use opentune_core::protocol::UdsClient;
use opentune_core::Error;
use opentune_udssim::{DoIpSimStream, TcpEntityServer, UdsServer};
use std::net::TcpStream;
use std::time::{Duration, Instant};

const ECU: u16 = 0x0010;
const TESTER: u16 = 0x0E80;

fn entity(silent: bool, pos_ack: bool) -> (TcpEntityServer, DoIpTransport<TcpStream>) {
    let srv = TcpEntityServer::spawn("127.0.0.1:0".parse().unwrap(), move || {
        let mut server = UdsServer::new();
        server.set_did(0xF190, b"SIMVIN".to_vec());
        let mut e = DoIpSimStream::new(server, ECU);
        e.set_silent(silent);
        e.set_pos_ack(pos_ack);
        e
    })
    .unwrap();
    let addr = srv.local_addr();
    let mut tp = DoIpTransport::connect(addr.ip(), addr.port(), TESTER, ECU).unwrap();
    tp.activate(2000).unwrap();
    (srv, tp)
}

/// Time one request; returns its result and how long it took.
fn timed(tp: &mut DoIpTransport<TcpStream>, timeout_ms: u64) -> (Result<Vec<u8>, Error>, Duration) {
    let t0 = Instant::now();
    let r = tp.request(&[0x22, 0xF1, 0x90], timeout_ms);
    (r, t0.elapsed())
}

#[test]
fn acknowledged_but_silent_target_fails_after_the_callers_timeout() {
    let (_srv, mut tp) = entity(true, true);
    let (r, took) = timed(&mut tp, 300);
    assert!(matches!(r, Err(Error::Timeout(300))), "{r:?}");
    assert!(took >= Duration::from_millis(290), "{took:?}");
    assert!(
        took < Duration::from_millis(1500),
        "waited {took:?}, not the 300 ms asked for"
    );
    // The transport stays usable for the next request.
    let (r, _) = timed(&mut tp, 200);
    assert!(matches!(r, Err(Error::Timeout(200))), "{r:?}");
}

#[test]
fn unacknowledged_silent_target_waits_only_the_ack_window() {
    // No diagnostic ack either: the tester allows A_DoIP_Diagnostic_Message
    // (2 s) for the ack, then gives up, still far below the 60 s cap.
    let (_srv, mut tp) = entity(true, false);
    let (r, took) = timed(&mut tp, 300);
    assert!(
        matches!(r, Err(Error::Timeout(t)) if t == DIAG_ACK_TIMEOUT_MS),
        "{r:?}"
    );
    assert!(
        took >= Duration::from_millis(DIAG_ACK_TIMEOUT_MS - 50),
        "{took:?}"
    );
    assert!(
        took < Duration::from_millis(DIAG_ACK_TIMEOUT_MS + 1500),
        "{took:?}"
    );
    assert!(took < Duration::from_millis(TOTAL_DIAG_TIMEOUT_MS));
}

#[test]
fn answering_target_is_unaffected() {
    for pos_ack in [true, false] {
        let (_srv, mut tp) = entity(false, pos_ack);
        let mut uds = UdsClient::new(&mut tp);
        assert_eq!(uds.read_data_by_id(0xF190).unwrap(), b"SIMVIN");
    }
}
