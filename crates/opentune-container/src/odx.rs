//! ODX and ODX-F flash-document parsing.
//!
//! VAG tester flash files are ODX XML. Two layouts exist and both are handled:
//!   * **ODX (inline)** — the flash bytes are hex text in each `FLASHDATA/DATA`.
//!   * **ODX-F (external)** — `FLASHDATA/DATAFILE` names a `.bin` member that
//!     travels alongside the XML inside the FRF's ZIP.
//!
//! Each block's `ENCRYPT-COMPRESS-METHOD` is two characters `CE`: `C` selects
//! compression and `E` selects encryption (see [`crate::Compression`] and the
//! method tables in the references). Ported from `VW_Flash` `extractodx.py`
//! (BSD-2) and `vag-tcu-tools` `core/odx.py` (MIT).

use crate::compress::Compression;
use crate::crypto::BlockCipher;
use crate::error::{Error, Result};
use crate::{Segment, SegmentSource};
use std::collections::HashMap;

/// Result of parsing an ODX/ODX-F document.
#[derive(Debug, Clone)]
pub struct OdxContents {
    /// Decoded flash segments, in document order.
    pub segments: Vec<Segment>,
    /// Expected box codes / idents the file is valid for (may be empty).
    pub box_codes: Vec<String>,
    /// SA2 seed/key scripts declared in `SECURITY` blocks (see [`sa2_scripts`]).
    pub sa2_scripts: Vec<Sa2Script>,
}

/// An SA2 seed/key script found in a container.
///
/// The bytes are the user's own container content; OpenTune bundles none.
/// Run them with the `opentune-sa2` crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sa2Script {
    /// The enclosing ODX `SESSION`'s `SHORT-NAME` (or `ID`), when there is one.
    pub session: Option<String>,
    /// Where the bytes came from: the ODX element name (e.g. `FW-SIGNATURE`)
    /// or `SGO` for an SGO header's SA2 field.
    pub field: String,
    /// The script bytes.
    pub bytes: Vec<u8>,
}

/// Elements of an ODX-F `SECURITY` block that can carry a byte field, in the
/// order they are tried. The ODX-F schema gives `SECURITY` the children
/// `SECURITY-METHOD`, `FW-SIGNATURE`, `FW-CHECKSUM` and `VALIDITY-FOR`.
const SA2_VALUE_FIELDS: [&str; 3] = ["FW-SIGNATURE", "FW-CHECKSUM", "VALIDITY-FOR"];

/// Collect the SA2 scripts an ODX/ODX-F document declares.
///
/// VAG testers read the script from the `SECURITY` block whose
/// `SECURITY-METHOD` is `SA2` (`SESSION/SECURITYS/SECURITY` in ODX-F). The
/// bytes are taken from that block's first byte-field child that holds
/// non-empty hex — `FW-SIGNATURE` first, then `FW-CHECKSUM`, `VALIDITY-FOR`
/// — and every such child is reported, so a caller can pick the one that
/// decodes as a valid SA2 program. Identical scripts (same field and bytes)
/// declared in several sessions are reported once.
pub fn sa2_scripts(xml: &str) -> Result<Vec<Sa2Script>> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| Error::Xml(e.to_string()))?;
    Ok(sa2_scripts_in(&doc))
}

fn sa2_scripts_in(doc: &roxmltree::Document) -> Vec<Sa2Script> {
    let mut out: Vec<Sa2Script> = Vec::new();
    for sec in doc.descendants().filter(|n| n.has_tag_name("SECURITY")) {
        let method = sec
            .children()
            .find(|n| n.has_tag_name("SECURITY-METHOD"))
            .and_then(|n| n.text())
            .map(str::trim)
            .unwrap_or("");
        if !method.eq_ignore_ascii_case("SA2") {
            continue;
        }
        let session = sec
            .ancestors()
            .find(|n| n.has_tag_name("SESSION"))
            .and_then(|s| {
                s.children()
                    .find(|n| n.has_tag_name("SHORT-NAME"))
                    .and_then(|n| n.text())
                    .map(|t| t.trim().to_string())
                    .or_else(|| s.attribute("ID").map(str::to_string))
            });
        for field in SA2_VALUE_FIELDS {
            let Some(text) = sec
                .children()
                .find(|n| n.has_tag_name(field))
                .and_then(|n| n.text())
            else {
                continue;
            };
            let Ok(bytes) = unhex(text.trim()) else {
                continue;
            };
            if bytes.is_empty() {
                continue;
            }
            if out.iter().any(|s| s.field == field && s.bytes == bytes) {
                continue;
            }
            out.push(Sa2Script {
                session: session.clone(),
                field: field.to_string(),
                bytes,
            });
        }
    }
    out
}

