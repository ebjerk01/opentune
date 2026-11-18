//! Ford / Volvo / Mazda **VBF** (Versatile Binary Format) flash containers.
//!
//! VBF is the plaintext flash file used by Ford / Volvo / Mazda module
//! programming — the files ELMConfig, FoCCCus and ForScan push to an ECU over
//! UDS. Unlike the VAG containers elsewhere in this crate it carries **no
//! encryption and no compression**, so it needs no caller-supplied key material.
//!
//! ## Layout
//!
//! ```text
//!   <ASCII header>                       // `vbf_version = X.Y;`  then  `header { ... }`
//!   [ start_addr:u32 BE | length:u32 BE | data[length] | crc16:u16 BE ] * N
//! ```
//!
//! * Each block's `crc16` is **CRC-16/CCITT-FALSE** (poly `0x1021`, init `0xFFFF`,
//!   no reflection, no final XOR) over that block's `data`.
//! * The header's `file_checksum` is the standard **CRC-32** (poly `0x04C11DB7`,
//!   reflected, init/xor `0xFFFFFFFF`) over the *entire binary section* — every
//!   block's `addr + len + data + crc16`, concatenated.
//!
//! Both were verified against 37 real Ford module VBFs (1–6 blocks each): every
//! block CRC and every `file_checksum` matched.
//!
//! This module parses and verifies VBF; it does **not** program ECUs. Writing a
//! VBF to a module (UDS `0x34`/`0x36`/`0x37` + security access) is a brick-risk
//! operation handled, when implemented, behind the flash-safety policy.

use crate::error::{Error, Result};
use crate::frf::{ContainerKind, Unpacked};
use crate::{Compression, Segment, SegmentSource};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// CRCs — const-built tables, no external deps, no unsafe.
// ---------------------------------------------------------------------------

const fn crc16_ccitt_table() -> [u16; 256] {
    let mut t = [0u16; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = (i as u16) << 8;
        let mut j = 0;
        while j < 8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
            j += 1;
        }
        t[i] = crc;
        i += 1;
    }
    t
}
static CRC16_TABLE: [u16; 256] = crc16_ccitt_table();

/// CRC-16/CCITT-FALSE (poly `0x1021`, init `0xFFFF`, no reflection, no final XOR)
/// — the per-block CRC a VBF stores after each block's data.
pub fn crc16_ccitt_false(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in data {
        let idx = ((crc >> 8) ^ b as u16) & 0xFF;
        crc = (crc << 8) ^ CRC16_TABLE[idx as usize];
    }
    crc
}

const fn crc32_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0u32;
    while i < 256 {
        let mut c = i;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[i as usize] = c;
        i += 1;
    }
    t
}
static CRC32_TABLE: [u32; 256] = crc32_table();

/// Standard CRC-32 (poly `0x04C11DB7`, reflected, init/xor `0xFFFFFFFF`) — the
/// zlib/PKZIP CRC the VBF `file_checksum` uses over the whole binary section.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        let idx = (crc ^ b as u32) & 0xFF;
        crc = (crc >> 8) ^ CRC32_TABLE[idx as usize];
    }
    crc ^ 0xFFFF_FFFF
}

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

/// The parsed ASCII header of a VBF. Common fields are typed; every raw
/// `key = value;` statement is also kept in [`VbfHeader::raw`] (value verbatim,
/// trimmed) so nothing is lost.
#[derive(Debug, Clone, Default)]
pub struct VbfHeader {
    /// `vbf_version` (e.g. `"2.3"`).
    pub vbf_version: Option<String>,
    /// `description` strings (the quoted lines of the `description = { .. }` list).
    pub description: Vec<String>,
    /// `sw_part_number` (e.g. `"BM5T-14C025-AD"`).
    pub sw_part_number: Option<String>,
    /// `sw_part_type` (e.g. `SBL`, `PBL`, `EXE`, `DATA`, `SIGCFG`, `CARCFG`).
    pub sw_part_type: Option<String>,
    /// `network` (e.g. `CAN_HS`, `CAN_MS`).
    pub network: Option<String>,
    /// `ecu_address` (diagnostic CAN address), if present.
    pub ecu_address: Option<u32>,
    /// `frame_format` (e.g. `CAN_STANDARD`, `CAN_EXTENDED`).
    pub frame_format: Option<String>,
    /// `call` (entry / start address), if present.
    pub call: Option<u32>,
    /// `file_checksum` (CRC-32 over the binary section), if present.
    pub file_checksum: Option<u32>,
    /// `data_format_identifier` (UDS compression/encryption id), if present.
    pub data_format_identifier: Option<u32>,
    /// Every `key = value;` statement, value kept verbatim (trimmed).
    pub raw: BTreeMap<String, String>,
}

