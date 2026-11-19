//! Bit extraction with DBC semantics, and raw -> physical conversion.
use crate::{ByteOrder, Signal, SignalKind};

/// Byte positions (0-based) a signal occupies, in bit order from the LSB
/// (`Little`) or MSB (`Big`). `None` when the layout runs off a 64-byte frame
/// or `len` is 0 / > 64.
fn bit_positions(start: u16, len: u8, order: ByteOrder) -> Option<Vec<u16>> {
    if len == 0 || len > 64 {
        return None;
    }
    let mut out = Vec::with_capacity(len as usize);
    match order {
        ByteOrder::Little => {
            for i in 0..len as u16 {
                out.push(start.checked_add(i)?);
            }
        }
        ByteOrder::Big => {
            // DBC Motorola: start is the MSB; walk towards bit 0 of the byte,
            // then continue at bit 7 of the next byte.
            let mut pos = start as i32;
            for _ in 0..len {
                out.push(pos as u16);
                if pos % 8 == 0 {
                    pos += 15;
                } else {
                    pos -= 1;
                }
            }
        }
    }
    if out.iter().any(|&p| p >= 512) {
        return None;
    }
    Some(out)
}

/// Bit positions (0-based, `byte * 8 + bit`) a signal occupies, or `None`
/// for an impossible layout.
pub fn bits(start: u16, len: u8, order: ByteOrder) -> Option<Vec<u16>> {
    bit_positions(start, len, order)
}

/// Bytes needed to hold the signal (highest byte index + 1), or `None` for an
/// impossible layout.
pub fn bytes_needed(start: u16, len: u8, order: ByteOrder) -> Option<usize> {
    bit_positions(start, len, order)
        .map(|v| v.iter().map(|&p| p as usize / 8).max().unwrap_or(0) + 1)
}

/// Extract the raw (unsigned) value. `None` if `data` is too short.
pub fn extract(data: &[u8], start: u16, len: u8, order: ByteOrder) -> Option<u64> {
    let pos = bit_positions(start, len, order)?;
    let bit = |p: u16| -> Option<u64> {
        let b = *data.get(p as usize / 8)?;
        Some(((b >> (p % 8)) & 1) as u64)
    };
    let mut v: u64 = 0;
    match order {
        ByteOrder::Little => {
            for (i, &p) in pos.iter().enumerate() {
                v |= bit(p)? << i;
            }
        }
        ByteOrder::Big => {
            for &p in &pos {
                v = (v << 1) | bit(p)?;
            }
        }
    }
    Some(v)
}

/// Write `value` into `data` (for building test frames and simulators).
/// Returns `false` if the layout does not fit.
pub fn insert(data: &mut [u8], start: u16, len: u8, order: ByteOrder, value: u64) -> bool {
    let Some(pos) = bit_positions(start, len, order) else {
        return false;
    };
    if pos.iter().any(|&p| p as usize / 8 >= data.len()) {
        return false;
    }
    let n = pos.len();
    for (i, &p) in pos.iter().enumerate() {
        let shift = match order {
            ByteOrder::Little => i,
            ByteOrder::Big => n - 1 - i,
        };
        let b = ((value >> shift) & 1) as u8;
        let byte = &mut data[p as usize / 8];
        *byte = (*byte & !(1 << (p % 8))) | (b << (p % 8));
    }
    true
}

/// Two's-complement sign extension of a `len`-bit raw value. A zero-width
/// value is 0 (it used to overflow the shift).
pub fn sign_extend(raw: u64, len: u8) -> i64 {
    if len == 0 {
        return 0;
    }
    if len >= 64 {
        return raw as i64;
    }
    let shift = 64 - len as u32;
    ((raw << shift) as i64) >> shift
}

/// Piecewise-linear interpolation through `curve` (raw ascending), clamped.
pub fn interpolate(curve: &[[f64; 2]], x: f64) -> f64 {
    match curve {
        [] => x,
        [only] => only[1],
        _ => {
            if x <= curve[0][0] {
                return curve[0][1];
            }
            for w in curve.windows(2) {
                let ([x0, y0], [x1, y1]) = (w[0], w[1]);
                if x <= x1 {
                    if x1 == x0 {
                        return y1;
                    }
                    return y0 + (y1 - y0) * (x - x0) / (x1 - x0);
                }
            }
            curve[curve.len() - 1][1]
        }
    }
}

impl Signal {
    /// Raw value from a payload (`None` if the payload is too short).
    pub fn raw(&self, data: &[u8]) -> Option<u64> {
        extract(data, self.start, self.len, self.byte_order)
    }