/// Parse an inline ODX document (`FLASHDATA/DATA` hex), decoding each block with
/// `cipher`. External `.bin` members are supplied via `bins` for the ODX-F case.
pub fn parse(
    xml: &str,
    cipher: &BlockCipher,
    bins: &HashMap<String, Vec<u8>>,
) -> Result<OdxContents> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| Error::Xml(e.to_string()))?;

    // Pre-index DATABLOCK FLASHDATA-REF -> UNCOMPRESSED-SIZE and SOURCE-START-ADDRESS.
    let mut size_by_ref: HashMap<String, usize> = HashMap::new();
    let mut addr_by_ref: HashMap<String, u32> = HashMap::new();
    for datablock in doc.descendants().filter(|n| n.has_tag_name("DATABLOCK")) {
        let ref_id = datablock
            .descendants()
            .find(|n| n.has_tag_name("FLASHDATA-REF"))
            .and_then(|n| n.attribute("ID-REF"));
        let Some(ref_id) = ref_id else { continue };
        if let Some(seg) = datablock.descendants().find(|n| n.has_tag_name("SEGMENT")) {
            if let Some(sz) = child_text(seg, "UNCOMPRESSED-SIZE").and_then(|t| parse_int(&t)) {
                size_by_ref.insert(ref_id.to_string(), sz as usize);
            }
            if let Some(a) = child_text(seg, "SOURCE-START-ADDRESS").and_then(|t| parse_int(&t)) {
                addr_by_ref.insert(ref_id.to_string(), a as u32);
            }
        }
    }

    // Collect expected idents / box codes.
    let mut box_codes = Vec::new();
    for iv in doc.descendants().filter(|n| n.has_tag_name("IDENT-VALUE")) {
        if let Some(t) = iv.text() {
            let t = t.trim();
            if !t.is_empty() {
                box_codes.push(t.to_string());
            }
        }
    }

    let mut segments = Vec::new();
    for fd in doc.descendants().filter(|n| n.has_tag_name("FLASHDATA")) {
        let id = fd.attribute("ID").unwrap_or("unknown").to_string();
        let method = child_text(fd, "ENCRYPT-COMPRESS-METHOD").unwrap_or_else(|| "00".to_string());
        let out_size = size_by_ref.get(&id).copied().unwrap_or(0);
        let address = addr_by_ref.get(&id).copied();

        // Resolve the raw stored bytes: inline hex, or an external .bin member.
        let (raw, source) = if let Some(datafile) = child_text(fd, "DATAFILE") {
            let datafile = datafile.trim().to_string();
            match bins.get(&datafile) {
                Some(b) => (b.clone(), SegmentSource::ExternalBin(datafile)),
                None => continue, // referenced bin not present
            }
        } else if let Some(hex) = child_text(fd, "DATA") {
            let hex = hex.trim();
            if hex.len() <= 4 {
                // 2-char entries are ERASE blocks with no data.
                continue;
            }
            (unhex(hex)?, SegmentSource::InlineHex)
        } else {
            continue;
        };

        segments.push(decode_block(
            id, &method, raw, out_size, address, source, cipher,
        ));
    }

    Ok(OdxContents {
        segments,
        box_codes,
        sa2_scripts: sa2_scripts_in(&doc),
    })
}

/// Apply the decrypt + decompress pipeline implied by `method` to one block.
/// On a missing key or a decode failure the raw bytes are kept and
/// `Segment::decoded` is set to `false` so partial extraction still succeeds.
fn decode_block(
    id: String,
    method: &str,
    raw: Vec<u8>,
    out_size: usize,
    address: Option<u32>,
    source: SegmentSource,
    cipher: &BlockCipher,
) -> Segment {
    let chars: Vec<char> = method.chars().collect();
    let comp_char = chars.first().copied().unwrap_or('0');
    let enc_char = chars.get(1).copied().unwrap_or('0');
    let compression = Compression::from_method_char(comp_char);

    let mut seg = Segment {
        id,
        address,
        data: raw.clone(),
        compression,
        method: method.to_string(),
        source,
        decoded: false,
        note: None,
    };

    // Decrypt (if the method asks for it).
    let decrypted = if enc_char == '0' {
        raw
    } else if matches!(cipher, BlockCipher::None) {
        seg.note = Some("encrypted block but no cipher/key supplied; kept raw".into());
        return seg;
    } else {
        match cipher.decrypt(&raw) {
            Ok(d) => d,
            Err(e) => {
                seg.note = Some(format!("decrypt failed: {e}; kept raw"));
                return seg;
            }
        }
    };

    // Decompress.
    match compression.decompress(&decrypted, out_size) {
        Ok(d) => {
            seg.data = d;
            seg.decoded = true;
        }
        Err(e) => {
            seg.data = decrypted;
            seg.decoded = false;
            seg.note = Some(format!(
                "decompress failed: {e}; kept decrypted-but-compressed"
            ));
        }
    }
    seg
}

