//! UCL **NRV2B** decompression (8-bit bit-buffer variant) — the codec BMW uses
//! for `COMPRESSED` segments of PSdZData SWE flash containers (see [`crate::bmw`]).
//!
//! NRV2B is one of Markus F.X.J. Oberhumer's NRV algorithms, published in the
//! UCL data compression library (GPL-2.0-or-later,
//! <https://www.oberhumer.com/opensource/ucl/>). BMW's own open-source
//! `uclcli` (GPL-3.0-or-later, <https://github.com/afflux/uclcli>) decompresses
//! these segments with `ucl_nrv2b_decompress_safe_8` over the raw segment bytes
//! (no UCL file header), so that exact variant is what this module implements.
//! It is written independently from the public algorithm description; no UCL
//! source is copied. GPL-2.0-or-later → compatible with OpenTune's GPL-3.0.
//!
//! ## Stream format
//!
//! Control bits come from a bit buffer that is refilled one byte at a time, most
//! significant bit first, at the moment a bit is needed; literal bytes and offset
//! low bytes are interleaved in the same stream.
//!
//! ```text
//!   1 <byte>                         literal
//!   0 gamma(v) [byte] len-code       match
//!       v == 2      → reuse the previous match distance
//!       v >= 3      → distance = ((v - 3) << 8 | byte) + 1
//!       (v - 3) << 8 | byte == 0xFFFF_FFFF → end of stream
//!     len-code: 2 bits b1 b0 → n = 2*b1 + b0; n == 0 → n = gamma + 2
//!     n += 1 when distance > 0xD00; the match copies n + 1 bytes
//!   gamma(v): v >= 2, its bits below the leading 1, MSB first, each followed
//!             by a stop bit (1 = last)
//! ```
//!
//! Like the reference "safe" decoder, this one rejects input overruns,
//! look-behind past the start of the output, output beyond the caller's cap and
//! input left over after the end marker.

use crate::error::{Error, Result};

/// Largest distance code the format can express (`v - 3` is a 24-bit value).
const MAX_GAMMA_OFFSET: u32 = 0x00FF_FFFF + 3;

struct BitReader<'a> {
    src: &'a [u8],
    pos: usize,
    bb: u32,
}

impl BitReader<'_> {
    fn byte(&mut self) -> Result<u8> {
        let b = *self.src.get(self.pos).ok_or_else(|| {
            Error::Corrupt(format!(
                "NRV2B input overrun at byte {:#x} of {:#x}",
                self.pos,
                self.src.len()
            ))
        })?;
        self.pos += 1;
        Ok(b)
    }

    fn bit(&mut self) -> Result<u32> {
        if self.bb & 0x7F == 0 {
            // Refill: the byte's bits sit above a sentinel 1 bit.
            self.bb = u32::from(self.byte()?) * 2 + 1;
        } else {
            self.bb *= 2;
        }
        Ok((self.bb >> 8) & 1)
    }
}

/// Decompress a raw NRV2B (8-bit) stream, producing at most `max_out` bytes.
///
/// Equivalent in behaviour to UCL's `ucl_nrv2b_decompress_safe_8` with an
/// output buffer of `max_out` bytes: every malformed stream is an error, and so
/// is input remaining after the end marker.
pub fn decompress_nrv2b_8(src: &[u8], max_out: usize) -> Result<Vec<u8>> {
    let mut r = BitReader { src, pos: 0, bb: 0 };
    let mut out: Vec<u8> = Vec::with_capacity(src.len().saturating_mul(2).min(max_out));
    let mut last_off: u32 = 1;
    let overrun = |at: usize| {
        Error::Corrupt(format!(
            "NRV2B output would exceed the {max_out:#x}-byte limit (at {at:#x})"
        ))
    };

    loop {
        while r.bit()? == 1 {
            if out.len() >= max_out {
                return Err(overrun(out.len()));
            }
            out.push(r.byte()?);
        }

        let mut m_off: u32 = 1;
        loop {
            m_off = m_off * 2 + r.bit()?;
            if m_off > MAX_GAMMA_OFFSET {
                return Err(Error::Corrupt(format!(
                    "NRV2B distance code too large at input byte {:#x}",
                    r.pos
                )));
            }
            if r.bit()? == 1 {
                break;
            }
        }
        if m_off == 2 {
            m_off = last_off;
        } else {
            let v = (m_off - 3) * 256 + u32::from(r.byte()?);
            if v == 0xFFFF_FFFF {
                break;
            }
            m_off = v + 1;
            last_off = m_off;
        }

        let mut m_len = r.bit()?;
        m_len = m_len * 2 + r.bit()?;
        if m_len == 0 {
            m_len = 1;
            loop {
                m_len = m_len * 2 + r.bit()?;
                if m_len as usize > max_out {
                    return Err(overrun(out.len()));
                }
                if r.bit()? == 1 {
                    break;
                }
            }
            m_len += 2;
        }
        if m_off > 0xD00 {
            m_len += 1;
        }

        let dist = m_off as usize;
        if dist > out.len() {
            return Err(Error::Corrupt(format!(
                "NRV2B look-behind of {dist:#x} bytes with only {:#x} bytes output",
                out.len()
            )));
        }
        let count = m_len as usize + 1;
        if out.len() + count > max_out {
            return Err(overrun(out.len()));
        }
        let start = out.len() - dist;
        // Byte-wise: overlapping copies (dist < count) repeat the window.
        for k in 0..count {
            let b = out[start + k];
            out.push(b);
        }
    }

    if r.pos != src.len() {
        return Err(Error::Corrupt(format!(
            "NRV2B end marker at byte {:#x} but the input is {:#x} bytes (input not consumed)",
            r.pos,
            src.len()
        )));
    }
    Ok(out)
}

