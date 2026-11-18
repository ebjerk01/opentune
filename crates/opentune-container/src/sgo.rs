//! SGO ("SGML Object File", v2) parsing — Simos / DQ flash objects.
//!
//! Port of the `lsdlsd88/TCU-FRF-SGO-UNPACKER` `sgo_unpack.py` reader (MIT),
//! which itself validated against calledit's `SGML_Object_File_reader`. SGO
//! members appear directly inside some FRF containers (DQ200 / DQ250 / AL-series
//! TCU flashes) instead of an ODX document.
//!
//! Block formats:
//!   * **0** — XOR-0xFF plaintext (no key needed).
//!   * **1** — a family substitution cipher. The 256-byte table is recovered,
//!     firmware-specific key material and is therefore **caller-supplied**; this
//!     crate bundles none. Two cipher conventions are supported: [`SgoCipher`].
//!
//! Header (LE u32 fields):
//! ```text
//!   0x00 "SGML Object File\0"  (17 bytes)
//!   0x11 compat (=2)        0x15 checksum       0x19 indexIDENT
//!   0x1d indexBaudrate      0x21 indexKwpRei    0x25 indexKwpAcp
//!   0x29 indexKwpSa2        0x2d indexDataBlocks
//! ```
//! Descriptor (0x19 bytes): 3B id, 1B format, 3B decomp-size, 3B erase-start,
//! 3B erase-end, 3B check-start, 3B check-end, 2B checksum, u32 payload-size.
//! Three-byte fields are big-endian.

use crate::compress::Compression;
use crate::error::{Error, Result};
use crate::{Segment, SegmentSource};

const MAGIC: &[u8] = b"SGML Object File\x00";

/// Upper bound on the number of block descriptors in an SGO container. Real
/// Simos/DQ SGO objects carry a handful (one per flash segment); this cap stops a
/// crafted block-index count from driving a quadratic blow-up where many block
/// descriptors each clone the same large payload.
const MAX_SGO_BLOCKS: usize = 4096;

/// The format-1 family cipher convention. Both take a caller-supplied table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SgoCipher {
    /// Later PQ-family "progressive" cipher (rolling 0x167 counter + feedback).
    #[default]
    Progressive,
    /// Early 02EA "feedback" cipher (previous-stored-byte feedback, no counter).
    Feedback,
}

/// Parsed SGO container.
#[derive(Debug, Clone)]
pub struct SgoFile {
    /// File name field (XOR-0xFF decoded).
    pub name: String,
    /// Version field (XOR-0xFF decoded).
    pub version: String,
    /// Compatibility word (always 2 for the supported layout).
    pub compat: u32,
    /// Header checksum word.
    pub checksum: u32,
    /// SA2 seed/key access script bytes.
    pub sa2: Vec<u8>,
    /// Raw block descriptors (undecoded), in file order.
    pub blocks: Vec<SgoBlock>,
}

/// One SGO block descriptor plus its stored payload.
#[derive(Debug, Clone)]
pub struct SgoBlock {
    /// 3-byte id / start, big-endian.
    pub id: u32,
    /// Block storage format (0 = XOR-FF plaintext, 1 = family cipher).
    pub format: u8,
    /// Declared size after decode.
    pub decomp_size: usize,
    /// (erase_start, erase_end) big-endian.
    pub erase: (u32, u32),
    /// (check_start, check_end) big-endian.
    pub check: (u32, u32),
    /// 2-byte checksum field.
    pub checksum: u16,
    /// Stored (encoded) payload bytes.
    pub data: Vec<u8>,
}

