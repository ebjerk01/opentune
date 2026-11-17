//! Intel HEX reader and writer.
//!
//! Each record is `:LLAAAATT[DD…]CC` in ASCII hex:
//! * `LL` — data byte count,
//! * `AAAA` — 16-bit load offset,
//! * `TT` — record type,
//! * `CC` — checksum (two's-complement of the sum of every prior byte).
//!
//! Record types handled:
//! * `00` data,
//! * `01` end-of-file (mandatory terminator),
//! * `02` extended segment address (`USBA << 4` base, 20-bit addressing),
//! * `03` start segment address (`CS:IP`, preserved),
//! * `04` extended linear address (`ULBA << 16` base, 32-bit addressing),
//! * `05` start linear address (preserved).
//!
//! Effective addresses therefore span 16, 20 and 32 bits via the extension
//! records. Every record's checksum and length are validated; a missing type-01
//! record is a [`ImageError::Truncated`].

use crate::error::{ImageError, Result};
use crate::sparse::{Overlap, SparseImage};

/// A parsed Intel HEX file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HexFile {
    /// The decoded data.
    pub image: SparseImage,
    /// The entry point, if the file carried a start record.
    pub start: Option<StartAddress>,
}

/// An Intel HEX entry-point record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartAddress {
    /// Type-05 start linear address (32-bit `EIP`).
    Linear(u32),
    /// Type-03 start segment address (`CS:IP`).
    Segment {
        /// Code segment.
        cs: u16,
        /// Instruction pointer.
        ip: u16,
    },
}

/// Parse an Intel HEX file into a [`HexFile`].
///
/// Data records are merged into the returned [`SparseImage`] with
/// [`Overlap::Error`], so a file that writes the same address twice is rejected
/// rather than silently resolved.
///
/// # Errors
/// [`ImageError::Record`] / [`ImageError::Checksum`] for malformed or
/// mis-checksummed records, and [`ImageError::Truncated`] if the terminating
/// type-01 record is absent.
pub fn read(input: &str) -> Result<HexFile> {
    let mut image = SparseImage::new();
    let mut start = None;
    let mut base: u64 = 0;
    let mut saw_eof = false;

    for (lineno, raw) in input.lines().enumerate() {
        let line = lineno + 1;
        let rec = raw.trim();
        if rec.is_empty() {
            continue;
        }
        let body = rec
            .strip_prefix(':')
            .ok_or_else(|| ImageError::Record {
                line,
                reason: "record does not start with ':'".into(),
            })?;
        if saw_eof {
            return Err(ImageError::Record {
                line,
                reason: "record follows the end-of-file record".into(),
            });
        }
        let raw_bytes = decode_hex(body, line)?;
        // Minimum record is LL + AAAA + TT + CC = 5 bytes.
        if raw_bytes.len() < 5 {
            return Err(ImageError::Record {
                line,
                reason: "record too short".into(),
            });
        }
        let count = raw_bytes[0] as usize;
        if raw_bytes.len() != count + 5 {
            return Err(ImageError::Record {
                line,
                reason: format!(
                    "byte count {count} disagrees with record length {}",
                    raw_bytes.len()
                ),
            });
        }
        verify_checksum(&raw_bytes, line)?;

        let offset = u16::from_be_bytes([raw_bytes[1], raw_bytes[2]]) as u64;
        let rtype = raw_bytes[3];
        let data = &raw_bytes[4..4 + count];

        match rtype {
            0x00 => {
                image.insert(base + offset, data, Overlap::Error)?;
            }
            0x01 => {
                if count != 0 {
                    return Err(ImageError::Record {
                        line,
                        reason: "end-of-file record carries data".into(),
                    });
                }
                saw_eof = true;
            }
            0x02 => {
                if count != 2 {
                    return Err(ImageError::Record {
                        line,
                        reason: "extended segment address record must be 2 bytes".into(),
                    });
                }
                let usba = u16::from_be_bytes([data[0], data[1]]) as u64;
                base = usba << 4;
            }
            0x03 => {
                if count != 4 {
                    return Err(ImageError::Record {
                        line,
                        reason: "start segment address record must be 4 bytes".into(),
                    });
                }
                start = Some(StartAddress::Segment {
                    cs: u16::from_be_bytes([data[0], data[1]]),
                    ip: u16::from_be_bytes([data[2], data[3]]),
                });
            }
            0x04 => {
                if count != 2 {
                    return Err(ImageError::Record {
                        line,
                        reason: "extended linear address record must be 2 bytes".into(),
                    });
                }
                let ulba = u16::from_be_bytes([data[0], data[1]]) as u64;
                base = ulba << 16;
            }
            0x05 => {
                if count != 4 {
                    return Err(ImageError::Record {
                        line,
                        reason: "start linear address record must be 4 bytes".into(),
                    });
                }
                start = Some(StartAddress::Linear(u32::from_be_bytes([
                    data[0], data[1], data[2], data[3],
                ])));
            }
            other => {
                return Err(ImageError::Record {
                    line,
                    reason: format!("unsupported record type {other:#04x}"),
                });
            }
        }
    }

    if !saw_eof {
        return Err(ImageError::Truncated {
            reason: "missing Intel HEX end-of-file (type 01) record".into(),
        });
    }
    Ok(HexFile { image, start })
}

