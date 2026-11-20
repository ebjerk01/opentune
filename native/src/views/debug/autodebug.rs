//! Auto-debug (row 22): the generic part of the desktop's guided self-test
//! (`feat_autodebug.js`). A capability-matched list of READ-ONLY checks of
//! the adapter and the car, each saying what it does and, where one is
//! needed, what to do in the car first. Every request goes through
//! [`super::backend::raw_request`] (Bench gate, read services only, session
//! log); nothing is written to the car. The report is JSON the user saves.
//!
//! Left out of the port: the desktop's run history in browser storage (a
//! report file is the record here).

use super::backend::{hexs, raw_request};
use crate::host::{AdapterAccess, BuildChannel, Capabilities, DiagTarget, LinkInfo};
use serde_json::{json, Value};
use std::time::Duration;

/// What a step needs from the adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Need {
    Nothing,
    Can,
    KLine,
    WriteMemory,
}

impl Need {
    fn met(self, caps: Option<&Capabilities>) -> bool {
        match (self, caps) {
            (Need::Nothing, _) => true,
            (_, None) => false,
            (Need::Can, Some(c)) => c.can,
            (Need::KLine, Some(c)) => c.kline,
            (Need::WriteMemory, Some(c)) => c.write_memory,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Check {
    Link,
    Voltage,
    Pids,
    Vin,
    Dtcs,
    Running,
    Rev,
    Coolant,
    Observe,
    KLineReady,
    FlashCapable,
    Stopped,
}

pub struct Step {
    pub title: &'static str,
    pub need: Need,
    /// What to do in the car before the check (the user confirms or skips).
    pub car: Option<&'static str>,
    /// What the check does.
    pub doing: &'static str,
    check: Check,
}

pub const STEPS: &[Step] = &[
    Step {
        title: "Adapter handshake",
        need: Need::Nothing,
        car: Some("Plug the adapter into the OBD-II port and turn the ignition ON (engine off)."),
        doing: "Checking the top bar's link: the adapter is open and identified.",
        check: Check::Link,
    },
    Step {
        title: "Battery / car power",
        need: Need::Nothing,
        car: None,
        doing: "Checking the OBD port shows about 12 V (car power present).",
        check: Check::Voltage,
    },
    Step {
        title: "OBD-II supported PIDs",
        need: Need::Can,
        car: None,
        doing: "Querying mode 01 PID 00 (which live PIDs the ECU supports).",
        check: Check::Pids,
    },
    Step {
        title: "Read VIN",
        need: Need::Can,
        car: None,
        doing: "Querying mode 09 PID 02 (vehicle identification number).",
        check: Check::Vin,
    },
    Step {
        title: "Stored fault codes",
        need: Need::Can,
        car: None,
        doing: "Querying mode 03 (stored trouble codes). Nothing is cleared.",
        check: Check::Dtcs,
    },
    Step {
        title: "Engine start: RPM",
        need: Need::Can,
        car: Some("Start the engine and let it idle."),
        doing: "Reading live RPM (PID 010C) to confirm the engine is running.",
        check: Check::Running,
    },
    Step {
        title: "Throttle response",
        need: Need::Can,
        car: Some("Gently rev the engine toward 2000 rpm, then release."),
        doing: "Sampling RPM 8 times over about 3 s to see it rise and fall.",
        check: Check::Rev,
    },
    Step {
        title: "Coolant temperature",
        need: Need::Can,
        car: None,
        doing: "Reading coolant temperature (PID 0105).",
        check: Check::Coolant,
    },
    Step {
        title: "Exterior lights (bus activity)",
        need: Need::Can,
        car: Some("Turn the headlights ON, then OFF."),
        doing: "Exercises the body / comfort CAN. Observational on an ELM327; noted for analysis.",
        check: Check::Observe,
    },
    Step {
        title: "K-line interface",
        need: Need::KLine,
        car: None,
        doing: "The adapter reports K-line. A KWP module probe needs a module address \
                (Adapter settings), so this step only notes the capability.",
        check: Check::KLineReady,
    },
    Step {
        title: "Flash capability",
        need: Need::WriteMemory,
        car: None,
        doing: "The adapter reports memory writes. This step only notes the capability: \
                no write is performed.",
        check: Check::FlashCapable,
    },
    Step {
        title: "Engine stop",
        need: Need::Can,
        car: Some("Turn the engine OFF (the ignition may stay on)."),
        doing: "Confirming RPM drops to 0.",
        check: Check::Stopped,
    },
];

/// The steps the adapter can run, in order (indices into [`STEPS`]).
pub fn plan(caps: Option<&Capabilities>) -> Vec<usize> {
    (0..STEPS.len())
        .filter(|&i| STEPS[i].need.met(caps))
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail,
    Info,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Pass => "PASS",
            Verdict::Fail => "FAIL",
            Verdict::Info => "INFO",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Outcome {
    pub title: String,
    pub verdict: Verdict,
    pub detail: String,
}

impl Outcome {
    pub fn new(title: &str, verdict: Verdict, detail: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            verdict,
            detail: detail.into(),
        }
    }
}

/// Where a step's requests go and how fast it samples.
pub struct Ctx<'a> {
    pub channel: &'a BuildChannel,
    pub adapter: &'a (dyn AdapterAccess + 'static),
    pub target: DiagTarget,
    /// Pause between the throttle-response samples.
    pub pace: Duration,
}

fn ascii(b: &[u8]) -> Option<String> {
    let s: String = b
        .iter()
        .filter(|c| (0x20..0x7F).contains(*c))
        .map(|&c| c as char)
        .collect();
    (s.len() >= 4).then_some(s)
}

/// RPM from a `41 0C A B` answer.
fn rpm(c: &Ctx<'_>) -> Result<Option<f64>, String> {
    let r = raw_request(c.channel, c.adapter, c.target, vec![0x01, 0x0C])?;
    Ok(match r.as_slice() {
        [0x41, 0x0C, a, b, ..] => Some(f64::from((u16::from(*a) << 8) | u16::from(*b)) / 4.0),
        _ => None,
    })
}

/// Run `step` (Bench gate first: every request is a `raw_request`).
pub fn run(c: &Ctx<'_>, step: &Step) -> Result<Outcome, String> {
    super::backend::gate(c.channel)?;
    let req = |b: &[u8]| raw_request(c.channel, c.adapter, c.target, b.to_vec());
    let o = |v, d: String| Ok(Outcome::new(step.title, v, d));
    match step.check {
        Check::Link => {
            let l = c.adapter.link();
            if !l.connected {
                return o(
                    Verdict::Fail,
                    "no adapter link: pick the cable and connect in the top bar".into(),
                );
            }
            let v = l
                .battery_v
                .map(|v| format!(" · {v:.1} V"))
                .unwrap_or_default();
            o(Verdict::Pass, format!("{} {}{v}", l.driver, l.port))
        }
        Check::Voltage => match c.adapter.link().battery_v {
            None => o(Verdict::Info, "this adapter cannot read the voltage".into()),
            Some(v) if v < 11.0 => o(
                Verdict::Fail,
                format!("{v:.1} V: low or no car power. Ignition on?"),
            ),
            Some(v) => o(Verdict::Pass, format!("{v:.1} V")),
        },
        Check::Pids => match req(&[0x01, 0x00]) {
            Ok(r) => o(
                if r.starts_with(&[0x41, 0x00]) {
                    Verdict::Pass
                } else {
                    Verdict::Fail
                },
                hexs(&r),
            ),
            Err(e) => o(Verdict::Fail, e),
        },
        Check::Vin => match req(&[0x09, 0x02]) {
            Ok(r) => {
                let v = if r.first() == Some(&0x7F) || r.is_empty() {
                    Verdict::Fail
                } else {
                    Verdict::Pass
                };
                let d = match ascii(r.get(3..).unwrap_or_default()) {
                    Some(a) => format!("\"{a}\"  ({})", hexs(&r)),
                    None => hexs(&r),
                };
                o(v, d)
            }
            Err(e) => o(Verdict::Fail, e),
        },
        Check::Dtcs => match req(&[0x03]) {
            Ok(r) if r.first() == Some(&0x43) => {
                o(Verdict::Pass, format!("DTC frame: {}", hexs(&r)))
            }
            Ok(r) => o(Verdict::Info, hexs(&r)),
            Err(e) => o(Verdict::Fail, e),
        },
        Check::Running => match rpm(c) {
            Ok(Some(n)) => o(
                if n > 300.0 {
                    Verdict::Pass
                } else {
                    Verdict::Fail
                },
                format!("RPM {n:.0}"),
            ),
            Ok(None) => o(
                Verdict::Fail,
                "no RPM: engine running? Correct CAN bus (powertrain = HS)?".into(),
            ),
            Err(e) => o(Verdict::Fail, e),
        },
        Check::Rev => {
            let (mut lo, mut hi, mut n) = (f64::MAX, 0f64, 0);
            for i in 0..8 {
                if i > 0 {
                    std::thread::sleep(c.pace);
                }
                if let Ok(Some(r)) = rpm(c) {
                    lo = lo.min(r);
                    hi = hi.max(r);
                    n += 1;
                }
            }
            if n == 0 {
                return o(Verdict::Fail, "no RPM samples".into());
            }
            o(
                if hi - lo > 300.0 {
                    Verdict::Pass
                } else {
                    Verdict::Fail
                },
                format!("RPM {lo:.0}–{hi:.0} ({n} samples)"),
            )
        }
        Check::Coolant => match req(&[0x01, 0x05]) {
            Ok(r) => match r.as_slice() {
                [0x41, 0x05, a, ..] => o(Verdict::Pass, format!("{} °C", i16::from(*a) - 40)),
                _ => o(Verdict::Info, format!("not reported ({})", hexs(&r))),
            },
            Err(e) => o(Verdict::Fail, e),
        },
        Check::Observe => o(Verdict::Info, "action performed: noted for analysis".into()),
        Check::KLineReady => o(
            Verdict::Info,
            "K-line capable: ready for KWP1281 / KWP2000 modules".into(),
        ),
        Check::FlashCapable => o(
            Verdict::Info,
            "memory-write capable (nothing was written)".into(),
        ),
        Check::Stopped => match rpm(c) {
            Ok(n) => o(
                if n.is_none_or(|n| n < 300.0) {
                    Verdict::Pass
                } else {
                    Verdict::Fail
                },
                format!("RPM {}", n.map_or("none".into(), |n| format!("{n:.0}"))),
            ),
            Err(e) => o(Verdict::Fail, e),
        },
    }
}

/// The car under test (free text, all optional).
#[derive(Clone, Debug, Default)]
pub struct Vehicle {
    pub make: String,
    pub model: String,
    pub year: String,
    pub engine: String,
    pub notes: String,
}

impl Vehicle {
    pub fn label(&self) -> String {
        let v: Vec<&str> = [&self.make, &self.model, &self.year, &self.engine]
            .into_iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        if v.is_empty() {
            "unknown vehicle".into()
        } else {
            v.join(" ")
        }
    }
}

/// The saveable report.
pub fn report(
    when_unix: u64,
    v: &Vehicle,
    link: &LinkInfo,
    caps: Option<&Capabilities>,
    results: &[Outcome],
) -> Value {
    let count = |x| results.iter().filter(|r| r.verdict == x).count();
    json!({
        "tool": "opentune-native auto-debug",
        "version": env!("CARGO_PKG_VERSION"),
        "when_unix": when_unix,
        "vehicle": {
            "make": v.make.trim(), "model": v.model.trim(), "year": v.year.trim(),
            "engine": v.engine.trim(), "notes": v.notes.trim(),
        },
        "adapter": link.connected.then(|| json!({
            "driver": link.driver,
            "battery_v": link.battery_v,
            "capabilities": caps.map(|c| crate::widgets::caps::summary(Some(c))),
        })),
        "summary": {
            "pass": count(Verdict::Pass), "fail": count(Verdict::Fail),
            "info": count(Verdict::Info), "total": results.len(),
        },
        "results": results.iter().map(|r| json!({
            "test": r.title,
            "result": r.verdict.label().to_ascii_lowercase(),
            "detail": r.detail,
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::super::backend::testing::Recorder;
    use super::*;

    fn car(req: &[u8]) -> Vec<u8> {
        match req {
            [0x01, 0x00] => vec![0x41, 0x00, 0xBE, 0x3F, 0xA8, 0x13],
            [0x09, 0x02] => {
                let mut r = vec![0x49, 0x02, 0x01];
                r.extend_from_slice(b"WVWZZZ3CZ9E000000");
                r
            }
            [0x03] => vec![0x43, 0x00],
            [0x01, 0x0C] => vec![0x41, 0x0C, 0x0B, 0xB8],
            [0x01, 0x05] => vec![0x41, 0x05, 0x5A],
            _ => vec![0x7F, req[0], 0x11],
        }
    }

    #[test]
    fn plan_follows_the_capabilities() {
        assert_eq!(plan(None), [0, 1], "no probe: only the adapter checks");
        let can = Capabilities {
            can: true,
            ..Default::default()
        };
        let p = plan(Some(&can));
        assert!(p.iter().all(|&i| STEPS[i].need != Need::KLine));
        assert!(p.iter().all(|&i| STEPS[i].need != Need::WriteMemory));
        assert_eq!(p.len(), STEPS.len() - 2);
        let all = Capabilities {
            can: true,
            kline: true,
            write_memory: true,
            ..Default::default()
        };
        assert_eq!(plan(Some(&all)).len(), STEPS.len());
    }

    #[test]
    fn a_run_reads_only_and_grades_the_answers() {
        let a = Recorder::new(car);
        let alpha = BuildChannel::new(BuildChannel::ALPHA);
        let c = Ctx {
            channel: &alpha,
            adapter: &a,
            target: DiagTarget::ENGINE,
            pace: Duration::ZERO,
        };
        let all = Capabilities {
            can: true,
            kline: true,
            write_memory: true,
            ..Default::default()
        };
        let out: Vec<Outcome> = plan(Some(&all))
            .into_iter()
            .map(|i| run(&c, &STEPS[i]).unwrap())
            .collect();
        let by = |t: &str| out.iter().find(|o| o.title == t).unwrap();
        assert_eq!(by("Adapter handshake").verdict, Verdict::Pass);
        assert_eq!(by("Battery / car power").detail, "12.6 V");
        assert_eq!(by("OBD-II supported PIDs").verdict, Verdict::Pass);
        assert!(by("Read VIN").detail.contains("WVWZZZ3CZ9E000000"));
        assert_eq!(by("Engine start: RPM").detail, "RPM 750");
        assert_eq!(by("Throttle response").verdict, Verdict::Fail, "flat RPM");
        assert_eq!(by("Coolant temperature").detail, "50 °C");
        assert_eq!(by("Engine stop").verdict, Verdict::Fail, "still 750 rpm");
        assert_eq!(by("Flash capability").verdict, Verdict::Info);
        // Only OBD read modes reached the adapter.
        let sent = a.sent();
        assert!(!sent.is_empty());
        assert!(
            sent.iter().all(|r| matches!(r[0], 0x01 | 0x03 | 0x09)),
            "{sent:?}"
        );

        let rep = report(1, &Vehicle::default(), &a.link(), Some(&all), &out);
        assert_eq!(rep["summary"]["total"], out.len());
        assert_eq!(rep["results"][0]["result"], "pass");
        assert_eq!(rep["adapter"]["driver"], "mock");
        assert_eq!(Vehicle::default().label(), "unknown vehicle");

        let stable = BuildChannel::new(BuildChannel::STABLE);
        let c = Ctx {
            channel: &stable,
            ..c
        };
        let before = a.sent().len();
        assert!(run(&c, &STEPS[2]).is_err());
        assert_eq!(a.sent().len(), before, "stable: nothing sent");
    }
}
