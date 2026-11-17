//! Motorola S-record (SREC) reader and writer.
//!
//! Each record is `S<t><CC><ADDR><DD…><SS>` in ASCII hex:
//! * `t` — record type digit,
//! * `CC` — byte count of everything after it (address + data + checksum),
//! * `ADDR` — 16/24/32-bit load address (width set by the type),
//! * `SS` — checksum (ones-complement of the low byte of the sum of the count,
//!   address and data bytes).
//!
//! Record types handled:
//! * `S0` header (preserved as raw bytes),
//! * `S1`/`S2`/`S3` data with 16/24/32-bit addresses,
//! * `S5`/`S6` record counts (16/24-bit, validated against the data seen),
//! * `S7`/`S8`/`S9` start address (32/24/16-bit, preserved).
//!
//! Every record's count and checksum are validated; a file without any
//! S7/S8/S9 termination record is a [`ImageError::Truncated`].

use crate::error::{ImageError, Result};
use crate::sparse::{Overlap, SparseImage};

/// A parsed SREC file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SRecFile {
    /// The raw bytes of the S0 header record, if present.
    pub header: Option<Vec<u8>>,
    /// The decoded data.
    pub image: SparseImage,
    /// The entry point from the S7/S8/S9 termination record, if present.
    pub start: Option<u32>,
}

/// The address width (and therefore data record type) used when writing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressWidth {
    /// 16-bit addresses: S1 data, S9 termination.
    Bits16,
    /// 24-bit addresses: S2 data, S8 termination.
    Bits24,
    /// 32-bit addresses: S3 data, S7 termination.
    Bits32,
}

impl AddressWidth {
    /// Number of address bytes this width serialises.
    fn addr_bytes(self) -> usize {
        match self {
            AddressWidth::Bits16 => 2,
            AddressWidth::Bits24 => 3,
            AddressWidth::Bits32 => 4,
        }
    }

    /// The narrowest width that can hold every address in `image`.
    fn fit(image: &SparseImage) -> Result<Self> {
        let Some((_, end)) = image.span() else {
            return Ok(AddressWidth::Bits16);
        };
        let max = end - 1;
        if max <= 0xFFFF {
            Ok(AddressWidth::Bits16)
        } else if max <= 0xFF_FFFF {
            Ok(AddressWidth::Bits24)
        } else if max <= 0xFFFF_FFFF {
            Ok(AddressWidth::Bits32)
        } else {
            Err(ImageError::AddressWidth {
                address: max,
                max_bits: 32,
            })
        }
    }
}

/// Parse an SREC file into a [`SRecFile`].
///
/// Data records are merged with [`Overlap::Error`], so a file that writes the
/// same address twice is rejected rather than silently resolved.
///
/// # Errors
/// [`ImageError::Record`] / [`ImageError::Checksum`] for malformed or
/// mis-checksummed records, and [`ImageError::Truncated`] if no S7/S8/S9
/// termination record is present.
pub fn read(input: &str) -> Result<SRecFile> {
    let mut out = SRecFile::default();
    let mut data_records: u64 = 0;
    let mut declared_count: Option<u64> = None;
    let mut saw_start = false;

    for (lineno, raw) in input.lines().enumerate() {
        let line = lineno + 1;
        let rec = raw.trim();
        if rec.is_empty() {
            continue;
        }
        let bytes = rec.as_bytes();
        if bytes[0] != b'S' {
            return Err(ImageError::Record {
                line,
                reason: "record does not start with 'S'".into(),
            });
        }
        if bytes.len() < 4 {
            return Err(ImageError::Record {
                line,
                reason: "record too short".into(),
            });
        }
        let kind = bytes[1];
        // Everything after the type digit is hex: CC + payload + checksum.
        let raw_bytes = decode_hex(&rec[2..], line)?;
        if raw_bytes.is_empty() {
            return Err(ImageError::Record {
                line,
                reason: "record has no count byte".into(),
            });
        }
        let count = raw_bytes[0] as usize;
        if raw_bytes.len() != count + 1 {
            return Err(ImageError::Record {
                line,
                reason: format!(
                    "count byte {count} disagrees with record length {}",
                    raw_bytes.len() - 1
                ),
            });
        }
        verify_checksum(&raw_bytes, line)?;

        // `count` covers address + data + the 1 checksum byte.
        let addr_len = match kind {
            b'0' | b'1' | b'5' | b'9' => 2,
            b'2' | b'6' | b'8' => 3,
            b'3' | b'7' => 4,
            b'4' => {
                return Err(ImageError::Record {
                    line,
                    reason: "reserved S4 record".into(),
                })
            }
            other => {
                return Err(ImageError::Record {
                    line,
                    reason: format!("unknown record type 'S{}'", other as char),
                })
            }
        };
        if count < addr_len + 1 {
            return Err(ImageError::Record {
                line,
                reason: "count too small for the record's address field".into(),
            });
        }
        let addr = be_addr(&raw_bytes[1..1 + addr_len]);
        let payload = &raw_bytes[1 + addr_len..raw_bytes.len() - 1];

        match kind {
            b'0' => out.header = Some(payload.to_vec()),
            b'1' | b'2' | b'3' => {
                out.image.insert(addr, payload, Overlap::Error)?;
                data_records += 1;
            }
            b'5' | b'6' => declared_count = Some(addr),
            b'7' | b'8' | b'9' => {
                out.start = Some(addr as u32);
                saw_start = true;
            }
            _ => unreachable!("address width table rejects other kinds"),
        }
    }

    if let Some(expected) = declared_count {
        if expected != data_records {
            return Err(ImageError::Record {
                line: 0,
                reason: format!(
                    "S5/S6 count {expected} disagrees with {data_records} data records"
                ),
            });
        }
    }
    if !saw_start {
        return Err(ImageError::Truncated {
            reason: "missing SREC termination (S7/S8/S9) record".into(),
        });
    }
    Ok(out)
}