fn child_text<'a>(node: roxmltree::Node<'a, 'a>, tag: &str) -> Option<String> {
    node.descendants()
        .find(|n| n.has_tag_name(tag))
        .and_then(|n| n.text())
        .map(|t| t.to_string())
}

fn parse_int(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()
    } else {
        s.parse::<u64>().ok()
    }
}

/// Decode an even-length ASCII hex string to bytes.
pub fn unhex(s: &str) -> Result<Vec<u8>> {
    let bytes: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if bytes.len() % 2 != 0 {
        return Err(Error::BadHex("odd-length hex string".into()));
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let hi = hex_val(pair[0])?;
        let lo = hex_val(pair[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_val(b: u8) -> Result<u8> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(Error::BadHex(format!("non-hex byte {b:#x}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_inline_plaintext_block() {
        // One uncompressed, unencrypted (method "00") block carrying "DEADBEEF".
        let xml = r#"
        <ODX><FLASH><ECU-MEMS><ECU-MEM><MEM>
          <DATABLOCKS><DATABLOCK>
            <FLASHDATA-REF ID-REF="fd1"/>
            <SEGMENTS><SEGMENT>
              <SOURCE-START-ADDRESS>0x8000</SOURCE-START-ADDRESS>
              <UNCOMPRESSED-SIZE>4</UNCOMPRESSED-SIZE>
            </SEGMENT></SEGMENTS>
          </DATABLOCK></DATABLOCKS>
          <FLASHDATAS><FLASHDATA ID="fd1">
            <ENCRYPT-COMPRESS-METHOD>00</ENCRYPT-COMPRESS-METHOD>
            <DATA>DEADBEEF</DATA>
          </FLASHDATA></FLASHDATAS>
        </MEM></ECU-MEM></ECU-MEMS></FLASH></ODX>"#;
        let contents = parse(xml, &BlockCipher::None, &HashMap::new()).unwrap();
        assert_eq!(contents.segments.len(), 1);
        let s = &contents.segments[0];
        assert_eq!(s.id, "fd1");
        assert_eq!(s.address, Some(0x8000));
        assert!(s.decoded);
        assert_eq!(s.data, vec![0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn sa2_script_from_security_block() {
        // Synthetic script (loop 2 { rsl; eor 0x11223344 } finish) — not an OEM script.
        let xml = r#"<ODX><FLASH><ECU-MEMS><ECU-MEM><MEM><SESSIONS>
          <SESSION ID="s1"><SHORT-NAME>Session_A</SHORT-NAME>
            <SECURITYS>
              <SECURITY>
                <SECURITY-METHOD TYPE="A_ASCIISTRING">CRC32</SECURITY-METHOD>
                <FW-CHECKSUM TYPE="A_BYTEFIELD">DEADBEEF</FW-CHECKSUM>
              </SECURITY>
              <SECURITY>
                <SECURITY-METHOD TYPE="A_ASCIISTRING"> SA2 </SECURITY-METHOD>
                <FW-SIGNATURE TYPE="A_BYTEFIELD">680281871122334449 4C</FW-SIGNATURE>
              </SECURITY>
            </SECURITYS>
          </SESSION>
          <SESSION ID="s2"><SECURITYS><SECURITY>
            <SECURITY-METHOD>sa2</SECURITY-METHOD>
            <FW-SIGNATURE>680281871122334449 4C</FW-SIGNATURE>
          </SECURITY></SECURITYS></SESSION>
        </SESSIONS></MEM></ECU-MEM></ECU-MEMS></FLASH></ODX>"#;
        let scripts = sa2_scripts(xml).unwrap();
        assert_eq!(scripts.len(), 1, "duplicate across sessions reported once");
        assert_eq!(scripts[0].session.as_deref(), Some("Session_A"));
        assert_eq!(scripts[0].field, "FW-SIGNATURE");
        assert_eq!(
            scripts[0].bytes,
            vec![0x68, 0x02, 0x81, 0x87, 0x11, 0x22, 0x33, 0x44, 0x49, 0x4C]
        );
        let parsed = parse(xml, &BlockCipher::None, &HashMap::new()).unwrap();
        assert_eq!(parsed.sa2_scripts, scripts);
        assert!(sa2_scripts("<ODX/>").unwrap().is_empty());
    }

    #[test]
    fn unhex_roundtrip() {
        assert_eq!(unhex("00ff10").unwrap(), vec![0, 255, 16]);
        assert!(unhex("abc").is_err());
    }
}
