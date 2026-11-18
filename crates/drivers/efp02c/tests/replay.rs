//! The EFP02C driver against scripted USB transfers: the captured bench
//! session byte for byte, a synthetic identification read, UDS over ISO-TP,
//! and the bus monitor.
use opentune_core::protocol::isotp::CanLink;
use opentune_core::{CanMonitor, ConnectConfig, Interface, ListenOnly, MonitorOpts};
use opentune_driver_efp02c::link::{Efp02cLink, LinkOptions, Timing};
use opentune_driver_efp02c::monitor::Efp02cMonitor;
use opentune_driver_efp02c::proto::{self, rec, RECORD_LEN};
use opentune_driver_efp02c::transport::{Replay, Step};
use opentune_driver_efp02c::Efp02cDriver;

fn hex(s: &str) -> Vec<u8> {
    s.split_whitespace()
        .filter(|t| *t != "|")
        .map(|h| u8::from_str_radix(h, 16).expect("hex"))
        .collect()
}

/// The captured session as replay steps.
fn captured_session() -> Vec<Step> {
    include_str!("fixtures/bench-1mbit-session.txt")
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| {
            let (dir, bytes) = l.split_once(char::is_whitespace).expect("dir");
            match dir {
                "OUT" => Step::Out(hex(bytes)),
                "IN" => Step::In(hex(bytes)),
                other => panic!("bad direction {other}"),
            }
        })
        .collect()
}

fn no_delay() -> LinkOptions {
    LinkOptions {
        timing: Timing::NONE,
        ..LinkOptions::default()
    }
}

/// A synthetic `03` record (DLC in byte 5 only, as the box reports it).
fn rx_record(id: u32, data: &[u8]) -> Vec<u8> {
    let mut r = vec![0u8; RECORD_LEN];
    r[0] = rec::RX;
    r[1] = (id >> 3) as u8;
    r[2] = ((id & 7) as u8) << 5;
    r[5] = data.len() as u8;
    r[6..6 + data.len()].copy_from_slice(data);
    r
}

/// A poll reply: records plus the `[flag, 0, seq]` trailer.
fn reply(records: &[Vec<u8>], flag: u8, seq: u8) -> Vec<u8> {
    let mut b: Vec<u8> = records.concat();
    b.extend_from_slice(&[flag, 0, seq]);
    b
}

#[test]
fn captured_session_replays_byte_for_byte() {
    let replay = Replay::new(captured_session());
    let mut link = Efp02cLink::open(Box::new(replay.clone()), no_delay()).expect("init");
    // The termination read-back answered `0A 01`.
    assert_eq!(link.termination(), Some(true));
    link.send_frame(0x3FB, &[0xFF; 4]).expect("tx");

    // The single captured poll returned 32 frames; the stale frame that came
    // with the termination reply (before enable) is not among them.
    let mut frames = Vec::new();
    while let Some(f) = link.next_rx(0).expect("rx") {
        frames.push(f);
        if frames.len() == 32 {
            break;
        }
    }
    assert_eq!(frames.len(), 32);
    assert_eq!(
        frames.iter().take(5).map(|f| f.id).collect::<Vec<_>>(),
        [0x4BB, 0x4A9, 0x4B2, 0x4BB, 0x7A0]
    );
    assert_eq!(frames[4].data, [0x00, 0x00, 0x60, 0x00]);
    assert!(frames.iter().all(|f| f.data.len() <= 8));
    assert_eq!(replay.remaining(), 0, "every captured transfer was used");

    // Closing resets the box.
    drop(link);
    assert_eq!(replay.written().last().unwrap(), &proto::reset().to_vec());
}

