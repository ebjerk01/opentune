//! Ford **As-Built** configuration data: the public line format, its per-line
//! checksum, the Ford `.ab` XML file, and the mapping between As-Built blocks and
//! the UDS data identifiers the coding engine reads and writes.
//!
//! This module carries **format only — no labels**. What a bit means in a given
//! module is not shipped (no openly licensed Ford label source was found, see
//! `docs/CODING.md`); a user-authored [`CodingDef`] supplies that, and
//! [`line_offset`] helps place a field given the usual "line, byte, mask"
//! notation of As-Built tables.
//!
//! ## Format (public sources)
//!
//! An As-Built line is written `MMM-BB-LL DDDD DDDD DDCC`:
//!
//! * `MMM` — the module's 11-bit diagnostic request id in hex (e.g. `726`);
//! * `BB` — the block number (1-based), `LL` — the line within the block
//!   (1-based), both read as hex here;
//! * up to five data bytes, then one checksum byte `CC`.
//!
//! The checksum is the low byte of the sum of the module id's two bytes, the
//! block number, the line number and the line's data bytes. Sources: AsBuilt
//! Browser `src/components/ChecksumCalculator.tsx` + its test (MIT,
//! github.com/carp3/AsBuilt-Browser @ bbcff56) and apim-asbuilt-decode
//! `src/asbuilt.py` `checksum()` (GPL-3.0, github.com/consp/apim-asbuilt-decode
//! @ 32ac874); both compute the same sum.
//!
//! Block `n` is UDS DID `0xDE00 + n - 1`, and its bytes are the block's lines
//! concatenated five bytes at a time (apim-asbuilt-decode: "Block 1 (7D0-01 or
//! DE00)", and its reader concatenates the `DE..` records of an XML export in
//! order). That mapping is implemented and synthetic-tested only.
//!
//! The `.ab` file (Ford's XML export of a vehicle's As-Built data) holds
//! `<DATA LABEL="MMM-BB-LL"><CODE>hhhh</CODE>…</DATA>` records under
//! `AS_BUILT_DATA/VEHICLE/…_MODULE`; the CODE words concatenated are the data
//! bytes followed by the checksum byte (AsBuilt Browser `src/types.ts` and its
//! tests; apim-asbuilt-decode `src/asbuilt.py`).
//!
//! **Label radix.** apim-asbuilt-decode writes `BB`/`LL` as hex (`%02X`);
//! AsBuilt Browser reads them as decimal. The two agree for 1–9. This module
//! reads them as hex; a checksum mismatch on a line whose block or line is ≥ 10
//! may be that ambiguity and is reported, never silently "fixed".

use std::collections::BTreeMap;
use std::fmt;

use crate::{Coding, CodingBlock, CodingDef, CodingError, Result};

/// UDS DID of As-Built block 1; block `n` is `AS_BUILT_DID_BASE + n - 1`.
pub const AS_BUILT_DID_BASE: u16 = 0xDE00;

/// Data bytes per As-Built line (the checksum byte is extra).
pub const LINE_DATA_BYTES: usize = 5;

fn fmt_err(msg: impl Into<String>) -> CodingError {
    CodingError::Format(msg.into())
}

/// The `MMM-BB-LL` address of one As-Built line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AsBuiltLabel {
    /// Module request id (`0x726` for `726-…`), at most `0xFFF`.
    pub module: u16,
    /// Block number, 1-based.
    pub block: u8,
    /// Line within the block, 1-based.
    pub line: u8,
}

impl AsBuiltLabel {
    /// Parse `MMM-BB-LL` (hex fields; `MMM` up to three digits, `BB`/`LL` one or
    /// two). Block and line must be at least 1.
    pub fn parse(s: &str) -> Result<Self> {
        let parts: Vec<&str> = s.trim().split('-').collect();
        let [m, b, l] = parts.as_slice() else {
            return Err(fmt_err(format!("`{s}` is not an MMM-BB-LL As-Built label")));
        };
        let hex = |t: &str, max_digits: usize, what: &str| -> Result<u16> {
            if t.is_empty() || t.len() > max_digits || !t.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(fmt_err(format!("`{s}`: bad {what} `{t}`")));
            }
            u16::from_str_radix(t, 16).map_err(|e| fmt_err(format!("`{s}`: {e}")))
        };
        let module = hex(m, 3, "module id")?;
        let block = hex(b, 2, "block")? as u8;
        let line = hex(l, 2, "line")? as u8;
        if block == 0 || line == 0 {
            return Err(fmt_err(format!("`{s}`: block and line start at 1")));
        }
        Ok(AsBuiltLabel {
            module,
            block,
            line,
        })
    }

    /// The UDS DID holding this line's block.
    pub fn did(&self) -> u16 {
        block_did(self.block)
    }
}

