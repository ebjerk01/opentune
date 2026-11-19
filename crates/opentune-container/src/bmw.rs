//! BMW **PSdZData SWE** flash containers (read-only).
//!
//! E-Sys / ISTA-P program F/G-series control units from the *PSdZData* tree,
//! where every SWE (SoftWare-Einheit) is a file pair under
//! `psdzdata/swe/<class>/`:
//!
//! ```text
//!   swfl_00005f9f.xml.142_011_020   descriptor: identity, signature/checksum
//!                                   metadata and the FLASH-SEGMENT table
//!   swfl_00005f9f.bin.142_011_020   the segment data the table points into
//! ```
//!
//! Name: `<class>_<8-hex id>.<kind>.<v>_<v>_<v>`. Process classes seen with
//! `.bin` + `.xml` pairs: `btld` (bootloader), `swfl` (software), `swfk`, `ibad`.
//! `cafd` (coding data) files are `.caf` and are **not** decoded: no public
//! description of their format exists.
//!
//! ## Descriptor (`.xml`)
//!
//! A `BINARY-FLASHBLOCK` with a `SHORT-NAME`, `SDGS/SDG` groups (captions
//! `Ident`, `SignatureTable`, `Checksum`, each a list of `SD SI=".."` values)
//! and `FLASH-SEGMENTS/FLASH-SEGMENT` entries:
//!
//! ```text
//!   <FLASH-SEGMENT COMPRESSION-STATUS="UNCOMPRESSED|COMPRESSED">
//!     SOURCE-START-ADDRESS / SOURCE-END-ADDRESS   byte offsets into the .bin
//!     TARGET-START-ADDRESS / TARGET-END-ADDRESS   ECU addresses
//!     (optional) SHORT-NAME, LONG-NAME, COMPRESSION-METHOD (gz | nrv ...), CHECKSUM
//! ```
//!
//! Addresses are hex text (with or without `0x`), end addresses inclusive.
//! Elements are matched by local name: some files carry a default namespace.
//!
//! ## What is decoded, what is refused
//!
//! * `UNCOMPRESSED` → stored bytes; source and target ranges must be equal in length.
//! * `COMPRESSED` with no method, or `nrv`/`ucl`/`nrv2b` → UCL NRV2B
//!   ([`crate::nrv2b`]); `gz`/`gzip` → gzip.
//! * The decoded length is compared with the target range. Public tools
//!   disagree on that relation for compressed segments, so a mismatch is
//!   reported and the segment is **not** marked flash-ready.
//! * Any other status or method is refused (reported, bytes kept raw). There is
//!   no public description of an encrypted segment form; nothing is bypassed.
//! * The `SignatureTable` (e.g. RSA-2048 / SHA-256) and `Checksum` (e.g. CRC16)
//!   metadata is reported **unverified**: which bytes the CRC covers, its
//!   parameters, and where the signature lives are not publicly documented.
//!
//! Sources and their licences: `docs/BMW-CONTAINERS.md` and `docs/PRIOR-ART.md`.
//! Validation state: **synthetic-tested** only (no real BMW file was used).

use crate::error::{Error, Result};
use crate::frf::{ContainerKind, Unpacked};
use crate::{Compression, Segment, SegmentSource};

/// Per-segment output cap for NRV2B / gzip (bounds a decompression bomb).
pub const MAX_SEGMENT_OUTPUT: usize = 256 * 1024 * 1024;

/// A PSdZData SWE process class (the `<class>` part of the file name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessClass {
    /// `btld` — bootloader.
    Btld,
    /// `swfl` — software (flash).
    Swfl,
    /// `swfk` — a flash-container class with the same `.bin` + `.xml` layout.
    Swfk,
    /// `ibad` — a flash-container class with the same `.bin` + `.xml` layout.
    Ibad,
    /// `cafd` — coding data (`.caf`); identified only, never decoded.
    Cafd,
    /// Any other class name, kept verbatim (lower-case).
    Other(String),
}

