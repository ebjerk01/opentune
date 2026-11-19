//! What the UDS module flasher writes: a [`ModuleImage`] of addressed
//! [`ModuleBlock`]s, built from a decoded flash container
//! ([`ModuleImage::from_container_bytes`], [`ModuleImage::from_unpacked`],
//! [`ModuleImage::from_vbf`]) or from a plain binary plus a layout
//! ([`ModuleImage::from_bin`]).
//!
//! Every block records how the container stored it ([`BlockEncoding`]). The
//! flasher only sends **plain** blocks, with dataFormatIdentifier `0x00`: a
//! container block that is compressed or encrypted on the wire (ODX methods
//! other than `00`, SGO blocks, a VBF with a non-zero
//! `data_format_identifier`) is refused, because the ECU expects the *encoded*
//! bytes with a matching dataFormatIdentifier, and that encoder is not
//! implemented. [`ModuleBlock::refusal`] says why.
use crate::policy::TargetIdentity;
use opentune_container::{vbf::Vbf, Compression, Keys, Segment, SegmentSource, Unpacked};
use opentune_core::{Error, MemoryRegion, Result};

/// How the container stored a block's bytes for the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockEncoding {
    /// Plain bytes, sent with dataFormatIdentifier 0x00. Implemented.
    Plain,
    /// Compressed on the wire (named scheme). Not implemented → refused.
    Compressed(String),
    /// Encrypted on the wire (named method). Not implemented → refused.
    Encrypted(String),
    /// Another container transform or an undecodable block. Refused.
    Other(String),
}

impl BlockEncoding {
    /// Whether the flasher can send this block.
    pub fn is_plain(&self) -> bool {
        matches!(self, BlockEncoding::Plain)
    }
}

/// One addressed block to program.
#[derive(Debug, Clone, PartialEq)]
pub struct ModuleBlock {
    /// Container-local id (for messages).
    pub id: String,
    /// Target address of the first byte. Meaningless when
    /// [`address_known`](Self::address_known) is false.
    pub address: u32,
    /// Whether the container declared an address.
    pub address_known: bool,
    /// The bytes to program.
    pub data: Vec<u8>,
    /// The region erased before programming: the container's erase entry that
    /// contains the block, else the block's own span.
    pub erase: MemoryRegion,
    /// Whether `erase` came from the container (or layout), rather than being
    /// the block's own span.
    pub erase_declared: bool,
    /// How the container stored the block.
    pub encoding: BlockEncoding,
    /// The container's own integrity check of the block (VBF CRC-16), if it
    /// carries one: `Some(true)` matched, `Some(false)` mismatched.
    pub container_crc_ok: Option<bool>,
}

impl ModuleBlock {
    /// A plain block at `address` whose erase region is its own span.
    pub fn plain(id: impl Into<String>, address: u32, data: Vec<u8>) -> Self {
        let erase = MemoryRegion::new(address, data.len() as u32);
        Self {
            id: id.into(),
            address,
            address_known: true,
            data,
            erase,
            erase_declared: false,
            encoding: BlockEncoding::Plain,
            container_crc_ok: None,
        }
    }

    /// The programmed span.
    pub fn region(&self) -> MemoryRegion {
        MemoryRegion::new(self.address, self.data.len() as u32)
    }

    /// Why this block cannot be flashed, or `None` when it can.
    pub fn refusal(&self) -> Option<String> {
        if !self.address_known {
            return Some("the container declares no target address".into());
        }
        if self.data.is_empty() {
            return Some("the block is empty".into());
        }
        if u64::from(self.address) + self.data.len() as u64 > u64::from(u32::MAX) + 1 {
            return Some("the block runs past the 32-bit address space".into());
        }
        match &self.encoding {
            BlockEncoding::Plain => {}
            BlockEncoding::Compressed(s) => {
                return Some(format!(
                    "compressed on the wire ({s}); only plain blocks (dataFormatIdentifier \
                     0x00) are implemented for UDS module flashing"
                ))
            }
            BlockEncoding::Encrypted(s) => {
                return Some(format!(
                    "encrypted on the wire ({s}); only plain blocks (dataFormatIdentifier \
                     0x00) are implemented for UDS module flashing"
                ))
            }
            BlockEncoding::Other(s) => return Some(s.clone()),
        }
        if self.container_crc_ok == Some(false) {
            return Some("the container's block CRC does not match its data".into());
        }
        let (bs, be) = span(self.region());
        let (es, ee) = span(self.erase);
        if bs < es || be > ee {
            return Some(format!(
                "the block is not inside its erase region 0x{:08X} +0x{:X}",
                self.erase.address, self.erase.length
            ));
        }
        None
    }
}

