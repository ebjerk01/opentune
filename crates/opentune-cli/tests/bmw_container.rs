//! `opentune container unpack|identify` on BMW PSdZData SWE pairs, through the
//! built binary. Every file here is SYNTHETIC (made-up id, version, addresses,
//! payload); no BMW file is used.
use opentune_container::nrv2b::compress_nrv2b_8;
use std::path::PathBuf;
use std::process::{Command, Output};

fn opentune(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_opentune"))
        .args(args)
        .output()
        .expect("run opentune")
}

fn ok_stdout(args: &[&str]) -> String {
    let out = opentune(args);
    assert!(out.status.success(), "opentune {args:?} failed: {out:?}");
    String::from_utf8(out.stdout).unwrap()
}

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("opentune-bmw-cli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Writes `swfl_0000beef.{bin,xml}.001_002_003`: a stored 16-byte segment and an
/// NRV2B segment, returning (dir, plain bytes of the two segments).
fn write_pair(tag: &str) -> (PathBuf, Vec<u8>, Vec<u8>) {
    let d = dir(tag);
    let head: Vec<u8> = (0..16u8).collect();
    let code: Vec<u8> = (0..0x400u32).map(|i| (i % 7) as u8 * 0x11).collect();
    let nrv = compress_nrv2b_8(&code);
    let mut bin = head.clone();
    bin.extend(&nrv);
    let xml = format!(
        r#"<?xml version="1.0"?>
<BINARY-FLASHBLOCK>
 <SHORT-NAME>swfl_0000beef</SHORT-NAME>
 <SDGS><SDG><SDG-CAPTION><SHORT-NAME>Ident</SHORT-NAME></SDG-CAPTION>
  <SD SI="ProcessClass">swfl</SD><SD SI="Identifier">0000beef</SD><SD SI="Version">001_002_003</SD></SDG>
  <SDG><SDG-CAPTION><SHORT-NAME>Checksum</SHORT-NAME></SDG-CAPTION>
  <SD SI="Mode">CRC16</SD><SD SI="Value">ABCD</SD></SDG></SDGS>
 <FLASH-SEGMENTS>
  <FLASH-SEGMENT COMPRESSION-STATUS="UNCOMPRESSED">
   <SOURCE-START-ADDRESS>0</SOURCE-START-ADDRESS><SOURCE-END-ADDRESS>F</SOURCE-END-ADDRESS>
   <TARGET-START-ADDRESS>20000</TARGET-START-ADDRESS><TARGET-END-ADDRESS>2000F</TARGET-END-ADDRESS>
  </FLASH-SEGMENT>
  <FLASH-SEGMENT COMPRESSION-STATUS="COMPRESSED">
   <SOURCE-START-ADDRESS>10</SOURCE-START-ADDRESS><SOURCE-END-ADDRESS>{:X}</SOURCE-END-ADDRESS>
   <TARGET-START-ADDRESS>20020</TARGET-START-ADDRESS><TARGET-END-ADDRESS>2041F</TARGET-END-ADDRESS>
  </FLASH-SEGMENT>
 </FLASH-SEGMENTS>
</BINARY-FLASHBLOCK>"#,
        bin.len() - 1
    );
    std::fs::write(d.join("swfl_0000beef.bin.001_002_003"), &bin).unwrap();
    std::fs::write(d.join("swfl_0000beef.xml.001_002_003"), xml).unwrap();
    (d, head, code)
}

#[test]
fn unpack_finds_the_sibling_and_writes_a_flat_image() {
    let (d, head, code) = write_pair("unpack");
    let bin = d.join("swfl_0000beef.bin.001_002_003");
    let flat = d.join("flat.bin");
    let out_dir = d.join("segs");
    let out = ok_stdout(&[
        "container",
        "unpack",
        "--in",
        bin.to_str().unwrap(),
        "--outdir",
        out_dir.to_str().unwrap(),
        "--flat",
        flat.to_str().unwrap(),
    ]);
    assert!(out.contains("Container : BmwSwe"), "{out}");
    assert!(
        out.contains("SWE       : swfl 0000beef version 001_002_003"),
        "{out}"
    );
    assert!(
        out.contains("CRC16 = ABCD") && out.contains("NOT verified"),
        "{out}"
    );
    assert!(out.contains("2 flash-ready, 0 not flash-ready"), "{out}");
    assert_eq!(std::fs::read(out_dir.join("seg01.bin")).unwrap(), code);

    let img = std::fs::read(&flat).unwrap();
    assert_eq!(img.len(), 0x420);
    assert_eq!(&img[..16], &head[..]);
    assert!(img[16..0x20].iter().all(|&b| b == 0xFF));
    assert_eq!(&img[0x20..], &code[..]);

    // The .xml works as --in too, and --fill changes the gap byte.
    let xml = d.join("swfl_0000beef.xml.001_002_003");
    let flat0 = d.join("flat0.bin");
    ok_stdout(&[
        "container",
        "unpack",
        "--in",
        xml.to_str().unwrap(),
        "--flat",
        flat0.to_str().unwrap(),
        "--fill",
        "00",
    ]);
    let img0 = std::fs::read(&flat0).unwrap();
    assert!(img0[16..0x20].iter().all(|&b| b == 0));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn identify_and_refusals() {
    let (d, _, _) = write_pair("identify");
    let bin = d.join("swfl_0000beef.bin.001_002_003");
    let out = ok_stdout(&["container", "identify", "--in", bin.to_str().unwrap()]);
    assert!(out.contains("BMW PSdZData SWE flash container"), "{out}");
    assert!(out.contains("Segments  : 2"), "{out}");
    assert!(out.contains("Nrv2b"), "{out}");

    // A CAFD is identified by name but never decoded.
    let caf = d.join("cafd_00001234.caf.003_001_000");
    std::fs::write(&caf, [0u8; 32]).unwrap();
    let out = ok_stdout(&["container", "identify", "--in", caf.to_str().unwrap()]);
    assert!(out.contains("coding data"), "{out}");
    let res = opentune(&["container", "unpack", "--in", caf.to_str().unwrap()]);
    assert!(!res.status.success());
    assert!(String::from_utf8_lossy(&res.stderr).contains("not publicly documented"));

    // A .bin without its descriptor is refused, not guessed.
    let lone = d.join("btld_0000cafe.bin.001_000_000");
    std::fs::write(&lone, [0u8; 32]).unwrap();
    let res = opentune(&["container", "unpack", "--in", lone.to_str().unwrap()]);
    assert!(!res.status.success());
    assert!(String::from_utf8_lossy(&res.stderr).contains("--pair"));
    let _ = std::fs::remove_dir_all(&d);
}
