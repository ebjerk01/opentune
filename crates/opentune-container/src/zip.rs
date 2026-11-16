//! Minimal read-only ZIP support for FRF payloads.
//!
//! An FRF container decrypts to an ordinary ZIP archive holding the ODX/ODX-F
//! document and (for the external-bin ODX-F variant) the flash `.bin` members.
//! Those archives use only STORE (method 0) and DEFLATE (method 8), so this
//! self-contained reader handles just those two, via the central directory, and
//! keeps the crate free of a heavier ZIP dependency.

use crate::error::{Error, Result};
use flate2::read::DeflateDecoder;
use std::io::Read;

/// One extracted ZIP member.
#[derive(Debug, Clone)]
pub struct ZipEntry {
    /// Member name as stored in the archive.
    pub name: String,
    /// Decompressed member bytes.
    pub data: Vec<u8>,
}

const EOCD_SIG: u32 = 0x0605_4b50;
const CDH_SIG: u32 = 0x0201_4b50;
const LFH_SIG: u32 = 0x0403_4b50;

/// Upper bound on the decompressed size of a single ZIP member. FRF/ODX flash
/// members are at most a few megabytes; this cap stops a crafted DEFLATE "zip
/// bomb" from expanding a tiny stored member into gigabytes of memory. A member
/// that inflates past this is rejected rather than allocated.
const MAX_MEMBER_SIZE: u64 = 512 * 1024 * 1024;

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// Read every member of a ZIP archive held entirely in memory.
pub fn read_all(buf: &[u8]) -> Result<Vec<ZipEntry>> {
    if buf.len() < 22 {
        return Err(Error::Zip("archive too small for an EOCD record".into()));
    }
    // The EOCD record is at the end, before an optional comment (<= 65535 bytes).
    let mut eocd = None;
    let scan_start = buf.len().saturating_sub(22 + 0xffff);
    for i in (scan_start..=buf.len() - 22).rev() {
        if u32le(buf, i) == EOCD_SIG {
            eocd = Some(i);
            break;
        }
    }
    let eocd = eocd.ok_or_else(|| Error::Zip("no End-Of-Central-Directory record".into()))?;
    let total_entries = u16le(buf, eocd + 10) as usize;
    let cd_offset = u32le(buf, eocd + 16) as usize;

    // `total_entries` is a u16 (<= 65535) so this hint cannot be abused.
    let mut entries = Vec::with_capacity(total_entries);
    let mut p = cd_offset;
    // Bounds-checked slice helper: every index into `buf` below goes through an
    // explicit `end <= buf.len()` test using checked arithmetic, so a truncated
    // or hostile header can never index past the slice (which would panic).
    for _ in 0..total_entries {
        let hdr_end = p
            .checked_add(46)
            .ok_or_else(|| Error::Zip("central directory header offset overflow".into()))?;
        if hdr_end > buf.len() || u32le(buf, p) != CDH_SIG {
            return Err(Error::Zip("malformed central directory header".into()));
        }
        let method = u16le(buf, p + 10);
        let comp_size = u32le(buf, p + 20) as usize;
        let name_len = u16le(buf, p + 28) as usize;
        let extra_len = u16le(buf, p + 30) as usize;
        let comment_len = u16le(buf, p + 32) as usize;
        let lfh_off = u32le(buf, p + 42) as usize;
        // The file name follows the 46-byte fixed header; reject if it would run
        // past the end of the buffer before slicing it.
        let name_end = hdr_end
            .checked_add(name_len)
            .filter(|&e| e <= buf.len())
            .ok_or_else(|| Error::Zip("central directory name runs past end of archive".into()))?;
        let name = String::from_utf8_lossy(&buf[hdr_end..name_end]).into_owned();

        // Locate the member data via the local file header (its name/extra lengths
        // can differ from the central directory's extra length).
        let lfh_end = lfh_off
            .checked_add(30)
            .ok_or_else(|| Error::Zip(format!("local header offset overflow for {name}")))?;
        if lfh_end > buf.len() || u32le(buf, lfh_off) != LFH_SIG {
            return Err(Error::Zip(format!("bad local header for {name}")));
        }
        let l_name_len = u16le(buf, lfh_off + 26) as usize;
        let l_extra_len = u16le(buf, lfh_off + 28) as usize;
        let data_start = lfh_end
            .checked_add(l_name_len)
            .and_then(|x| x.checked_add(l_extra_len))
            .ok_or_else(|| Error::Zip(format!("member {name} data offset overflow")))?;
        let data_end = data_start
            .checked_add(comp_size)
            .filter(|&e| e <= buf.len())
            .ok_or_else(|| Error::Zip(format!("member {name} runs past end of archive")))?;
        let raw = &buf[data_start..data_end];

        let data = match method {
            0 => raw.to_vec(),
            8 => {
                let mut out = Vec::new();
                // Cap the inflated size so a crafted member cannot exhaust memory.
                DeflateDecoder::new(raw)
                    .take(MAX_MEMBER_SIZE + 1)
                    .read_to_end(&mut out)
                    .map_err(|e| Error::Zip(format!("inflate failed for {name}: {e}")))?;
                if out.len() as u64 > MAX_MEMBER_SIZE {
                    return Err(Error::Zip(format!(
                        "member {name} inflates past the {MAX_MEMBER_SIZE}-byte limit"
                    )));
                }
                out
            }
            other => {
                return Err(Error::Zip(format!(
                    "member {name} uses unsupported compression method {other}"
                )))
            }
        };

        entries.push(ZipEntry { name, data });
        // Advance to the next central-directory header with checked arithmetic.
        p = p
            .checked_add(46)
            .and_then(|x| x.checked_add(name_len))
            .and_then(|x| x.checked_add(extra_len))
            .and_then(|x| x.checked_add(comment_len))
            .ok_or_else(|| Error::Zip("central directory cursor overflow".into()))?;
    }
    Ok(entries)
}