impl ProcessClass {
    /// Parse a class name (case-insensitive).
    pub fn parse(s: &str) -> ProcessClass {
        match s.to_ascii_lowercase().as_str() {
            "btld" => ProcessClass::Btld,
            "swfl" => ProcessClass::Swfl,
            "swfk" => ProcessClass::Swfk,
            "ibad" => ProcessClass::Ibad,
            "cafd" => ProcessClass::Cafd,
            other => ProcessClass::Other(other.to_string()),
        }
    }

    /// The lower-case class name.
    pub fn as_str(&self) -> &str {
        match self {
            ProcessClass::Btld => "btld",
            ProcessClass::Swfl => "swfl",
            ProcessClass::Swfk => "swfk",
            ProcessClass::Ibad => "ibad",
            ProcessClass::Cafd => "cafd",
            ProcessClass::Other(s) => s,
        }
    }

    /// One-line description for reports.
    pub fn describe(&self) -> &'static str {
        match self {
            ProcessClass::Btld => "bootloader (flash container)",
            ProcessClass::Swfl => "software (flash container)",
            ProcessClass::Swfk | ProcessClass::Ibad => "flash container (.bin + .xml)",
            ProcessClass::Cafd => {
                "coding data (.caf) — format not publicly documented, not decoded"
            }
            ProcessClass::Other(_) => "unrecognised process class",
        }
    }
}

/// A parsed SWE file name: `<class>_<id>.<kind>.<v1>_<v2>_<v3>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweName {
    /// Process class.
    pub class: ProcessClass,
    /// The class text exactly as written in the name (case kept).
    pub class_raw: String,
    /// Identifier, hex digits exactly as written (normally 8).
    pub id: String,
    /// File kind: `bin`, `xml`, `caf`, … (lower-case).
    pub kind: String,
    /// Version text exactly as written, e.g. `142_011_020`.
    pub version: String,
    /// The three version fields as numbers.
    pub version_fields: [u32; 3],
}

impl SweName {
    /// Parse a file name (not a path). `None` when it does not follow the pattern.
    pub fn parse(file_name: &str) -> Option<SweName> {
        let mut parts = file_name.splitn(3, '.');
        let stem = parts.next()?;
        let kind = parts.next()?;
        let version = parts.next()?;
        let (class_raw, id) = stem.split_once('_')?;
        if class_raw.is_empty()
            || !class_raw.chars().all(|c| c.is_ascii_alphabetic())
            || id.is_empty()
            || id.len() > 16
            || !id.chars().all(|c| c.is_ascii_hexdigit())
            || kind.is_empty()
            || !kind.chars().all(|c| c.is_ascii_alphanumeric())
        {
            return None;
        }
        let mut fields = [0u32; 3];
        let mut n = 0;
        for f in version.split('_') {
            if n == 3 || f.is_empty() || f.len() > 6 || !f.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            fields[n] = f.parse().ok()?;
            n += 1;
        }
        if n != 3 {
            return None;
        }
        Some(SweName {
            class: ProcessClass::parse(class_raw),
            class_raw: class_raw.to_string(),
            id: id.to_string(),
            kind: kind.to_ascii_lowercase(),
            version: version.to_string(),
            version_fields: fields,
        })
    }

    /// The file name of the same SWE with another kind (`bin` ↔ `xml`).
    pub fn sibling(&self, kind: &str) -> String {
        format!("{}_{}.{}.{}", self.class_raw, self.id, kind, self.version)
    }
}

/// One `SDG` group of the descriptor: its caption and `SD` values, verbatim.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sdg {
    /// `SDG-CAPTION/SHORT-NAME` (e.g. `Ident`, `SignatureTable`, `Checksum`).
    pub caption: String,
    /// `(SI, text)` pairs in document order.
    pub values: Vec<(String, String)>,
}

impl Sdg {
    /// The first value with this `SI` name.
    pub fn get(&self, si: &str) -> Option<&str> {
        self.values
            .iter()
            .find(|(k, _)| k == si)
            .map(|(_, v)| v.as_str())
    }
}

/// A segment's `COMPRESSION-STATUS` attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompressionStatus {
    /// `UNCOMPRESSED` (also assumed when the attribute is absent).
    Uncompressed,
    /// `COMPRESSED`.
    Compressed,
    /// Anything else, verbatim — refused.
    Other(String),
}

