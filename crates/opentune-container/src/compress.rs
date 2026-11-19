//! LZSS / LZZ decompression and compression for VAG flash blocks.
//!
//! Ported from the open-source references (see crate root for attribution):
//!   * `bri3d/VW_Flash` (BSD-2) — the canonical Simos LZSS10 and legacy-Simos codecs.
//!   * `dspl1236/vag-tcu-tools` (MIT) — the DQ381 LZSS and ZF-8HP LZZ (2047 window) codecs.
//!
//! There are three genuinely distinct back-reference formats in the wild and
//! they are NOT interchangeable, so each has its own function and the
//! [`Compression`] selector records which one a container asked for.

use crate::error::{Error, Result};

/// Upper bound on a decompressed block / declared output size. Flash segments are
/// a few megabytes at most; this caps the *pre-allocation hint* derived from an
/// attacker-controlled declared size so a tiny input cannot ask the allocator for
/// gigabytes. The decoders are still correct for any real `out_size` below it.
const MAX_DECOMPRESSED: usize = 512 * 1024 * 1024;

/// The compression scheme applied to a flash block, as named by the container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    /// No compression; the stored bytes are the payload.
    None,
    /// Simos "LZSS10": flag bit 1 = back-reference, 6-bit count, 10-bit
    /// displacement (1023-byte window), big-endian reference word.
    /// Used by Simos ODX blocks with method compression char `A`/`a`.
    Lzss10,
    /// DQ-family LZSS: flag bit 1 = *literal*, count = `(b0>>2)+3`, 10-bit
    /// displacement (1023-byte window). Used by some DQ TCU ODX blocks.
    LzssDq,
    /// ZF-8HP "LZZ": flag bit 1 = back-reference, 5-bit count, 11-bit
    /// displacement (2047-byte window). Method compression char `2`.
    Lzz,
    /// Legacy Simos signifier-byte compression (early Simos8-era blocks).
    LegacySimos,
    /// UCL NRV2B, 8-bit variant (BMW PSdZData SWE `COMPRESSED` segments).
    /// See [`crate::nrv2b`].
    Nrv2b,
    /// gzip (BMW SWE segments whose `COMPRESSION-METHOD` is `gz`).
    Gzip,
}

impl Compression {
    /// Map an ODX `ENCRYPT-COMPRESS-METHOD` compression character to a scheme.
    ///
    /// `'0'` → none, `'A'`/`'a'` → [`Compression::Lzss10`], `'2'` → [`Compression::Lzz`],
    /// `'1'` → [`Compression::LegacySimos`]. Unknown characters yield `None` so the
    /// caller can decide whether to treat the block as stored.
    pub fn from_method_char(c: char) -> Compression {
        match c {
            'A' | 'a' => Compression::Lzss10,
            '2' => Compression::Lzz,
            '1' => Compression::LegacySimos,
            _ => Compression::None,
        }
    }

    /// Decompress `data` to `out_size` bytes using this scheme.
    ///
    /// `out_size` is the declared uncompressed size; for [`Compression::None`] it
    /// is advisory. Returns [`Error::SizeMismatch`] if a sized scheme cannot
    /// reach the declared length.
    pub fn decompress(self, data: &[u8], out_size: usize) -> Result<Vec<u8>> {
        let out = match self {
            Compression::None => data.to_vec(),
            Compression::Lzss10 => decompress_lzss10(data, out_size),
            Compression::LzssDq => decompress_lzss_dq(data, out_size),
            Compression::Lzz => decompress_lzz(data, out_size),
            Compression::LegacySimos => decompress_legacy_simos(data)?,
            Compression::Nrv2b => crate::nrv2b::decompress_nrv2b_8(data, cap(out_size))?,
            Compression::Gzip => decompress_gzip(data, cap(out_size))?,
        };
        if out_size != 0 && self != Compression::None && out.len() != out_size {
            return Err(Error::SizeMismatch {
                expected: out_size,
                got: out.len(),
            });
        }
        Ok(out)
    }
}

/// Output cap for the self-terminating codecs: the declared size, else the global limit.
fn cap(out_size: usize) -> usize {
    if out_size == 0 {
        MAX_DECOMPRESSED
    } else {
        out_size.min(MAX_DECOMPRESSED)
    }
}

/// gunzip `data`, refusing output larger than `cap` bytes.
pub fn decompress_gzip(data: &[u8], cap: usize) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(data)
        .take(cap as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| Error::Corrupt(format!("gzip: {e}")))?;
    if out.len() > cap {
        return Err(Error::Corrupt(format!(
            "gzip output exceeds the {cap:#x}-byte limit"
        )));
    }
    Ok(out)
}

