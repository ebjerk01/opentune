//! # opentune-container
//!
//! Decode (and, where feasible, repack) automotive flash containers for
//! OpenTune: the VAG/VW-Group **FRF**, **ODX / ODX-F** and **SGO** tester files
//! for Simos engine ECUs and the DQ/DL/AL/VL/ZF transmission (TCU) families
//! (DQ200 → AL1000, DQ250-MQB, DQ381, DL382/DL501, ZF-8HP, …), plus the Ford /
//! Volvo / Mazda **VBF** (Versatile Binary Format) module-flash file (see
//! [`vbf`]) — a plaintext container that needs no key material — and the BMW
//! **PSdZData SWE** flash container (`.xml` descriptor + `.bin`, NRV2B/gzip
//! segments; see [`bmw`]), read-only and also key-free.
//!
//! ## Pipeline
//!
//! ```text
//!   FRF  --rolling-XOR(frf.key)-->  ZIP  --->  ODX / ODX-F / SGO
//!   ODX block  --decrypt(method)--> --decompress(method)-->  flash segment
//!   SGO block  --XOR-FF | family-cipher-->                   flash segment
//! ```
//!
//! ## No bundled keys — ever
//!
//! This crate **ships no key material**. The FRF key (`frf.key`), Simos/DQ AES
//! keys + IVs, the ZF-8HP repeating-XOR string, the DSG substitution table and
//! the SGO format-1 family cipher tables are all recovered, firmware-specific
//! material with legal/security concerns. Every one is supplied by the caller
//! through [`Keys`] / [`crypto::BlockCipher`]; nothing is hardcoded.
//!
//! ## Quick start
//!
//! ```no_run
//! use opentune_container::{unpack, Keys, crypto::BlockCipher};
//!
//! let frf_bytes = std::fs::read("flash.frf").unwrap();
//! let keys = Keys {
//!     // Caller supplies the recovered frf.key bytes; this crate ships none.
//!     frf_key: Some(std::fs::read("frf.key").unwrap()),
//!     block_cipher: BlockCipher::Aes128Cbc { key: [0; 16], iv: [0; 16] },
//!     ..Keys::default()
//! };
//! let result = unpack(&frf_bytes, &keys).unwrap();
//! for seg in &result.segments {
//!     println!("{} {} bytes decoded={}", seg.id, seg.data.len(), seg.decoded);
//! }
//! ```
//!
//! ## Attribution
//!
//! Algorithms ported, under license-compatible open-source terms, from:
//! * `bri3d/VW_Flash` (BSD-2-Clause) — FRF rolling-XOR, Simos LZSS10, legacy-Simos
//!   compression, Simos-XOR, DSG substitution, ODX extraction.
//! * `lsdlsd88/TCU-FRF-SGO-UNPACKER` (MIT) — SGO v2 structure and format-1 ciphers.
//! * `dspl1236/vag-tcu-tools` (MIT) — DQ381 LZSS, ZF-8HP LZZ (2047 window),
//!   repeating-XOR / AES-CBC block crypto, ODX-F external-bin parsing.
//!
//! Implemented independently from public descriptions (no code copied):
//! * UCL NRV2B (M. F. X. J. Oberhumer, GPL-2.0-or-later) — [`nrv2b`]; checked
//!   against the public vector in BMW's `uclcli` (GPL-3.0-or-later).
//! * The BMW SWE descriptor layout — sources listed in `docs/BMW-CONTAINERS.md`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod bmw;
pub mod compress;
pub mod crypto;
pub mod error;
pub mod frf;
pub mod nrv2b;
pub mod odx;
pub mod sgo;
pub mod vbf;
pub mod zip;

pub use compress::Compression;
pub use crypto::BlockCipher;
pub use error::{Error, Result};
pub use frf::{ContainerKind, Unpacked};
pub use odx::Sa2Script;
pub use sgo::SgoCipher;
pub use vbf::{Vbf, VbfBlock, VbfHeader};

/// Where a segment's bytes came from inside the container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentSource {
    /// Inline hex in an ODX `FLASHDATA/DATA` element.
    InlineHex,
    /// An external `.bin` ZIP member referenced by an ODX-F `DATAFILE` (named).
    ExternalBin(String),
    /// A block inside an SGO object.
    Sgo,
    /// A block from a Ford / Volvo / Mazda VBF container.
    Vbf,
    /// A `FLASH-SEGMENT` of a BMW PSdZData SWE `.bin`.
    BmwSwe,
}

/// One decoded flash segment.
#[derive(Debug, Clone)]
pub struct Segment {
    /// Container-local identifier (ODX `FLASHDATA` ID, or a synthesised SGO id).
    pub id: String,
    /// Target flash address, if the container declared one.
    pub address: Option<u32>,
    /// The segment bytes. Fully decoded when `decoded` is true; otherwise this is
    /// the best partial result (raw or decrypted-but-still-compressed).
    pub data: Vec<u8>,
    /// Compression scheme the container declared for this segment.
    pub compression: Compression,
    /// Raw method string / tag (e.g. ODX `"AA"`, or `"sgo-fmt1"`).
    pub method: String,
    /// Where the bytes came from.
    pub source: SegmentSource,
    /// True when decrypt + decompress fully succeeded.
    pub decoded: bool,
    /// Diagnostic note when a segment could only be partially decoded.
    pub note: Option<String>,
}

