//! Datalogging triggers end to end in the library: a seeded fuzz of the
//! evaluator (never panics, windows never overlap or run backwards, the
//! pre-trigger buffer stays bounded, every row is written or dropped exactly
//! once), a fuzz of the CSV layer, and `run_csv_resilient` through a
//! [`TriggerCsvWriter`] over a link that drops out mid-pull: one window, the
//! link rows inside it, opened by the Log viewer. Virtual time, no sleeps.

use opentune_core::resilient::{Clock, VirtualClock};
use opentune_core::RawDiag;
use opentune_logdef::trigger::{
    Action, ChannelRef, Compare, Condition, Emit, Op, Trigger, TriggerSet, TRIGGER_FORMAT,
};
use opentune_logdef::{
    FastLogConfig, LiveSession, ReconnectPolicy, RunLimits, RunOptions, Sample, TriggerCsvWriter,
    TriggerEngine, TriggerOutput, LINK_PARAM_ID,
};
use opentune_udssim::server::UdsServer;
use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

/// A small deterministic generator (PCG-style LCG), so failures reproduce.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[self.below(v.len() as u64) as usize]
    }
}

const CHANNELS: [&str; 3] = ["a", "b", "c"];
const UNITS: [&str; 5] = ["", "%", "°C", "°F", "bar"];

fn random_condition(g: &mut Lcg, depth: u32) -> Condition {
    if depth < 3 && g.below(3) == 0 {
        let n = 1 + g.below(3) as usize;
        let members = (0..n).map(|_| random_condition(g, depth + 1)).collect();
        return if g.below(2) == 0 {
            Condition::All { all: members }
        } else {
            Condition::Any { any: members }
        };
    }
    let ops = [Op::Gt, Op::Ge, Op::Lt, Op::Le, Op::Eq, Op::Ne];
    Condition::Compare(Compare {
        channel: if g.below(2) == 0 {
            ChannelRef::One((*g.pick(&CHANNELS)).into())
        } else {
            ChannelRef::Any(vec!["zz".into(), (*g.pick(&CHANNELS)).into()])
        },
        op: *g.pick(&ops),
        value: g.below(100) as f64,
        units: (*g.pick(&UNITS)).into(),
    })
}

fn random_set(g: &mut Lcg) -> TriggerSet {
    let n = 1 + g.below(4) as usize;
    let triggers = (0..n)
        .map(|i| {
            let action = *g.pick(&[Action::Record, Action::Mark, Action::Split]);
            let gates = action.gates();
            Trigger {
                name: format!("t{i}"),
                description: String::new(),
                action,
                when: random_condition(g, 0),
                arm_ms: g.below(4) * 100,
                release_ms: g.below(4) * 100,
                pre_ms: if gates { g.below(6) * 100 } else { 0 },
                post_ms: if gates { g.below(6) * 100 } else { 0 },
            }
        })
        .collect();
    let set = TriggerSet {
        format: TRIGGER_FORMAT.into(),
        version: 1,
        id: String::new(),
        name: String::new(),
        description: String::new(),
        max_pre_rows: 1 + g.below(40) as usize,
        triggers,
    };
    set.check().unwrap();
    set
}

fn sample(id: &str, value: Option<f64>, units: &str) -> Sample {
    Sample {
        param_id: id.into(),
        name: id.into(),
        raw: None,
        value,
        units: units.into(),
        note: String::new(),
        text: None,
    }
}