/// Simos "LZSS10" decompressor (port of `VW_Flash` `decompress_raw_lzss10`).
///
/// Control byte bits are consumed MSB-first: a 1 bit introduces a big-endian
/// 16-bit reference word (top 6 bits = count, low 10 bits = displacement), a 0
/// bit copies a literal byte.
pub fn decompress_lzss10(data: &[u8], out_size: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(out_size.min(MAX_DECOMPRESSED));
    let mut i = 0usize;
    'outer: while i < data.len() {
        let flags = data[i];
        i += 1;
        for bit in 0..8 {
            if out_size != 0 && out.len() >= out_size {
                break 'outer;
            }
            if (flags >> (7 - bit)) & 1 == 1 {
                if i + 1 >= data.len() {
                    break 'outer;
                }
                let word = ((data[i] as u16) << 8) | data[i + 1] as u16;
                i += 2;
                let count = (word >> 10) as usize;
                let disp = (word & 0x3ff) as usize;
                let disp = if disp == 0 { 1 } else { disp };
                for _ in 0..count {
                    let src = out.len().wrapping_sub(disp);
                    let b = out.get(src).copied().unwrap_or(0);
                    out.push(b);
                    if out_size != 0 && out.len() >= out_size {
                        break 'outer;
                    }
                }
            } else {
                if i >= data.len() {
                    break 'outer;
                }
                out.push(data[i]);
                i += 1;
            }
        }
    }
    if out_size != 0 {
        out.truncate(out_size);
    }
    out
}

/// Compress to the Simos "LZSS10" format. Greedy longest-match; not byte-identical
/// to VW's encoder but a valid encoding that [`decompress_lzss10`] reverses exactly.
pub fn compress_lzss10(data: &[u8]) -> Vec<u8> {
    const WINMAX: usize = 1023;
    const MAXLEN: usize = 63; // 6-bit count
    const MINLEN: usize = 3;
    lzss_compress_generic(
        data,
        WINMAX,
        MAXLEN,
        MINLEN,
        |out, count, disp| {
            // flag bit 1 = reference; word = (count<<10)|disp, big-endian
            let word = ((count as u16) << 10) | (disp as u16 & 0x3ff);
            out.push((word >> 8) as u8);
            out.push((word & 0xff) as u8);
        },
        true,
    )
}

/// DQ-family LZSS decompressor (port of `vag-tcu-tools` `decompress_lzss`).
///
/// Note the inverted flag polarity relative to [`decompress_lzss10`]: here a 1
/// bit is a literal and a 0 bit a reference, and the count carries a `+3` bias.
pub fn decompress_lzss_dq(data: &[u8], out_size: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(out_size.min(MAX_DECOMPRESSED));
    let mut i = 0usize;
    'outer: while i < data.len() {
        let flags = data[i];
        i += 1;
        for bit in 0..8 {
            if out_size != 0 && out.len() >= out_size {
                break 'outer;
            }
            if (flags >> (7 - bit)) & 1 == 1 {
                if i >= data.len() {
                    break 'outer;
                }
                out.push(data[i]);
                i += 1;
            } else {
                if i + 1 >= data.len() {
                    break 'outer;
                }
                let b0 = data[i];
                let b1 = data[i + 1];
                i += 2;
                let count = (b0 >> 2) as usize + 3;
                let disp = (((b0 & 0x03) as usize) << 8) | b1 as usize;
                let disp = if disp == 0 { 1 } else { disp };
                for _ in 0..count {
                    let src = out.len().wrapping_sub(disp);
                    let b = out.get(src).copied().unwrap_or(0);
                    out.push(b);
                }
            }
        }
    }
    if out_size != 0 {
        out.truncate(out_size);
    }
    out
}

/// ZF-8HP "LZZ" decompressor, 2047-byte window (port of `vag-tcu-tools` `decompress_lzz`).
pub fn decompress_lzz(data: &[u8], out_size: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(out_size.min(MAX_DECOMPRESSED));
    let mut i = 0usize;
    'outer: while i < data.len() {
        if out_size != 0 && out.len() >= out_size {
            break;
        }
        let flags = data[i];
        i += 1;
        for bit in 0..8 {
            if i >= data.len() || (out_size != 0 && out.len() >= out_size) {
                break 'outer;
            }
            if (flags >> (7 - bit)) & 1 == 1 {
                if i + 1 >= data.len() {
                    break 'outer;
                }
                let b0 = data[i];
                let b1 = data[i + 1];
                i += 2;
                let count = (b0 >> 3) as usize;
                let disp = (((b0 & 0x07) as usize) << 8) | b1 as usize;
                let disp = if disp == 0 { 1 } else { disp };
                let start = out.len() as isize - disp as isize;
                for j in 0..count as isize {
                    if start + j < 0 {
                        out.push(0);
                    } else {
                        out.push(out[(start + j) as usize]);
                    }
                }
            } else {
                out.push(data[i]);
                i += 1;
            }
        }
    }
    if out_size != 0 {
        out.truncate(out_size);
    }
    out
}