#[test]
fn link_refuses_unconfirmed_frames() {
    let replay = Replay::lenient(vec![]);
    let mut link = Efp02cLink::open(Box::new(replay.clone()), no_delay()).unwrap();
    let before = replay.written().len();
    assert!(link.send_frame(0x18DA_10F1, &[1, 2]).is_err());
    assert!(link.send_frame(0x7E0, &[0; 9]).is_err());
    assert_eq!(replay.written().len(), before, "nothing sent");
    // No data: a timeout, not a hang.
    assert!(matches!(
        link.recv_frame(5),
        Err(opentune_core::Error::Timeout(5))
    ));
}

/// A synthetic identification block: zeros, a model string, valid checksum.
fn synthetic_ident() -> [u8; 32] {
    let mut b = [0u8; 32];
    b[20..25].copy_from_slice(b"EFP02");
    b[31] = b[..31].iter().fold(0u8, |a, &x| a.wrapping_add(x));
    b
}

fn ident_chunk(block: &[u8; 32], i: usize) -> Vec<u8> {
    let mut r = vec![0u8; RECORD_LEN];
    r[0] = rec::RESPONSE;
    r[2] = 0x03;
    r[7] = i as u8;
    r[8..].copy_from_slice(&block[i * 8..i * 8 + 8]);
    r
}

#[test]
fn identify_follows_the_captured_polling_pattern() {
    let block = synthetic_ident();
    let poll = |seq| proto::Poll::IDENTIFY.record(seq).to_vec();
    let with_09 = |seq| [proto::identify().to_vec(), poll(seq)].concat();
    let mut termination_reply = vec![0u8; RECORD_LEN];
    termination_reply[0] = rec::RESPONSE;
    termination_reply[1] = 0x01;
    // An unrelated record type the driver never requests (zeroed).
    let mut other = vec![0u8; RECORD_LEN];
    other[0] = 0x08;
    let steps = vec![
        Step::Out(proto::reset().to_vec()),
        Step::Out(proto::termination_on().to_vec()),
        Step::Out(
            [
                proto::termination_read().to_vec(),
                proto::Poll::ONE.record(0).to_vec(),
            ]
            .concat(),
        ),
        Step::In(reply(&[termination_reply], 1, 0)),
        // Like the capture: the first answers are stale or empty (one with a
        // repeated seq), then all four chunks arrive in one reply.
        Step::Out(with_09(1)),
        Step::In(reply(&[other], 0, 1)),
        Step::Out(poll(2)),
        Step::In(reply(&[], 1, 2)),
        Step::Out(with_09(3)),
        Step::In(reply(&[], 0, 2)),
        Step::Out(poll(4)),
        Step::In(reply(&[], 1, 4)),
        Step::Out(with_09(5)),
        Step::In(reply(
            &(0..4).map(|i| ident_chunk(&block, i)).collect::<Vec<_>>(),
            0,
            5,
        )),
        Step::Out(proto::flush(0).to_vec()),
        Step::Out(proto::config_1mbit_accept_all().to_vec()),
        Step::Out(proto::enable().to_vec()),
    ];
    let replay = Replay::new(steps);
    let mut cfg = ConnectConfig::default();
    cfg.options.insert("bitrate".into(), "1000000".into());
    cfg.options.insert("identify".into(), "on".into());
    let mut d = Efp02cDriver::new();
    d.open_with_transport(Box::new(replay.clone()), &cfg, Timing::NONE)
        .expect("open");
    assert_eq!(replay.remaining(), 0);
    let info = d.info();
    assert!(info.firmware.unwrap().starts_with("EFP02"));
    assert_eq!(info.serial, None);
}