#[test]
fn fuzz_evaluator_invariants() {
    for seed in 0..400u64 {
        let mut g = Lcg(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x5EED);
        let set = random_set(&mut g);
        let max_pre = set.max_pre_rows;
        let gated = set.gates();
        let mut e: TriggerEngine<u64> = TriggerEngine::new(set);
        let mut ts: u64 = 1_700_000_000_000;
        let mut next_row = 0u64;
        let mut emitted: Vec<u64> = Vec::new();
        let mut open: Option<u32> = None;
        let mut windows_seen = 0u32;
        let mut last_marker_ts = 0u64;
        let mut check = |out: Vec<Emit<u64>>, emitted: &mut Vec<u64>| {
            for em in out {
                match em {
                    Emit::Open { window } => {
                        assert!(open.is_none(), "seed {seed}: window opened inside a window");
                        windows_seen += 1;
                        assert_eq!(window, windows_seen, "seed {seed}: window numbering");
                        open = Some(window);
                    }
                    Emit::Close { window } => {
                        assert_eq!(open.take(), Some(window), "seed {seed}: close without open");
                    }
                    Emit::Row(r) => {
                        assert!(
                            !gated || open.is_some(),
                            "seed {seed}: row outside a window"
                        );
                        assert!(
                            emitted.last().map_or(true, |l| *l < r),
                            "seed {seed}: row {r} written twice or out of order"
                        );
                        emitted.push(r);
                    }
                    Emit::Marker(m) => {
                        assert!(
                            !gated || open.is_some(),
                            "seed {seed}: marker outside a window"
                        );
                        // Start markers carry the window's first row time,
                        // which may precede the last mark of the window
                        // before only if windows overlapped.
                        assert!(
                            m.ts >= last_marker_ts,
                            "seed {seed}: marker time ran backwards"
                        );
                        last_marker_ts = m.ts;
                    }
                }
            }
        };
        for _ in 0..300 {
            // Mostly forward, sometimes the same ms, occasionally backwards.
            match g.below(10) {
                0 => ts = ts.saturating_sub(g.below(500)),
                1 => {}
                _ => ts += g.below(150),
            }
            let samples: Vec<Sample> = if g.below(15) == 0 {
                vec![sample(LINK_PARAM_ID, None, "")]
            } else {
                let mut v = Vec::new();
                for c in CHANNELS {
                    if g.below(8) == 0 {
                        continue;
                    }
                    let x = match g.below(10) {
                        0 => None,
                        1 => Some(f64::NAN),
                        _ => Some(g.below(110) as f64),
                    };
                    v.push(sample(c, x, g.pick(&UNITS)));
                }
                v
            };
            let rows: Vec<u64> = (0..1 + g.below(3))
                .map(|_| {
                    next_row += 1;
                    next_row
                })
                .collect();
            let out = e.push(ts, &samples, rows);
            check(out, &mut emitted);
            assert!(
                e.buffered_rows() <= max_pre,
                "seed {seed}: pre buffer {} > {max_pre}",
                e.buffered_rows()
            );
        }
        check(e.finish(), &mut emitted);
        assert!(open.is_none(), "seed {seed}: window left open");
        assert!(!e.window_open());
        let r = e.report();
        assert_eq!(r.windows.len() as u32, windows_seen, "seed {seed}");
        for w in &r.windows {
            assert!(w.start_ts <= w.end_ts, "seed {seed}: negative window");
        }
        for p in r.windows.windows(2) {
            assert!(
                p[1].start_ts >= p[0].end_ts,
                "seed {seed}: windows overlap: {:?}",
                p
            );
        }
        let in_windows: u64 = r.windows.iter().map(|w| w.rows).sum();
        if gated {
            assert_eq!(in_windows, emitted.len() as u64, "seed {seed}");
            assert_eq!(
                emitted.len() as u64 + r.dropped_rows,
                next_row,
                "seed {seed}: every row is written or dropped exactly once"
            );
        } else {
            assert_eq!(emitted.len() as u64, next_row, "seed {seed}");
            assert!(r.windows.is_empty());
        }
    }
}

#[test]
fn fuzz_csv_layer_never_panics() {
    let pieces = [
        "1",
        "rpm",
        "a",
        "b",
        "\"",
        "\"\"",
        ",",
        "\n",
        "\r\n",
        "3000",
        "NaN",
        "-1",
        "link",
        "°C",
        "æøå",
        "",
        "1e400",
        "18446744073709551616",
    ];
    for seed in 0..200u64 {
        let mut g = Lcg(seed + 77);
        let set = TriggerSet::load_json(
            r#"{"triggers":[
              {"name":"r","action":"record","when":{"channel":"rpm","op":">","value":5},"pre_ms":100,"post_ms":100},
              {"name":"m","action":"mark","when":{"channel":"a","op":"<","value":5}}
            ]}"#,
        )
        .unwrap();
        let sink = Rc::new(RefCell::new(0usize));
        struct Count(Rc<RefCell<usize>>);
        impl Write for Count {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                *self.0.borrow_mut() += b.len();
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let header = if seed % 2 == 0 {
            "round,param_id,name,raw,value,units,note,text,ts\n"
        } else {
            "ts,param_id,name,raw,value,units,note,text\n"
        };
        let mut w = TriggerCsvWriter::new(
            set,
            TriggerOutput::Single(Box::new(Count(Rc::clone(&sink)))),
        )
        .unwrap();
        w.write_all(header.as_bytes()).unwrap();
        let mut text = String::new();
        for _ in 0..400 {
            text.push_str(g.pick(&pieces));
        }
        for chunk in text.as_bytes().chunks(1 + g.below(9) as usize) {
            w.write_all(chunk).unwrap();
        }
        w.finish().unwrap();
        assert!(*sink.borrow() >= header.len());
    }
}

const LOGGER: &str = r#"<logger><protocols><protocol id="UDS"><parameters mode="22">
  <parameter id="rpm" name="Engine speed" desc="">
    <address length="2">0x1000</address>
    <conversions><conversion units="rpm" expr="x/4" storagetype="uint16"/></conversions>
  </parameter>
  <parameter id="tps" name="Throttle" desc="">
    <address length="1">0x1001</address>
    <conversions><conversion units="%" expr="x" storagetype="uint8"/></conversions>
  </parameter>
</parameters></protocol></protocols></logger>"#;

/// A car on a scripted drive, by virtual time since the start: idle, a
/// wide-open-throttle pull from 400 ms to 1400 ms, and the adapter silent
/// from 700 ms to 1000 ms (in the middle of the pull).
struct Car {
    srv: UdsServer,
    clock: VirtualClock,
}