    /// Whether `raw` is marked invalid (listed in `invalid` or outside `raw_range`).
    pub fn is_invalid(&self, raw: u64) -> bool {
        self.invalid.contains(&raw) || self.raw_range.is_some_and(|[lo, hi]| raw < lo || raw > hi)
    }

    /// Physical value for a raw value (ignores `kind`; used for numbers).
    pub fn physical(&self, raw: u64) -> f64 {
        let x = if self.signed {
            sign_extend(raw, self.len) as f64
        } else {
            raw as f64
        };
        if self.curve.is_empty() {
            x * self.factor + self.offset
        } else {
            interpolate(&self.curve, x)
        }
    }

    /// Raw value encoding a physical value (inverse of the linear scaling;
    /// for building test frames). Not defined for curves.
    pub fn raw_for(&self, physical: f64) -> u64 {
        let r = ((physical - self.offset) / self.factor).round() as i64;
        let mask = if self.len >= 64 {
            u64::MAX
        } else {
            (1u64 << self.len) - 1
        };
        (r as u64) & mask
    }

    /// Name of a raw value, if listed.
    pub fn value_name(&self, raw: u64) -> Option<&str> {
        self.values
            .iter()
            .find(|v| v.raw == raw)
            .map(|v| v.name.as_str())
    }

    /// True for a single-bit flag.
    pub fn is_flag(&self) -> bool {
        self.kind == SignalKind::Bool
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn little_endian_matches_dbc_intel() {
        // vw_pq.dbc Motordrehzahl @16 16 bit: bytes 2-3 little-endian.
        let mut d = [0u8; 8];
        d[2..4].copy_from_slice(&12000u16.to_le_bytes());
        assert_eq!(extract(&d, 16, 16, ByteOrder::Little), Some(12000));
        // 15-bit at bit 1.
        let v = 5000u64 << 1;
        assert_eq!(
            extract(&v.to_le_bytes(), 1, 15, ByteOrder::Little),
            Some(5000)
        );
        assert_eq!(extract(&[0u8; 3], 16, 16, ByteOrder::Little), None);
    }

    #[test]
    fn big_endian_matches_dbc_motorola() {
        // Motorola 16-bit starting at bit 7 of byte 0 = bytes 0..2 big-endian.
        let d = [0x12, 0x34, 0, 0, 0, 0, 0, 0];
        assert_eq!(extract(&d, 7, 16, ByteOrder::Big), Some(0x1234));
        assert_eq!(bytes_needed(7, 16, ByteOrder::Big), Some(2));
        // 4 bits starting at bit 3 of byte 2 = low nibble of byte 2.
        let d = [0, 0, 0xA5, 0, 0, 0, 0, 0];
        assert_eq!(extract(&d, 19, 4, ByteOrder::Big), Some(0x5));
        // 13 bits from bit 4 of byte 0: low 5 bits of byte 0 + byte 1.
        let d = [0b1110_0001, 0xFF];
        assert_eq!(extract(&d, 4, 13, ByteOrder::Big), Some(0x1FFF & 0x01FF));
    }

    #[test]
    fn insert_round_trips_both_orders() {
        for (start, len, order) in [
            (1u16, 15u8, ByteOrder::Little),
            (60, 4, ByteOrder::Little),
            (4, 13, ByteOrder::Big),
            (23, 16, ByteOrder::Big),
            (53, 11, ByteOrder::Big),
        ] {
            let mut d = [0xAAu8; 8];
            let v = 0x1234_5678u64 & ((1u64 << len) - 1);
            assert!(insert(&mut d, start, len, order, v));
            assert_eq!(
                extract(&d, start, len, order),
                Some(v),
                "{start} {len} {order:?}"
            );
        }
        assert!(!insert(&mut [0u8; 2], 16, 8, ByteOrder::Little, 1));
    }

    #[test]
    fn sign_and_curve() {
        assert_eq!(sign_extend(0x80, 8), -128);
        assert_eq!(sign_extend(0x1FFF, 13), -1);
        // Regression: len 0 shifted a u64 by 64 (a panic in debug builds).
        assert_eq!(sign_extend(0xFFFF, 0), 0);
        assert_eq!(sign_extend(u64::MAX, 64), -1);
        assert_eq!(sign_extend(1, 1), -1);
        let lever = [[0.0, 0.0], [128.0, 0.0], [253.0, 100.0], [255.0, 100.0]];
        assert_eq!(interpolate(&lever, 0x7F as f64), 0.0);
        assert!((interpolate(&lever, 0xBF as f64) - 50.4).abs() < 1e-9);
        assert_eq!(interpolate(&lever, 254.0), 100.0);
        assert_eq!(interpolate(&lever, 999.0), 100.0);
    }
}