impl fmt::Display for AsBuiltLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:03X}-{:02X}-{:02X}",
            self.module, self.block, self.line
        )
    }
}

/// The UDS DID of As-Built block `block` (1-based). Block 0 does not exist;
/// [`AsBuiltLabel::parse`] rejects it and it is clamped to block 1 here.
pub fn block_did(block: u8) -> u16 {
    AS_BUILT_DID_BASE + u16::from(block.max(1)) - 1
}

/// The As-Built block held by `did`, if it is in the As-Built DID range
/// (`0xDE00..=0xDEFE`).
pub fn did_block(did: u16) -> Option<u8> {
    if (AS_BUILT_DID_BASE..AS_BUILT_DID_BASE + 0xFF).contains(&did) {
        Some((did - AS_BUILT_DID_BASE + 1) as u8)
    } else {
        None
    }
}

/// Byte offset inside a block's DID data of byte `byte_in_line` (0-based) of
/// line `line` (1-based) — for placing a [`crate::CodingParam`] from an As-Built
/// table that names a line, a byte and a mask (see [`crate::mask_field`]).
/// `None` for line 0 or a byte past the line.
pub fn line_offset(line: u8, byte_in_line: usize) -> Option<usize> {
    if line == 0 || byte_in_line >= LINE_DATA_BYTES {
        return None;
    }
    Some((line as usize - 1) * LINE_DATA_BYTES + byte_in_line)
}

/// The As-Built line checksum: low byte of module-id bytes + block + line +
/// data bytes.
pub fn checksum(label: AsBuiltLabel, data: &[u8]) -> u8 {
    let mut sum = u32::from(label.module >> 8) + u32::from(label.module & 0xFF);
    sum += u32::from(label.block) + u32::from(label.line);
    sum += data.iter().map(|&b| u32::from(b)).sum::<u32>();
    (sum & 0xFF) as u8
}

/// One As-Built line: its label, 1–5 data bytes and the stored checksum byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsBuiltLine {
    /// The line's address.
    pub label: AsBuiltLabel,
    /// The data bytes (1..=5).
    pub data: Vec<u8>,
    /// The checksum byte as stored (see [`AsBuiltLine::is_valid`]).
    pub checksum: u8,
}

impl AsBuiltLine {
    /// A line with a freshly computed checksum.
    pub fn new(label: AsBuiltLabel, data: Vec<u8>) -> Self {
        let checksum = checksum(label, &data);
        AsBuiltLine {
            label,
            data,
            checksum,
        }
    }

    /// The checksum this line should carry.
    pub fn expected_checksum(&self) -> u8 {
        checksum(self.label, &self.data)
    }

    /// Whether the stored checksum matches.
    pub fn is_valid(&self) -> bool {
        self.checksum == self.expected_checksum()
    }

    /// Build a line from the label and the raw bytes *including* the trailing
    /// checksum byte (as `.ab` CODE words and text lines carry them). The
    /// checksum is kept as stored, not verified.
    pub fn from_raw(label: AsBuiltLabel, raw: &[u8]) -> Result<Self> {
        if raw.len() < 2 || raw.len() > LINE_DATA_BYTES + 1 {
            return Err(fmt_err(format!(
                "{label}: {} byte(s); a line is 1-{LINE_DATA_BYTES} data bytes plus a checksum",
                raw.len()
            )));
        }
        let (data, cs) = raw.split_at(raw.len() - 1);
        Ok(AsBuiltLine {
            label,
            data: data.to_vec(),
            checksum: cs[0],
        })
    }

