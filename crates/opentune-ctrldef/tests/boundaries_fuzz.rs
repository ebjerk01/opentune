//! Input boundaries of controller definitions: signal layouts and payloads
//! (from a capture), and definition JSON (from a contributor or a pack).
//!
//! * Deterministic seeded fuzzing (splitmix64, house style of the core
//!   `tp20_fuzz.rs`): bit layouts at and beyond the 64-byte frame edge, both
//!   byte orders, payloads of every length, signed / curve / enum signals.
//!   Invariants: no panic; `extract` answers exactly when the payload holds
//!   the layout and agrees with `insert`; decoded numbers are finite; a
//!   curve never extrapolates.
//! * One table-driven test per validation rule: each mutation of a clean
//!   definition must raise exactly the rule it breaks, so a rule cannot
//!   silently stop firing.
use opentune_ctrldef::decode::{decode_signal, summarize, RawFrame};
use opentune_ctrldef::signal::{bits, bytes_needed, extract, insert, interpolate, sign_extend};
use opentune_ctrldef::{ControllerDef, Decoder, Severity, Signal, Value};
use serde_json::json;

const CASES: u64 = 4000;

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
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

fn order(r: &mut Rng) -> opentune_ctrldef::ByteOrder {
    if r.below(2) == 0 {
        opentune_ctrldef::ByteOrder::Little
    } else {
        opentune_ctrldef::ByteOrder::Big
    }
}

fn mask(len: u8) -> u64 {
    if len >= 64 {
        u64::MAX
    } else {
        (1u64 << len) - 1
    }
}

#[test]
fn layouts_extract_and_insert_agree_at_every_edge() {
    let mut r = Rng(0xC7D1_0001);
    for case in 0..CASES {
        // Bias towards the interesting edges: the 512-bit frame end, the
        // u16 range, len 0 / 64 / 65.
        let start = match r.below(4) {
            0 => r.below(64) as u16,
            1 => 480 + r.below(40) as u16,
            2 => u16::MAX - r.below(4) as u16,
            _ => r.below(600) as u16,
        };
        let len = match r.below(4) {
            0 => r.pick_len(),
            _ => r.below(70) as u8,
        };
        let ord = order(&mut r);
        let layout = bits(start, len, ord);
        let need = bytes_needed(start, len, ord);
        assert_eq!(layout.is_some(), need.is_some());
        let Some(pos) = layout else {
            // An impossible layout never reads or writes.
            let mut buf = vec![0u8; 64];
            assert_eq!(extract(&buf, start, len, ord), None, "case {case}");
            assert!(!insert(&mut buf, start, len, ord, 1));
            continue;
        };
        let need = need.unwrap();
        assert_eq!(pos.len(), len as usize);
        assert!(pos.iter().all(|&p| p < 512));
        assert!(need <= 64 && need == pos.iter().map(|&p| p as usize / 8).max().unwrap() + 1);
        let mut uniq = pos.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(uniq.len(), pos.len(), "a bit used twice: {start} {len} {ord:?}");

        let n = r.below(66) as usize;
        let mut data = r.bytes(n);
        let got = extract(&data, start, len, ord);
        assert_eq!(got.is_some(), n >= need, "case {case}: {start} {len} {ord:?} n={n}");
        if let Some(v) = got {
            assert_eq!(v & !mask(len), 0, "value wider than the signal");
            let before = data.clone();
            let w = r.next() & mask(len);
            assert!(insert(&mut data, start, len, ord, w));
            assert_eq!(extract(&data, start, len, ord), Some(w));
            // Only the signal's own bits changed.
            for (i, (a, b)) in before.iter().zip(&data).enumerate() {
                for bit in 0..8u16 {
                    let p = i as u16 * 8 + bit;
                    if !pos.contains(&p) {
                        assert_eq!((a >> bit) & 1, (b >> bit) & 1, "bit {p} clobbered");
                    }
                }
            }
        } else {
            assert!(!insert(&mut data, start, len, ord, 0));
        }
    }
}

impl Rng {
    fn pick_len(&mut self) -> u8 {
        [0u8, 1, 63, 64, 65, 255][self.below(6) as usize]
    }
}