/// Compress to the ZF-8HP "LZZ" format (inverse of [`decompress_lzz`]).
pub fn compress_lzz(data: &[u8]) -> Vec<u8> {
    const WINMAX: usize = 2047;
    const MAXLEN: usize = 31; // 5-bit count
    const MINLEN: usize = 3;
    lzss_compress_generic(
        data,
        WINMAX,
        MAXLEN,
        MINLEN,
        |out, count, disp| {
            out.push(((count as u8) << 3) | ((disp >> 8) as u8 & 0x07));
            out.push((disp & 0xff) as u8);
        },
        true,
    )
}

/// Legacy Simos signifier-byte decompressor (port of `VW_Flash` `legacysimos.decompress`).
pub fn decompress_legacy_simos(data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < 11 {
        return Err(Error::Truncated {
            what: "legacy-simos header",
            need: 11,
            at: 0,
            len: data.len(),
        });
    }
    let signifier = data[0];
    let offset_size = data[1] as u32;
    // The signifier word packs `offset_size` offset bits and the rest as length
    // bits, so a valid `offset_size` leaves room for both: 1..=15. Anything else
    // (notably 0 or >=16) would make `16 - offset_size` underflow and the shift
    // panic, so reject it as unsupported rather than crash.
    if !(1..=15).contains(&offset_size) {
        return Err(Error::Unsupported(format!(
            "legacy-simos offset size {offset_size} out of range (expected 1..=15)"
        )));
    }
    let shift = 16 - offset_size; // in 1..=15, so `ol >> shift` is always valid
    let dict_size = fill_bits(data[2] as u32) as usize;
    let block_size = u32::from_be_bytes([data[3], data[4], data[5], data[6]]) as usize;
    // Bound the declared output size before allocating it.
    if block_size > MAX_DECOMPRESSED {
        return Err(Error::Unsupported(format!(
            "legacy-simos declared block size {block_size:#x} exceeds the {MAX_DECOMPRESSED:#x}-byte limit"
        )));
    }
    let mut out = vec![0u8; block_size];
    let mut oc = 0usize;
    let mut cursor = 11usize;
    while cursor < data.len() && oc < block_size {
        if data[cursor] == signifier {
            cursor += 1;
            if cursor + 2 > data.len() {
                break;
            }
            let ol = u16::from_be_bytes([data[cursor], data[cursor + 1]]) as usize;
            cursor += 2;
            let off = ol >> shift;
            let length = ol & dict_size;
            let block_offset = oc;
            // A back-reference must point inside already-produced output; a bad
            // `off` (greater than the current position) is malformed, so stop the
            // run instead of underflowing the index.
            let Some(src0) = block_offset.checked_sub(off) else {
                break;
            };
            for k in 0..length {
                if oc >= block_size {
                    break;
                }
                // Read from the produced buffer by bounds-checked index; a source
                // past the buffer end ends the run rather than panicking.
                match src0.checked_add(k).and_then(|idx| out.get(idx).copied()) {
                    Some(b) => {
                        out[oc] = b;
                        oc += 1;
                    }
                    None => break,
                }
            }
            if cursor < data.len() && oc < block_size {
                out[oc] = data[cursor];
                cursor += 1;
                oc += 1;
            }
        } else {
            out[oc] = data[cursor];
            cursor += 1;
            oc += 1;
        }
    }
    Ok(out)
}

fn fill_bits(count: u32) -> u32 {
    if count >= 32 {
        u32::MAX
    } else {
        (1u32 << count) - 1
    }
}

