//! Generic, ECU-agnostic checksum examples.
use crate::{crc32, BlockStatus, ChecksumAlgo, Fix};
use opentune_core::{Error, MemoryRegion, Result};

/// The last 4 bytes of the image hold a little-endian CRC32 of everything before.
pub struct Crc32Tail;

impl ChecksumAlgo for Crc32Tail {
    fn id(&self) -> &str {
        "crc32-tail"
    }

    fn verify(&self, image: &[u8]) -> Result<Vec<BlockStatus>> {
        if image.len() < 5 {
            return Err(Error::Config("image too small".into()));
        }
        let split = image.len() - 4;
        let computed = crc32(&image[..split]) as u64;
        let stored =
            u32::from_le_bytes([image[split], image[split + 1], image[split + 2], image[split + 3]])
                as u64;
        Ok(vec![BlockStatus {
            name: "crc32-tail".into(),
            region: MemoryRegion::new(0, split as u32),
            stored,
            computed,
            ok: stored == computed,
        }])
    }

    fn correct(&self, image: &mut [u8]) -> Result<Vec<Fix>> {
        if image.len() < 5 {
            return Err(Error::Config("image too small".into()));
        }
        let split = image.len() - 4;
        let computed = crc32(&image[..split]);
        let old = u32::from_le_bytes([
            image[split],
            image[split + 1],
            image[split + 2],
            image[split + 3],
        ]);
        let mut fixes = Vec::new();
        if old != computed {
            image[split..].copy_from_slice(&computed.to_le_bytes());
            fixes.push(Fix {
                name: "crc32-tail".into(),
                offset: split as u32,
                old: old as u64,
                new: computed as u64,
            });
        }
        Ok(fixes)
    }
}