/// Caller-supplied key material and cipher selection. **No field is populated by
/// this crate** — every value is provided by the caller. `Default` yields an
/// all-empty, no-crypto configuration suitable for already-plaintext containers.
#[derive(Debug, Clone, Default)]
pub struct Keys {
    /// FRF container rolling-XOR key material (`frf.key`). Required only to open
    /// an encrypted `.frf`; not needed for a bare ODX/SGO or an already-unwrapped ZIP.
    pub frf_key: Option<Vec<u8>>,
    /// Block cipher for ODX/ODX-F flash blocks (Simos/DQ AES, ZF-8HP XOR, …).
    pub block_cipher: BlockCipher,
    /// 256-byte family cipher table for SGO format-1 blocks (caller-supplied).
    pub sgo_table: Option<[u8; 256]>,
    /// Which SGO format-1 cipher convention to use.
    pub sgo_cipher: SgoCipher,
}

/// The detected top-level input format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputFormat {
    /// An encrypted FRF container (needs `frf_key`).
    Frf,
    /// A raw (already-decrypted) ZIP archive.
    Zip,
    /// A bare ODX/ODX-F XML document (no ZIP wrapper, no external bins available).
    Odx,
    /// A bare SGO object.
    Sgo,
    /// A Ford / Volvo / Mazda VBF flash container (plaintext; no keys needed).
    Vbf,
    /// A BMW PSdZData SWE `.xml` descriptor. Its data lives in the matching
    /// `.bin`, so decode it with [`bmw::unpack_pair`], not [`unpack`].
    BmwSweXml,
}

/// Best-effort detection of the top-level container format from a byte prefix.
pub fn detect(bytes: &[u8]) -> InputFormat {
    if bytes.starts_with(b"SGML Object File\x00") {
        InputFormat::Sgo
    } else if bytes.starts_with(b"PK\x03\x04") {
        InputFormat::Zip
    } else if vbf::looks_like_vbf(bytes) {
        InputFormat::Vbf
    } else if bmw::looks_like_descriptor(bytes) {
        InputFormat::BmwSweXml
    } else if is_xml_odx(bytes) {
        InputFormat::Odx
    } else {
        // Encrypted FRF has no plaintext magic; treat anything else as FRF.
        InputFormat::Frf
    }
}

fn is_xml_odx(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(256)];
    let s = String::from_utf8_lossy(head);
    let t = s.trim_start();
    t.starts_with("<?xml") || t.starts_with("<ODX") || t.starts_with('<')
}

/// Decode a VAG flash container, auto-detecting FRF / ZIP / ODX / SGO.
///
/// This is the primary entry point. Supply caller-owned key material via
/// [`Keys`]; this crate bundles none. Returns the decoded [`Segment`]s plus
/// container metadata. Segments that could not be fully decoded (e.g. a missing
/// key) are still returned with `decoded == false` and a `note`.
pub fn unpack(bytes: &[u8], keys: &Keys) -> Result<Unpacked> {
    match detect(bytes) {
        InputFormat::Frf => {
            let key = keys.frf_key.as_deref().ok_or(Error::MissingKey(
                "frf_key required to open an FRF container",
            ))?;
            let zip = frf::decrypt_frf(key, bytes)?;
            frf::unpack_zip(
                &zip,
                &keys.block_cipher,
                keys.sgo_table.as_ref(),
                keys.sgo_cipher,
            )
        }
        InputFormat::Zip => frf::unpack_zip(
            bytes,
            &keys.block_cipher,
            keys.sgo_table.as_ref(),
            keys.sgo_cipher,
        ),
        InputFormat::Odx => {
            let xml = String::from_utf8_lossy(bytes);
            let has_datafile = xml.contains("DATAFILE");
            let contents = odx::parse(&xml, &keys.block_cipher, &std::collections::HashMap::new())?;
            Ok(Unpacked {
                kind: if has_datafile {
                    ContainerKind::OdxF
                } else {
                    ContainerKind::Odx
                },
                segments: contents.segments,
                box_codes: contents.box_codes,
                members: Vec::new(),
                sa2_scripts: contents.sa2_scripts,
            })
        }
        InputFormat::Sgo => {
            let parsed = sgo::parse(bytes)?;
            let mut sa2_scripts = Vec::new();
            sgo::push_sa2(&mut sa2_scripts, &parsed);
            let segments = parsed.decode(keys.sgo_table.as_ref(), keys.sgo_cipher);
            Ok(Unpacked {
                kind: ContainerKind::Sgo,
                segments,
                box_codes: Vec::new(),
                members: Vec::new(),
                sa2_scripts,
            })
        }
        // VBF is plaintext (Ford/Volvo/Mazda); no key material is consulted.
        InputFormat::Vbf => Ok(vbf::Vbf::parse(bytes)?.into_unpacked()),
        InputFormat::BmwSweXml => Err(Error::Unsupported(
            "this is a BMW SWE .xml descriptor; its flash data is in the matching .bin — \
             decode the pair (opentune_container::bmw::unpack_pair, or `opentune container \
             unpack`, which finds the sibling file)"
                .into(),
        )),
    }
}

