//! `import-xdf` / `import-fastecu`: the per-object report, `-o` JSON output
//! and `--strict` refusal, end to end through the binary (synthetic inputs).

use std::path::PathBuf;
use std::process::Command;

fn tmp(name: &str, body: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("opentune-defimport-{}-{name}", std::process::id()));
    std::fs::write(&p, body).expect("write temp");
    p
}

fn run(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_opentune"))
        .args(args)
        .output()
        .expect("spawn opentune");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

const XDF: &str = r#"<XDFFORMAT version="1.60"><XDFHEADER><deftitle>T</deftitle></XDFHEADER>
  <XDFTABLE uniqueid="0x1"><title>Good</title><XDFAXIS id="z">
    <EMBEDDEDDATA mmedaddress="0x10" mmedelementsizebits="16" mmedcolcount="2" mmedtypeflags="0x02"/>
    <MATH equation="X*0.5"/></XDFAXIS></XDFTABLE>
  <XDFTABLE uniqueid="0x2"><title>Curved</title><XDFAXIS id="z">
    <EMBEDDEDDATA mmedaddress="0x20" mmedelementsizebits="8"/>
    <MATH equation="1000/X"/></XDFAXIS></XDFTABLE>
  <XDFFLAG uniqueid="0x3"><title>Bit</title><mask>0x01</mask>
    <EMBEDDEDDATA mmedaddress="0x30" mmedelementsizebits="8"/></XDFFLAG>
</XDFFORMAT>"#;

#[test]
fn import_xdf_reports_writes_json_and_strict_refuses() {
    let f = tmp("a.xdf", XDF);
    let out =
        std::env::temp_dir().join(format!("opentune-defimport-{}-a.json", std::process::id()));
    let (ok, stdout, _) = run(&[
        "import-xdf",
        f.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
    ]);
    assert!(ok, "{stdout}");
    assert!(
        stdout.contains("1 clean, 1 warned, 0 refused, 1 out-of-scope"),
        "{stdout}"
    );
    assert!(stdout.contains("math-nonlinear"), "{stdout}");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!(v["maps"].as_array().unwrap().len(), 2);
    assert_eq!(v["maps"][0]["scaling"]["factor"], 0.5);
    let report = v["report"].as_array().unwrap();
    assert_eq!(report.len(), 3);
    assert_eq!(report[2]["object"], "flag");
    assert_eq!(report[2]["status"], "refused");

    // --strict: refused, nothing written.
    let _ = std::fs::remove_file(&out);
    let (ok, _, stderr) = run(&[
        "import-xdf",
        f.to_str().unwrap(),
        "--strict",
        "-o",
        out.to_str().unwrap(),
    ]);
    assert!(!ok);
    assert!(stderr.contains("Curved"), "{stderr}");
    assert!(!out.exists(), "strict failure must not write output");

    // A clean file passes --strict.
    let clean = tmp("b.xdf", &XDF.replace("1000/X", "X"));
    let (ok, stdout, stderr) = run(&["import-xdf", clean.to_str().unwrap(), "--strict"]);
    assert!(ok, "{stdout}{stderr}");
}

#[test]
fn import_fastecu_xml_and_csv() {
    let xml = tmp(
        "c.xml",
        r#"<rom><romid><xmlid>T1</xmlid><memmodel>SH7058</memmodel></romid>
          <scaling name="B" units="x" toexpr="x*2" storagetype="uint16"/>
          <table name="Lim" type="1D" address="100" scaling="B"/>
          <table name="Mixed" type="2D" address="200" scaling="B" interval="2">
            <table name="X" type="Static X Axis" elements="2"><data>1</data><data>2</data></table>
          </table></rom>"#,
    );
    let out =
        std::env::temp_dir().join(format!("opentune-defimport-{}-c.json", std::process::id()));
    let (ok, stdout, stderr) = run(&[
        "import-fastecu",
        xml.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
    ]);
    assert!(ok, "{stdout}{stderr}");
    assert!(stdout.contains("1 clean, 0 warned, 1 refused"), "{stdout}");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!(v["maps"][0]["address"], 0x100);
    assert_eq!(v["source"], "fastecu:ecuflash-xml");
    let (ok, _, stderr) = run(&["import-fastecu", xml.to_str().unwrap(), "--strict"]);
    assert!(!ok && stderr.contains("stride-noncontiguous"), "{stderr}");

    let csv = tmp(
        "d.csv",
        "Name;DataOrg;Columns;Rows;Fieldvalues.Factor;Fieldvalues.StartAddr;Fieldvalues.Signed\nK;eLoHi;1;1;0,5;$40;0\n",
    );
    let (ok, stdout, stderr) = run(&["import-fastecu", csv.to_str().unwrap(), "--strict"]);
    assert!(ok, "{stdout}{stderr}");
    assert!(stdout.contains("mappack-csv"), "{stdout}");
}
