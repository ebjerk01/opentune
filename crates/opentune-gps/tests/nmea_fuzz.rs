//! Deterministic seeded fuzzing of the GPS input boundary: NMEA text from a
//! receiver or a file (house style of the core `tp20_fuzz.rs`: splitmix64, no
//! extra dependencies, fast enough for the normal `cargo test`).
//!
//! Invariants: nothing panics; an accepted sentence carries a checksum that
//! matches and only in-range values (latitude, longitude, course, speed, time,
//! calendar date); hemispheres give the right sign; the line assembler gives
//! the same lines however the bytes are chunked; the tracker's counters add
//! up and a "good" snapshot is never stale or invalid; the replay clock never
//! runs backwards.
use opentune_gps::nmea::{checksum, parse_sentence, Body, NmeaError};
use opentune_gps::{FixTracker, LineAssembler, LineItem, ReplayFeed, MAX_LINE};

const CASES: u64 = 5000;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[self.below(xs.len() as u64) as usize]
    }
}

fn sentence(body: &str) -> String {
    format!("${body}*{:02X}", checksum(body))
}

/// `ddmm.mmmm` / `dddmm.mmmm` for a signed decimal-degree value (rounded to
/// the 1e-4 minute the format carries, never to "60.0000" minutes).
fn coord(v: f64, lat: bool) -> (String, &'static str) {
    let e4 = (v.abs() * 600_000.0).round() as u64;
    let (deg, min) = (e4 / 600_000, e4 % 600_000);
    let hemi = match (lat, v < 0.0) {
        (true, false) => "N",
        (true, true) => "S",
        (false, false) => "E",
        (false, true) => "W",
    };
    let m = format!("{:02}.{:04}", min / 10_000, min % 10_000);
    let s = if lat {
        format!("{deg:02}{m}")
    } else {
        format!("{deg:03}{m}")
    };
    (s, hemi)
}

fn random_time(r: &mut Rng) -> String {
    format!(
        "{:02}{:02}{:02}.{:03}",
        r.below(24),
        r.below(60),
        r.below(61),
        r.below(1000)
    )
}

/// A valid sentence of a random supported type, with random empty fields.
fn valid(r: &mut Rng) -> String {
    let talker = r.pick(&["GP", "GN", "GL", "GA", "GB", "BD", "GQ"]);
    let lat = (r.below(18_000_001) as f64 / 100_000.0) - 90.0;
    let lon = (r.below(36_000_001) as f64 / 100_000.0) - 180.0;
    let (la, ns) = coord(lat, true);
    let (lo, ew) = coord(lon, false);
    // Each optional field is blank one time in eight.
    let mut e = |s: String| if r.below(8) == 0 { String::new() } else { s };
    let (time, sog, cog) = (
        e(random_time(&mut Rng(lat.to_bits()))),
        e(format!("{:.1}", (lon.to_bits() % 3000) as f64 / 10.0)),
        e(format!("{:.1}", (lat.to_bits() % 3601) as f64 / 10.0)),
    );
    let body = match r.below(4) {
        0 => format!(
            "{talker}RMC,{time},{},{la},{ns},{lo},{ew},{sog},{cog},{:02}{:02}{:02},,,{}",
            r.pick(&["A", "V"]),
            1 + r.below(28),
            1 + r.below(12),
            r.below(100),
            r.pick(&["A", "D", "N", "E", ""]),
        ),
        1 => format!(
            "{talker}GGA,{time},{la},{ns},{lo},{ew},{},{:02},{:.1},{:.1},M,{:.1},M,,",
            r.below(9),
            r.below(40),
            r.below(500) as f64 / 10.0,
            r.below(20000) as f64 / 10.0 - 500.0,
            r.below(1000) as f64 / 10.0 - 50.0,
        ),
        2 => format!(
            "{talker}VTG,{cog},T,,M,{:.1},N,{:.1},K,{}",
            r.below(1000) as f64 / 10.0,
            r.below(1800) as f64 / 10.0,
            r.pick(&["A", "N", ""]),
        ),
        _ => format!(
            "{talker}VTG,{:.1},{:.1},{:.1},{:.1}",
            r.below(3601) as f64 / 10.0,
            r.below(3601) as f64 / 10.0,
            r.below(1000) as f64 / 10.0,
            r.below(1800) as f64 / 10.0,
        ),
    };
    sentence(&body)
}