impl RawDiag for Car {
    fn request(&mut self, req: &[u8]) -> Result<Vec<u8>, String> {
        let t = self.clock.now_ms();
        if (700..1000).contains(&t) {
            return Err("timed out waiting for a response".into());
        }
        let (rpm, tps) = if (400..1400).contains(&t) {
            (4500u16, 97u8)
        } else {
            (900, 8)
        };
        self.srv.set_did(0x1000, (rpm * 4).to_be_bytes().to_vec());
        self.srv.set_did(0x1001, vec![tps]);
        Ok(self.srv.handle(req))
    }
}

const WOT: &str = r#"{
  "format": "opentune-triggers", "version": 1,
  "triggers": [
    { "name": "wot", "action": "split",
      "when": { "all": [
        { "channel": "rpm", "op": ">", "value": 3000, "units": "1/min" },
        { "channel": "tps", "op": ">", "value": 90, "units": "%" } ] },
      "arm_ms": 100, "release_ms": 200, "pre_ms": 200, "post_ms": 300 }
  ]
}"#;

#[derive(Clone, Default)]
struct Buf(Rc<RefCell<Vec<u8>>>);
impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_pull_through_a_link_drop_is_one_window_the_viewer_opens() {
    const BASE: u64 = 1_700_000_000_000;
    let clock = VirtualClock::new(BASE);
    let def = opentune_logdef::parse(LOGGER).unwrap();
    let mut session = LiveSession::new(&def, &[], FastLogConfig::default()).unwrap();
    let mut car = Car {
        srv: UdsServer::new(),
        clock: clock.clone(),
    };
    session.start(&mut car);
    let files: Rc<RefCell<Vec<Buf>>> = Rc::default();
    let f2 = Rc::clone(&files);
    let mut w = TriggerCsvWriter::new(
        TriggerSet::load_json(WOT).unwrap(),
        TriggerOutput::PerWindow(Box::new(move |_: u32| -> std::io::Result<Box<dyn Write>> {
            let b = Buf::default();
            f2.borrow_mut().push(b.clone());
            Ok(Box::new(b))
        })),
    )
    .unwrap();
    let opts = RunOptions {
        limits: RunLimits {
            rounds: Some(25),
            duration_ms: None,
            interval_ms: 100,
        },
        policy: ReconnectPolicy {
            dead_after: 2,
            initial_backoff_ms: 100,
            max_backoff_ms: 100,
            ..ReconnectPolicy::default()
        },
        stop: None,
        clock: &clock,
    };
    let mut open =
        || -> Result<Box<dyn RawDiag>, String> { panic!("no reopen without --reconnect") };
    let rep = session
        .run_csv_resilient(Box::new(car), &mut open, &opts, None, &mut w)
        .unwrap();
    assert!(rep.drops >= 1, "{rep:?}");
    let r = w.finish().unwrap();
    assert_eq!(r.fires, vec![("wot".to_string(), 1)], "{r:?}");
    assert_eq!(
        r.windows.len(),
        1,
        "the link drop did not split the pull: {r:?}"
    );
    let win = &r.windows[0];
    assert!(!win.cut_by_end);
    // pre (200 ms) + the pull + release hold + post: well over a second.
    assert!(win.duration_ms() >= 1300, "{win:?}");
    assert!(
        r.dropped_rows > 0,
        "idle rounds outside the window are dropped"
    );

    let files = files.borrow();
    assert_eq!(files.len(), 1);
    let csv = String::from_utf8(files[0].0.borrow().clone()).unwrap();
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(lines[0], "round,param_id,name,raw,value,units,note,text,ts");
    assert!(
        lines[1].contains(",trigger,wot,") && lines[1].contains(",start,"),
        "{csv}"
    );
    let last = lines.last().unwrap();
    assert!(
        last.contains(",trigger,wot,") && last.contains(",end,"),
        "{csv}"
    );
    // The link rows are inside the window, in order.
    let link: Vec<&str> = lines
        .iter()
        .filter(|l| l.split(',').nth(1) == Some(LINK_PARAM_ID))
        .map(|l| l.split(',').nth(7).unwrap())
        .collect();
    assert_eq!(link, ["lost", "recovered"], "{csv}");
    // Rows before the pull's pre window are gone: no 900 rpm row older
    // than 200 ms before the fire.
    let rpm_rows = lines.iter().filter(|l| l.contains(",rpm,")).count();
    assert!(rpm_rows < 25, "{csv}");

    let log = opentune_logview::parse(&csv, &opentune_logview::ParseOptions::default()).unwrap();
    assert_eq!(log.format, opentune_logview::LogFormat::OpentuneLong);
    let names: Vec<&str> = log.channels.iter().map(|c| c.name.as_str()).collect();
    assert!(
        names.contains(&"Engine speed") && names.contains(&"Throttle"),
        "{names:?}"
    );
    let rpm = &log.channels[names.iter().position(|n| *n == "Engine speed").unwrap()].values;
    assert!(rpm.contains(&4500.0), "{rpm:?}");
}