/// Options for [`write`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteOptions {
    /// Maximum data bytes per record (1..=255). Common values are 16 and 32.
    pub record_len: u8,
}

impl Default for WriteOptions {
    fn default() -> Self {
        Self { record_len: 16 }
    }
}

/// Serialise a [`HexFile`] to canonical Intel HEX text (lines joined by `\n`,
/// trailing newline included).
///
/// Addresses above 64 KiB are expressed with type-04 extended-linear records,
/// emitted whenever the upper 16 bits change; no record crosses a 64 KiB
/// boundary. A start record is emitted when present. All addresses must fit 32
/// bits.
///
/// # Errors
/// [`ImageError::AddressWidth`] if any segment reaches beyond the 32-bit address
/// space Intel HEX can encode.
pub fn write(file: &HexFile, opts: WriteOptions) -> Result<String> {
    let rec_len = opts.record_len.max(1) as u64;
    let mut out = String::new();
    // The reader starts with a zero base, so a leading `high == 0` needs no
    // type-04 record; one is emitted only when the upper 16 bits change.
    let mut cur_high: u16 = 0;

    for seg in file.image.segments() {
        let end = seg.end();
        if end > u64::from(u32::MAX) + 1 {
            return Err(ImageError::AddressWidth {
                address: end - 1,
                max_bits: 32,
            });
        }
        let mut addr = seg.address;
        while addr < end {
            let high = (addr >> 16) as u16;
            if high != cur_high {
                emit_record(&mut out, 0, 0x04, &high.to_be_bytes());
                cur_high = high;
            }
            // Clamp this record so it neither exceeds `rec_len` nor crosses the
            // 64 KiB boundary of the current extended-linear base.
            let next_boundary = (u64::from(high) + 1) << 16;
            let chunk_end = (addr + rec_len).min(end).min(next_boundary);
            let off = (addr - seg.address) as usize;
            let len = (chunk_end - addr) as usize;
            emit_record(
                &mut out,
                (addr & 0xFFFF) as u16,
                0x00,
                &seg.bytes[off..off + len],
            );
            addr = chunk_end;
        }
    }

    match file.start {
        Some(StartAddress::Linear(eip)) => emit_record(&mut out, 0, 0x05, &eip.to_be_bytes()),
        Some(StartAddress::Segment { cs, ip }) => {
            let mut d = [0u8; 4];
            d[..2].copy_from_slice(&cs.to_be_bytes());
            d[2..].copy_from_slice(&ip.to_be_bytes());
            emit_record(&mut out, 0, 0x03, &d);
        }
        None => {}
    }

    // End-of-file.
    emit_record(&mut out, 0, 0x01, &[]);
    Ok(out)
}

/// Format and append one record, computing its checksum.
fn emit_record(out: &mut String, offset: u16, rtype: u8, data: &[u8]) {
    use core::fmt::Write as _;
    let count = data.len() as u8;
    let off = offset.to_be_bytes();
    let mut sum = count
        .wrapping_add(off[0])
        .wrapping_add(off[1])
        .wrapping_add(rtype);
    let _ = write!(out, ":{count:02X}{offset:04X}{rtype:02X}");
    for &b in data {
        let _ = write!(out, "{b:02X}");
        sum = sum.wrapping_add(b);
    }
    let checksum = sum.wrapping_neg();
    let _ = writeln!(out, "{checksum:02X}");
}

/// Decode an ASCII-hex record body (after the `:`) into bytes.
fn decode_hex(body: &str, line: usize) -> Result<Vec<u8>> {
    let bytes = body.as_bytes();
    if bytes.len() % 2 != 0 {
        return Err(ImageError::Record {
            line,
            reason: "record has an odd number of hex digits".into(),
        });
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        let hi = nibble(bytes[i], line)?;
        let lo = nibble(bytes[i + 1], line)?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

/// One hex nibble.
fn nibble(c: u8, line: usize) -> Result<u8> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(ImageError::Record {
            line,
            reason: format!("'{}' is not a hex digit", c as char),
        }),
    }
}

/// Validate the trailing two's-complement checksum byte of a raw record.
fn verify_checksum(raw: &[u8], line: usize) -> Result<()> {
    let (body, &[found]) = raw.split_at(raw.len() - 1) else {
        unreachable!("record length checked by caller");
    };
    let sum = body.iter().fold(0u8, |a, &b| a.wrapping_add(b));
    let expected = sum.wrapping_neg();
    if expected == found {
        Ok(())
    } else {
        Err(ImageError::Checksum {
            line,
            expected: found,
            found: expected,
        })
    }
}
