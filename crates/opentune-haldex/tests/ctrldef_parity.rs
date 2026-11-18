//! The data-defined controller definitions (`ctrldefs/borgwarner-haldex-gen4`,
//! `-gen5`, `openhaldex-c6`) describe the same frames this crate decodes by
//! hand. Decode a spread of frames both ways and require the same values, so
//! the migration to the generic schema loses no behaviour and the two cannot
//! drift apart.
use opentune_ctrldef::{ControllerDef, DecodedFrame, Decoder, Value};
use opentune_haldex::can::{decode_frame, Bus, HaldexFrame};
use std::path::Path;

fn load(id: &str) -> ControllerDef {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../ctrldefs/{id}.json"));
    ControllerDef::from_json(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn num(f: &DecodedFrame, s: &str) -> Option<f32> {
    match &f.get(s)?.value {
        Value::Number(v) => Some(*v as f32),
        _ => None,
    }
}

fn flag(f: &DecodedFrame, s: &str) -> bool {
    match f.get(s).map(|x| &x.value) {
        Some(Value::Bool(b)) => *b,
        other => panic!("{s}: {other:?}"),
    }
}

fn close(a: Option<f32>, b: Option<f32>, what: &str) {
    match (a, b) {
        (Some(x), Some(y)) => assert!((x - y).abs() < 1e-3, "{what}: {x} vs {y}"),
        (None, None) => {}
        _ => panic!("{what}: {a:?} vs {b:?}"),
    }
}

/// Frames covering every decoded field, including short (Gen1) and edge values.
fn frames() -> Vec<(u32, Vec<u8>)> {
    let mut v: Vec<(u32, Vec<u8>)> = vec![
        (0x2A0, vec![0x7F, 0x00, 0x00]),
        (0x2A0, vec![0xFE, 0x10]),
        (0x2A0, vec![0xBF]),
        (0x2C0, vec![0x18, 0x80, 0x00, 0x80, 0x90, 0xC0, 0xFE, 0x73]),
        (0x2C0, vec![0x7F, 0xC6, 0x00, 0x00, 0xFF, 0x00, 0x00, 0xF3]),
        (0x2C0, vec![0x01, 0xBF, 0x00, 0x80, 0x56, 0x00, 0x00, 0xF3]),
        (0x2C0, vec![0x00, 0xC6]),
        (0x2C0, vec![0x00]),
        (0x4A0, vec![0x88, 0x13, 0x00, 0x00, 0xA0, 0x0F, 0x00, 0x00]),
        (0x4A0, vec![0xFF; 8]),
        (0x4A0, vec![0x88, 0x13, 0x00, 0x00]),
        (0x280, vec![0, 0, 0xE0, 0x2E, 0, 125, 0, 0]),
        (0x118, vec![0x00, 0x45, 125, 0x12]),
        (0x118, vec![0x00, 0x20, 0xFE]),
        (0x118, vec![0x00, 0x7F, 250]),
        (0x0B2, vec![0x10, 0x27, 0x20, 0x4E, 0xA0, 0x0F, 0xFF, 0xFF]),
        (0x6B0, vec![0x00, 0x0F, 0x40, 0x28, 0x64, 0x00, 0x03, 0x32]),
        (0x6B0, vec![0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x09, 0x00]),
        (0x6A0, vec![0x03, 0, 0, 0, 0, 0, 0, 0]),
        (0x6A0, vec![0x09]),
    ];
    // A deterministic spread of full payloads for every id.
    let mut x: u32 = 0x1234_5678;
    for id in [0x2A0, 0x2C0, 0x4A0, 0x280, 0x118, 0x0B2, 0x6B0, 0x6A0] {
        for _ in 0..40 {
            let mut d = vec![0u8; 8];
            for b in &mut d {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                *b = x as u8;
            }
            v.push((id, d));
        }
    }
    v
}

fn generic(decs: &[Decoder<'_>], id: u32, data: &[u8]) -> Option<DecodedFrame> {
    decs.iter().find_map(|d| d.decode(id, false, data))
}

#[test]
fn generic_decode_matches_the_hand_written_decoder() {
    let gen4 = load("borgwarner-haldex-gen4");
    let gen5 = load("borgwarner-haldex-gen5");
    let oh = load("openhaldex-c6");
    let decs = [
        Decoder::new(&gen4, Some("pq")).unwrap(),
        Decoder::new(&gen5, Some("mqb")).unwrap(),
        Decoder::new(&oh, Some("openhaldex")).unwrap(),
    ];
    let mut checked = 0;
    for (id, data) in frames() {
        let hand = decode_frame(id, false, &data, Bus::Auto);
        let gen = generic(&decs, id, &data);
        let Some(hand) = hand else {
            assert!(
                gen.is_none(),
                "0x{id:X} {data:02X?}: generic decoded, hand-written did not"
            );
            continue;
        };
        let g = gen.unwrap_or_else(|| {
            panic!("0x{id:X} {data:02X?}: hand-written decoded, generic did not")
        });
        checked += 1;
        let ctx = format!("0x{id:X} {data:02X?}");
        match hand {
            HaldexFrame::LockRequestPq {
                raw,
                center_nm_per_min,
                lock_pct,
                rear_pct,
            } => {
                assert_eq!(g.get("lock_request").unwrap().raw, raw as u64, "{ctx}");
                close(
                    num(&g, "ABS_Vorgabewert_mitte_Kupplung"),
                    Some(center_nm_per_min as f32),
                    &ctx,
                );
                close(num(&g, "lock_request"), Some(lock_pct), &ctx);
                close(num(&g, "ABS_Vorgabewert_hinten_Kupplung"), rear_pct, &ctx);
            }
            HaldexFrame::StatusPq {
                flags,
                actual_raw,
                actual_nm_per_min,
                engagement_pct,
                fault_stored,
                byte4_raw,
                gen4_temp_c,
                counter,
            } => {
                assert_eq!(
                    flag(&g, "Fehler_Allrad_Kupplung"),
                    flags.clutch_fault,
                    "{ctx}"
                );
                assert_eq!(
                    flag(&g, "Ubertemperaturschutz"),
                    flags.overtemp_protection,
                    "{ctx}"
                );
                assert_eq!(
                    flag(&g, "Fehlerstatus_Kupplungssteifigkeit"),
                    flags.torque_signal_fault,
                    "{ctx}"
                );
                assert_eq!(
                    flag(&g, "Kupplung_komplett_offen"),
                    flags.coupling_open,
                    "{ctx}"
                );
                assert_eq!(flag(&g, "Notlauf"), flags.limp, "{ctx}");
                assert_eq!(flag(&g, "Allrad_Warnlampe"), flags.warning_lamp, "{ctx}");
                assert_eq!(
                    flag(&g, "Geschwindigkeitsbegrenzung"),
                    flags.speed_limit,
                    "{ctx}"
                );
                assert_eq!(
                    g.get("lock_actual").unwrap().raw,
                    actual_raw as u64,
                    "{ctx}"
                );
                close(
                    num(&g, "Kupplungssteifigkeit_Mitte_Ist"),
                    Some(actual_nm_per_min as f32),
                    &ctx,
                );
                close(num(&g, "lock_actual"), Some(engagement_pct), &ctx);
                assert_eq!(
                    g.get("Fehlerspeichereintrag")
                        .map(|s| s.value == Value::Bool(true)),
                    fault_stored,
                    "{ctx}"
                );
                assert_eq!(
                    g.get("gen4_temperature").map(|s| s.raw as u8),
                    byte4_raw,
                    "{ctx}"
                );
                close(
                    num(&g, "gen4_temperature"),
                    gen4_temp_c.map(|t| t as f32),
                    &ctx,
                );
                close(num(&g, "counter"), counter.map(|c| c as f32), &ctx);
            }
            HaldexFrame::StatusMqb {
                counter,
                status,
                request_accepted,
                actual_raw,
                engagement_pct,
            } => {
                assert_eq!(g.get("counter").unwrap().raw, counter as u64, "{ctx}");
                assert_eq!(g.get("status").unwrap().raw, status as u64, "{ctx}");
                assert_eq!(flag(&g, "request_accepted"), request_accepted, "{ctx}");
                assert_eq!(
                    g.get("lock_actual").unwrap().raw,
                    actual_raw as u64,
                    "{ctx}"
                );
                close(num(&g, "lock_actual"), engagement_pct, &ctx);
            }
            HaldexFrame::WheelSpeeds(w) => {
                let names: [&str; 4] = if id == 0x4A0 {
                    [
                        "Radgeschw__VL",
                        "Radgeschw__VR",
                        "Radgeschw__HL",
                        "Radgeschw__HR",
                    ]
                } else {
                    [
                        "ESP_VL_Radgeschw_02",
                        "ESP_VR_Radgeschw_02",
                        "ESP_HL_Radgeschw_02",
                        "ESP_HR_Radgeschw_02",
                    ]
                };
                for (n, v) in names.iter().zip([w.fl, w.fr, w.rl, w.rr]) {
                    close(num(&g, n), Some(v), &ctx);
                }
            }
            HaldexFrame::EnginePq { rpm, pedal_pct } => {
                close(num(&g, "Motordrehzahl"), Some(rpm), &ctx);
                close(num(&g, "Fahrpedalwert"), Some(pedal_pct), &ctx);
            }
            HaldexFrame::OpenHaldexBroadcast(b) => {
                for (n, v) in [
                    ("standalone_flags", b.standalone_flags),
                    ("requested", b.requested),
                    ("actual", b.actual_pct),
                    ("speed", b.speed_kmh),
                    ("mode_override", b.mode_override),
                    ("mode", b.mode_index),
                    ("pedal", b.pedal_pct),
                ] {
                    assert_eq!(g.get(n).unwrap().raw, v as u64, "{ctx} {n}");
                }
                let gname = match &g.get("mode").unwrap().value {
                    Value::Enum(n) => n.clone(),
                    v => panic!("{v:?}"),
                };
                assert_eq!(gname, b.mode.map(|m| m.to_string()), "{ctx}");
            }
            HaldexFrame::OpenHaldexCommand { value, mode } => {
                let s = g.get("mode_command").unwrap();
                assert_eq!(s.raw, value as u64, "{ctx}");
                assert_eq!(s.value, Value::Enum(mode.map(|m| m.to_string())), "{ctx}");
            }
        }
    }
    assert!(checked > 300, "only {checked} frames compared");
}
