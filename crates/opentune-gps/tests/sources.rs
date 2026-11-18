//! Replay and reader tests over the synthetic NMEA fixtures in
//! `tests/fixtures/` (made-up route in open water; generated with valid
//! checksums, then deliberately damaged in `broken.nmea`).

use opentune_gps::{GpsFeed, GpsReader, GpsSnapshot, ReplayFeed};
use std::collections::BTreeSet;
use std::io::{ErrorKind, Read};
use std::path::PathBuf;
use std::time::Duration;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn replay(name: &str) -> Vec<GpsSnapshot> {
    ReplayFeed::open(&fixture(name)).unwrap().collect()
}

#[test]
fn route_replays_one_epoch_per_snapshot() {
    let epochs = replay("route_gn.nmea");
    assert_eq!(epochs.len(), 20);
    for (i, s) in epochs.iter().enumerate() {
        assert!(s.good, "epoch {i}: {}", s.note);
        assert_eq!(s.fix.talker.as_deref(), Some("GN"));
        let lat = s.fix.lat.unwrap();
        let lon = s.fix.lon.unwrap();
        assert!((56.49..56.52).contains(&lat) && (2.99..3.05).contains(&lon));
        let want = (i as f64 * 6.0).min(90.0);
        assert!((s.fix.speed_kmh.unwrap() - want).abs() < 0.01, "epoch {i}");
        assert_eq!(s.fix.fix_code(), 1);
        assert_eq!(s.age_ms, Some(0));
        assert!(s.fix.sats.unwrap() >= 9);
        assert_eq!(s.fix.hdop, Some(0.9));
        assert_eq!(s.fix.alt_m, Some(4.2));
    }
    let last = epochs.last().unwrap();
    assert_eq!(last.fix.time.unwrap().to_string(), "10:00:19.000");
    assert_eq!(last.fix.date.unwrap().to_string(), "2026-06-15");
    let st = &last.stats;
    assert_eq!((st.rmc, st.gga, st.vtg, st.ignored), (20, 20, 20, 40));
    assert_eq!(st.rejected(), 0);
    // Moving north-east: latitude and longitude both grow.
    assert!(last.fix.lat > epochs[0].fix.lat && last.fix.lon > epochs[0].fix.lon);
}

#[test]
fn every_talker_and_the_old_vtg_form() {
    let epochs = replay("multi_talker.nmea");
    assert_eq!(epochs.len(), 8);
    let talkers: BTreeSet<String> = epochs.iter().filter_map(|s| s.fix.talker.clone()).collect();
    assert_eq!(
        talkers.into_iter().collect::<Vec<_>>(),
        ["BD", "GA", "GB", "GL", "GP"]
    );
    assert!(epochs.iter().all(|s| s.good));
    // Epoch 3 (BD) ends with an NMEA 1.x VTG without unit letters.
    assert!((epochs[3].fix.speed_kmh.unwrap() - 18.0).abs() < 1e-9);
    assert_eq!(epochs[7].stats.rejected(), 0);
}

#[test]
fn broken_input_is_counted_and_the_fix_recovers() {
    let epochs = replay("broken.nmea");
    assert_eq!(epochs.len(), 10);
    for i in [0, 1, 2, 3, 6, 7, 8, 9] {
        assert!(epochs[i].good, "epoch {i}: {}", epochs[i].note);
    }
    for i in [4, 5] {
        let s = &epochs[i];
        assert!(!s.good && !s.stale, "epoch {i}");
        assert!(s.note.starts_with("no fix"), "{}", s.note);
        assert_eq!(s.fix.fix_code(), 0);
        assert_eq!(s.fix.sats, Some(4));
    }
    // Position age grows across the outage and resets after it.
    assert_eq!(epochs[5].age_ms, Some(2000));
    assert_eq!(epochs[6].age_ms, Some(0));
    let st = &epochs[9].stats;
    assert_eq!(st.lines, 56);
    assert_eq!((st.rmc, st.gga, st.vtg, st.ignored), (10, 10, 10, 20));
    assert_eq!(
        (
            st.bad_checksum,
            st.partial,
            st.malformed,
            st.noise,
            st.overlong
        ),
        (2, 1, 1, 1, 1)
    );
}

#[test]
fn replay_feed_goes_stale_at_the_end() {
    let mut feed = ReplayFeed::open(&fixture("multi_talker.nmea")).unwrap();
    for _ in 0..8 {
        assert!(feed.snapshot().good);
    }
    let end = feed.snapshot();
    assert!(end.stale && !end.good);
    assert!(end.ended.is_some());
    assert_eq!(end.note, "replay ended after 8 epoch(s)");
    assert!(feed.describe().contains("multi_talker.nmea"));

    // A looping replay never ends and keeps its clock monotonic.
    let mut lp = ReplayFeed::open(&fixture("multi_talker.nmea"))
        .unwrap()
        .looping(true);
    let snaps: Vec<_> = (0..20).map(|_| lp.snapshot()).collect();
    assert!(snaps.iter().all(|s| s.good));
    assert_eq!(lp.epochs(), 20);
}

/// A serial-port-like reader: small chunks, with read timeouts in between.
struct Chunky {
    data: Vec<u8>,
    pos: usize,
    chunk: usize,
    tick: u32,
}

impl Read for Chunky {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.tick += 1;
        if self.tick % 3 == 0 {
            return Err(ErrorKind::TimedOut.into());
        }
        let n = self.chunk.min(buf.len()).min(self.data.len() - self.pos);
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[test]
fn reader_reassembles_chunks_and_goes_stale() {
    let data = std::fs::read(fixture("route_gn.nmea")).unwrap();
    let src = Chunky {
        data,
        pos: 0,
        chunk: 7,
        tick: 0,
    };
    let mut r = GpsReader::spawn("test", src).with_stale_after(150);
    let s = r.wait_until(Duration::from_secs(5), |s| s.ended.is_some());
    assert_eq!(s.ended.as_deref(), Some("end of stream"));
    assert_eq!((s.stats.rmc, s.stats.gga, s.stats.vtg), (20, 20, 20));
    assert_eq!(s.stats.rejected(), 0);
    assert!((s.fix.speed_kmh.unwrap() - 90.0).abs() < 0.01);
    assert_eq!(r.describe(), "test");

    std::thread::sleep(Duration::from_millis(300));
    let s = r.snapshot();
    assert!(s.stale && !s.good, "{s:?}");
    assert!(s.note.starts_with("stale: no NMEA for"), "{}", s.note);
    // The last values stay available, flagged stale.
    assert!(s.fix.lat.is_some());
    r.stop();
    assert!(!r.running());
}

#[test]
fn reader_reports_read_errors() {
    struct Broken;
    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(ErrorKind::BrokenPipe, "unplugged"))
        }
    }
    let r = GpsReader::spawn("broken", Broken);
    let s = r.wait_until(Duration::from_secs(5), |s| s.ended.is_some());
    assert!(s.ended.unwrap().contains("unplugged"));
    assert!(s.stale && !s.good);
    assert!(s.note.contains("unplugged"), "{}", s.note);
}
