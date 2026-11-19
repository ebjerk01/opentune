//! `opentune coding` with the brand label sets and formats: the VAG MIB2 pack
//! over the offline `mock` store, Ford As-Built checking/export, and the CCF
//! descriptor importer. All coding bytes and descriptors are synthetic.
use std::path::{Path, PathBuf};
use std::{fs, process::Command};

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("opentune-coding-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir(&dir).unwrap();
    dir
}

fn opentune(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_opentune"))
        .args(args)
        .output()
        .unwrap()
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

fn stdout(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn mib2_pack_decodes_and_writes_a_mock_store() {
    let dir = tmp("mib2");
    let store = dir.join("store.json");
    // Synthetic 25 bytes: Brand VW, Country Navigation EU, BT visibility auto.
    fs::write(
        &store,
        r#"{ "0x0600": "02370001030000000000000000000000150000000000000000" }"#,
    )
    .unwrap();
    let def = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packs/vag-mib2-coding/coding/mib2-high-5f.json");
    let conn = ["--def", s(&def), "--driver", "mock", "--store", s(&store)];

    let mut args = vec!["coding", "decode"];
    args.extend(conn);
    let out = opentune(&args);
    assert!(out.status.success(), "{out:?}");
    let text = stdout(&out);
    assert!(
        text.contains("VW") && text.contains("EU") && text.contains("auto"),
        "{text}"
    );

    let mut args = vec!["coding", "write", "BT visibility", "on"];
    args.extend(conn);
    let out = opentune(&args);
    assert!(out.status.success(), "{out:?}");
    assert!(stdout(&out).contains("written and verified"));
    let store_text = fs::read_to_string(&store).unwrap();
    assert!(
        store_text.contains("02370001030000000000000000000000250000000000000000"),
        "{store_text}"
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn asbuilt_check_verifies_checksums_and_exports_a_store() {
    let dir = tmp("asbuilt");
    // Synthetic module 7A0: checksums = low byte of 07+A0+block+line+data.
    let good = dir.join("good.txt");
    fs::write(
        &good,
        "; synthetic\n7A0-01-01 8000 0000 0029\n7A0-01-02 0005 AF\n7A0-02-01 0000 0000 2AD4\n",
    )
    .unwrap();
    let store = dir.join("store.json");
    let out = opentune(&[
        "coding",
        "asbuilt",
        s(&good),
        "--module",
        "7A0",
        "--store-out",
        s(&store),
    ]);
    assert!(out.status.success(), "{out:?}");
    assert!(stdout(&out).contains("3 line(s), 0 bad"));
    let store_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&store).unwrap()).unwrap();
    assert_eq!(store_json["0xDE00"], "80000000000005");
    assert_eq!(store_json["0xDE01"], "000000002A");
    // Never overwrites the store.
    assert!(!opentune(&[
        "coding",
        "asbuilt",
        s(&good),
        "--module",
        "7A0",
        "--store-out",
        s(&store)
    ])
    .status
    .success());

    // A wrong checksum fails and names the expected value.
    let bad = dir.join("bad.txt");
    fs::write(&bad, "7A0-01-01 8000 0000 0028\n").unwrap();
    let out = opentune(&["coding", "asbuilt", s(&bad)]);
    assert!(!out.status.success());
    assert!(stdout(&out).contains("should be 29"));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn import_ccf_writes_a_definition_that_decodes() {
    let dir = tmp("ccf");
    let xml = dir.join("descriptor.xml");
    fs::write(
        &xml,
        r#"<d><block name="CCF"><group start="1" stop="1">
  <parameter id="001_001" name="SYN_MODE" mask="0x0C" type="ENUM">
    <parameter_title><tm>Synthetic mode</tm></parameter_title>
    <select><option value="0x00" name="A"><tm>Mode A</tm></option>
            <option value="0x08" name="C"><tm>Mode C</tm></option></select>
  </parameter>
  <parameter id="001_005" name="SYN_TEXT" mask="0xFF" type="ASCII"/>
</group></block></d>"#,
    )
    .unwrap();
    let def = dir.join("def.json");
    let out = opentune(&[
        "coding",
        "import-ccf",
        s(&xml),
        "--did",
        "0x1234",
        "--out",
        s(&def),
    ]);
    assert!(out.status.success(), "{out:?}");
    let text = stdout(&out);
    assert!(text.contains("Imported 1 parameter"), "{text}");
    assert!(text.contains("skipped SYN_TEXT"), "{text}");

    let store = dir.join("store.json");
    fs::write(&store, r#"{ "0x1234": "0008" }"#).unwrap();
    let out = opentune(&[
        "coding",
        "decode",
        "--def",
        s(&def),
        "--driver",
        "mock",
        "--store",
        s(&store),
    ]);
    assert!(out.status.success(), "{out:?}");
    assert!(stdout(&out).contains("Mode C"));
    // The output definition is never overwritten.
    assert!(!opentune(&[
        "coding",
        "import-ccf",
        s(&xml),
        "--did",
        "0x1234",
        "--out",
        s(&def)
    ])
    .status
    .success());
    fs::remove_dir_all(dir).unwrap();
}