/// Largest address span [`flatten`] will allocate (256 MiB).
pub const MAX_FLAT_SPAN: u64 = 256 * 1024 * 1024;

/// A flat memory image built from decoded segments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlatImage {
    /// Address of `data[0]` (the lowest segment address).
    pub base: u32,
    /// The image; gaps between segments hold the fill byte.
    pub data: Vec<u8>,
}

/// Place every segment at its address in one flat image, filling gaps with
/// `fill`. Refuses (rather than guesses) when a segment is not fully decoded,
/// has no confirmed address, overlaps another, or the span exceeds
/// [`MAX_FLAT_SPAN`].
pub fn flatten(segments: &[Segment], fill: u8) -> Result<FlatImage> {
    let mut placed: Vec<(u64, u64, &Segment)> = Vec::with_capacity(segments.len());
    for s in segments {
        let addr = match (s.decoded, s.address) {
            (true, Some(a)) => u64::from(a),
            _ => {
                return Err(Error::Unsupported(format!(
                    "segment {} is not flash-ready ({}); no flat image built",
                    s.id,
                    s.note.as_deref().unwrap_or(if s.decoded {
                        "no confirmed address"
                    } else {
                        "not fully decoded"
                    })
                )))
            }
        };
        if s.data.is_empty() {
            continue;
        }
        placed.push((addr, addr + s.data.len() as u64, s));
    }
    if placed.is_empty() {
        return Err(Error::Unsupported("no segment data to place".into()));
    }
    placed.sort_by_key(|p| p.0);
    for w in placed.windows(2) {
        if w[1].0 < w[0].1 {
            return Err(Error::Unsupported(format!(
                "segments {} and {} overlap; no flat image built",
                w[0].2.id, w[1].2.id
            )));
        }
    }
    let base = placed[0].0;
    let end = placed.iter().map(|p| p.1).max().unwrap_or(base);
    if end - base > MAX_FLAT_SPAN {
        return Err(Error::Unsupported(format!(
            "segments span 0x{:X} bytes (limit 0x{MAX_FLAT_SPAN:X}); no flat image built",
            end - base
        )));
    }
    let mut data = vec![fill; (end - base) as usize];
    for (a, _, s) in &placed {
        let off = (a - base) as usize;
        data[off..off + s.data.len()].copy_from_slice(&s.data);
    }
    Ok(FlatImage {
        base: base as u32,
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_formats() {
        assert_eq!(detect(b"SGML Object File\x00xxxx"), InputFormat::Sgo);
        assert_eq!(detect(b"PK\x03\x04rest"), InputFormat::Zip);
        assert_eq!(detect(b"<?xml version=\"1.0\"?><ODX/>"), InputFormat::Odx);
        assert_eq!(detect(&[0x9f, 0x3a, 0x11, 0x00]), InputFormat::Frf);
        assert_eq!(
            detect(b"<?xml version=\"1.0\"?><BINARY-FLASHBLOCK><FLASH-SEGMENTS/></BINARY-FLASHBLOCK>"),
            InputFormat::BmwSweXml
        );
    }

    fn seg(id: &str, address: Option<u32>, data: &[u8], decoded: bool) -> Segment {
        Segment {
            id: id.into(),
            address,
            data: data.to_vec(),
            compression: Compression::None,
            method: String::new(),
            source: SegmentSource::BmwSwe,
            decoded,
            note: None,
        }
    }

    #[test]
    fn flatten_places_segments_and_refuses_ambiguity() {
        let img = flatten(
            &[
                seg("b", Some(0x1008), &[3, 4], true),
                seg("a", Some(0x1000), &[1, 2], true),
            ],
            0xFF,
        )
        .unwrap();
        assert_eq!(img.base, 0x1000);
        assert_eq!(img.data, vec![1, 2, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 3, 4]);
        // Overlap, undecoded and unaddressed segments are refused.
        assert!(flatten(
            &[seg("a", Some(0), &[1, 2], true), seg("b", Some(1), &[3], true)],
            0
        )
        .is_err());
        assert!(flatten(&[seg("a", Some(0), &[1], false)], 0).is_err());
        assert!(flatten(&[seg("a", None, &[1], true)], 0).is_err());
        // Span limit.
        assert!(flatten(
            &[seg("a", Some(0), &[1], true), seg("b", Some(0x2000_0000), &[1], true)],
            0
        )
        .is_err());
    }
}