/// Build a minimal STORE-only ZIP archive from `(name, data)` members.
///
/// Used to round-trip FRF repacking in tests and available for callers that
/// want to re-wrap an edited ODX/SGO into a fresh container. CRC-32 is computed
/// so the output is accepted by standard ZIP tooling.
pub fn write_stored(members: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    let mut offsets = Vec::new();

    for (name, data) in members {
        let crc = crc32(data);
        let name_bytes = name.as_bytes();
        offsets.push(out.len() as u32);
        // Local file header.
        out.extend_from_slice(&LFH_SIG.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&0u16.to_le_bytes()); // method STORE
        out.extend_from_slice(&0u16.to_le_bytes()); // time
        out.extend_from_slice(&0u16.to_le_bytes()); // date
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra len
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(data);
    }

    let cd_start = out.len() as u32;
    for (i, (name, data)) in members.iter().enumerate() {
        let crc = crc32(data);
        let name_bytes = name.as_bytes();
        central.extend_from_slice(&CDH_SIG.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes()); // version made by
        central.extend_from_slice(&20u16.to_le_bytes()); // version needed
        central.extend_from_slice(&0u16.to_le_bytes()); // flags
        central.extend_from_slice(&0u16.to_le_bytes()); // method STORE
        central.extend_from_slice(&0u16.to_le_bytes()); // time
        central.extend_from_slice(&0u16.to_le_bytes()); // date
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // extra
        central.extend_from_slice(&0u16.to_le_bytes()); // comment
        central.extend_from_slice(&0u16.to_le_bytes()); // disk start
        central.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        central.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        central.extend_from_slice(&offsets[i].to_le_bytes());
        central.extend_from_slice(name_bytes);
    }
    let cd_size = central.len() as u32;
    out.extend_from_slice(&central);

    // End of central directory.
    out.extend_from_slice(&EOCD_SIG.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // disk
    out.extend_from_slice(&0u16.to_le_bytes()); // cd start disk
    out.extend_from_slice(&(members.len() as u16).to_le_bytes());
    out.extend_from_slice(&(members.len() as u16).to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_start.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // comment len
    out
}

/// Standard CRC-32 (IEEE) over `data`.
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xffff_ffff;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_roundtrip() {
        let members = vec![
            ("flash.odx".to_string(), b"<ODX>hello</ODX>".to_vec()),
            ("block_1.bin".to_string(), vec![0xabu8; 200]),
        ];
        let archive = write_stored(&members);
        let got = read_all(&archive).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].name, "flash.odx");
        assert_eq!(got[0].data, b"<ODX>hello</ODX>");
        assert_eq!(got[1].data, vec![0xabu8; 200]);
    }

    #[test]
    fn crc32_known_answer() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn rejects_too_small_for_eocd() {
        assert!(read_all(&[0u8; 10]).is_err());
        assert!(read_all(&[]).is_err());
    }

    #[test]
    fn rejects_garbage_without_eocd() {
        let buf = vec![0x55u8; 1024];
        assert!(read_all(&buf).is_err());
    }

    #[test]
    fn truncated_central_directory_name_does_not_panic() {
        // Build a valid archive, then corrupt the central-directory name length
        // so the stored name claims to extend past the end of the buffer.
        let mut archive = write_stored(&[("a.odx".to_string(), b"<ODX/>".to_vec())]);
        // Find the central directory header (CDH_SIG) and inflate its name length.
        let sig = CDH_SIG.to_le_bytes();
        let cdh = archive
            .windows(4)
            .position(|w| w == sig)
            .expect("central header");
        // name length lives at CDH + 28 (u16 LE).
        archive[cdh + 28] = 0xff;
        archive[cdh + 29] = 0xff;
        // Must be a clean Err, never a panic.
        assert!(read_all(&archive).is_err());
    }

    #[test]
    fn corrupt_local_header_offset_is_clean_error() {
        let mut archive = write_stored(&[("a.odx".to_string(), b"<ODX/>".to_vec())]);
        let sig = CDH_SIG.to_le_bytes();
        let cdh = archive
            .windows(4)
            .position(|w| w == sig)
            .expect("central header");
        // Local-header offset lives at CDH + 42 (u32 LE): point it past the end.
        archive[cdh + 42..cdh + 46].copy_from_slice(&0xffff_fffeu32.to_le_bytes());
        assert!(read_all(&archive).is_err());
    }

    #[test]
    fn huge_declared_comp_size_is_rejected_not_allocated() {
        let mut archive = write_stored(&[("a.odx".to_string(), b"<ODX/>".to_vec())]);
        let sig = CDH_SIG.to_le_bytes();
        let cdh = archive
            .windows(4)
            .position(|w| w == sig)
            .expect("central header");
        // Compressed size lives at CDH + 20 (u32 LE).
        archive[cdh + 20..cdh + 24].copy_from_slice(&0xffff_fff0u32.to_le_bytes());
        assert!(read_all(&archive).is_err());
    }
}