#[test]
fn sign_extension_stays_in_range_and_round_trips() {
    let mut r = Rng(0x51A0);
    for len in 0..=70u8 {
        for _ in 0..50 {
            let raw = r.next();
            let v = sign_extend(raw, len);
            if len == 0 {
                assert_eq!(v, 0);
            } else if len < 64 {
                let half = 1i64 << (len - 1);
                assert!((-half..half).contains(&v), "{raw:#x} {len} -> {v}");
                assert_eq!((v as u64) & mask(len), raw & mask(len));
            } else {
                assert_eq!(v, raw as i64);
            }
        }
    }
}

#[test]
fn curves_clamp_and_never_extrapolate() {
    let mut r = Rng(0xC0_47E5);
    for _ in 0..CASES {
        let n = r.below(6) as usize;
        let mut xs: Vec<f64> = (0..n).map(|_| r.below(1000) as f64).collect();
        xs.sort_by(f64::total_cmp);
        let curve: Vec<[f64; 2]> = xs
            .iter()
            .map(|&x| [x, r.below(2001) as f64 - 1000.0])
            .collect();
        let x = r.below(1400) as f64 - 200.0;
        let y = interpolate(&curve, x);
        assert!(y.is_finite());
        if curve.is_empty() {
            assert_eq!(y, x);
            continue;
        }
        let lo = curve.iter().map(|p| p[1]).fold(f64::MAX, f64::min);
        let hi = curve.iter().map(|p| p[1]).fold(f64::MIN, f64::max);
        assert!(y >= lo - 1e-9 && y <= hi + 1e-9, "{curve:?} at {x} -> {y}");
    }
}

fn random_signal(r: &mut Rng) -> Signal {
    let len = 1 + r.below(64) as u8;
    let kind = ["number", "bool", "enum", "ascii"][r.below(4) as usize];
    let factor = [1.0, 0.25, -3.5, 1e-6, 1e6][r.below(5) as usize];
    let offset = [0.0, -40.0, 1e9][r.below(3) as usize];
    let mut v = json!({
        "name": "s",
        "start": r.below(520),
        "len": len,
        "byte_order": if r.below(2) == 0 { "little" } else { "big" },
        "signed": r.below(2) == 0,
        "kind": kind,
        "factor": factor,
        "offset": offset,
        "invalid": [mask(len.min(63))],
        "values": [{"raw": 1, "name": "one"}],
    });
    if r.below(4) == 0 {
        v["factor"] = json!(1.0);
        v["offset"] = json!(0.0);
        v["curve"] = json!([[0.0, 0.0], [128.0, 50.0], [255.0, 100.0]]);
    }
    serde_json::from_value(v).unwrap()
}

#[test]
fn decoding_random_payloads_gives_finite_or_marked_values() {
    let mut r = Rng(0xDEC0_DE00);
    for case in 0..CASES {
        let sig = random_signal(&mut r);
        let n = r.below(66) as usize;
        let data = r.bytes(n);
        let need = bytes_needed(sig.start, sig.len, sig.byte_order);
        let d = decode_signal(&sig, &data);
        assert_eq!(d.is_some(), need.is_some_and(|k| n >= k), "case {case}");
        let Some(d) = d else { continue };
        if sig.is_invalid(d.raw) {
            assert_eq!(d.value, Value::Invalid);
            continue;
        }
        match &d.value {
            Value::Number(x) => assert!(x.is_finite(), "case {case}: {x}"),
            Value::Bool(b) => assert_eq!(*b, d.raw != 0),
            Value::Ascii(s) => assert!(!s.is_empty() && s.is_ascii()),
            Value::Enum(name) => assert_eq!(name.is_some(), d.raw == 1),
            Value::Invalid => unreachable!(),
        }
        // Display never panics and never prints an empty value.
        assert!(!d.value.to_string().is_empty());
    }
}

// ---------------------------------------------------------------------------
// Definition JSON
// ---------------------------------------------------------------------------

