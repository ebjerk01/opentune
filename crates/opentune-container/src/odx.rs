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
    fn unhex_roundtrip() {
        assert_eq!(unhex("00ff10").unwrap(), vec![0, 255, 16]);
        assert!(unhex("abc").is_err());
    }
}