fn mutate(r: &mut Rng, line: &str) -> String {
    let mut b: Vec<u8> = line.as_bytes().to_vec();
    for _ in 0..1 + r.below(3) {
        if b.is_empty() {
            break;
        }
        let i = r.below(b.len() as u64) as usize;
        match r.below(7) {
            0 => b[i] = r.next() as u8,
            1 => {
                b.remove(i);
            }
            2 => b.insert(i, r.pick(b",.*$-+0129ENSWAV\r\n\0 \xff")),
            3 => b.truncate(i),
            4 => {
                let dup = b[i..].iter().take(8).copied().collect::<Vec<_>>();
                b.splice(i..i, dup);
            }
            5 => b.insert(i, b','),
            _ => b[i] = r.pick(b"0123456789"),
        }
    }
    let mut s = String::from_utf8_lossy(&b).into_owned();
    // Half the time re-seal the checksum so field-level garbage gets through
    // to the field parsers instead of stopping at the checksum.
    if r.below(2) == 0 {
        if let (Some(rest), Some(star)) = (s.strip_prefix('$'), s.rfind('*')) {
            let body = rest[..star - 1].to_string();
            s = sentence(&body);
        }
    }
    s
}

fn check_accepted(line: &str) {
    let Ok(p) = parse_sentence(line) else { return };
    // The checksum really matched.
    let t = line.trim_matches(|c: char| c.is_ascii_whitespace() || c == '\0');
    let rest = &t[1..];
    let star = rest.rfind('*').unwrap();
    assert_eq!(
        u8::from_str_radix(&rest[star + 1..], 16).unwrap(),
        checksum(&rest[..star]),
        "{line:?}"
    );
    assert_eq!(p.talker.len(), 2);
    if let Some(tm) = p.time() {
        assert!(tm.hour < 24 && tm.minute < 60 && tm.second <= 60 && tm.millis < 1000);
        assert!(tm.ms_of_day() < 86_401_000);
    }
    let pos_ok = |p: Option<(f64, f64)>| {
        p.map_or(true, |(a, o)| {
            a.is_finite()
                && o.is_finite()
                && (-90.0..=90.0).contains(&a)
                && (-180.0..=180.0).contains(&o)
        })
    };
    let course_ok = |c: Option<f64>| c.map_or(true, |c| (0.0..=360.0).contains(&c));
    let speed_ok = |s: Option<f64>| s.map_or(true, |s| s.is_finite() && s >= 0.0);
    match &p.body {
        Body::Rmc(r) => {
            assert!(pos_ok(r.position) && course_ok(r.course_deg) && speed_ok(r.speed_knots));
            if let Some(d) = r.date {
                assert!((1980..=2079).contains(&d.year) && (1..=12).contains(&d.month));
                let dim = match d.month {
                    2 if d.year % 4 == 0 => 29,
                    2 => 28,
                    4 | 6 | 9 | 11 => 30,
                    _ => 31,
                };
                assert!((1..=dim).contains(&d.day), "{d}");
            }
        }
        Body::Gga(g) => {
            assert!(pos_ok(g.position) && speed_ok(g.hdop));
            assert!(g.quality.map_or(true, |q| q <= 8));
            assert!(g.alt_m.map_or(true, f64::is_finite));
        }
        Body::Vtg(v) => {
            assert!(course_ok(v.course_deg) && speed_ok(v.speed_knots) && speed_ok(v.speed_kmh));
            assert!(v.kmh().map_or(true, |k| k.is_finite() && k >= 0.0));
        }
    }
}