/// One VBF data block: a target address, the bytes, and the stored block CRC.
#[derive(Debug, Clone)]
pub struct VbfBlock {
    /// Target flash address of the first byte.
    pub start_address: u32,
    /// Block payload (plaintext; VBF is neither compressed nor encrypted).
    pub data: Vec<u8>,
    /// The CRC-16/CCITT-FALSE stored in the file for this block.
    pub stored_crc16: u16,
}

impl VbfBlock {
    /// CRC-16/CCITT-FALSE recomputed over [`VbfBlock::data`].
    pub fn computed_crc16(&self) -> u16 {
        crc16_ccitt_false(&self.data)
    }
    /// Whether the stored block CRC matches the recomputed one.
    pub fn crc_ok(&self) -> bool {
        self.computed_crc16() == self.stored_crc16
    }
    /// Serialized form of this block as it appears in the file
    /// (`addr || len || data || crc16`, all big-endian) — used to recompute the
    /// file checksum.
    fn to_bytes(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(self.data.len() + 10);
        v.extend_from_slice(&self.start_address.to_be_bytes());
        v.extend_from_slice(&(self.data.len() as u32).to_be_bytes());
        v.extend_from_slice(&self.data);
        v.extend_from_slice(&self.stored_crc16.to_be_bytes());
        v
    }
}

/// A parsed VBF file.
#[derive(Debug, Clone)]
pub struct Vbf {
    /// The parsed header.
    pub header: VbfHeader,
    /// The data blocks, in file order.
    pub blocks: Vec<VbfBlock>,
}

impl Vbf {
    /// Parse a VBF file. Fails on a missing/!`vbf_version` magic, an unterminated
    /// `header { .. }` block, or a truncated data block.
    pub fn parse(bytes: &[u8]) -> Result<Vbf> {
        if !looks_like_vbf(bytes) {
            return Err(Error::BadMagic("VBF (expected leading `vbf_version`)"));
        }
        let bin_start = find_header_end(bytes)?;
        let header = parse_header(&bytes[..bin_start]);
        let blocks = parse_blocks(bytes, bin_start)?;
        Ok(Vbf { header, blocks })
    }

    /// Indices of blocks whose stored CRC-16 does not match their data.
    pub fn crc_errors(&self) -> Vec<usize> {
        self.blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| !b.crc_ok())
            .map(|(i, _)| i)
            .collect()
    }

    /// The binary section bytes (all blocks, `addr || len || data || crc16`),
    /// i.e. exactly what the `file_checksum` is computed over.
    pub fn binary_section(&self) -> Vec<u8> {
        let mut v = Vec::new();
        for b in &self.blocks {
            v.extend_from_slice(&b.to_bytes());
        }
        v
    }

    /// CRC-32 recomputed over the binary section.
    pub fn computed_file_checksum(&self) -> u32 {
        crc32(&self.binary_section())
    }

    /// `Some(true/false)` when the header declared a `file_checksum`, comparing it
    /// to the recomputed CRC-32; `None` when the header carried none.
    pub fn file_checksum_ok(&self) -> Option<bool> {
        self.header
            .file_checksum
            .map(|fc| fc == self.computed_file_checksum())
    }

    /// True when every block CRC matches and the `file_checksum` (if present) does.
    pub fn is_intact(&self) -> bool {
        self.crc_errors().is_empty() && self.file_checksum_ok() != Some(false)
    }

    /// One [`Segment`] per block, addressed by `start_address`. VBF blocks are
    /// plaintext, so `decoded` is always true; a block whose CRC fails carries a
    /// `note`.
    pub fn to_segments(&self) -> Vec<Segment> {
        let part = self.header.sw_part_number.as_deref().unwrap_or("vbf");
        self.blocks
            .iter()
            .enumerate()
            .map(|(i, b)| Segment {
                id: format!("{part}#{i}@0x{:08X}", b.start_address),
                address: Some(b.start_address),
                data: b.data.clone(),
                compression: Compression::None,
                method: "vbf".to_string(),
                source: SegmentSource::Vbf,
                decoded: true,
                note: (!b.crc_ok()).then(|| {
                    format!(
                        "block CRC-16 mismatch: stored 0x{:04X}, computed 0x{:04X}",
                        b.stored_crc16,
                        b.computed_crc16()
                    )
                }),
            })
            .collect()
    }

    /// Convert to the crate's common [`Unpacked`] result.
    pub fn into_unpacked(self) -> Unpacked {
        let segments = self.to_segments();
        let box_codes = self
            .header
            .sw_part_number
            .as_ref()
            .map(|p| vec![p.clone()])
            .unwrap_or_default();
        Unpacked {
            kind: ContainerKind::Vbf,
            segments,
            box_codes,
            members: Vec::new(),
            sa2_scripts: Vec::new(),
        }
    }
}