/// How a segment's bytes are turned into target data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentCodec {
    /// Stored bytes.
    Stored,
    /// UCL NRV2B (8-bit).
    Nrv2b,
    /// gzip.
    Gzip,
}

/// One `FLASH-SEGMENT` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlashSegment {
    /// 0-based position in the descriptor.
    pub index: usize,
    /// Optional `SHORT-NAME`.
    pub short_name: Option<String>,
    /// Optional `LONG-NAME`.
    pub long_name: Option<String>,
    /// First byte of the segment in the `.bin`.
    pub source_start: u64,
    /// Last byte (inclusive) of the segment in the `.bin`.
    pub source_end: u64,
    /// First target (ECU) address.
    pub target_start: u64,
    /// Last target address (inclusive).
    pub target_end: u64,
    /// `COMPRESSION-STATUS`.
    pub status: CompressionStatus,
    /// Optional `COMPRESSION-METHOD` text.
    pub method: Option<String>,
    /// Optional per-segment `CHECKSUM` text (reported, not verified).
    pub checksum: Option<String>,
}

impl FlashSegment {
    /// Length of the source range in bytes.
    pub fn source_len(&self) -> u64 {
        self.source_end - self.source_start + 1
    }

    /// Length of the target range in bytes.
    pub fn target_len(&self) -> u64 {
        self.target_end - self.target_start + 1
    }

    /// The codec this entry asks for, or why it is refused.
    pub fn codec(&self) -> std::result::Result<SegmentCodec, String> {
        let method = self
            .method
            .as_deref()
            .map(|m| m.trim().to_ascii_lowercase())
            .filter(|m| !m.is_empty());
        match (&self.status, method.as_deref()) {
            (CompressionStatus::Uncompressed, None | Some("none")) => Ok(SegmentCodec::Stored),
            (CompressionStatus::Uncompressed, Some(m)) => Err(format!(
                "UNCOMPRESSED segment names compression method '{m}' — contradictory, refused"
            )),
            (CompressionStatus::Compressed, None | Some("nrv" | "ucl" | "nrv2b")) => {
                Ok(SegmentCodec::Nrv2b)
            }
            (CompressionStatus::Compressed, Some("gz" | "gzip")) => Ok(SegmentCodec::Gzip),
            (CompressionStatus::Compressed, Some(m)) => Err(format!(
                "compression method '{m}' is not publicly documented — not decoded"
            )),
            (CompressionStatus::Other(s), _) => Err(format!(
                "COMPRESSION-STATUS '{s}' is not publicly documented (encrypted?) — not decoded"
            )),
        }
    }

    /// A container-local id: `seg00`, `seg01_<name>`, …
    pub fn id(&self) -> String {
        let name = self
            .short_name
            .as_deref()
            .or(self.long_name.as_deref())
            .map(str::trim)
            .filter(|n| !n.is_empty());
        match name {
            Some(n) => format!("seg{:02}_{n}", self.index),
            None => format!("seg{:02}", self.index),
        }
    }
}

/// A parsed `.xml` descriptor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweDescriptor {
    /// `BINARY-FLASHBLOCK/SHORT-NAME`, if present.
    pub short_name: Option<String>,
    /// Every `SDG` group, verbatim.
    pub sdgs: Vec<Sdg>,
    /// The `FLASH-SEGMENT` table in document order.
    pub segments: Vec<FlashSegment>,
}

impl SweDescriptor {
    /// The `SDG` group with this caption.
    pub fn sdg(&self, caption: &str) -> Option<&Sdg> {
        self.sdgs.iter().find(|g| g.caption == caption)
    }

    /// `Ident`: (process class, identifier, version) as declared.
    pub fn ident(&self) -> Option<(Option<&str>, Option<&str>, Option<&str>)> {
        self.sdg("Ident")
            .map(|g| (g.get("ProcessClass"), g.get("Identifier"), g.get("Version")))
    }

