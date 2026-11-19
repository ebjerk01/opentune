//! Deterministic seeded fuzzing of the CAN log reader (candump, OpenTune CSV,
//! SavvyCAN CSV, Vector ASC, mixed per line) and the Haldex summary over what
//! it reads. House style of the core `tp20_fuzz.rs`: splitmix64, no extra
//! dependencies, fast in the normal `cargo test`.
//!
//! Invariants: nothing panics; every non-blank, non-header line is either a
//! frame or a counted bad line (never silently both or neither for a frame
//! line); every frame has an id that fits its id type, at most 64 data bytes
//! and a finite timestamp (or none); a well-formed frame line round-trips
//! exactly, whatever noise (comments, events, CAN FD lines, odd timestamps)
//! surrounds it; the summary's ranges are finite and ordered.
use opentune_haldex::can::Bus;
use opentune_haldex::canlog::{parse_log, summarize, LogFrame};

const CASES: u64 = 3000;

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

fn hex(d: &[u8], sep: &str) -> String {
    d.iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(sep)
}

/// A random frame the formats can carry.
fn frame(r: &mut Rng) -> LogFrame {
    let extended = r.below(3) == 0;
    let id = if extended {
        r.pick(&[0x800, 0x1FFF_FFFF, 0x18DA_F110])
            .max(r.below(0x2000_0000) as u32)
            & 0x1FFF_FFFF
    } else {
        r.pick(&[0, 0x7FF, 0x2C0]).max(r.below(0x800) as u32)
    };
    let n = r.below(9) as usize;
    LogFrame {
        ts: Some((r.below(100_000_000) as f64) / 1e6),
        channel: None,
        id,
        extended,
        data: (0..n).map(|_| r.next() as u8).collect(),
    }
}

/// `f` written as one line of a random format; returns the line and the
/// frame as that format reads it back.
fn write(r: &mut Rng, f: &LogFrame) -> (String, LogFrame) {
    let ts = f.ts.unwrap();
    let id = if f.extended {
        format!("{:08X}", f.id)
    } else {
        format!("{:03X}", f.id)
    };
    let mut back = f.clone();
    let line = match r.below(4) {
        0 => {
            back.channel = Some("can0".into());
            format!("({ts:.6}) can0 {id}#{}", hex(&f.data, ""))
        }
        1 => format!(
            "{ts:.6},{id},{},{},{}",
            u8::from(f.extended),
            f.data.len(),
            hex(&f.data, "")
        ),
        2 => {
            // SavvyCAN times are microseconds.
            back.channel = Some("bus0".into());
            back.ts = Some((ts * 1e6).round() / 1e6);
            format!(
                "{},{:08X},{},Rx,0,{},{},",
                (ts * 1e6).round() as u64,
                f.id,
                f.extended,
                f.data.len(),
                hex(&f.data, ",")
            )
        }
        _ => {
            back.channel = Some("1".into());
            let ascid = if f.extended {
                format!("{:X}x", f.id)
            } else {
                format!("{:X}", f.id)
            };
            format!(
                "{ts:>11.6} 1  {ascid:<15} Rx   d {} {}",
                f.data.len(),
                hex(&f.data, " ")
            )
        }
    };
    (line, back)
}

/// Lines every reader must skip without counting them.
fn noise(r: &mut Rng) -> String {
    r.pick(&[
        "",
        "   ",
        "# a comment",
        "// version 9.0.0",
        "date Tue Nov 14 10:13:20.000 pm 2023",
        "base hex  timestamps absolute",
        "Begin Triggerblock Tue Nov 14 10:13:20.000 pm 2023",
        "End TriggerBlock",
        "   0.000000 Start of measurement",
        "   2.700000 1  ErrorFrame",
        "   3.100000 1  Statistic: D 0 R 0 XD 0 XR 0 E 0 O 0 B 0.00%",
        "ts,id,ext,dlc,data",
        "Time Stamp,ID,Extended,Dir,Bus,LEN,D1,D2,D3,D4,D5,D6,D7,D8",
        "internal events logged",
    ])
    .to_string()
}

fn check_frame(f: &LogFrame) {
    assert!(
        f.id <= if f.extended { 0x1FFF_FFFF } else { 0x7FF },
        "{f:?}"
    );
    assert!(f.data.len() <= 64, "{f:?}");
    assert!(f.ts.map_or(true, f64::is_finite), "{f:?}");
}

