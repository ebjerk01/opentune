//! FRF container handling.
//!
//! An FRF (`*.frf`) is a ZIP archive obfuscated with the rolling-XOR stream
//! cipher in [`crate::crypto::frf_xor`]. Decrypting it yields a ZIP whose
//! members are an ODX/ODX-F document (plus any external `.bin` flash members)
//! or one or more raw `.sgo` objects. Ported from `VW_Flash`
//! `lib/extract_flash.py` (BSD-2) and `vag-tcu-tools` `core/frf.py` (MIT).
//!
//! The FRF key material (`frf.key`) is **caller-supplied**; this crate bundles none.

use crate::crypto::{frf_xor, BlockCipher};
use crate::error::{Error, Result};
use crate::sgo::{self, SgoCipher};
use crate::{odx, zip, Segment};
use std::collections::HashMap;

/// What a decoded FRF/ZIP turned out to contain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerKind {
    /// An ODX document with inline hex flash data.
    Odx,
    /// An ODX-F document referencing external `.bin` members.
    OdxF,
    /// One or more raw SGO objects.
    Sgo,
    /// A Ford / Volvo / Mazda VBF flash container (plaintext).
    Vbf,
}

/// Result of decoding a container.
#[derive(Debug, Clone)]
pub struct Unpacked {
    /// What the container held.
    pub kind: ContainerKind,
    /// Decoded flash segments.
    pub segments: Vec<Segment>,
    /// Expected box codes / idents (ODX only; empty otherwise).
    pub box_codes: Vec<String>,
    /// ZIP member names seen inside the FRF (for diagnostics).
    pub members: Vec<String>,
}

/// Decrypt an FRF to its inner ZIP bytes using caller-supplied key material.
/// The transform is symmetric, so the same routine repacks (see [`encrypt_frf`]).
pub fn decrypt_frf(key_material: &[u8], frf_data: &[u8]) -> Result<Vec<u8>> {
    let out = frf_xor(key_material, frf_data)?;
    if out.len() < 2 || &out[..2] != b"PK" {
        return Err(Error::Unsupported(
            "FRF decryption did not produce a ZIP (wrong key?)".into(),
        ));
    }
    Ok(out)
}

/// Re-encrypt ZIP bytes back into FRF form (symmetric with [`decrypt_frf`]).
pub fn encrypt_frf(key_material: &[u8], zip_data: &[u8]) -> Result<Vec<u8>> {
    frf_xor(key_material, zip_data)
}

/// Decode an already-decrypted ZIP (the inner payload of an FRF), dispatching to
/// the ODX/ODX-F or SGO path. `cipher` decrypts ODX blocks; `sgo_table`/`sgo_cipher`
/// decode format-1 SGO blocks. All key material is caller-supplied.
pub fn unpack_zip(
    zip_bytes: &[u8],
    cipher: &BlockCipher,
    sgo_table: Option<&[u8; 256]>,
    sgo_cipher: SgoCipher,
) -> Result<Unpacked> {
    let entries = zip::read_all(zip_bytes)?;
    let members: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();

    // Separate the document from any external .bin members.
    let mut odx_doc: Option<(bool, Vec<u8>)> = None; // (is_odxf, bytes)
    let mut sgo_members: Vec<Vec<u8>> = Vec::new();
    let mut bins: HashMap<String, Vec<u8>> = HashMap::new();

    for e in &entries {
        let lower = e.name.to_ascii_lowercase();
        if lower.ends_with(".odx-f") || lower.ends_with(".odxf") {
            odx_doc = Some((true, e.data.clone()));
        } else if lower.ends_with(".odx") {
            odx_doc = Some((false, e.data.clone()));
        } else if lower.ends_with(".sgo") {
            sgo_members.push(e.data.clone());
        } else if lower.ends_with(".bin") {
            bins.insert(e.name.clone(), e.data.clone());
        } else if odx_doc.is_none() && looks_like_odx(&e.data) {
            // Some containers store the ODX without an obvious extension.
            let is_odxf = contains(&e.data, b"DATAFILE");
            odx_doc = Some((is_odxf, e.data.clone()));
        }
    }

    if let Some((is_odxf, bytes)) = odx_doc {
        let xml = String::from_utf8_lossy(&bytes);
        let contents = odx::parse(&xml, cipher, &bins)?;
        return Ok(Unpacked {
            kind: if is_odxf {
                ContainerKind::OdxF
            } else {
                ContainerKind::Odx
            },
            segments: contents.segments,
            box_codes: contents.box_codes,
            members,
        });
    }

    if !sgo_members.is_empty() {
        let mut segments = Vec::new();
        for m in &sgo_members {
            let parsed = sgo::parse(m)?;
            segments.extend(parsed.decode(sgo_table, sgo_cipher));
        }
        return Ok(Unpacked {
            kind: ContainerKind::Sgo,
            segments,
            box_codes: Vec::new(),
            members,
        });
    }

    Err(Error::Unsupported(format!(
        "no ODX/ODX-F or SGO member found in container: {members:?}"
    )))
}

fn looks_like_odx(data: &[u8]) -> bool {
    let head = &data[..data.len().min(512)];
    contains(head, b"<ODX") || contains(head, b"FLASHDATA") || contains(head, b"ECU-MEM")
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zip::write_stored;

    #[test]
    fn frf_wrap_unwrap_odx() {
        // Build a tiny ODX, zip it, FRF-encrypt it, then unpack end to end.
        let odx = br#"<ODX><FLASH><ECU-MEMS><ECU-MEM><MEM>
          <DATABLOCKS><DATABLOCK><FLASHDATA-REF ID-REF="fd1"/>
            <SEGMENTS><SEGMENT><UNCOMPRESSED-SIZE>3</UNCOMPRESSED-SIZE></SEGMENT></SEGMENTS>
          </DATABLOCK></DATABLOCKS>
          <FLASHDATAS><FLASHDATA ID="fd1">
            <ENCRYPT-COMPRESS-METHOD>00</ENCRYPT-COMPRESS-METHOD><DATA>010203</DATA>
          </FLASHDATA></FLASHDATAS>
        </MEM></ECU-MEM></ECU-MEMS></FLASH></ODX>"#;
        let zip = write_stored(&[("flash.odx".to_string(), odx.to_vec())]);

        // Synthetic key material — NOT a real frf.key.
        let key: Vec<u8> = (0..300u16).map(|i| (i * 13 % 251) as u8).collect();
        let frf = encrypt_frf(&key, &zip).unwrap();
        assert_ne!(&frf[..2], b"PK");

        let back = decrypt_frf(&key, &frf).unwrap();
        assert_eq!(&back[..2], b"PK");

        let unpacked = unpack_zip(&back, &BlockCipher::None, None, SgoCipher::Progressive).unwrap();
        assert_eq!(unpacked.kind, ContainerKind::Odx);
        assert_eq!(unpacked.segments.len(), 1);
        assert_eq!(unpacked.segments[0].data, vec![1, 2, 3]);
    }
}
