//! Brand coding-label sets and formats, end to end over the `opentune-udssim`
//! UDS server (via `SimTransport` + `CoreDiag`): the shipped VAG MIB2 long-coding
//! pack, Ford As-Built blocks (format only, synthetic labels) and a CCF
//! descriptor import (synthetic descriptor). Every coding value here is
//! invented; nothing claims a real car's coding.

use std::path::PathBuf;

use opentune_coding::asbuilt::{self, AsBuiltLabel, AsBuiltLine};
use opentune_coding::ccf::{import_descriptor, CcfOptions};
use opentune_coding::{
    decode, get, read_coding, set, write_coding, Addressing, CodingDef, CodingParam, ParamKind,
    WriteOptions,
};
use opentune_core::CoreDiag;
use opentune_udssim::server::UdsServer;
use opentune_udssim::transport::SimTransport;

fn mib2_def() -> CodingDef {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packs/vag-mib2-coding/coding/mib2-high-5f.json");
    let def = CodingDef::from_json(&std::fs::read_to_string(path).unwrap()).unwrap();
    def.validate().unwrap();
    def
}

/// Synthetic 25-byte MIB2 long coding: brand VW, navigation country EU,
/// channels 1 HT/TT, Bluetooth available + phone + visibility auto.
fn mib2_bytes() -> Vec<u8> {
    let mut b = vec![0u8; 25];
    b[0] = 0x02; // Brand = VW
    b[1] = 0x37; // Car Class 7, Car Generation 3
    b[3] = 0x01; // Country Navigation = EU
    b[4] = 0b0000_0011; // Channel 1 HT + TT
    b[16] = 0b0001_0101; // BT available, BT phone, BT visibility = auto (1)
    b
}

#[test]
fn mib2_pack_layout_is_sane() {
    let def = mib2_def();
    assert_eq!(def.addressing.dids(), &[0x0600]);
    assert_eq!(def.params.len(), 118);
    // No two parameters share a bit, and all sit inside bytes 0..=24.
    let mut used = std::collections::HashSet::new();
    for p in &def.params {
        for i in 0..p.len as usize {
            let abs = p.byte * 8 + p.bit as usize + i;
            assert!(abs < 25 * 8, "{} past byte 24", p.name);
            assert!(used.insert(abs), "{} overlaps another parameter", p.name);
        }
    }
}

#[test]
fn mib2_every_option_encodes_and_decodes() {
    // Pure engine: for every parameter, every declared option (or both bool
    // states) set on the synthetic coding decodes back to itself and changes
    // nothing else.
    let def = mib2_def();
    let base = opentune_coding::Coding {
        blocks: vec![opentune_coding::CodingBlock {
            did: 0x0600,
            bytes: mib2_bytes(),
        }],
    };
    let mut checked = 0;
    for p in &def.params {
        let values: Vec<u64> = match p.kind {
            ParamKind::Bool => vec![0, 1],
            ParamKind::Enum => p.options.iter().map(|o| o.value).collect(),
            ParamKind::Number => vec![0, (1u64 << p.len) - 1],
        };
        for v in values {
            let edited = set(&def, &base, &p.name, v).unwrap();
            assert_eq!(get(&def, &edited, &p.name).unwrap(), v, "{}", p.name);
            let changed = opentune_coding::diff_params(&def, &base, &edited).unwrap();
            assert!(changed.iter().all(|c| c.name == p.name), "{}", p.name);
            checked += 1;
        }
    }
    assert!(checked > 250, "{checked}");
}

#[test]
fn mib2_coding_round_trips_over_the_uds_sim() {
    let def = mib2_def();
    let mut server = UdsServer::new();
    server.set_did(0x0600, mib2_bytes());
    let mut sim = SimTransport::new(server);
    {
        let mut diag = CoreDiag::new(&mut sim, 1000);
        let current = read_coding(&mut diag, &def).unwrap();
        let d = decode(&def, &current).unwrap();
        let label = |n: &str| d.iter().find(|p| p.name == n).unwrap().label.clone();
        assert_eq!(label("Brand"), "VW");
        assert_eq!(label("Car Class"), "7");
        assert_eq!(label("Car Generation"), "3");
        assert_eq!(label("Country Navigation"), "EU");
        assert_eq!(label("Channel 1 HT"), "on");
        assert_eq!(label("Channel 2 HT"), "off");
        assert_eq!(label("BT phone"), "on");
        assert_eq!(label("BT visibility"), "auto");
        assert_eq!(label("USB"), "off");

        // Edit one option, write (backup + verify), read back.
        let desired = set(&def, &current, "BT visibility", 2).unwrap(); // "on"
        let rep = write_coding(
            &mut diag,
            &def,
            &current,
            &desired,
            &WriteOptions::default(),
        )
        .unwrap();
        assert!(rep.is_verified(), "{:?}", rep.outcome);
        assert_eq!(rep.written, vec![0x0600]);
        assert_eq!(rep.byte_changes.len(), 1);
        assert_eq!(rep.byte_changes[0].byte, 16);
        assert_eq!(rep.backup.blocks[0].bytes, mib2_bytes());
        let after = read_coding(&mut diag, &def).unwrap();
        assert_eq!(get(&def, &after, "BT visibility").unwrap(), 2);
    }
    let mut want = mib2_bytes();
    want[16] = 0b0010_0101;
    assert_eq!(sim.server().did(0x0600), Some(&want[..]));
}

