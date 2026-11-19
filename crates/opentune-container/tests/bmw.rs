//! BMW PSdZData SWE pairs, end to end through the public API.
//!
//! Every fixture is SYNTHETIC and built here: made-up identifiers, versions,
//! addresses and payloads. No BMW file (and nothing derived from one) is used.
//! The descriptor shape follows the public examples cited in
//! docs/BMW-CONTAINERS.md.

use opentune_container::bmw::{self, CompressionStatus, ProcessClass, SweName};
use opentune_container::nrv2b::compress_nrv2b_8;
use opentune_container::{
    detect, flatten, unpack, Compression, ContainerKind, InputFormat, Keys, SegmentSource,
};
use std::io::Write;

fn payload(n: usize, seed: u32) -> Vec<u8> {
    (0..n as u32)
        .map(|i| {
            if (i / 64) % 3 == 0 {
                0xFF
            } else {
                (i.wrapping_mul(seed) >> 7) as u8
            }
        })
        .collect()
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

struct Fixture {
    xml: String,
    bin: Vec<u8>,
    plain: [Vec<u8>; 3],
}

/// `.bin` = [stored 0x40 bytes][NRV2B segment][padding][stored segment]; the
/// XML has a default namespace, the Ident / SignatureTable / Checksum groups and
/// one segment written with `0x` prefixes.
fn fixture() -> Fixture {
    let header = payload(0x40, 2654435761);
    let code = payload(0x3000, 40503);
    let tail = payload(0x200, 69069);
    let nrv = compress_nrv2b_8(&code);
    assert!(nrv.len() < code.len(), "synthetic payload should compress");

    let mut bin = header.clone();
    let nrv_start = bin.len();
    bin.extend(&nrv);
    let nrv_end = bin.len() - 1;
    bin.extend([0u8; 7]); // gap the descriptor does not reference
    let tail_start = bin.len();
    bin.extend(&tail);
    let tail_end = bin.len() - 1;

    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<BINARY-HEADER xmlns="urn:example:synthetic" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
 <BINARY-FLASHBLOCK>
  <SHORT-NAME>swfl_0000beef</SHORT-NAME>
  <SDGS>
   <SDG><SDG-CAPTION><SHORT-NAME>Ident</SHORT-NAME></SDG-CAPTION>
    <SD SI="ProcessClass">swfl</SD><SD SI="Identifier">0000beef</SD><SD SI="Version">001_002_003</SD></SDG>
   <SDG><SDG-CAPTION><SHORT-NAME>SignatureTable</SHORT-NAME></SDG-CAPTION>
    <SD SI="SignatureStatus">SIGNED</SD><SD SI="SignatureKeyLength">2048</SD>
    <SD SI="SignatureHashMode">SHA256</SD><SD SI="SignatureMode">rsa</SD></SDG>
   <SDG><SDG-CAPTION><SHORT-NAME>Checksum</SHORT-NAME></SDG-CAPTION>
    <SD SI="Mode">CRC16</SD><SD SI="Value">1234</SD></SDG>
  </SDGS>
  <FLASH-SEGMENTS>
   <FLASH-SEGMENT COMPRESSION-STATUS="UNCOMPRESSED">
    <SOURCE-START-ADDRESS>0000000</SOURCE-START-ADDRESS>
    <SOURCE-END-ADDRESS>000003F</SOURCE-END-ADDRESS>
    <TARGET-START-ADDRESS>0800000</TARGET-START-ADDRESS>
    <TARGET-END-ADDRESS>080003F</TARGET-END-ADDRESS>
   </FLASH-SEGMENT>
   <FLASH-SEGMENT COMPRESSION-STATUS="COMPRESSED">
    <SHORT-NAME>code</SHORT-NAME>
    <SOURCE-START-ADDRESS>{nrv_start:07X}</SOURCE-START-ADDRESS>
    <SOURCE-END-ADDRESS>{nrv_end:07X}</SOURCE-END-ADDRESS>
    <TARGET-START-ADDRESS>0800100</TARGET-START-ADDRESS>
    <TARGET-END-ADDRESS>08030FF</TARGET-END-ADDRESS>
   </FLASH-SEGMENT>
   <FLASH-SEGMENT>
    <SOURCE-START-ADDRESS>0x{tail_start:X}</SOURCE-START-ADDRESS>
    <SOURCE-END-ADDRESS>0x{tail_end:X}</SOURCE-END-ADDRESS>
    <TARGET-START-ADDRESS>0x0810000</TARGET-START-ADDRESS>
    <TARGET-END-ADDRESS>0x08101FF</TARGET-END-ADDRESS>
   </FLASH-SEGMENT>
  </FLASH-SEGMENTS>
 </BINARY-FLASHBLOCK>
</BINARY-HEADER>
"#
    );
    Fixture {
        xml,
        bin,
        plain: [header, code, tail],
    }
}