    /// Human-readable notes on the signature/checksum metadata (always
    /// "not verified") and on target-range overlaps.
    pub fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if let Some(g) = self.sdg("SignatureTable") {
            let field = |k: &str| g.get(k).unwrap_or("?").to_string();
            notes.push(format!(
                "SignatureTable: status {}, mode {}, key length {}, hash {} — signature NOT \
                 verified (its location and keys are not publicly documented)",
                field("SignatureStatus"),
                field("SignatureMode"),
                field("SignatureKeyLength"),
                field("SignatureHashMode"),
            ));
        }
        if let Some(g) = self.sdg("Checksum") {
            notes.push(format!(
                "Checksum: {} = {} — NOT verified (coverage and CRC parameters are not \
                 publicly documented)",
                g.get("Mode").unwrap_or("?"),
                g.get("Value").unwrap_or("?"),
            ));
        }
        for (a, b) in self.overlapping_targets() {
            notes.push(format!(
                "target ranges of {} and {} overlap",
                self.segments[a].id(),
                self.segments[b].id()
            ));
        }
        notes
    }

    /// Index pairs of segments whose target ranges overlap.
    pub fn overlapping_targets(&self) -> Vec<(usize, usize)> {
        let mut pairs = Vec::new();
        for (i, a) in self.segments.iter().enumerate() {
            for (j, b) in self.segments.iter().enumerate().skip(i + 1) {
                if a.target_start <= b.target_end && b.target_start <= a.target_end {
                    pairs.push((i, j));
                }
            }
        }
        pairs
    }
}

/// True when `bytes` look like a SWE `.xml` descriptor (an XML document naming
/// `BINARY-FLASHBLOCK` or `FLASH-SEGMENTS` near its start).
pub fn looks_like_descriptor(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(16 * 1024)];
    let s = String::from_utf8_lossy(head);
    s.trim_start_matches('\u{feff}')
        .trim_start()
        .starts_with('<')
        && (s.contains("BINARY-FLASHBLOCK") || s.contains("FLASH-SEGMENTS"))
}

fn parse_hex_addr(what: &str, text: &str) -> Result<u64> {
    let t = text.trim();
    let t = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    if t.is_empty() || t.len() > 16 || !t.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::BadHex(format!("{what}: '{}'", text.trim())));
    }
    u64::from_str_radix(t, 16).map_err(|e| Error::BadHex(format!("{what}: {e}")))
}

fn child<'a, 'i>(n: roxmltree::Node<'a, 'i>, name: &str) -> Option<roxmltree::Node<'a, 'i>> {
    n.children()
        .find(|c| c.is_element() && c.tag_name().name() == name)
}

fn child_text(n: roxmltree::Node, name: &str) -> Option<String> {
    child(n, name)
        .and_then(|c| c.text())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// Parse a SWE `.xml` descriptor.
pub fn parse_descriptor(xml: &str) -> Result<SweDescriptor> {
    let doc = roxmltree::Document::parse(xml.trim_start_matches('\u{feff}'))
        .map_err(|e| Error::Xml(e.to_string()))?;
    let mut desc = SweDescriptor::default();

    if let Some(block) = doc
        .descendants()
        .find(|n| n.is_element() && n.tag_name().name() == "BINARY-FLASHBLOCK")
    {
        desc.short_name = child_text(block, "SHORT-NAME");
    }

    for g in doc
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "SDG")
    {
        let caption = child(g, "SDG-CAPTION")
            .and_then(|c| child_text(c, "SHORT-NAME"))
            .unwrap_or_default();
        let values = g
            .children()
            .filter(|c| c.is_element() && c.tag_name().name() == "SD")
            .map(|sd| {
                (
                    sd.attribute("SI").unwrap_or_default().to_string(),
                    sd.text().unwrap_or_default().trim().to_string(),
                )
            })
            .collect();
        desc.sdgs.push(Sdg { caption, values });
    }

    for (index, s) in doc
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "FLASH-SEGMENT")
        .enumerate()
    {
        let addr = |name: &str| -> Result<u64> {
            let text = child_text(s, name)
                .ok_or_else(|| Error::Xml(format!("FLASH-SEGMENT #{index} has no {name}")))?;
            parse_hex_addr(&format!("FLASH-SEGMENT #{index} {name}"), &text)
        };
        let source_start = addr("SOURCE-START-ADDRESS")?;
        let source_end = addr("SOURCE-END-ADDRESS")?;
        let target_start = addr("TARGET-START-ADDRESS")?;
        let target_end = addr("TARGET-END-ADDRESS")?;
        if source_end < source_start || target_end < target_start {
            return Err(Error::Xml(format!(
                "FLASH-SEGMENT #{index} has a descending address range"
            )));
        }
        let status = match s
            .attributes()
            .find(|a| a.name() == "COMPRESSION-STATUS")
            .map(|a| a.value().trim().to_ascii_uppercase())
        {
            None => CompressionStatus::Uncompressed,
            Some(v) if v == "UNCOMPRESSED" => CompressionStatus::Uncompressed,
            Some(v) if v == "COMPRESSED" => CompressionStatus::Compressed,
            Some(v) => CompressionStatus::Other(v),
        };
        desc.segments.push(FlashSegment {
            index,
            short_name: child_text(s, "SHORT-NAME"),
            long_name: child_text(s, "LONG-NAME"),
            source_start,
            source_end,
            target_start,
            target_end,
            status,
            method: child_text(s, "COMPRESSION-METHOD"),
            checksum: child_text(s, "CHECKSUM"),
        });
    }

    if desc.segments.is_empty() {
        return Err(Error::Xml(
            "no FLASH-SEGMENT entries: not a SWE flash-container descriptor".into(),
        ));
    }
    Ok(desc)
}