/// A synthetic As-Built definition: invented parameter names on two As-Built
/// blocks of an invented module `0x7A0`. Format test only — not a Ford label.
fn asbuilt_def() -> CodingDef {
    let byte = |line, b| asbuilt::line_offset(line, b).unwrap();
    let p = |name: &str, block, byte, mask: u64, kind| {
        let (bit, len) = opentune_coding::mask_field(mask).unwrap();
        CodingParam {
            name: name.into(),
            block,
            byte,
            bit,
            len,
            kind,
            options: vec![],
            note: "synthetic".into(),
        }
    };
    CodingDef {
        id: "synthetic-asbuilt".into(),
        name: "Synthetic As-Built module".into(),
        module: "test only".into(),
        requires_security: false,
        addressing: Addressing::Did {
            dids: vec![asbuilt::block_did(1), asbuilt::block_did(2)],
        },
        params: vec![
            p("Flag A", 0, byte(1, 0), 0x80, ParamKind::Bool),
            p("Level B", 0, byte(2, 1), 0x0F, ParamKind::Number),
            p("Byte C", 1, byte(1, 4), 0xFF, ParamKind::Number),
        ],
    }
}

#[test]
fn asbuilt_blocks_round_trip_over_the_uds_sim() {
    let def = asbuilt_def();
    // Block 1 = two lines (5 + 2 bytes), block 2 = one line; checksums valid.
    let text = [
        AsBuiltLine::new(
            AsBuiltLabel::parse("7A0-01-01").unwrap(),
            vec![0x80, 0, 0, 0, 0],
        ),
        AsBuiltLine::new(AsBuiltLabel::parse("7A0-01-02").unwrap(), vec![0, 0x05]),
        AsBuiltLine::new(
            AsBuiltLabel::parse("7A0-02-01").unwrap(),
            vec![0, 0, 0, 0, 0x2A],
        ),
    ]
    .iter()
    .map(AsBuiltLine::to_text)
    .collect::<Vec<_>>()
    .join("\n");
    let lines = asbuilt::parse_text(&format!("; synthetic\n{text}\n")).unwrap();
    let offline = asbuilt::to_coding(&def, 0x7A0, &lines).unwrap();

    // Serve the same bytes from the UDS sim on the As-Built DIDs.
    let mut server = UdsServer::new();
    for b in &offline.blocks {
        server.set_did(b.did, b.bytes.clone());
    }
    let mut sim = SimTransport::new(server);
    let mut diag = CoreDiag::new(&mut sim, 1000);
    let live = read_coding(&mut diag, &def).unwrap();
    assert_eq!(live, offline);
    assert_eq!(get(&def, &live, "Flag A").unwrap(), 1);
    assert_eq!(get(&def, &live, "Level B").unwrap(), 5);
    assert_eq!(get(&def, &live, "Byte C").unwrap(), 0x2A);

    let desired = set(&def, &live, "Level B", 9).unwrap();
    let rep = write_coding(&mut diag, &def, &live, &desired, &WriteOptions::default()).unwrap();
    assert!(rep.is_verified());
    assert_eq!(rep.written, vec![0xDE00]);

    // Rendered back as As-Built lines: valid checksums, only 7A0-01-02 differs.
    let after = read_coding(&mut diag, &def).unwrap();
    let new_lines = asbuilt::from_coding(0x7A0, &after).unwrap();
    assert!(new_lines.iter().all(AsBuiltLine::is_valid));
    let changed: Vec<String> = new_lines
        .iter()
        .zip(&lines)
        .filter(|(a, b)| a != b)
        .map(|(a, _)| a.label.to_string())
        .collect();
    assert_eq!(changed, vec!["7A0-01-02"]);
}

#[test]
fn imported_ccf_descriptor_round_trips_over_the_uds_sim() {
    // Synthetic descriptor (invented names) in the documented structure.
    let xml = r#"<d><block name="CCF"><group start="1" stop="1">
        <parameter id="001_001" name="SYN_MODE" mask="0x0C" type="ENUM">
          <select>
            <option value="0x00" name="A"><tm>Mode A</tm></option>
            <option value="0x04" name="B"><tm>Mode B</tm></option>
          </select>
        </parameter></group></block></d>"#;
    let imp = import_descriptor(
        xml,
        &CcfOptions {
            block: "CCF".into(),
            did: 0xFD10,
            id: "synthetic-ccf".into(),
            name: "Synthetic CCF".into(),
            requires_security: false,
        },
    )
    .unwrap();
    assert!(imp.skipped.is_empty());
    let def = imp.def;
    // The definition survives a JSON round trip (as `coding import-ccf` writes it).
    let def = CodingDef::from_json(&serde_json_roundtrip(&def)).unwrap();

    let mut server = UdsServer::new();
    server.set_did(0xFD10, vec![0xFF, 0x00, 0xFF]);
    let mut sim = SimTransport::new(server);
    let mut diag = CoreDiag::new(&mut sim, 1000);
    let cur = read_coding(&mut diag, &def).unwrap();
    assert_eq!(decode(&def, &cur).unwrap()[0].label, "Mode A");
    let desired = set(&def, &cur, "SYN_MODE", 1).unwrap();
    let rep = write_coding(&mut diag, &def, &cur, &desired, &WriteOptions::default()).unwrap();
    assert!(rep.is_verified());
    let after = read_coding(&mut diag, &def).unwrap();
    assert_eq!(after.blocks[0].bytes, vec![0xFF, 0x04, 0xFF]);
    assert_eq!(decode(&def, &after).unwrap()[0].label, "Mode B");
}

fn serde_json_roundtrip(def: &CodingDef) -> String {
    serde_json::to_string(def).unwrap()
}