#[test]
fn valid_sentences_parse_and_mutations_never_panic_or_leak_bad_values() {
    let mut r = Rng(0x93EA_0001);
    for case in 0..CASES {
        let line = valid(&mut r);
        assert!(parse_sentence(&line).is_ok(), "case {case}: {line}");
        check_accepted(&line);
        let bad = mutate(&mut r, &line);
        check_accepted(&bad);
        // Without a re-sealed checksum a changed body is a checksum error
        // (or an earlier structural one), never an accepted sentence with
        // different content.
        let mut b = line.clone().into_bytes();
        let i = 1 + r.below(line.find('*').unwrap() as u64 - 1) as usize;
        let old = b[i];
        b[i] = r.pick(b"0123456789ABCDEFNSEW,.");
        if b[i] != old {
            let changed = String::from_utf8(b).unwrap();
            assert!(
                matches!(parse_sentence(&changed), Err(NmeaError::BadChecksum { .. })),
                "case {case}: {changed}"
            );
        }
    }
}

#[test]
fn hemispheres_give_the_sign_and_the_minutes() {
    let mut r = Rng(0x4E5);
    for _ in 0..CASES {
        let lat = (r.below(18_000_001) as f64 / 100_000.0) - 90.0;
        let lon = (r.below(36_000_001) as f64 / 100_000.0) - 180.0;
        let (la, ns) = coord(lat, true);
        let (lo, ew) = coord(lon, false);
        let line = sentence(&format!(
            "GNGGA,000000,{la},{ns},{lo},{ew},1,08,0.9,1.0,M,0.0,M,,"
        ));
        let Body::Gga(g) = parse_sentence(&line).unwrap().body else {
            panic!()
        };
        let (a, o) = g.position.unwrap();
        // 4 decimal places of minutes: within 1e-4 / 60 degrees.
        assert!((a - lat).abs() < 2e-6, "{line}: {a} vs {lat}");
        assert!((o - lon).abs() < 2e-6, "{line}: {o} vs {lon}");
        assert!(lat == 0.0 || (a < 0.0) == (ns == "S") || a == 0.0);
        assert!(lon == 0.0 || (o < 0.0) == (ew == "W") || o == 0.0);
    }
    // Just past the poles / antimeridian is refused, the edges are kept.
    for (la, lo, ok) in [
        ("9000.0000", "18000.0000", true),
        ("9000.0001", "00000.0000", false),
        ("0000.0000", "18000.0001", false),
        ("0059.9999", "00059.9999", true),
        ("0060.0000", "00000.0000", false),
    ] {
        let line = sentence(&format!(
            "GPGGA,000000,{la},S,{lo},W,1,08,0.9,1.0,M,0.0,M,,"
        ));
        assert_eq!(parse_sentence(&line).is_ok(), ok, "{line}");
    }
}

#[test]
fn chunking_does_not_change_the_lines() {
    let mut r = Rng(0xC4_0000);
    for _ in 0..500 {
        let mut stream = Vec::new();
        for _ in 0..r.below(12) {
            let l = if r.below(4) == 0 {
                {
                    let v = valid(&mut r);
                    mutate(&mut r, &v)
                }
            } else {
                valid(&mut r)
            };
            stream.extend_from_slice(l.as_bytes());
            stream.extend_from_slice(r.pick(&[&b"\r\n"[..], b"\n", b"\r", b""]));
            if r.below(10) == 0 {
                // A run of noise longer than any sentence.
                stream.extend(std::iter::repeat(b'x').take(150 + r.below(100) as usize));
            }
        }
        let mut one = LineAssembler::new();
        let mut want = one.push(&stream);
        want.extend(one.finish());
        let mut a = LineAssembler::new();
        let mut got = Vec::new();
        let mut i = 0;
        while i < stream.len() {
            let n = 1 + r.below(40) as usize;
            let j = (i + n).min(stream.len());
            got.extend(a.push(&stream[i..j]));
            i = j;
        }
        got.extend(a.finish());
        assert_eq!(got, want);
        for item in &got {
            if let LineItem::Line(l) = item {
                assert!(l.len() <= MAX_LINE + 4);
                assert!(!l.contains(['\r', '\n']));
            }
        }
    }
}