    /// Parse a text line `MMM-BB-LL hhhh hhhh hhhh` (hex digits may be grouped
    /// or not; a `:` after the label is allowed). The checksum is kept as
    /// stored — check [`AsBuiltLine::is_valid`].
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        let (lab, rest) = text
            .split_once(|c: char| c.is_whitespace() || c == ':')
            .ok_or_else(|| fmt_err(format!("`{text}`: no data after the label")))?;
        let label = AsBuiltLabel::parse(lab)?;
        let raw = parse_hex_digits(rest).map_err(|e| fmt_err(format!("{label}: {e}")))?;
        Self::from_raw(label, &raw)
    }

    /// Format as `MMM-BB-LL hhhh hhhh hhhh` (data then checksum, in 2-byte
    /// words; a final odd byte stands alone).
    pub fn to_text(&self) -> String {
        let mut bytes = self.data.clone();
        bytes.push(self.checksum);
        let words: Vec<String> = bytes
            .chunks(2)
            .map(|c| c.iter().map(|b| format!("{b:02X}")).collect())
            .collect();
        format!("{} {}", self.label, words.join(" "))
    }
}

fn parse_hex_digits(s: &str) -> std::result::Result<Vec<u8>, String> {
    let digits: String = s
        .chars()
        .filter(|c| !c.is_whitespace() && *c != ':')
        .collect();
    if digits.is_empty() || digits.len() % 2 != 0 {
        return Err(format!("`{s}` is not whole hex bytes"));
    }
    (0..digits.len())
        .step_by(2)
        .map(|i| {
            digits
                .get(i..i + 2)
                .and_then(|d| u8::from_str_radix(d, 16).ok())
                .ok_or_else(|| format!("`{s}` is not hex"))
        })
        .collect()
}

/// Parse As-Built text: one line per row, blank rows and rows starting with
/// `;` or `#` ignored. Checksums are kept as stored.
pub fn parse_text(text: &str) -> Result<Vec<AsBuiltLine>> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with(';') && !l.starts_with('#'))
        .map(AsBuiltLine::parse)
        .collect()
}

/// What [`parse_ab_xml`] found in a Ford `.ab` file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AbFile {
    /// Every As-Built line, in file order (checksums kept as stored).
    pub lines: Vec<AsBuiltLine>,
    /// `DATA` records that were not As-Built lines, with the reason (never
    /// dropped silently).
    pub skipped: Vec<(String, String)>,
}

/// Read a Ford `.ab` XML file: every `DATA` element with a `LABEL` attribute
/// and `CODE` children, anywhere under the root `AS_BUILT_DATA`. Nothing else
/// (VIN, part numbers) is kept.
pub fn parse_ab_xml(xml: &str) -> Result<AbFile> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| fmt_err(format!(".ab XML: {e}")))?;
    let root = doc.root_element();
    if root.tag_name().name() != "AS_BUILT_DATA" {
        return Err(fmt_err(format!(
            ".ab XML: root element is <{}>, expected <AS_BUILT_DATA>",
            root.tag_name().name()
        )));
    }
    let mut out = AbFile::default();
    for data in root
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "DATA")
    {
        let Some(label_text) = data.attribute("LABEL") else {
            continue;
        };
        let label = match AsBuiltLabel::parse(label_text) {
            Ok(l) => l,
            Err(e) => {
                out.skipped.push((label_text.to_string(), e.to_string()));
                continue;
            }
        };
        let hex: String = data
            .children()
            .filter(|n| n.is_element() && n.tag_name().name() == "CODE")
            .filter_map(|n| n.text())
            .collect();
        let line = parse_hex_digits(&hex)
            .map_err(|e| fmt_err(format!("{label}: {e}")))
            .and_then(|raw| AsBuiltLine::from_raw(label, &raw));
        match line {
            Ok(l) => out.lines.push(l),
            Err(e) => out.skipped.push((label_text.to_string(), e.to_string())),
        }
    }
    Ok(out)
}

/// Join one module's lines into per-block DID data (`DID -> bytes`). Every line
/// must carry a valid checksum, lines of a block must run 1..=n without gaps or
/// duplicates, and every line but the last must hold five data bytes.
pub fn blocks_by_did(module: u16, lines: &[AsBuiltLine]) -> Result<BTreeMap<u16, Vec<u8>>> {
    let mut by_block: BTreeMap<u8, BTreeMap<u8, &AsBuiltLine>> = BTreeMap::new();
    for l in lines.iter().filter(|l| l.label.module == module) {
        if !l.is_valid() {
            return Err(fmt_err(format!(
                "{}: checksum {:02X}, expected {:02X}",
                l.label,
                l.checksum,
                l.expected_checksum()
            )));
        }
        if by_block
            .entry(l.label.block)
            .or_default()
            .insert(l.label.line, l)
            .is_some()
        {
            return Err(fmt_err(format!("{}: duplicate line", l.label)));
        }
    }
    let mut out = BTreeMap::new();
    for (block, ls) in by_block {
        let n = ls.len();
        let mut bytes = Vec::with_capacity(n * LINE_DATA_BYTES);
        for (i, (line_no, l)) in ls.into_iter().enumerate() {
            if usize::from(line_no) != i + 1 {
                return Err(fmt_err(format!(
                    "{:03X}-{block:02X}: line {:02X} present but line {:02X} missing",
                    module,
                    line_no,
                    i + 1
                )));
            }
            if i + 1 < n && l.data.len() != LINE_DATA_BYTES {
                return Err(fmt_err(format!(
                    "{}: short line ({} bytes) before the block's last line",
                    l.label,
                    l.data.len()
                )));
            }
            bytes.extend_from_slice(&l.data);
        }
        out.insert(block_did(block), bytes);
    }
    Ok(out)
}