#[test]
fn decodes_a_synthetic_pair() {
    let f = fixture();
    assert_eq!(detect(f.xml.as_bytes()), InputFormat::BmwSweXml);
    // The bare descriptor is not decodable on its own.
    assert!(unpack(f.xml.as_bytes(), &Keys::default()).is_err());

    let swe = bmw::unpack_pair(&f.xml, &f.bin).unwrap();
    let d = &swe.descriptor;
    assert_eq!(d.short_name.as_deref(), Some("swfl_0000beef"));
    assert_eq!(
        d.ident(),
        Some((Some("swfl"), Some("0000beef"), Some("001_002_003")))
    );
    assert_eq!(d.segments.len(), 3);
    assert_eq!(d.segments[1].status, CompressionStatus::Compressed);
    // Absent attribute = uncompressed.
    assert_eq!(d.segments[2].status, CompressionStatus::Uncompressed);
    assert!(swe.notes.iter().any(|n| n.contains("signature NOT")));
    assert!(swe.notes.iter().any(|n| n.contains("CRC16 = 1234")));

    let s = &swe.segments;
    assert_eq!(s[0].address, Some(0x0800000));
    assert_eq!(s[1].id, "seg01_code");
    assert_eq!(s[1].compression, Compression::Nrv2b);
    assert_eq!(s[1].source, SegmentSource::BmwSwe);
    for (seg, want) in s.iter().zip(&f.plain) {
        assert!(seg.decoded, "{}: {:?}", seg.id, seg.note);
        assert_eq!(&seg.data, want);
    }

    let u = swe.into_unpacked();
    assert_eq!(u.kind, ContainerKind::BmwSwe);
    let img = flatten(&u.segments, 0xFF).unwrap();
    assert_eq!(img.base, 0x0800000);
    assert_eq!(img.data.len(), 0x10200);
    assert_eq!(&img.data[..0x40], &f.plain[0][..]);
    assert!(img.data[0x40..0x100].iter().all(|&b| b == 0xFF));
    assert_eq!(&img.data[0x100..0x3100], &f.plain[1][..]);
    assert_eq!(&img.data[0x10000..], &f.plain[2][..]);
}

#[test]
fn gzip_segments_and_length_mismatch_are_reported() {
    let plain = payload(0x800, 1103515245);
    let gz = gzip(&plain);
    let xml = |target_end: u32| {
        format!(
            r#"<BINARY-FLASHBLOCK><FLASH-SEGMENTS>
            <FLASH-SEGMENT COMPRESSION-STATUS="COMPRESSED">
              <LONG-NAME>part.img</LONG-NAME>
              <SOURCE-START-ADDRESS>0</SOURCE-START-ADDRESS>
              <SOURCE-END-ADDRESS>{:X}</SOURCE-END-ADDRESS>
              <TARGET-START-ADDRESS>0</TARGET-START-ADDRESS>
              <TARGET-END-ADDRESS>{target_end:X}</TARGET-END-ADDRESS>
              <COMPRESSION-METHOD>gz</COMPRESSION-METHOD>
            </FLASH-SEGMENT></FLASH-SEGMENTS></BINARY-FLASHBLOCK>"#,
            gz.len() - 1
        )
    };
    // Target range == decoded length: flash-ready.
    let ok = bmw::unpack_pair(&xml(0x7FF), &gz).unwrap();
    assert!(ok.segments[0].decoded);
    assert_eq!(ok.segments[0].compression, Compression::Gzip);
    assert_eq!(ok.segments[0].id, "seg00_part.img");
    assert_eq!(ok.segments[0].data, plain);

    // Target range == compressed length (one public example has this): the data
    // decodes, but placement is ambiguous, so it is not flash-ready and no flat
    // image is built.
    let amb = bmw::unpack_pair(&xml(gz.len() as u32 - 1), &gz).unwrap();
    assert!(!amb.segments[0].decoded);
    assert_eq!(amb.segments[0].data, plain);
    assert!(amb.segments[0].note.as_deref().unwrap().contains("ambiguous"));
    assert!(flatten(&amb.segments, 0xFF).is_err());
}

