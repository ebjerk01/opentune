//! The shipped brand coding-label packs, driven over the `vcar` virtual car the
//! way the apps drive a real adapter (driver open per module address, the core
//! ISO-TP stack in between). The car is a test-only profile with invented
//! coding bytes; nothing here describes a real vehicle's coding.
use opentune_coding::{decode, get, read_coding, set, write_coding, WriteOptions};
use opentune_core::{ConnectConfig, CoreDiag, Interface};
use opentune_driver_vcar::{Profile, SharedVehicle, VcarDriver, Vehicle};
use opentune_packs::load_pack;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const INFOTAINMENT: (u32, u32) = (0x7A1, 0x7A9);
const ASBUILT: (u32, u32) = (0x7A2, 0x7AA);

/// A two-module virtual car: a synthetic infotainment unit with a 25-byte long
/// coding on DID 0x0600, and a synthetic module exposing two As-Built blocks
/// (DIDs 0xDE00/0xDE01). Fictional ids and values.
fn car() -> SharedVehicle {
    let profile = Profile::from_json(
        r#"{
      "id": "brand-coding-test",
      "name": "Brand coding test car",
      "vin": "TESTVCAR0D0000002",
      "modules": [
        { "name": "Synthetic infotainment", "addr": 95, "req_id": "7A1", "resp_id": "7A9",
          "dids": [ { "did": "0600", "hex": "02 37 00 01 03 00 00 00 00 00 00 00 00 00 00 00 15 00 00 00 00 00 00 00 00" } ] },
        { "name": "Synthetic As-Built module", "addr": 1, "req_id": "7A2", "resp_id": "7AA",
          "dids": [ { "did": "DE00", "hex": "80 00 00 00 00 00 05" },
                     { "did": "DE01", "hex": "00 00 00 00 2A" } ] }
      ]
    }"#,
    )
    .unwrap();
    Arc::new(Mutex::new(Vehicle::from_profile(&profile).unwrap()))
}

fn with_diag<R>(
    car: &SharedVehicle,
    (tx, rx): (u32, u32),
    f: impl FnOnce(&mut CoreDiag) -> R,
) -> R {
    let mut d = VcarDriver::with_vehicle(car.clone());
    d.open(&ConnectConfig {
        tx_id: tx,
        rx_id: rx,
        ..ConnectConfig::default()
    })
    .unwrap();
    let t = d.diag().expect("vcar exposes a diagnostic transport");
    let mut diag = CoreDiag::new(t, 100);
    f(&mut diag)
}

#[test]
fn mib2_pack_reads_edits_and_writes_over_vcar() {
    let pack_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../packs/vag-mib2-coding");
    let pack = load_pack(&pack_dir).unwrap();
    assert!(pack.report.is_clean(), "{:?}", pack.report.notes);
    let def = &pack.coding[0];
    let car = car();

    let before = with_diag(&car, INFOTAINMENT, |d| read_coding(d, def).unwrap());
    let dec = decode(def, &before).unwrap();
    let label = |n: &str| dec.iter().find(|p| p.name == n).unwrap().label.clone();
    assert_eq!(label("Brand"), "VW");
    assert_eq!(label("Country Navigation"), "EU");
    assert_eq!(label("Channel 1 TT"), "on");
    assert_eq!(label("BT visibility"), "auto");

    let desired = set(def, &before, "USB", 3).unwrap(); // "ipod"
    let rep = with_diag(&car, INFOTAINMENT, |d| {
        write_coding(d, def, &before, &desired, &WriteOptions::default()).unwrap()
    });
    assert!(rep.is_verified(), "{:?}", rep.outcome);
    let after = with_diag(&car, INFOTAINMENT, |d| read_coding(d, def).unwrap());
    assert_eq!(get(def, &after, "USB").unwrap(), 3);
    assert_eq!(after.blocks[0].bytes[19], 0xC0);
}

#[test]
fn asbuilt_blocks_read_over_vcar_render_as_checksummed_lines() {
    use opentune_coding::asbuilt::{self, AsBuiltLine};
    use opentune_coding::{Addressing, CodingDef};
    let def = CodingDef {
        id: "synthetic-asbuilt".into(),
        name: "Synthetic".into(),
        module: "test only".into(),
        requires_security: false,
        addressing: Addressing::Did {
            dids: vec![0xDE00, 0xDE01],
        },
        params: vec![],
    };
    let car = car();
    let coding = with_diag(&car, ASBUILT, |d| read_coding(d, &def).unwrap());
    let lines = asbuilt::from_coding(0x7A2, &coding).unwrap();
    let text: Vec<String> = lines.iter().map(AsBuiltLine::to_text).collect();
    // 07+A2+01+01+80 = 0x12B -> 2B; 07+A2+01+02+05 = 0xB1; 07+A2+02+01+2A = 0xD6.
    assert_eq!(
        text,
        vec![
            "7A2-01-01 8000 0000 002B",
            "7A2-01-02 0005 B1",
            "7A2-02-01 0000 0000 2AD6",
        ]
    );
    // And back: the lines rebuild the exact DID data.
    let parsed = asbuilt::parse_text(&text.join("\n")).unwrap();
    assert_eq!(asbuilt::to_coding(&def, 0x7A2, &parsed).unwrap(), coding);
}