/// Build the [`Coding`] for `def` (whose DIDs are As-Built DIDs) from one
/// module's As-Built lines, in the definition's block order — e.g. to decode a
/// saved `.ab` file offline. A DID the lines do not cover is an error.
pub fn to_coding(def: &CodingDef, module: u16, lines: &[AsBuiltLine]) -> Result<Coding> {
    let mut map = blocks_by_did(module, lines)?;
    let mut blocks = Vec::new();
    for &did in def.addressing.dids() {
        let bytes = map.remove(&did).ok_or_else(|| {
            fmt_err(format!(
                "no As-Built lines for block 0x{did:04X} of module {module:03X}"
            ))
        })?;
        blocks.push(CodingBlock { did, bytes });
    }
    Ok(Coding { blocks })
}

/// Render a module's coding (As-Built DIDs) as As-Built lines with fresh
/// checksums — e.g. to save what was read from a module in the familiar form.
pub fn from_coding(module: u16, coding: &Coding) -> Result<Vec<AsBuiltLine>> {
    if module > 0xFFF {
        return Err(fmt_err(format!("module id 0x{module:X} exceeds 0xFFF")));
    }
    let mut out = Vec::new();
    for b in &coding.blocks {
        let block = did_block(b.did)
            .ok_or_else(|| fmt_err(format!("DID 0x{:04X} is not an As-Built block DID", b.did)))?;
        for (i, chunk) in b.bytes.chunks(LINE_DATA_BYTES).enumerate() {
            let line = u8::try_from(i + 1)
                .map_err(|_| fmt_err(format!("block 0x{:04X} has too many lines", b.did)))?;
            out.push(AsBuiltLine::new(
                AsBuiltLabel {
                    module,
                    block,
                    line,
                },
                chunk.to_vec(),
            ));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_matches_the_published_vector() {
        // AsBuilt Browser ChecksumCalculator.test.tsx (MIT): 726-02-01 with
        // 1E1E 0000 00.. -> checksum 6C.
        let label = AsBuiltLabel::parse("726-02-01").unwrap();
        assert_eq!(checksum(label, &[0x1E, 0x1E, 0, 0, 0]), 0x6C);
        let line = AsBuiltLine::parse("726-02-01 1E1E 0000 006C").unwrap();
        assert!(line.is_valid());
        assert_eq!(line.data, vec![0x1E, 0x1E, 0, 0, 0]);
        assert_eq!(line.to_text(), "726-02-01 1E1E 0000 006C");
        // Same line, ungrouped and with a colon.
        assert_eq!(AsBuiltLine::parse("726-02-01: 1E1E0000006C").unwrap(), line);
        // A one-byte line: 706-01-01 00 -> 07+06+01+01 = 0F.
        let short = AsBuiltLine::parse("706-01-01 000F").unwrap();
        assert!(short.is_valid() && short.data == vec![0]);
    }

    #[test]
    fn bad_checksum_and_bad_labels_are_reported() {
        let l = AsBuiltLine::parse("726-02-01 1E1E 0000 006D").unwrap();
        assert!(!l.is_valid());
        assert_eq!(l.expected_checksum(), 0x6C);
        for bad in [
            "726-02",
            "726-00-01",
            "7260-01-01",
            "72G-01-01",
            "726-01-001",
        ] {
            assert!(AsBuiltLabel::parse(bad).is_err(), "{bad}");
        }
        assert!(AsBuiltLine::parse("726-01-01").is_err()); // no data
        assert!(AsBuiltLine::parse("726-01-01 12").is_err()); // checksum only
        assert!(AsBuiltLine::parse("726-01-01 1122334455667788").is_err()); // 7 data bytes
        assert!(AsBuiltLine::parse("726-01-01 123").is_err()); // odd nibbles
    }

    #[test]
    fn blocks_map_to_de_dids() {
        assert_eq!(block_did(1), 0xDE00);
        assert_eq!(block_did(0x10), 0xDE0F);
        assert_eq!(did_block(0xDE00), Some(1));
        assert_eq!(did_block(0xDEFE), Some(0xFF));
        assert_eq!(did_block(0xDEFF), None);
        assert_eq!(did_block(0x0600), None);
        assert_eq!(line_offset(1, 0), Some(0));
        assert_eq!(line_offset(2, 4), Some(9));
        assert_eq!(line_offset(0, 0), None);
        assert_eq!(line_offset(1, 5), None);
    }

    #[test]
    fn lines_round_trip_through_dids() {
        // Synthetic module 0x7A0, block 1 = 7 bytes (two lines), block 2 = 5 bytes.
        let coding = Coding {
            blocks: vec![
                CodingBlock {
                    did: 0xDE00,
                    bytes: vec![1, 2, 3, 4, 5, 6, 7],
                },
                CodingBlock {
                    did: 0xDE01,
                    bytes: vec![0xAA; 5],
                },
            ],
        };
        let lines = from_coding(0x7A0, &coding).unwrap();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1].label.to_string(), "7A0-01-02");
        assert_eq!(lines[1].data, vec![6, 7]);
        assert!(lines.iter().all(AsBuiltLine::is_valid));
        let map = blocks_by_did(0x7A0, &lines).unwrap();
        assert_eq!(map[&0xDE00], vec![1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(map[&0xDE01], vec![0xAA; 5]);
        // Lines of another module are ignored.
        assert!(blocks_by_did(0x726, &lines).unwrap().is_empty());
        // A gap, a duplicate, a bad checksum and a short middle line are errors.
        assert!(blocks_by_did(0x7A0, &[lines[1].clone()]).is_err());
        assert!(blocks_by_did(0x7A0, &[lines[0].clone(), lines[0].clone()]).is_err());
        let mut bad = lines.clone();
        bad[2].checksum ^= 1;
        assert!(blocks_by_did(0x7A0, &bad).is_err());
        let short_first = vec![
            AsBuiltLine::new(AsBuiltLabel::parse("7A0-01-01").unwrap(), vec![1]),
            lines[1].clone(),
        ];
        assert!(blocks_by_did(0x7A0, &short_first).is_err());
        // Non-As-Built DIDs cannot be rendered as lines.
        let other = Coding {
            blocks: vec![CodingBlock {
                did: 0x0600,
                bytes: vec![0],
            }],
        };
        assert!(from_coding(0x7A0, &other).is_err());
    }

    #[test]
    fn reads_an_ab_xml_file() {
        // Synthetic .ab file in the published structure; checksums computed
        // with the formula above.
        let ok = AsBuiltLine::new(
            AsBuiltLabel::parse("7A0-01-01").unwrap(),
            vec![0x12, 0x34, 0x56, 0x78, 0x9A],
        );
        let raw: Vec<u8> = ok.data.iter().copied().chain([ok.checksum]).collect();
        let words: Vec<String> = raw
            .chunks(2)
            .map(|c| c.iter().map(|b| format!("{b:02X}")).collect())
            .collect();
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<AS_BUILT_DATA><VEHICLE VIN="TEST-NOT-A-VIN">
  <NODEID>07A0</NODEID>
  <BCE_MODULE>
    <DATA LABEL="7A0-01-01"><CODE>{}</CODE><CODE>{}</CODE><CODE>{}</CODE></DATA>
    <DATA LABEL="7A0-01-02"><CODE>12</CODE></DATA>
    <DATA LABEL="NOT-A-LABEL"><CODE>0000</CODE></DATA>
  </BCE_MODULE>
</VEHICLE></AS_BUILT_DATA>"#,
            words[0], words[1], words[2]
        );
        let ab = parse_ab_xml(&xml).unwrap();
        assert_eq!(ab.lines, vec![ok]);
        assert_eq!(ab.skipped.len(), 2); // checksum-only line + bad label
        assert!(parse_ab_xml("<OTHER/>").is_err());
        assert!(parse_ab_xml("not xml").is_err());
    }
}