#[test]
fn refuses_what_is_not_understood() {
    let bin = vec![0xAAu8; 0x20];
    let seg = |attr: &str, extra: &str, src_end: &str| {
        format!(
            r#"<FLASH-SEGMENT {attr}><SOURCE-START-ADDRESS>0</SOURCE-START-ADDRESS>
            <SOURCE-END-ADDRESS>{src_end}</SOURCE-END-ADDRESS>
            <TARGET-START-ADDRESS>100</TARGET-START-ADDRESS>
            <TARGET-END-ADDRESS>10F</TARGET-END-ADDRESS>{extra}</FLASH-SEGMENT>"#
        )
    };
    let xml = format!(
        "<BINARY-FLASHBLOCK><FLASH-SEGMENTS>{}{}{}{}{}</FLASH-SEGMENTS></BINARY-FLASHBLOCK>",
        seg(r#"COMPRESSION-STATUS="ENCRYPTED""#, "", "F"),
        seg(
            r#"COMPRESSION-STATUS="COMPRESSED""#,
            "<COMPRESSION-METHOD>lzma</COMPRESSION-METHOD>",
            "F"
        ),
        // NRV2B that is not a valid stream.
        seg(r#"COMPRESSION-STATUS="COMPRESSED""#, "", "F"),
        // Source range beyond the .bin.
        seg("", "", "40"),
        // Stored, but source (0x11) and target (0x10) lengths differ.
        seg("", "", "10"),
    );
    let swe = bmw::unpack_pair(&xml, &bin).unwrap();
    assert_eq!(swe.segments.len(), 5);
    for s in &swe.segments {
        assert!(!s.decoded, "{} should be refused", s.id);
        assert!(s.note.is_some());
    }
    assert!(swe.segments[0]
        .note
        .as_deref()
        .unwrap()
        .contains("ENCRYPTED"));
    // Every segment targets 0x100..0x10F, so overlaps are reported.
    assert!(swe.notes.iter().any(|n| n.contains("overlap")));

    // Malformed descriptors are errors, not partial results.
    assert!(bmw::unpack_pair("<BINARY-FLASHBLOCK/>", &bin).is_err());
    assert!(bmw::unpack_pair("<a><FLASH-SEGMENT/></a>", &bin).is_err());
    assert!(bmw::unpack_pair("not xml", &bin).is_err());
    let descending = "<a><FLASH-SEGMENT><SOURCE-START-ADDRESS>5</SOURCE-START-ADDRESS>\
        <SOURCE-END-ADDRESS>1</SOURCE-END-ADDRESS><TARGET-START-ADDRESS>0</TARGET-START-ADDRESS>\
        <TARGET-END-ADDRESS>4</TARGET-END-ADDRESS></FLASH-SEGMENT></a>";
    assert!(bmw::unpack_pair(descending, &bin).is_err());
}

#[test]
fn file_names() {
    let n = SweName::parse("btld_0000abcd.xml.010_000_002").unwrap();
    assert_eq!(n.class, ProcessClass::Btld);
    assert_eq!(n.sibling("bin"), "btld_0000abcd.bin.010_000_002");
    assert_eq!(
        SweName::parse("cafd_00001234.caf.003_001_000")
            .unwrap()
            .class,
        ProcessClass::Cafd
    );
}