pub(crate) fn span(r: MemoryRegion) -> (u64, u64) {
    let s = u64::from(r.address);
    (s, s + u64::from(r.length))
}

/// The image a module flash writes.
#[derive(Debug, Clone, PartialEq)]
pub struct ModuleImage {
    /// Where it came from (`"vbf"`, `"odx"`, `"odx-f"`, `"sgo"`, `"bin"`).
    pub source: String,
    /// Identity the connected ECU must report (from the container, extended
    /// by the caller).
    pub identity: TargetIdentity,
    /// The blocks, in programming order.
    pub blocks: Vec<ModuleBlock>,
}

impl ModuleImage {
    /// Decode container bytes (VBF, ODX/ODX-F, FRF, SGO) with caller-supplied
    /// `keys`. A VBF keeps its header's erase table and
    /// `data_format_identifier`; the other formats go through
    /// [`opentune_container::unpack`].
    pub fn from_container_bytes(bytes: &[u8], keys: &Keys) -> Result<Self> {
        if opentune_container::vbf::looks_like_vbf(bytes) {
            let v = Vbf::parse(bytes).map_err(|e| Error::Config(format!("VBF: {e}")))?;
            return Ok(Self::from_vbf(&v));
        }
        let u = opentune_container::unpack(bytes, keys)
            .map_err(|e| Error::Config(format!("container: {e}")))?;
        Ok(Self::from_unpacked(&u))
    }

    /// Blocks from a parsed VBF. Erase regions come from the header's
    /// `erase = { { start, length }, … }` table; a non-zero
    /// `data_format_identifier` marks every block as not plain.
    pub fn from_vbf(v: &Vbf) -> Self {
        let erases = v
            .header
            .raw
            .get("erase")
            .map(|s| parse_erase_table(s))
            .unwrap_or_default();
        let dfi = v.header.data_format_identifier.unwrap_or(0);
        let part = v.header.sw_part_number.as_deref().unwrap_or("vbf");
        let blocks = v
            .blocks
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let mut blk = ModuleBlock::plain(
                    format!("{part}#{i}@0x{:08X}", b.start_address),
                    b.start_address,
                    b.data.clone(),
                );
                if let Some(e) = erases.iter().find(|e| {
                    let (es, ee) = span(**e);
                    let (bs, be) = span(blk.region());
                    bs >= es && be <= ee
                }) {
                    blk.erase = *e;
                    blk.erase_declared = true;
                }
                blk.container_crc_ok = Some(b.crc_ok());
                if dfi != 0 {
                    blk.encoding = BlockEncoding::Other(format!(
                        "VBF data_format_identifier 0x{dfi:02X} (compressed/encrypted per the \
                         manufacturer); only 0x00 is implemented"
                    ));
                }
                blk
            })
            .collect();
        Self {
            source: "vbf".into(),
            // A VBF names the *new* software, not what the ECU reports today:
            // the caller supplies the identity to match.
            identity: TargetIdentity::default(),
            blocks,
        }
    }

    /// Blocks from any decoded container. ODX box codes become the part
    /// numbers the ECU must report.
    pub fn from_unpacked(u: &Unpacked) -> Self {
        use opentune_container::ContainerKind;
        let source = match u.kind {
            ContainerKind::Odx => "odx",
            ContainerKind::OdxF => "odx-f",
            ContainerKind::Sgo => "sgo",
            ContainerKind::Vbf => "vbf",
            ContainerKind::BmwSwe => "bmw-swe",
        };
        let identity = TargetIdentity {
            part_numbers: if u.kind == ContainerKind::Vbf {
                Vec::new()
            } else {
                u.box_codes.clone()
            },
            sw_numbers: Vec::new(),
            label: None,
        };
        Self {
            source: source.into(),
            identity,
            blocks: u.segments.iter().map(block_from_segment).collect(),
        }
    }

    /// A plain binary `data` loaded at `base`, cut into the `layout` regions
    /// (each must lie inside the file). An empty layout makes one block of the
    /// whole file. Each layout region is also the block's erase region.
    pub fn from_bin(data: &[u8], base: u32, layout: &[MemoryRegion]) -> Result<Self> {
        let file = (u64::from(base), u64::from(base) + data.len() as u64);
        let regions: Vec<MemoryRegion> = if layout.is_empty() {
            vec![MemoryRegion::new(base, data.len() as u32)]
        } else {
            layout.to_vec()
        };
        let mut blocks = Vec::new();
        for (i, r) in regions.iter().enumerate() {
            let (s, e) = span(*r);
            if s < file.0 || e > file.1 || r.length == 0 {
                return Err(Error::Config(format!(
                    "layout region 0x{:08X} +0x{:X} is not inside the file (0x{:08X}..0x{:08X})",
                    r.address, r.length, file.0, file.1
                )));
            }
            let off = (s - file.0) as usize;
            let mut b = ModuleBlock::plain(
                format!("bin#{i}@0x{:08X}", r.address),
                r.address,
                data[off..off + r.length as usize].to_vec(),
            );
            b.erase_declared = !layout.is_empty();
            blocks.push(b);
        }
        Ok(Self {
            source: "bin".into(),
            identity: TargetIdentity::default(),
            blocks,
        })
    }
}