#[test]
fn uds_runs_over_isotp_on_the_link() {
    let mut steps = vec![Step::In(reply(&[], 1, 0))]; // termination read: no answer
    let ecu = |data: &[u8]| {
        let mut d = data.to_vec();
        d.resize(8, 0xAA);
        Step::In(reply(&[rx_record(0x7E8, &d)], 1, 0))
    };
    // F190: requestOutOfRange.
    steps.push(ecu(&[0x03, 0x7F, 0x22, 0x31]));
    // F189: single frame "SW1" — behind an unrelated frame on the bus.
    steps.push(Step::In(reply(
        &[
            rx_record(0x123, &[1, 2, 3]),
            rx_record(0x7E8, &[0x06, 0x62, 0xF1, 0x89, b'S', b'W', b'1', 0xAA]),
        ],
        1,
        0,
    )));
    // F191: multi-frame "HW-123456789" (15-byte payload).
    steps.push(ecu(&[0x10, 0x0F, 0x62, 0xF1, 0x91, b'H', b'W', b'-']));
    steps.push(ecu(&[0x21, b'1', b'2', b'3', b'4', b'5', b'6', b'7']));
    steps.push(ecu(&[0x22, b'8', b'9']));

    let replay = Replay::lenient(steps);
    let mut cfg = ConnectConfig::default();
    cfg.options.insert("bitrate".into(), "1000000".into());
    let mut d = Efp02cDriver::new();
    d.open_with_transport(Box::new(replay.clone()), &cfg, Timing::NONE)
        .expect("open");
    let id = d.read_ecu_id().expect("ecu id");
    assert_eq!(id.vin, None);
    assert_eq!(id.sw_number.as_deref(), Some("SW1"));
    assert_eq!(id.hw_number.as_deref(), Some("HW-123456789"));

    // The requests and the flow control went out as 0x7E0 TX records.
    let tx: Vec<Vec<u8>> = replay
        .written()
        .into_iter()
        .filter(|w| w.len() == RECORD_LEN && w[0] == rec::TX)
        .collect();
    let want = |data: &[u8]| {
        let mut d = data.to_vec();
        d.resize(8, 0xAA);
        proto::tx(0x7E0, &d).unwrap().to_vec()
    };
    assert_eq!(
        tx,
        vec![
            want(&[0x03, 0x22, 0xF1, 0x90]),
            want(&[0x03, 0x22, 0xF1, 0x89]),
            want(&[0x03, 0x22, 0xF1, 0x91]),
            want(&[0x30, 0x00, 0x00]),
        ]
    );
    // The diagnostic transport is exposed.
    assert!(d.diag().is_some());
    d.close().unwrap();
    assert!(d.diag().is_none());
}

#[test]
fn monitor_captures_passively_at_1mbit() {
    let replay = Replay::lenient(vec![
        Step::In(reply(&[], 1, 0)),
        Step::In(reply(
            &[
                rx_record(0x100, &[1, 2, 3]),
                rx_record(0x7FF, &[]),
                rx_record(0x000, &[9; 8]),
            ],
            1,
            1,
        )),
    ]);
    let mut m = Efp02cMonitor::with_transport(Box::new(replay.clone()), Timing::NONE);
    m.start(&MonitorOpts::default()).expect("start");
    assert_eq!(m.listen_only(), ListenOnly::Passive);
    assert!(m.describe().contains("1000 kbit/s"));
    let ids: Vec<(u32, usize)> = (0..3)
        .map(|_| {
            let f = m.next_frame(10).unwrap().unwrap();
            assert!(!f.extended && !f.rtr);
            (f.id(), f.data().len())
        })
        .collect();
    assert_eq!(ids, [(0x100, 3), (0x7FF, 0), (0x000, 8)]);
    assert_eq!(m.next_frame(5).unwrap(), None);
    // The monitor never wrote a TX record.
    assert!(replay
        .written()
        .iter()
        .all(|w| w.chunks(RECORD_LEN).all(|r| r[0] != rec::TX)));
    m.stop().unwrap();
    assert_eq!(replay.written().last().unwrap(), &proto::reset().to_vec());
}

#[test]
fn monitor_refuses_other_bitrates_without_touching_usb() {
    let replay = Replay::lenient(vec![]);
    let mut m = Efp02cMonitor::with_transport(Box::new(replay.clone()), Timing::NONE);
    let mut o = MonitorOpts::default();
    o.options.insert("bus".into(), "ms".into());
    assert!(m.start(&o).is_err());
    assert!(replay.written().is_empty());
}