#[test]
fn written_frames_read_back_exactly_among_noise() {
    let mut r = Rng(0xCA41_0600);
    for case in 0..CASES {
        let mut text = String::new();
        let mut want = Vec::new();
        for _ in 0..r.below(8) {
            if r.below(3) == 0 {
                text.push_str(&noise(&mut r));
                text.push('\n');
            }
            let f = frame(&mut r);
            let (line, back) = write(&mut r, &f);
            text.push_str(&line);
            text.push_str(r.pick(&["\n", "\r\n"]));
            want.push(back);
        }
        let p = parse_log(&text);
        assert_eq!(p.bad_lines, 0, "case {case}:\n{text}");
        assert_eq!(p.frames.len(), want.len(), "case {case}:\n{text}");
        for (got, want) in p.frames.iter().zip(&want) {
            check_frame(got);
            assert_eq!(
                (got.id, got.extended, &got.data),
                (want.id, want.extended, &want.data)
            );
            assert_eq!(got.channel, want.channel);
            let (g, w) = (got.ts.unwrap(), want.ts.unwrap());
            assert!((g - w).abs() < 1e-6, "case {case}: {g} vs {w}");
        }
    }
}

#[test]
fn mutated_logs_never_panic_or_yield_impossible_frames() {
    let mut r = Rng(0x0BAD_C0DE);
    let junk: &[&str] = &[
        "#",
        "##",
        "R",
        ",",
        " ",
        "x",
        "X",
        "CANFD",
        "d",
        "r",
        "nan",
        "inf",
        "-1",
        "1e9",
        "+",
        "FFFFFFFF",
        "18446744073709551615",
        "65",
        "0x",
        "Rx",
        "\u{2603}",
        "(",
        ")",
    ];
    for _ in 0..CASES {
        let mut text = String::new();
        for _ in 0..1 + r.below(6) {
            let f = frame(&mut r);
            let (line, _) = write(&mut r, &f);
            let mut toks: Vec<String> = if r.below(2) == 0 {
                line.split(' ').map(str::to_string).collect()
            } else {
                line.chars().map(String::from).collect()
            };
            for _ in 0..1 + r.below(3) {
                if toks.is_empty() {
                    break;
                }
                let i = r.below(toks.len() as u64) as usize;
                match r.below(4) {
                    0 => toks[i] = r.pick(junk).to_string(),
                    1 => {
                        toks.remove(i);
                    }
                    2 => toks.insert(i, r.pick(junk).to_string()),
                    _ => toks.truncate(i),
                }
            }
            let sep = if line.contains(' ') && toks.iter().all(|t| t.chars().count() > 1) {
                " "
            } else {
                ""
            };
            text.push_str(&toks.join(sep));
            text.push('\n');
        }
        if r.below(4) == 0 {
            text.push_str("base dec\n   1.0 1 4096 Rx d 2 300 16\n");
        }
        let p = parse_log(&text);
        for f in &p.frames {
            check_frame(f);
        }
        let lines = text.lines().filter(|l| !l.trim().is_empty()).count();
        assert!(p.frames.len() + p.bad_lines <= lines);
        let (s, decoded) = summarize(&p, Bus::Auto);
        assert_eq!(s.frames as usize, p.frames.len());
        assert_eq!(s.decoded as usize, decoded.len());
        for range in [s.lock_request_pct, s.lock_actual_pct, s.gen4_temp_c]
            .into_iter()
            .flatten()
        {
            assert!(range.min.is_finite() && range.max.is_finite() && range.min <= range.max);
            assert!(range.min <= range.last && range.last <= range.max);
        }
    }
}

#[test]
fn asc_canfd_lines_with_and_without_a_symbol_name() {
    let mut r = Rng(0xFD);
    for _ in 0..500 {
        let n = r.below(65) as usize;
        let data: Vec<u8> = (0..n).map(|_| r.next() as u8).collect();
        let id = r.below(0x800) as u32;
        let name = if r.below(2) == 0 { "  Sym_Name" } else { "" };
        let line = format!(
            "   2.500000 CANFD   1 Rx        {id:X}{name}  1 0 {:X} {n} {}        0    0     3000        0",
            n.min(15),
            hex(&data, " ")
        );
        let p = parse_log(&line);
        assert_eq!(p.bad_lines, 0, "{line}");
        assert_eq!(p.frames[0].id, id);
        assert_eq!(p.frames[0].data, data, "{line}");
    }
}