/// A decoded SWE pair.
#[derive(Debug, Clone)]
pub struct BmwSwe {
    /// The parsed descriptor.
    pub descriptor: SweDescriptor,
    /// One [`Segment`] per `FLASH-SEGMENT`, in descriptor order.
    pub segments: Vec<Segment>,
    /// Container-level notes: unverified signature/checksum, overlaps, …
    pub notes: Vec<String>,
}

impl BmwSwe {
    /// Convert into the crate-wide [`Unpacked`] result.
    pub fn into_unpacked(self) -> Unpacked {
        Unpacked {
            kind: ContainerKind::BmwSwe,
            segments: self.segments,
            box_codes: Vec::new(),
            members: Vec::new(),
            sa2_scripts: Vec::new(),
        }
    }
}

/// Decode a SWE pair: the `.xml` descriptor text and the `.bin` bytes.
///
/// Malformed XML is an error. Per-segment problems (range outside the `.bin`,
/// undocumented status/method, decode failure, length that does not match the
/// target range) are not: the segment comes back with `decoded == false` and a
/// `note`, so one bad entry does not hide the others.
pub fn unpack_pair(xml: &str, bin: &[u8]) -> Result<BmwSwe> {
    let descriptor = parse_descriptor(xml)?;
    let segments = descriptor
        .segments
        .iter()
        .map(|fs| decode_segment(fs, bin))
        .collect();
    let notes = descriptor.notes();
    Ok(BmwSwe {
        descriptor,
        segments,
        notes,
    })
}