/// Options for [`write`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteOptions {
    /// Maximum data bytes per record (1..=252, bounded by the count field).
    pub record_len: u8,
    /// Address width to use. `None` picks the narrowest that fits every address.
    pub width: Option<AddressWidth>,
    /// Emit an S5 record-count record after the data.
    pub emit_count: bool,
}

impl Default for WriteOptions {
    fn default() -> Self {
        Self {
            record_len: 16,
            width: None,
            emit_count: true,
        }
    }
}

/// Serialise a [`SRecFile`] to canonical SREC text (lines joined by `\n`,
/// trailing newline included).
///
/// The data records use the requested [`AddressWidth`], or the narrowest that
/// fits every address. An S0 header, an optional S5 count and a matching
/// S7/S8/S9 terminator are emitted around the data.
///
/// # Errors
/// [`ImageError::AddressWidth`] if an address does not fit the chosen width.
pub fn write(file: &SRecFile, opts: WriteOptions) -> Result<String> {
    let width = match opts.width {
        Some(w) => {
            if let Some((_, end)) = file.image.span() {
                let max = end - 1;
                let bits = (w.addr_bytes() * 8) as u8;
                if max >= 1u64 << bits {
                    return Err(ImageError::AddressWidth {
                        address: max,
                        max_bits: bits,
                    });
                }
            }
            w
        }
        None => AddressWidth::fit(&file.image)?,
    };
    // Record-length ceiling: count byte limits address+data+checksum to 255.
    let max_data = 255 - width.addr_bytes() - 1;
    let rec_len = (opts.record_len as usize).clamp(1, max_data);

    let mut out = String::new();
    let (data_type, term_type) = match width {
        AddressWidth::Bits16 => (b'1', b'9'),
        AddressWidth::Bits24 => (b'2', b'8'),
        AddressWidth::Bits32 => (b'3', b'7'),
    };

    // S0 header.
    emit_record(&mut out, b'0', 0, 2, file.header.as_deref().unwrap_or(&[]));

    let mut data_records: u64 = 0;
    for seg in file.image.segments() {
        let mut addr = seg.address;
        let end = seg.end();
        while addr < end {
            let chunk_end = (addr + rec_len as u64).min(end);
            let off = (addr - seg.address) as usize;
            let len = (chunk_end - addr) as usize;
            emit_record(
                &mut out,
                data_type,
                addr,
                width.addr_bytes(),
                &seg.bytes[off..off + len],
            );
            data_records += 1;
            addr = chunk_end;
        }
    }

    if opts.emit_count && data_records <= 0xFFFF {
        emit_record(&mut out, b'5', data_records, 2, &[]);
    } else if opts.emit_count {
        emit_record(&mut out, b'6', data_records, 3, &[]);
    }

    let start = file.start.unwrap_or(0) as u64;
    emit_record(&mut out, term_type, start, width.addr_bytes(), &[]);
    Ok(out)
}

/// Format and append one S-record, computing its count and checksum.
fn emit_record(out: &mut String, kind: u8, address: u64, addr_bytes: usize, data: &[u8]) {
    use core::fmt::Write as _;
    let count = (addr_bytes + data.len() + 1) as u8;
    let addr_be = address.to_be_bytes();
    let addr_slice = &addr_be[8 - addr_bytes..];

    let mut sum = count;
    for &b in addr_slice {
        sum = sum.wrapping_add(b);
    }
    for &b in data {
        sum = sum.wrapping_add(b);
    }
    let checksum = !sum;

    let _ = write!(out, "S{}{count:02X}", kind as char);
    for &b in addr_slice {
        let _ = write!(out, "{b:02X}");
    }
    for &b in data {
        let _ = write!(out, "{b:02X}");
    }
    let _ = writeln!(out, "{checksum:02X}");
}

/// Interpret 2–4 big-endian bytes as an address.
fn be_addr(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0u64, |a, &b| (a << 8) | u64::from(b))
}

/// Decode an ASCII-hex string into bytes.
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

/// Validate the trailing ones-complement checksum byte of a raw record (the
/// count byte, address and data; the final byte is the checksum itself).
fn verify_checksum(raw: &[u8], line: usize) -> Result<()> {
    let (body, &[found]) = raw.split_at(raw.len() - 1) else {
        unreachable!("record length checked by caller");
    };
    let sum = body.iter().fold(0u8, |a, &b| a.wrapping_add(b));
    let expected = !sum;
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