/// Shared greedy LZ encoder used by the LZSS10 and LZZ compressors. `emit_ref`
/// writes the back-reference word(s) for the concrete format. When `ref_is_one`
/// the control bit for a reference is 1 (both formats here use 1 = reference).
fn lzss_compress_generic(
    data: &[u8],
    winmax: usize,
    maxlen: usize,
    minlen: usize,
    emit_ref: impl Fn(&mut Vec<u8>, usize, usize),
    ref_is_one: bool,
) -> Vec<u8> {
    let n = data.len();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < n {
        let flag_pos = out.len();
        out.push(0u8);
        let mut flag = 0u8;
        for bit in 0..8 {
            if i >= n {
                break;
            }
            let (mut best_len, mut best_disp) = (0usize, 0usize);
            let start = i.saturating_sub(winmax);
            // Greedy search for the longest back-reference in the window.
            for cand in start..i {
                let d = i - cand;
                let mut l = 0usize;
                while l < maxlen && i + l < n && data[i + l] == data[i - d + l] {
                    l += 1;
                }
                if l > best_len {
                    best_len = l;
                    best_disp = d;
                }
            }
            if best_len >= minlen {
                if ref_is_one {
                    flag |= 1 << (7 - bit);
                }
                emit_ref(&mut out, best_len, best_disp);
                i += best_len;
            } else {
                if !ref_is_one {
                    flag |= 1 << (7 - bit);
                }
                out.push(data[i]);
                i += 1;
            }
        }
        out[flag_pos] = flag;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip_lzss10(input: &[u8]) {
        let packed = compress_lzss10(input);
        let out = decompress_lzss10(&packed, input.len());
        assert_eq!(out, input, "lzss10 round-trip mismatch");
    }

    fn roundtrip_lzz(input: &[u8]) {
        let packed = compress_lzz(input);
        let out = decompress_lzz(&packed, input.len());
        assert_eq!(out, input, "lzz round-trip mismatch");
    }

    #[test]
    fn lzss10_roundtrip_patterns() {
        roundtrip_lzss10(b"");
        roundtrip_lzss10(b"A");
        roundtrip_lzss10(b"ABCABCABCABCABCABC");
        roundtrip_lzss10(&[0u8; 2000]);
        let mut s = Vec::new();
        for i in 0..5000u32 {
            s.push((i.wrapping_mul(31) ^ (i >> 3)) as u8);
        }
        roundtrip_lzss10(&s);
        roundtrip_lzss10(b"the quick brown fox the quick brown fox jumped the quick brown fox");
    }

    #[test]
    fn lzz_roundtrip_patterns() {
        roundtrip_lzz(b"");
        roundtrip_lzz(b"hello hello hello world world world");
        roundtrip_lzz(&[0x5au8; 4096]);
        let mut s = Vec::new();
        for i in 0..6000u32 {
            s.push((i.wrapping_mul(7) ^ (i >> 1)) as u8);
        }
        roundtrip_lzz(&s);
    }

    #[test]
    fn lzss10_known_literal_stream() {
        // A control byte of 0x00 means eight literals follow.
        let packed = [0x00u8, b'a', b'b', b'c', b'd', b'e', b'f', b'g', b'h'];
        let out = decompress_lzss10(&packed, 8);
        assert_eq!(&out, b"abcdefgh");
    }

    #[test]
    fn huge_declared_out_size_does_not_preallocate_gigabytes() {
        // A tiny compressed input declaring an absurd uncompressed size must not
        // try to reserve that much memory; it just produces what the data allows.
        let data = [0x00u8, 1, 2, 3];
        let big = usize::MAX / 2;
        let _ = decompress_lzss10(&data, big);
        let _ = decompress_lzss_dq(&data, big);
        let _ = decompress_lzz(&data, big);
    }

    #[test]
    fn lzss10_back_reference_before_start_is_safe() {
        // Control byte 0x80: first slot is a back-reference word. A displacement
        // with nothing produced yet must not panic.
        let packed = [0x80u8, 0xFF, 0xFF];
        let _ = decompress_lzss10(&packed, 64);
    }

    #[test]
    fn legacy_simos_rejects_truncated_header() {
        assert!(decompress_legacy_simos(&[0u8; 5]).is_err());
    }

    #[test]
    fn legacy_simos_rejects_bad_offset_size() {
        // offset_size (byte 1) = 0 and = 0xFF would both make the shift underflow.
        let mut hdr = vec![0xAAu8; 16];
        hdr[1] = 0; // offset_size 0
        assert!(decompress_legacy_simos(&hdr).is_err());
        hdr[1] = 0xFF; // offset_size 255
        assert!(decompress_legacy_simos(&hdr).is_err());
    }

    #[test]
    fn legacy_simos_rejects_absurd_block_size() {
        // A valid-looking header declaring a ~4 GiB output must be rejected, not
        // allocated.
        let mut hdr = vec![0u8; 16];
        hdr[0] = 0x00; // signifier
        hdr[1] = 11; // offset_size in range
        hdr[2] = 0x0F; // dict size bits
        hdr[3..7].copy_from_slice(&0xFFFF_FFFFu32.to_be_bytes()); // block size
        assert!(decompress_legacy_simos(&hdr).is_err());
    }

    #[test]
    fn legacy_simos_back_reference_underflow_is_safe() {
        // A signifier run at the very start references output that does not exist
        // yet; it must terminate cleanly rather than underflow the index.
        let mut d = vec![0u8; 11];
        d[0] = 0x5A; // signifier
        d[1] = 11; // offset_size
        d[2] = 0x1F; // dict/length mask bits
        d[3..7].copy_from_slice(&64u32.to_be_bytes()); // block size 64
                                                       // A signifier byte immediately, then a word with a large offset.
        d.push(0x5A);
        d.push(0xFF);
        d.push(0xFF);
        let out = decompress_legacy_simos(&d).expect("clean decode");
        assert_eq!(out.len(), 64);
    }
}