/// Whether the bytes look like a VBF (leading `vbf_version`, after optional
/// whitespace).
pub fn looks_like_vbf(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(64)];
    let s = String::from_utf8_lossy(head);
    s.trim_start().starts_with("vbf_version")
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

/// Find the end of the ASCII header: the byte just past the matching `}` of the
/// top-level `header { .. }` block, then any trailing whitespace. Comment and
/// string contents are ignored while matching braces.
fn find_header_end(bytes: &[u8]) -> Result<usize> {
    #[derive(PartialEq)]
    enum S {
        Normal,
        Line,
        Block,
        Str,
    }
    let mut st = S::Normal;
    let mut i = 0usize;
    let n = bytes.len();
    // 1. locate the `header` keyword in normal state.
    let hdr_kw = b"header";
    let mut hk = None;
    while i < n {
        let c = bytes[i];
        match st {
            S::Line => {
                if c == b'\n' {
                    st = S::Normal;
                }
            }
            S::Block => {
                if c == b'*' && i + 1 < n && bytes[i + 1] == b'/' {
                    st = S::Normal;
                    i += 1;
                }
            }
            S::Str => {
                if c == b'"' {
                    st = S::Normal;
                }
            }
            S::Normal => {
                if c == b'/' && i + 1 < n && bytes[i + 1] == b'/' {
                    st = S::Line;
                    i += 1;
                } else if c == b'/' && i + 1 < n && bytes[i + 1] == b'*' {
                    st = S::Block;
                    i += 1;
                } else if c == b'"' {
                    st = S::Str;
                } else if bytes[i..].starts_with(hdr_kw) {
                    hk = Some(i);
                    break;
                }
            }
        }
        i += 1;
    }
    let hk = hk.ok_or(Error::Unsupported("VBF header: no `header` block".into()))?;
    // 2. brace-match from the first '{' after the keyword, ignoring comments/strings.
    i = hk + hdr_kw.len();
    st = S::Normal;
    let mut depth = 0i32;
    let mut started = false;
    while i < n {
        let c = bytes[i];
        match st {
            S::Line => {
                if c == b'\n' {
                    st = S::Normal;
                }
            }
            S::Block => {
                if c == b'*' && i + 1 < n && bytes[i + 1] == b'/' {
                    st = S::Normal;
                    i += 1;
                }
            }
            S::Str => {
                if c == b'"' {
                    st = S::Normal;
                }
            }
            S::Normal => {
                if c == b'/' && i + 1 < n && bytes[i + 1] == b'/' {
                    st = S::Line;
                    i += 1;
                } else if c == b'/' && i + 1 < n && bytes[i + 1] == b'*' {
                    st = S::Block;
                    i += 1;
                } else if c == b'"' {
                    st = S::Str;
                } else if c == b'{' {
                    depth += 1;
                    started = true;
                } else if c == b'}' {
                    depth -= 1;
                    if started && depth == 0 {
                        i += 1;
                        // skip trailing whitespace/newlines to the binary section
                        while i < n && matches!(bytes[i], b'\r' | b'\n' | b'\t' | b' ') {
                            i += 1;
                        }
                        return Ok(i);
                    }
                }
            }
        }
        i += 1;
    }
    Err(Error::Unsupported(
        "VBF header: unterminated `header { .. }` block".into(),
    ))
}

/// Strip `//` line and `/* */` block comments, respecting `"` strings.
fn strip_comments(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut in_str = false;
    while i < b.len() {
        let c = b[i];
        if in_str {
            out.push(c as char);
            if c == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if c == b'"' {
            in_str = true;
            out.push('"');
            i += 1;
            continue;
        }
        if c == b'/' && i + 1 < b.len() && b[i + 1] == b'/' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
            continue;
        }
        out.push(c as char);
        i += 1;
    }
    out
}