#[test]
fn tracker_counters_add_up_and_good_means_fresh_and_valid() {
    let mut r = Rng(0x7AC4_E700);
    for _ in 0..200 {
        let mut t = FixTracker::new(1 + r.below(3000));
        let mut now = 0u64;
        let mut nonempty = 0u64;
        for _ in 0..r.below(40) {
            now += r.below(1500);
            let line = match r.below(5) {
                0 => {
                    let v = valid(&mut r);
                    mutate(&mut r, &v)
                }
                1 => String::new(),
                2 => r
                    .pick(&["garbage", "$GPGSV,1,1,00*79", "$PUBX,00*33"])
                    .to_string(),
                _ => valid(&mut r),
            };
            if !line
                .trim_matches(|c: char| c.is_ascii_whitespace() || c == '\0')
                .is_empty()
            {
                nonempty += 1;
            }
            t.ingest_line(&line, now);
            let snap = t.snapshot(now + r.below(4000));
            let st = &snap.stats;
            assert_eq!(st.lines, nonempty);
            assert_eq!(st.accepted() + st.rejected() + st.ignored, st.lines);
            if snap.good {
                assert!(!snap.stale && snap.fix.valid() && snap.note.is_empty());
                assert!(snap.age_ms.unwrap() <= t.stale_after_ms);
            } else {
                assert!(!snap.note.is_empty());
            }
            if let (Some(age), Some(silent)) = (snap.age_ms, snap.silent_ms) {
                assert!(age >= silent, "a position newer than the last sentence");
            }
            if let (Some(a), Some(o)) = (snap.fix.lat, snap.fix.lon) {
                assert!((-90.0..=90.0).contains(&a) && (-180.0..=180.0).contains(&o));
            }
        }
    }
}

#[test]
fn replay_clock_never_runs_backwards() {
    let mut r = Rng(0x2E_91A7);
    for _ in 0..300 {
        let mut text = String::new();
        let mut ms = r.below(86_400_000);
        for _ in 0..r.below(30) {
            // Mostly forward steps, some midnights, some steps back.
            ms = match r.below(8) {
                0 => ms.saturating_sub(r.below(5000)),
                1 => (ms + 86_400_000 - 1000 - r.below(1000)) % 86_400_000,
                _ => (ms + r.below(3000)) % 86_400_000,
            };
            let s = ms / 1000;
            let body = format!(
                "GPGGA,{:02}{:02}{:02}.{:03},4807.038,N,01131.000,E,{},08,0.9,1.0,M,0.0,M,,",
                s / 3600,
                s / 60 % 60,
                s % 60,
                ms % 1000,
                r.below(2)
            );
            text.push_str(&sentence(&body));
            text.push_str("\r\n");
            if r.below(6) == 0 {
                text.push_str("$GPRMC,broken\r\n");
            }
        }
        let feed = ReplayFeed::from_bytes("fuzz", text.as_bytes()).with_stale_after(u64::MAX);
        let mut last_pos_clock: Option<u64> = None;
        for s in feed {
            // age = clock - last position time; a new position resets it to 0,
            // otherwise it only grows: the clock never runs backwards.
            if let (Some(prev), Some(age)) = (last_pos_clock, s.age_ms) {
                assert!(age == 0 || age >= prev, "{prev} -> {age}");
            }
            last_pos_clock = s.age_ms;
            // A step of more than 12 h between epochs never happens here.
            assert!(s.silent_ms.unwrap_or(0) <= 43_200_000);
        }
    }
}