fn decode_segment(fs: &FlashSegment, bin: &[u8]) -> Segment {
    let status = match &fs.status {
        CompressionStatus::Uncompressed => "UNCOMPRESSED".to_string(),
        CompressionStatus::Compressed => "COMPRESSED".to_string(),
        CompressionStatus::Other(s) => s.clone(),
    };
    let method = match &fs.method {
        Some(m) => format!("{status}/{m}"),
        None => status,
    };
    let address = u32::try_from(fs.target_start).ok();
    let mut seg = Segment {
        id: fs.id(),
        address,
        data: Vec::new(),
        compression: Compression::None,
        method,
        source: SegmentSource::BmwSwe,
        decoded: false,
        note: None,
    };

    let in_bin = usize::try_from(fs.source_end)
        .ok()
        .filter(|&end| end < bin.len());
    let Some(end) = in_bin else {
        seg.note = Some(format!(
            "source range 0x{:X}..=0x{:X} lies outside the {}-byte .bin",
            fs.source_start,
            fs.source_end,
            bin.len()
        ));
        return seg;
    };
    let raw = &bin[fs.source_start as usize..=end];

    let codec = match fs.codec() {
        Ok(c) => c,
        Err(why) => {
            seg.data = raw.to_vec();
            seg.note = Some(format!("{why}; raw source bytes kept"));
            return seg;
        }
    };
    seg.compression = match codec {
        SegmentCodec::Stored => Compression::None,
        SegmentCodec::Nrv2b => Compression::Nrv2b,
        SegmentCodec::Gzip => Compression::Gzip,
    };
    let decoded = match codec {
        SegmentCodec::Stored => Ok(raw.to_vec()),
        SegmentCodec::Nrv2b => crate::nrv2b::decompress_nrv2b_8(raw, MAX_SEGMENT_OUTPUT),
        SegmentCodec::Gzip => crate::compress::decompress_gzip(raw, MAX_SEGMENT_OUTPUT),
    };
    match decoded {
        Err(e) => {
            seg.data = raw.to_vec();
            seg.note = Some(format!("decode failed ({e}); raw source bytes kept"));
        }
        Ok(data) => {
            let target_len = fs.target_len();
            seg.data = data;
            if seg.data.len() as u64 != target_len {
                seg.note = Some(format!(
                    "decoded {} bytes but the target range is {} bytes; placement is \
                     ambiguous (public tools disagree on this relation for compressed \
                     segments)",
                    seg.data.len(),
                    target_len
                ));
            } else if address.is_none() || fs.target_end > u64::from(u32::MAX) {
                seg.address = None;
                seg.note = Some(format!(
                    "target range 0x{:X}..=0x{:X} does not fit a 32-bit address",
                    fs.target_start, fs.target_end
                ));
            } else {
                seg.decoded = true;
            }
        }
    }
    seg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        let n = SweName::parse("swfl_00005f9f.bin.142_011_020").unwrap();
        assert_eq!(n.class, ProcessClass::Swfl);
        assert_eq!(n.id, "00005f9f");
        assert_eq!(n.kind, "bin");
        assert_eq!(n.version_fields, [142, 11, 20]);
        assert_eq!(n.sibling("xml"), "swfl_00005f9f.xml.142_011_020");
        let c = SweName::parse("cafd_00004146.caf.003_037_007").unwrap();
        assert_eq!(c.class, ProcessClass::Cafd);
        assert_eq!(
            SweName::parse("BTLD_0000ABCD.xml.001_002_003")
                .unwrap()
                .class,
            ProcessClass::Btld
        );
        for bad in [
            "swfl_00005f9f.bin",
            "swfl_xyz.bin.001_002_003",
            "swfl00005f9f.bin.001_002_003",
            "swfl_00005f9f.bin.001_002",
            "swfl_00005f9f.bin.001_002_003_004",
            "flash.frf",
        ] {
            assert!(SweName::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn codec_rules() {
        let mut fs = FlashSegment {
            index: 0,
            short_name: None,
            long_name: None,
            source_start: 0,
            source_end: 0,
            target_start: 0,
            target_end: 0,
            status: CompressionStatus::Uncompressed,
            method: None,
            checksum: None,
        };
        assert_eq!(fs.codec(), Ok(SegmentCodec::Stored));
        fs.method = Some("gz".into());
        assert!(fs.codec().is_err());
        fs.status = CompressionStatus::Compressed;
        assert_eq!(fs.codec(), Ok(SegmentCodec::Gzip));
        fs.method = None;
        assert_eq!(fs.codec(), Ok(SegmentCodec::Nrv2b));
        fs.method = Some("NRV".into());
        assert_eq!(fs.codec(), Ok(SegmentCodec::Nrv2b));
        fs.method = Some("lzma".into());
        assert!(fs.codec().is_err());
        fs.status = CompressionStatus::Other("ENCRYPTED".into());
        fs.method = None;
        assert!(fs.codec().is_err());
    }

    #[test]
    fn hex_addresses() {
        assert_eq!(parse_hex_addr("a", "0057FFD0").unwrap(), 0x57FFD0);
        assert_eq!(parse_hex_addr("a", " 0x10 ").unwrap(), 0x10);
        assert!(parse_hex_addr("a", "").is_err());
        assert!(parse_hex_addr("a", "12G").is_err());
    }
}