fn be24(b: &[u8]) -> u32 {
    ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn require(data: &[u8], at: usize, size: usize, what: &'static str) -> Result<()> {
    if at
        .checked_add(size)
        .map(|end| end > data.len())
        .unwrap_or(true)
    {
        return Err(Error::Truncated {
            what,
            need: size,
            at,
            len: data.len(),
        });
    }
    Ok(())
}

/// XOR-0xFF decode a NUL-terminated latin-1 string field.
fn xorff_str(b: &[u8]) -> String {
    let mut out = Vec::new();
    for &c in b {
        if c == 0 {
            break;
        }
        out.push(c ^ 0xff);
    }
    out.iter().map(|&c| c as char).collect()
}

/// Parse the SGO container structure (does not decode block payloads).
pub fn parse(data: &[u8]) -> Result<SgoFile> {
    if !data.starts_with(MAGIC) {
        return Err(Error::BadMagic("SGML Object File"));
    }
    require(data, 0, 0x31, "SGO header")?;
    let compat = u32le(data, 0x11);
    let checksum = u32le(data, 0x15);
    let i_ident = u32le(data, 0x19) as usize;
    let i_sa2 = u32le(data, 0x29) as usize;
    let i_blocks = u32le(data, 0x2d) as usize;
    if compat != 2 {
        return Err(Error::Unsupported(format!(
            "SGO compatibility {compat} (only 2 supported)"
        )));
    }

    require(data, i_ident, 269, "SGO IDENT")?;
    let name = xorff_str(&data[i_ident..i_ident + 260]);
    let version = xorff_str(&data[i_ident + 260..i_ident + 265]);

    require(data, i_sa2, 4, "SGO SA2 count")?;
    let sa2_count = u32le(data, i_sa2) as usize;
    require(data, i_sa2 + 4, sa2_count, "SGO SA2 script")?;
    let sa2 = data[i_sa2 + 4..i_sa2 + 4 + sa2_count].to_vec();

    require(data, i_blocks, 4, "SGO block index count")?;
    let nb = u32le(data, i_blocks) as usize;
    if nb > MAX_SGO_BLOCKS {
        return Err(Error::Unsupported(format!(
            "SGO declares {nb} blocks (limit {MAX_SGO_BLOCKS})"
        )));
    }
    // `nb <= MAX_SGO_BLOCKS`, so `nb * 4` cannot overflow; the require still
    // guarantees the whole index table is present before we read it.
    let index_bytes = nb
        .checked_mul(4)
        .ok_or(Error::Unsupported("SGO block index size overflow".into()))?;
    require(data, i_blocks + 4, index_bytes, "SGO block index")?;
    let mut blocks = Vec::with_capacity(nb);
    for k in 0..nb {
        let bo = u32le(data, i_blocks + 4 + k * 4) as usize;
        require(data, bo, 0x19, "SGO block descriptor")?;
        let raw = &data[bo..bo + 0x19];
        let dsize = u32le(raw, 0x15) as usize;
        require(data, bo + 0x19, dsize, "SGO block payload")?;
        blocks.push(SgoBlock {
            id: be24(&raw[0..3]),
            format: raw[3],
            decomp_size: be24(&raw[4..7]) as usize,
            erase: (be24(&raw[7..10]), be24(&raw[10..13])),
            check: (be24(&raw[13..16]), be24(&raw[16..19])),
            checksum: u16::from_be_bytes([raw[19], raw[20]]),
            data: data[bo + 0x19..bo + 0x19 + dsize].to_vec(),
        });
    }

    Ok(SgoFile {
        name,
        version,
        compat,
        checksum,
        sa2,
        blocks,
    })
}

impl SgoFile {
    /// Decode every block into [`Segment`]s. `table`/`cipher` are only needed for
    /// format-1 blocks; pass `None` to extract format-0 blocks and keep format-1
    /// blocks raw (with a note) rather than failing.
    pub fn decode(&self, table: Option<&[u8; 256]>, cipher: SgoCipher) -> Vec<Segment> {
        self.blocks
            .iter()
            .enumerate()
            .map(|(i, blk)| decode_block(i, blk, table, cipher))
            .collect()
    }
}

/// Append `sgo`'s SA2 script to `out` (skipped when empty or already listed).
pub(crate) fn push_sa2(out: &mut Vec<crate::odx::Sa2Script>, sgo: &SgoFile) {
    if sgo.sa2.is_empty() || out.iter().any(|s| s.bytes == sgo.sa2) {
        return;
    }
    out.push(crate::odx::Sa2Script {
        session: None,
        field: "SGO".to_string(),
        bytes: sgo.sa2.clone(),
    });
}

fn decode_block(
    index: usize,
    blk: &SgoBlock,
    table: Option<&[u8; 256]>,
    cipher: SgoCipher,
) -> Segment {
    let mut seg = Segment {
        id: format!("block_{index}_{:#08x}", blk.id),
        address: Some(blk.check.0),
        data: blk.data.clone(),
        compression: Compression::None,
        method: format!("sgo-fmt{}", blk.format),
        source: SegmentSource::Sgo,
        decoded: false,
        note: None,
    };

    let decoded = match blk.format {
        0 => Some(blk.data.iter().map(|b| b ^ 0xff).collect::<Vec<u8>>()),
        1 => match table {
            Some(t) => Some(match cipher {
                SgoCipher::Progressive => sgo_progressive(t, &blk.data),
                SgoCipher::Feedback => sgo_feedback(t, &blk.data),
            }),
            None => {
                seg.note = Some(
                    "format-1 block needs a caller-supplied family cipher table; kept raw".into(),
                );
                None
            }
        },
        other => {
            seg.note = Some(format!("unsupported SGO block format {other}; kept raw"));
            None
        }
    };

    if let Some(d) = decoded {
        if blk.decomp_size != 0 && d.len() != blk.decomp_size {
            seg.data = d;
            seg.note = Some(format!(
                "decoded size {} != descriptor {}; compression unsupported",
                seg.data.len(),
                blk.decomp_size
            ));
        } else {
            seg.data = d;
            seg.decoded = true;
        }
    }
    seg
}

/// SGO format-1 "progressive" substitution cipher (port of `sgo_unpack.decrypt_table`).
/// `table` is the caller-supplied 256-byte family permutation.
pub fn sgo_progressive(table: &[u8; 256], data: &[u8]) -> Vec<u8> {
    let mut offset: u32 = 0;
    let mut rolling: u32 = 0;
    let mut last: u32 = 0;
    let mut out = Vec::with_capacity(data.len());
    for &value in data {
        let plain = table[(((value ^ 0xff) as u32 + offset) & 0xff) as usize] as u32;
        rolling = (rolling + 0x167) & 0xffff;
        offset = (offset + plain + last + table[(rolling >> 8) as usize] as u32) & 0xff;
        last = plain;
        out.push(plain as u8);
    }
    out
}

/// SGO early "feedback" cipher (port of `sgo_early.decrypt_feedback`).
/// Previous stored byte starts at 0xFF for each block.
pub fn sgo_feedback(table: &[u8; 256], data: &[u8]) -> Vec<u8> {
    let mut previous: u32 = 0xff;
    let mut out = Vec::with_capacity(data.len());
    for &stored in data {
        out.push(table[((stored as u32 + previous) & 0xff) as usize]);
        previous = stored as u32;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal valid SGO with a single format-0 block for structure tests.
    fn build_sgo(payload: &[u8]) -> Vec<u8> {
        let decomp = payload.len() as u32;
        let stored: Vec<u8> = payload.iter().map(|b| b ^ 0xff).collect();

        // Layout: header(0x31) | IDENT(269) | SA2(4) | block index(4+4) | descriptor(0x19)+data
        let mut d = Vec::new();
        d.extend_from_slice(MAGIC); // 0x00..0x11
                                    // 8 u32 header fields starting at 0x11.
        let i_ident = 0x31u32;
        let i_sa2 = i_ident + 269;
        let i_blocks = i_sa2 + 4;
        let desc_off = i_blocks + 8;
        for v in [2u32, 0xdead_beef, i_ident, 0, 0, 0, i_sa2, i_blocks] {
            d.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(d.len(), 0x31);
        // IDENT: 260-byte name (XOR-FF) + 5-byte version + 4 (reader reads 269).
        // Name "TEST" XOR-0xFF encoded, then 0x00-padded (stored 0x00 is the
        // terminator the reader stops on).
        let mut ident = vec![0u8; 269];
        for (i, c) in b"TEST".iter().enumerate() {
            ident[i] = c ^ 0xff;
        }
        d.extend_from_slice(&ident);
        // SA2 count = 0.
        d.extend_from_slice(&0u32.to_le_bytes());
        // Block index: count=1, offset -> desc_off.
        d.extend_from_slice(&1u32.to_le_bytes());
        d.extend_from_slice(&desc_off.to_le_bytes());
        // Descriptor (0x19 bytes).
        let mut desc = vec![0u8; 0x19];
        desc[0..3].copy_from_slice(&[0x00, 0x80, 0x00]); // id/start be24
        desc[3] = 0; // format 0
        desc[4..7].copy_from_slice(&decomp.to_be_bytes()[1..4]); // 3B decomp size
        desc[0x15..0x19].copy_from_slice(&(stored.len() as u32).to_le_bytes());
        d.extend_from_slice(&desc);
        d.extend_from_slice(&stored);
        d
    }

    #[test]
    fn parse_and_decode_format0() {
        let payload = b"HELLO SGO WORLD".repeat(4);
        let sgo_bytes = build_sgo(&payload);
        let sgo = parse(&sgo_bytes).unwrap();
        assert_eq!(sgo.compat, 2);
        assert_eq!(sgo.name, "TEST");
        assert_eq!(sgo.blocks.len(), 1);
        let segs = sgo.decode(None, SgoCipher::Progressive);
        assert_eq!(segs.len(), 1);
        assert!(segs[0].decoded);
        assert_eq!(segs[0].data, payload);
        assert_eq!(segs[0].address, Some(0));
    }

    #[test]
    fn rejects_bad_magic() {
        assert!(parse(b"not an sgo file at all................").is_err());
    }

    #[test]
    fn rejects_truncated_header() {
        // Just the magic, nothing else.
        assert!(parse(MAGIC).is_err());
        // Magic plus a few bytes, short of the 0x31 header.
        let mut d = MAGIC.to_vec();
        d.extend_from_slice(&[0u8; 8]);
        assert!(parse(&d).is_err());
    }

    #[test]
    fn rejects_offsets_past_eof() {
        // Valid magic + header with indices pointing far past the end of input.
        let mut d = MAGIC.to_vec();
        // header fields from 0x11: compat=2, then offsets = 0xFFFF_FFFF.
        d.extend_from_slice(&2u32.to_le_bytes()); // compat
        for _ in 0..7 {
            d.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        }
        // Pad to the 0x31 header length if short.
        d.resize(0x31, 0);
        assert!(parse(&d).is_err());
    }

    #[test]
    fn rejects_absurd_block_count() {
        // Build a header whose block index count is enormous; must be rejected
        // before any allocation, not OOM.
        let mut d = MAGIC.to_vec();
        let i_ident = 0x31u32;
        let i_sa2 = i_ident + 269;
        let i_blocks = i_sa2 + 4;
        for v in [2u32, 0, i_ident, 0, 0, 0, i_sa2, i_blocks] {
            d.extend_from_slice(&v.to_le_bytes());
        }
        d.resize(i_ident as usize, 0);
        d.resize(i_sa2 as usize, 0); // IDENT region
        d.extend_from_slice(&0u32.to_le_bytes()); // SA2 count = 0
        d.extend_from_slice(&0xFFFF_FFFEu32.to_le_bytes()); // block count ~4 billion
        assert!(parse(&d).is_err());
    }
}