/// Split the comment-stripped header into `(key, raw_value)` statements. A brace
/// that follows an identifier with no `=` (a block opener like `header`) is
/// dropped; a brace inside a value (`description = { .. }`) is kept.
fn statements(header: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut vdepth = 0i32;
    for ch in header.chars() {
        match ch {
            '{' => {
                if cur.contains('=') {
                    vdepth += 1;
                    cur.push('{');
                } else {
                    cur.clear(); // block opener label (e.g. `header`)
                }
            }
            '}' => {
                if vdepth > 0 {
                    vdepth -= 1;
                    cur.push('}');
                } else {
                    cur.clear(); // closing the header block
                }
            }
            ';' => {
                let s = cur.trim();
                if let Some(eq) = s.find('=') {
                    out.push((s[..eq].trim().to_string(), s[eq + 1..].trim().to_string()));
                }
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    out
}

fn parse_header(header_bytes: &[u8]) -> VbfHeader {
    let text = strip_comments(&String::from_utf8_lossy(header_bytes));
    let mut h = VbfHeader::default();
    for (k, v) in statements(&text) {
        match k.as_str() {
            "vbf_version" => h.vbf_version = Some(strip_quotes(&v)),
            "description" => h.description = extract_strings(&v),
            "sw_part_number" => h.sw_part_number = Some(strip_quotes(&v)),
            "sw_part_type" => h.sw_part_type = Some(strip_quotes(&v)),
            "network" => h.network = Some(strip_quotes(&v)),
            "ecu_address" => h.ecu_address = parse_u32(&v),
            "frame_format" => h.frame_format = Some(strip_quotes(&v)),
            "call" => h.call = parse_u32(&v),
            "file_checksum" => h.file_checksum = parse_u32(&v),
            "data_format_identifier" => h.data_format_identifier = parse_u32(&v),
            _ => {}
        }
        h.raw.insert(k, v);
    }
    h
}

fn strip_quotes(v: &str) -> String {
    v.trim().trim_matches('"').trim().to_string()
}

fn extract_strings(v: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = v.chars();
    while let Some(c) = it.next() {
        if c == '"' {
            let mut s = String::new();
            for d in it.by_ref() {
                if d == '"' {
                    break;
                }
                s.push(d);
            }
            out.push(s);
        }
    }
    out
}

fn parse_u32(v: &str) -> Option<u32> {
    let t = v.trim().trim_end_matches(';').trim();
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).ok()
    } else {
        t.parse::<u32>().ok()
    }
}

// ---------------------------------------------------------------------------
// Blocks
// ---------------------------------------------------------------------------

fn be_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn parse_blocks(bytes: &[u8], mut off: usize) -> Result<Vec<VbfBlock>> {
    let n = bytes.len();
    let mut blocks = Vec::new();
    loop {
        let remaining = n.saturating_sub(off);
        // A minimal block is 10 bytes (addr + len + 0 data + crc). Fewer than 8
        // trailing bytes = clean end (possibly padding); stop.
        if remaining < 10 {
            break;
        }
        let start_address = be_u32(bytes, off);
        let length = be_u32(bytes, off + 4) as usize;
        // A zero-length record is treated as the terminator.
        if length == 0 {
            break;
        }
        let data_start = off + 8;
        let crc_at = data_start + length;
        if crc_at + 2 > n {
            return Err(Error::Truncated {
                what: "VBF block data+crc",
                need: length + 2,
                at: data_start,
                len: n,
            });
        }
        blocks.push(VbfBlock {
            start_address,
            data: bytes[data_start..crc_at].to_vec(),
            stored_crc16: u16::from_be_bytes([bytes[crc_at], bytes[crc_at + 1]]),
        });
        off = crc_at + 2;
    }
    if blocks.is_empty() {
        return Err(Error::Truncated {
            what: "VBF: no data blocks",
            need: 10,
            at: off,
            len: n,
        });
    }
    Ok(blocks)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Standard CRC check values ("123456789").
    #[test]
    fn crc_known_vectors() {
        assert_eq!(crc16_ccitt_false(b"123456789"), 0x29B1);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    /// Build a minimal valid VBF with the given blocks and a correct file_checksum.
    fn make_vbf(part: &str, network: &str, blocks: &[(u32, &[u8])]) -> Vec<u8> {
        let mut bin = Vec::new();
        for (addr, data) in blocks {
            bin.extend_from_slice(&addr.to_be_bytes());
            bin.extend_from_slice(&(data.len() as u32).to_be_bytes());
            bin.extend_from_slice(data);
            bin.extend_from_slice(&crc16_ccitt_false(data).to_be_bytes());
        }
        let file_checksum = crc32(&bin);
        let header = format!(
            "vbf_version = 2.3;\nheader {{\n  // a comment with a brace {{ not real\n  description = {{ \"test {part}\" }};\n  sw_part_number = \"{part}\";\n  sw_part_type = SBL;\n  network = {network};\n  ecu_address = 0x720;\n  frame_format = CAN_STANDARD;\n  file_checksum = 0x{file_checksum:08X};\n}}\n"
        );
        let mut out = header.into_bytes();
        out.extend_from_slice(&bin);
        out
    }

    #[test]
    fn parse_roundtrip_and_verify() {
        let b0 = vec![0xAAu8; 32];
        let b1 = vec![0x5Au8; 48];
        let raw = make_vbf(
            "BM5T-14C025-AD",
            "CAN_MS",
            &[(0x03FF_0000, &b0), (0x0001_0000, &b1)],
        );

        assert!(looks_like_vbf(&raw));
        assert_eq!(crate::detect(&raw), crate::InputFormat::Vbf);

        let v = Vbf::parse(&raw).unwrap();
        assert_eq!(v.header.vbf_version.as_deref(), Some("2.3"));
        assert_eq!(v.header.sw_part_number.as_deref(), Some("BM5T-14C025-AD"));
        assert_eq!(v.header.sw_part_type.as_deref(), Some("SBL"));
        assert_eq!(v.header.network.as_deref(), Some("CAN_MS"));
        assert_eq!(v.header.ecu_address, Some(0x720));
        assert_eq!(v.header.description, vec!["test BM5T-14C025-AD"]);

        assert_eq!(v.blocks.len(), 2);
        assert_eq!(v.blocks[0].start_address, 0x03FF_0000);
        assert_eq!(v.blocks[0].data, b0);
        assert_eq!(v.blocks[1].start_address, 0x0001_0000);
        assert!(v.crc_errors().is_empty());
        assert_eq!(v.file_checksum_ok(), Some(true));
        assert!(v.is_intact());

        let segs = v.to_segments();
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].address, Some(0x03FF_0000));
        assert_eq!(segs[0].source, SegmentSource::Vbf);
        assert!(segs[0].decoded && segs[0].note.is_none());

        let up = v.into_unpacked();
        assert_eq!(up.kind, ContainerKind::Vbf);
        assert_eq!(up.box_codes, vec!["BM5T-14C025-AD".to_string()]);
    }

    #[test]
    fn detects_corrupt_block_crc() {
        let data = vec![0x11u8; 16];
        let mut raw = make_vbf("AB12-1234-AA", "CAN_HS", &[(0x1000, &data)]);
        // Flip a data byte after the header so the block CRC no longer matches.
        let pos = raw.len() - 4;
        raw[pos] ^= 0xFF;
        let v = Vbf::parse(&raw).unwrap();
        assert_eq!(v.crc_errors(), vec![0]);
        // The file_checksum covers the data too, so a flipped data byte breaks both.
        assert_eq!(v.file_checksum_ok(), Some(false));
        assert!(!v.is_intact());
        assert!(v.to_segments()[0].note.is_some());
    }

    #[test]
    fn rejects_non_vbf() {
        assert!(matches!(
            Vbf::parse(b"PK\x03\x04not a vbf"),
            Err(Error::BadMagic(_))
        ));
    }

    #[test]
    fn rejects_truncated_block() {
        let data = vec![0u8; 64];
        let mut raw = make_vbf("AB12-1234-AA", "CAN_HS", &[(0x1000, &data)]);
        raw.truncate(raw.len() - 20); // cut into the block data
        assert!(matches!(Vbf::parse(&raw), Err(Error::Truncated { .. })));
    }

    /// Real Ford VBFs, when available via `OPENTUNE_FIXTURES/vbf/*.vbf` (never
    /// shipped — they are vendor firmware kept on the RE volume). Every block CRC
    /// and every file_checksum must verify.
    #[test]
    #[ignore = "needs owner VBF fixtures via OPENTUNE_FIXTURES/vbf"]
    fn reference_vbf_fixtures() {
        let Ok(dir) = std::env::var("OPENTUNE_FIXTURES") else {
            return;
        };
        let vdir = std::path::Path::new(&dir).join("vbf");
        let Ok(entries) = std::fs::read_dir(&vdir) else {
            return;
        };
        let mut checked = 0;
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("vbf") {
                continue;
            }
            let bytes = std::fs::read(&p).unwrap();
            let v =
                Vbf::parse(&bytes).unwrap_or_else(|e| panic!("{}: parse failed: {e}", p.display()));
            assert!(
                v.crc_errors().is_empty(),
                "{}: block CRC errors at {:?}",
                p.display(),
                v.crc_errors()
            );
            assert_eq!(
                v.file_checksum_ok(),
                Some(true),
                "{}: file_checksum mismatch",
                p.display()
            );
            checked += 1;
        }
        assert!(checked >= 1, "no VBF fixtures under OPENTUNE_FIXTURES/vbf");
    }
}