fn clean() -> serde_json::Value {
    json!({
        "id": "acme-tcu",
        "name": "Acme TCU",
        "kind": "tcu",
        "maker": "Acme",
        "family": "T1",
        "validation": "implemented",
        "identify": {"part_numbers": ["123"]},
        "diagnostics": [
            {"name": "uds", "transport": "iso_tp", "layer": "uds", "bitrate": 500000,
             "tx_id": "0x7E1", "rx_id": "0x7E9", "validation": "implemented"},
            {"name": "tp20", "transport": "vwtp20", "layer": "kwp2000", "bitrate": 500000,
             "tp20_address": "0x02", "rx_id": "0x300", "identification": "1A9B",
             "validation": "implemented"}
        ],
        "buses": [{"name": "main", "bitrate": 500000, "frames": [
            {"id": "0x100", "name": "F", "dlc": 8, "signals": [
                {"name": "a", "start": 0, "len": 8},
                {"name": "flag", "start": 8, "len": 1, "kind": "bool"},
                {"name": "mode", "start": 16, "len": 4, "kind": "enum",
                 "values": [{"raw": 1, "name": "x"}], "invalid": [15], "raw_range": [0, 14]},
                {"name": "lever", "start": 24, "len": 8,
                 "curve": [[0, 0], [255, 100]]}
            ]}
        ]}],
        "live": [
            {"name": "a", "source": {"kind": "can_signal", "bus": "main", "frame": "F", "signal": "a"}},
            {"name": "g1c0", "source": {"kind": "kwp_measuring_block", "group": 1, "cell": 0}},
            {"name": "did", "source": {"kind": "uds_did", "did": "0xF190", "len": 2}}
        ],
        "tables": [{"name": "t", "role": "shift_up", "rows": 2, "cols": 2,
            "x_axis": {"name": "x", "values": [0, 50]},
            "y_axis": {"name": "y", "labels": ["a", "b"]},
            "confidence": "structure_only", "source": "s"}],
        "adaptations": [{"name": "ad", "kind": "reset", "table": "t", "validation": "implemented"}],
        "gearsets": [{"name": "g", "gears": [{"gear": "1", "ratio": 4.0}, {"gear": "2", "ratio": 2.0},
            {"gear": "R", "ratio": -3.0}]}],
        "flash": {"size": 4096, "regions": [{"name": "cal", "region": {"address": 0, "length": 4096}}]},
        "safety": ["careful"],
        "sources": [{"title": "t", "license": "MIT", "used_for": "x"}]
    })
}

fn issues(v: &serde_json::Value) -> Vec<(&'static str, Severity)> {
    let d: ControllerDef = serde_json::from_value(v.clone()).expect("parses");
    d.validate().into_iter().map(|i| (i.code, i.severity)).collect()
}

#[test]
fn the_clean_definition_is_clean() {
    assert_eq!(issues(&clean()), vec![]);
}

