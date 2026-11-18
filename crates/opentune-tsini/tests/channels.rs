//! Realtime output-channel decoding.
use opentune_tsini::*;
use std::path::PathBuf;

fn demo(opts: &IniOptions) -> Ini {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../samples/ts/opentune-demo.ini");
    load_ini(&p, opts).unwrap()
}

fn block() -> Vec<u8> {
    let mut b = vec![0u8; 12];
    b[0] = 42; // secl
    b[1] = 0b01; // running
    b[2..4].copy_from_slice(&95u16.to_le_bytes()); // map
    b[4] = 130; // coolantRaw -> 90 C
    b[5..7].copy_from_slice(&3000u16.to_le_bytes()); // rpm
    b[7] = 147; // afr 14.7
    b[8] = (-5i8) as u8; // advance
    b[9] = 138; // 13.8 V
    b[10..12].copy_from_slice(&50u16.to_le_bytes()); // tps 25 %
    b
}

fn get<'a>(s: &'a [ChannelSample], n: &str) -> &'a ChannelSample {
    s.iter().find(|x| x.name == n).unwrap()
}

#[test]
fn decodes_scalars_bits_and_expressions() {
    let ini = demo(&IniOptions::default());
    let vars = |n: &str| (n == "stoich").then_some(14.7);
    let s = decode_channels(&ini, &block(), &vars, None);
    assert_eq!(s.len(), ini.output_channels.len());
    assert_eq!(get(&s, "secl").value, Some(42.0));
    assert_eq!(get(&s, "rpm").value, Some(3000.0));
    assert_eq!(get(&s, "advance").value, Some(-5.0));
    assert!((get(&s, "afr").value.unwrap() - 14.7).abs() < 1e-9);
    assert!((get(&s, "batteryV").value.unwrap() - 13.8).abs() < 1e-9);
    assert_eq!(get(&s, "tps").value, Some(25.0));
    assert_eq!(get(&s, "tps").raw, Some(50.0));
    assert_eq!(get(&s, "running").value, Some(1.0));
    assert_eq!(get(&s, "crank").value, Some(0.0));
    assert_eq!(get(&s, "coolant").value, Some(90.0));
    assert_eq!(get(&s, "coolant").units, "C");
    assert_eq!(get(&s, "coolant").raw, None);
    assert!((get(&s, "lambda").value.unwrap() - 1.0).abs() < 1e-9);
    assert_eq!(get(&s, "loadPct").value, Some(47.5));
    assert_eq!(get(&s, "isRunning").value, Some(1.0));
    let sm = get(&s, "smoothRpm");
    assert_eq!(sm.value, None);
    assert!(sm.note.contains("unsupported"), "{}", sm.note);
}

#[test]
fn missing_inputs_are_reported() {
    let ini = demo(&IniOptions::with_set(["FAHRENHEIT"]));
    let s = decode_channels(&ini, &block(), &|_| None, None);
    assert!((get(&s, "coolant").value.unwrap() - 194.0).abs() < 1e-9);
    let l = get(&s, "lambda");
    assert_eq!(l.value, None);
    assert!(l.note.contains("stoich"), "{}", l.note);
    // A short block: channels past its end say so.
    let short = decode_channels(&ini, &block()[..6], &|_| None, None);
    assert!(get(&short, "tps").note.contains("beyond"));
    assert_eq!(get(&short, "map").value, Some(95.0));
}

#[test]
fn wanted_channels_in_order() {
    let ini = demo(&IniOptions::default());
    let want = vec!["rpm".to_string(), "nope".into(), "coolant".into()];
    let s = decode_channels(&ini, &block(), &|_| None, Some(&want));
    assert_eq!(
        s.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(),
        ["rpm", "nope", "coolant"]
    );
    assert!(s[1].note.contains("not an output channel"));
    assert_eq!(s[2].value, Some(90.0), "dependencies still evaluated");
}
