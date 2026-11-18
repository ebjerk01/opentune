//! `opentune ecudefs relocate` end to end on synthetic images.
use opentune_cli::{try_run_from, CliConfig};
use opentune_core::ecudef::EcuDef;

fn noise(len: usize, seed: u32) -> Vec<u8> {
    let mut s = seed;
    (0..len)
        .map(|_| {
            s = s.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (s >> 16) as u8
        })
        .collect()
}

fn table(base: u16) -> Vec<u8> {
    (0..32u16)
        .flat_map(|i| (base + 11 * (i / 8) + 7 * (i % 8)).to_be_bytes())
        .collect()
}

#[test]
fn relocate_writes_a_derived_definition() {
    let dir = std::env::temp_dir().join(format!("opentune-cli-relocate-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // Source: two tables; target: the block holding them moved 0x180 later,
    // and a third map that does not exist in the target at all.
    let mut a = noise(0x8000, 21);
    a[0x2000..0x2040].copy_from_slice(&table(1000));
    a[0x2040..0x2080].copy_from_slice(&table(3000));
    let mut b = noise(0x8000, 22);
    b[0x1180..0x3180].copy_from_slice(&a[0x1000..0x3000]);
    let mut def = EcuDef::from_json(
        &std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../ecudefs/bosch-me7.json"
        ))
        .unwrap(),
    )
    .unwrap();
    let map = |name: &str, at: u32| opentune_core::ecudef::MapDef {
        name: name.into(),
        notes: "CONFIDENCE: MEDIUM. synthetic".into(),
        address: at,
        data_type: opentune_core::ecudef::DataType::U16,
        endian: opentune_core::ecudef::Endian::Big,
        rows: 4,
        cols: 8,
        ..Default::default()
    };
    def.id = "synthetic".into();
    def.maps = vec![map("one", 0x2000), map("two", 0x2040), map("gone", 0x6000)];
    let p = |n: &str| dir.join(n).to_string_lossy().into_owned();
    std::fs::write(p("synthetic.json"), serde_json::to_string(&def).unwrap()).unwrap();
    std::fs::write(p("a.bin"), &a).unwrap();
    std::fs::write(p("b.bin"), &b).unwrap();
    // "gone" sits on noise that also moved: make it unrelated in the target.
    let mut b2 = b.clone();
    b2[0x6000..0x6200].copy_from_slice(&noise(0x200, 99));
    b2[0x6180..0x6380].copy_from_slice(&noise(0x200, 98));
    std::fs::write(p("b.bin"), &b2).unwrap();
    let args = [
        "opentune",
        "ecudefs",
        "relocate",
        "--def",
        &p("synthetic.json"),
        "--source",
        &p("a.bin"),
        "--image",
        &p("b.bin"),
        "-o",
        &p("synthetic-b.json"),
    ];
    try_run_from(CliConfig::default(), args.iter().copied()).expect("relocate runs");
    let out = EcuDef::from_json(&std::fs::read_to_string(p("synthetic-b.json")).unwrap()).unwrap();
    assert_eq!(out.id, "synthetic-b");
    let addrs: Vec<(String, u32)> = out
        .maps
        .iter()
        .map(|m| (m.name.clone(), m.address))
        .collect();
    assert!(addrs.contains(&("one".into(), 0x2180)), "{addrs:?}");
    assert!(addrs.contains(&("two".into(), 0x21C0)), "{addrs:?}");
    assert!(out.notes.contains("from definition 'synthetic'"));
    // A confidence outside 0..1 is refused before any work.
    let bad = [
        "opentune",
        "ecudefs",
        "relocate",
        "--def",
        &p("synthetic.json"),
        "--source",
        &p("a.bin"),
        "--image",
        &p("b.bin"),
        "--min-confidence",
        "1.5",
    ];
    assert!(try_run_from(CliConfig::default(), bad.iter().copied()).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}