#[test]
fn every_rule_fires_on_its_own_mutation() {
    type Mutation = fn(&mut serde_json::Value);
    let cases: Vec<(&str, Severity, Mutation)> = vec![
        ("identity.empty", Severity::Error, |v| v["maker"] = json!(" ")),
        ("sources.incomplete", Severity::Error, |v| v["sources"][0]["license"] = json!("")),
        ("identify.empty", Severity::Warn, |v| v["identify"] = json!({})),
        ("checksum.empty", Severity::Error, |v| v["checksum"] = json!(" ")),
        ("ecudefs.duplicate", Severity::Error, |v| v["ecudefs"] = json!(["x-y", "x-y"])),
        ("diag.name", Severity::Error, |v| v["diagnostics"][0]["name"] = json!("")),
        ("diag.bitrate", Severity::Error, |v| v["diagnostics"][0]["bitrate"] = json!(0)),
        ("diag.can_id", Severity::Error, |v| v["diagnostics"][0]["tx_id"] = json!("0x800")),
        ("diag.isotp_ids", Severity::Error, |v| v["diagnostics"][0]["rx_id"] = json!(null)),
        ("diag.tp20_address", Severity::Error, |v| {
            v["diagnostics"][1]["tp20_address"] = json!(null)
        }),
        ("diag.kline_address", Severity::Warn, |v| {
            v["diagnostics"][1]["transport"] = json!("iso14230_kline")
        }),
        ("diag.identification", Severity::Error, |v| {
            v["diagnostics"][1]["identification"] = json!("1A9")
        }),
        ("bus.name", Severity::Error, |v| v["buses"][0]["name"] = json!("Main Bus")),
        ("bus.duplicate", Severity::Error, |v| {
            let b = v["buses"][0].clone();
            v["buses"].as_array_mut().unwrap().push(b)
        }),
        ("bus.bitrate", Severity::Error, |v| v["buses"][0]["bitrate"] = json!(0)),
        ("frame.id", Severity::Error, |v| v["buses"][0]["frames"][0]["id"] = json!("0x800")),
        ("frame.duplicate_id", Severity::Error, |v| {
            let mut f = v["buses"][0]["frames"][0].clone();
            f["name"] = json!("G");
            v["buses"][0]["frames"].as_array_mut().unwrap().push(f)
        }),
        ("frame.duplicate_name", Severity::Error, |v| {
            let mut f = v["buses"][0]["frames"][0].clone();
            f["id"] = json!("0x101");
            v["buses"][0]["frames"].as_array_mut().unwrap().push(f)
        }),
        ("frame.min_len", Severity::Error, |v| v["buses"][0]["frames"][0]["min_len"] = json!(9)),
        ("frame.empty", Severity::Warn, |v| {
            v["buses"][0]["frames"].as_array_mut().unwrap().push(
                json!({"id": "0x200", "name": "E", "signals": []}),
            )
        }),
        ("signal.duplicate", Severity::Error, |v| {
            v["buses"][0]["frames"][0]["signals"][1]["name"] = json!("a")
        }),
        ("signal.layout", Severity::Error, |v| {
            v["buses"][0]["frames"][0]["signals"][0]["start"] = json!(60)
        }),
        ("signal.kind", Severity::Error, |v| {
            v["buses"][0]["frames"][0]["signals"][0]["kind"] = json!("ascii");
            v["buses"][0]["frames"][0]["signals"][0]["len"] = json!(7)
        }),
        ("signal.scaling", Severity::Error, |v| {
            v["buses"][0]["frames"][0]["signals"][0]["factor"] = json!(0.0)
        }),
        ("signal.curve", Severity::Error, |v| {
            v["buses"][0]["frames"][0]["signals"][3]["curve"] = json!([[255, 0], [0, 100]])
        }),
        ("signal.value", Severity::Error, |v| {
            v["buses"][0]["frames"][0]["signals"][2]["values"] =
                json!([{"raw": 1, "name": "x"}, {"raw": 1, "name": "y"}])
        }),
        ("signal.invalid", Severity::Error, |v| {
            v["buses"][0]["frames"][0]["signals"][2]["invalid"] = json!([16])
        }),
        ("signal.raw_range", Severity::Error, |v| {
            v["buses"][0]["frames"][0]["signals"][2]["raw_range"] = json!([5, 2])
        }),
        ("signal.overlap", Severity::Warn, |v| {
            v["buses"][0]["frames"][0]["signals"][1]["start"] = json!(7)
        }),
        ("live.duplicate", Severity::Error, |v| v["live"][2]["name"] = json!("g1c0")),
        ("live.ref", Severity::Error, |v| v["live"][0]["source"]["frame"] = json!("Nope")),
        ("live.uds_len", Severity::Error, |v| v["live"][2]["source"]["len"] = json!(5)),
        ("live.scaling", Severity::Error, |v| v["live"][2]["source"]["factor"] = json!(0.0)),
        ("live.kwp_duplicate", Severity::Error, |v| {
            v["live"].as_array_mut().unwrap().push(json!({"name": "again",
                "source": {"kind": "kwp_measuring_block", "group": 1, "cell": 0}}))
        }),
        ("adaptation.duplicate", Severity::Error, |v| {
            let a = v["adaptations"][0].clone();
            v["adaptations"].as_array_mut().unwrap().push(a)
        }),
        ("adaptation.table", Severity::Error, |v| v["adaptations"][0]["table"] = json!("zz")),
        ("table.duplicate", Severity::Error, |v| {
            let t = v["tables"][0].clone();
            v["tables"].as_array_mut().unwrap().push(t)
        }),
        ("table.axis_kind", Severity::Error, |v| {
            v["tables"][0]["x_axis"]["labels"] = json!(["p", "q"])
        }),
        ("table.axis_count", Severity::Error, |v| {
            v["tables"][0]["x_axis"] = json!({"name": "x", "derived": "rpm / 2"})
        }),
        ("table.axis_monotonic", Severity::Warn, |v| {
            v["tables"][0]["x_axis"]["values"] = json!([50, 50])
        }),
        ("table.dims", Severity::Error, |v| {
            v["tables"][0]["rows"] = json!(0);
            v["tables"][0]["y_axis"] = json!(null)
        }),
        ("table.source", Severity::Warn, |v| v["tables"][0]["source"] = json!(" ")),
        ("table.confidence", Severity::Error, |v| v["tables"][0]["confidence"] = json!("high")),
        ("table.storage", Severity::Error, |v| {
            v["tables"][0]["confidence"] = json!("low");
            v["tables"][0]["storage"] = json!({"address": 0, "data_type": "u8",
                "endian": "big", "factor": 0.0})
        }),
        ("table.ecudef_map", Severity::Error, |v| {
            v["tables"][0]["confidence"] = json!("low");
            v["tables"][0]["ecudef_map"] = json!({"ecudef": "x-y", "map": "m"})
        }),
        ("gearset.empty", Severity::Error, |v| v["gearsets"][0]["gears"] = json!([])),
        ("gearset.duplicate", Severity::Error, |v| v["gearsets"][0]["gears"][1]["gear"] = json!("1")),
        ("gearset.ratio", Severity::Error, |v| v["gearsets"][0]["gears"][1]["ratio"] = json!(0.0)),
        ("gearset.order", Severity::Warn, |v| v["gearsets"][0]["gears"][1]["ratio"] = json!(5.0)),
        ("flash.region", Severity::Error, |v| {
            v["flash"]["regions"][0]["region"]["length"] = json!(4097)
        }),
    ];
    let mut failures = Vec::new();
    for (code, sev, mutate) in cases {
        let mut v = clean();
        mutate(&mut v);
        let got = issues(&v);
        if !got.contains(&(code, sev)) {
            failures.push(format!("{code}: got {got:?}"));
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));

    // A definition with nothing to act on.
    let mut v = clean();
    for k in ["diagnostics", "buses", "ecudefs"] {
        v[k] = json!([]);
    }
    v["flash"] = json!(null);
    v["live"] = json!([]);
    assert!(issues(&v).contains(&("scope.empty", Severity::Warn)));
}