fn block_from_segment(s: &Segment) -> ModuleBlock {
    let mut b = ModuleBlock::plain(s.id.clone(), s.address.unwrap_or(0), s.data.clone());
    b.address_known = s.address.is_some();
    b.encoding = match &s.source {
        SegmentSource::Vbf => BlockEncoding::Plain,
        // Decoding a BMW SWE to plain bytes is supported; how those bytes go
        // over the wire (compressed or not, signature handling) is not
        // publicly documented, so the module flasher refuses them.
        SegmentSource::BmwSwe => BlockEncoding::Other(
            "BMW SWE segment: the UDS transfer encoding and signature handling are not \
             publicly documented; flashing BMW containers is not implemented"
                .into(),
        ),
        SegmentSource::Sgo => BlockEncoding::Other(format!(
            "SGO block (storage '{}'): SGO is a KWP2000-era container and its UDS wire \
             encoding is not implemented",
            s.method
        )),
        SegmentSource::InlineHex | SegmentSource::ExternalBin(_) => {
            let mut c = s.method.chars();
            let comp = c.next().unwrap_or('0');
            let enc = c.next().unwrap_or('0');
            if comp != '0' || s.compression != Compression::None {
                BlockEncoding::Compressed(format!("ODX method '{}', {:?}", s.method, s.compression))
            } else if enc != '0' {
                BlockEncoding::Encrypted(format!("ODX method '{}'", s.method))
            } else {
                BlockEncoding::Plain
            }
        }
    };
    if b.encoding.is_plain() && !s.decoded {
        b.encoding = BlockEncoding::Other(format!(
            "the container block could not be decoded{}",
            s.note
                .as_deref()
                .map(|n| format!(": {n}"))
                .unwrap_or_default()
        ));
    }
    b
}

/// Parse a VBF `erase` value (`{ { 0x1000, 0x800 }, { … } }`) into regions:
/// the numbers in order, taken as (start, length) pairs.
pub fn parse_erase_table(s: &str) -> Vec<MemoryRegion> {
    let nums: Vec<u32> = s
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .filter_map(|t| {
            let t = t.to_ascii_lowercase();
            match t.strip_prefix("0x") {
                Some(h) => u32::from_str_radix(h, 16).ok(),
                None => t.parse().ok(),
            }
        })
        .collect();
    (0..nums.len() / 2)
        .map(|k| MemoryRegion::new(nums[2 * k], nums[2 * k + 1]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn erase_table_pairs() {
        let r = parse_erase_table("{ { 0x00010000, 0x00008000 }, { 0x20000, 0x100 } }");
        assert_eq!(
            r,
            vec![
                MemoryRegion::new(0x10000, 0x8000),
                MemoryRegion::new(0x20000, 0x100)
            ]
        );
    }

    #[test]
    fn bin_layout_cuts_blocks() {
        let data: Vec<u8> = (0..=255u8).collect();
        let img = ModuleImage::from_bin(
            &data,
            0x1000,
            &[MemoryRegion::new(0x1000, 16), MemoryRegion::new(0x1080, 32)],
        )
        .unwrap();
        assert_eq!(img.blocks.len(), 2);
        assert_eq!(img.blocks[1].data, data[0x80..0xA0].to_vec());
        assert!(img.blocks[1].erase_declared);
        assert!(ModuleImage::from_bin(&data, 0x1000, &[MemoryRegion::new(0x10F0, 32)]).is_err());
    }

    #[test]
    fn odx_methods_classify() {
        let seg = |method: &str, comp: Compression| Segment {
            id: "b".into(),
            address: Some(0x8000),
            data: vec![1, 2, 3],
            compression: comp,
            method: method.into(),
            source: SegmentSource::InlineHex,
            decoded: true,
            note: None,
        };
        assert!(block_from_segment(&seg("00", Compression::None))
            .refusal()
            .is_none());
        let c = block_from_segment(&seg("A0", Compression::Lzss10));
        assert!(c.refusal().unwrap().contains("compressed"));
        let e = block_from_segment(&seg("0A", Compression::None));
        assert!(e.refusal().unwrap().contains("encrypted"));
        let mut s = seg("00", Compression::None);
        s.address = None;
        assert!(block_from_segment(&s)
            .refusal()
            .unwrap()
            .contains("address"));
    }
}