struct BitWriter {
    out: Vec<u8>,
    bit_byte: usize,
    free: u32,
}

impl BitWriter {
    fn bit(&mut self, b: u32) {
        if self.free == 0 {
            // Reserve the control byte where the decoder will refill from.
            self.bit_byte = self.out.len();
            self.out.push(0);
            self.free = 8;
        }
        self.free -= 1;
        if b & 1 == 1 {
            self.out[self.bit_byte] |= 1 << self.free;
        }
    }

    fn gamma(&mut self, v: u32) {
        debug_assert!(v >= 2);
        let top = 31 - v.leading_zeros();
        for k in (0..top).rev() {
            self.bit((v >> k) & 1);
            self.bit(u32::from(k == 0));
        }
    }
}

/// A small, greedy NRV2B (8-bit) **encoder** whose output
/// [`decompress_nrv2b_8`] (and UCL) decode. It exists to build synthetic test
/// fixtures; it is not bit-identical to UCL's `nrv2b_99` compressor and makes no
/// attempt at a good ratio (4 KiB search window).
pub fn compress_nrv2b_8(data: &[u8]) -> Vec<u8> {
    const WINDOW: usize = 4096;
    let mut w = BitWriter {
        out: Vec::with_capacity(data.len() + data.len() / 8 + 16),
        bit_byte: 0,
        free: 0,
    };
    let mut last_off = 1usize;
    let mut i = 0usize;
    while i < data.len() {
        let mut best = (0usize, 0usize); // (len, distance)
        for dist in 1..=i.min(WINDOW) {
            let mut len = 0;
            while i + len < data.len() && data[i + len] == data[i + len - dist] {
                len += 1;
            }
            if len > best.0 {
                best = (len, dist);
            }
        }
        let (len, dist) = best;
        let bonus = usize::from(dist > 0xD00);
        if len >= 3 + bonus {
            w.bit(0);
            if dist == last_off {
                w.gamma(2);
            } else {
                w.gamma((((dist - 1) >> 8) + 3) as u32);
                w.out.push(((dist - 1) & 0xFF) as u8);
                last_off = dist;
            }
            let n = (len - 1 - bonus) as u32;
            if n <= 3 {
                w.bit(n >> 1);
                w.bit(n & 1);
            } else {
                w.bit(0);
                w.bit(0);
                w.gamma(n - 2);
            }
            i += len;
        } else {
            w.bit(1);
            w.out.push(data[i]);
            i += 1;
        }
    }
    // End marker: distance code 0xFFFF_FFFF.
    w.bit(0);
    w.gamma(MAX_GAMMA_OFFSET);
    w.out.push(0xFF);
    w.out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The public test vector from BMW's `uclcli` (GPL-3.0-or-later) doctest:
    /// 12 bytes decompress to 1024 × 0xA5 with `ucl_nrv2b_decompress_safe_8`.
    const UCLCLI_VECTOR: &[u8] = b"\x92\xa5\xaa\xa1\x00\x00\x00\x00\x00\x04\x80\xff";

    #[test]
    fn uclcli_public_vector() {
        let out = decompress_nrv2b_8(UCLCLI_VECTOR, 1024).unwrap();
        assert_eq!(out, vec![0xA5u8; 1024]);
        // A cap one byte short is an output overrun, as in UCL.
        assert!(decompress_nrv2b_8(UCLCLI_VECTOR, 1023).is_err());
    }

    #[test]
    fn round_trips() {
        // xorshift noise: no accidental matches.
        let mut x: u32 = 0x1234_5678;
        let mut data: Vec<u8> = (0..4000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect();
        // A repeat at distance 4000 (> 0xD00) exercises the length bonus.
        let far = data[..300].to_vec();
        data.extend(&far);
        data.extend(std::iter::repeat(0xFF).take(5000));
        data.extend(b"BMW-SWE synthetic fixture ".repeat(200));
        for sample in [&b""[..], &b"a"[..], &b"abcabcabcabc"[..], &data[..]] {
            let c = compress_nrv2b_8(sample);
            assert_eq!(decompress_nrv2b_8(&c, sample.len()).unwrap(), sample);
        }
    }

    #[test]
    fn rejects_malformed_streams() {
        // Truncated (no end marker).
        assert!(decompress_nrv2b_8(&UCLCLI_VECTOR[..6], 4096).is_err());
        // Trailing garbage after the end marker.
        let mut v = UCLCLI_VECTOR.to_vec();
        v.push(0);
        assert!(decompress_nrv2b_8(&v, 4096).is_err());
        // A match before any output is a look-behind overrun.
        assert!(decompress_nrv2b_8(&[0x40, 0x00], 16).is_err());
        // Empty input.
        assert!(decompress_nrv2b_8(&[], 16).is_err());
    }
}