#[test]
fn hex_fields_refuse_values_that_do_not_fit() {
    for (path, bad) in [
        (vec!["buses", "0", "frames", "0", "id"], json!("0x100000000")),
        (vec!["buses", "0", "frames", "0", "id"], json!("256")),
        (vec!["diagnostics", "1", "tp20_address"], json!("0x100")),
        (vec!["diagnostics", "0", "tx_id"], json!("7E1")),
        (vec!["diagnostics", "0", "tx_id"], json!(-1)),
        (vec!["live", "2", "source", "did"], json!("0x10000")),
    ] {
        let mut v = clean();
        let mut slot = &mut v;
        for k in &path {
            slot = match k.parse::<usize>() {
                Ok(i) => &mut slot[i],
                Err(_) => &mut slot[*k],
            };
        }
        *slot = bad.clone();
        let parsed = serde_json::from_value::<ControllerDef>(v);
        // "256" is a valid decimal id; everything else must be refused.
        if bad == json!("256") {
            assert_eq!(parsed.unwrap().buses[0].frames[0].id, 256);
        } else {
            assert!(parsed.is_err(), "{path:?} = {bad} accepted");
        }
    }
}

#[test]
fn random_definitions_never_panic_the_validator_or_decoder() {
    let mut r = Rng(0xDEF5_0000);
    for _ in 0..300 {
        let mut v = clean();
        let sigs = v["buses"][0]["frames"][0]["signals"].as_array_mut().unwrap();
        for _ in 0..r.below(6) {
            let s = random_signal(&mut r);
            let mut j = serde_json::to_value(&s).unwrap();
            j["name"] = json!(format!("r{}", r.below(1000)));
            sigs.push(j);
        }
        if r.below(3) == 0 {
            v["buses"][0]["frames"][0]["dlc"] = json!(r.below(70));
        }
        let d: ControllerDef = serde_json::from_value(v).unwrap();
        let _ = d.validate();
        let dec = Decoder::new(&d, None).unwrap();
        let frames: Vec<(u32, Vec<u8>)> = (0..20)
            .map(|_| {
                let n = r.below(66) as usize;
                ([0x100, 0x101, 0x7FF][r.below(3) as usize], r.bytes(n))
            })
            .collect();
        let (sum, decoded) = summarize(
            &dec,
            "main",
            frames.iter().map(|(id, data)| RawFrame {
                ts: Some(0.0),
                id: *id,
                extended: false,
                data,
            }),
        );
        assert_eq!(sum.frames, 20);
        assert_eq!(sum.decoded as usize, decoded.len());
        for (_, f) in &decoded {
            for s in &f.signals {
                if let Value::Number(x) = s.value {
                    assert!(x.is_finite());
                }
            }
        }
    }
}
