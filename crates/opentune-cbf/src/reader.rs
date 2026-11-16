//! CAESAR bit-flag field reader.
//!
//! CBF records are packed as a bit-flag word followed by a sequence of
//! optional fields: for each field a bit in the flag word says whether the
//! field is present. Reading a field consumes the lowest remaining flag bit
//! (shifting the word right); if the bit is clear the field takes its default
//! value and nothing is read from the stream.
//!
//! This is a clean port of OpenVehicleDiag's `caesar::creader` /
//! `CaesarPrimitive` (GPL-3.0). Strings are stored out-of-line in a pool and
//! referenced by a relative offset; [`read_bitflag_string`] follows that
//! indirection and decodes the bytes as ISO-8859-10 (Latin-6), matching the
//! original tool's text handling.

use crate::error::Result;
use crate::raf::Raf;

/// Test the lowest bit of the flag word and shift it right by one.
///
/// Returns whether the field guarded by that bit is present.
fn check_and_advance(bit_flag: &mut u32) -> bool {
    let is_set = (*bit_flag & 1) > 0;
    *bit_flag >>= 1;
    is_set
}

/// Decode a NUL-less byte string from the CBF pool to a `String`.
///
/// CBF files are not consistent about their text encoding: modern UDS files use
/// ASCII, while some older KWP files store UTF-8. The upstream parser decodes
/// everything as ISO-8859-10 (Latin-6), which mangles the UTF-8 files. We
/// improve on that by trying UTF-8 first (a valid-UTF-8 run with high bytes is
/// almost never also a meaningful Latin-6 run) and only falling back to the
/// Latin-6 table when the bytes are not valid UTF-8.
pub fn decode_string(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => decode_latin6(bytes),
    }
}

/// Decode a slice strictly as ISO-8859-10 (Latin-6).
///
/// Bytes `0x00..=0x9F` map to the identical Unicode code points; `0xA0..=0xFF`
/// use the Latin-6 high-half table. This mirrors the `encoding_rs::ISO_8859_10`
/// decode the upstream parser uses, without pulling in that dependency.
pub fn decode_latin6(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| {
            if b < 0xA0 {
                b as char
            } else {
                ISO_8859_10_HIGH[(b - 0xA0) as usize]
            }
        })
        .collect()
}

/// If the next flag bit is set, read a 32-bit string offset, follow it to the
/// string pool at `base_addr`, decode a NUL-terminated ISO-8859-10 string and
/// restore the cursor. Returns an empty string when the bit is clear.
pub fn read_bitflag_string(
    bit_flag: &mut u32,
    reader: &mut Raf,
    base_addr: usize,
) -> Result<String> {
    if check_and_advance(bit_flag) {
        let string_offset = reader.read_i32()? as usize;
        let saved = reader.pos;
        reader.seek(string_offset + base_addr);
        let bytes = reader.read_cstr_bytes().unwrap_or_default();
        reader.seek(saved);
        Ok(decode_string(&bytes))
    } else {
        Ok(String::new())
    }
}

/// If the next flag bit is set, read a 32-bit offset, follow it to `base_addr`
/// and read `dump_size` raw bytes, restoring the cursor. Empty when the bit is
/// clear. Used for the opaque buffers inside variant patterns.
pub fn read_bitflag_dump(
    bit_flag: &mut u32,
    reader: &mut Raf,
    dump_size: usize,
    base_addr: usize,
) -> Result<Vec<u8>> {
    if check_and_advance(bit_flag) {
        let dump_offset = reader.read_i32()? as usize;
        let saved = reader.pos;
        reader.seek(dump_offset + base_addr);
        let res = reader.read_bytes(dump_size).unwrap_or_default();
        reader.seek(saved);
        Ok(res)
    } else {
        Ok(Vec::new())
    }
}

/// A packed `(count, offset)` pair used throughout CBF records to point at a
/// sub-pool of entries.
#[derive(Debug, Copy, Clone, Default)]
pub struct PoolTuple {
    /// Number of entries in the referenced pool.
    pub count: usize,
    /// Offset (relative to the record base) of the referenced pool.
    pub offset: usize,
}

impl PoolTuple {
    /// Read a `(count, offset)` pair as two bit-flagged `i32` fields.
    pub fn new_int(reader: &mut Raf, bf: &mut u32) -> Result<Self> {
        Ok(Self {
            count: read_primitive(bf, reader, 0i32)?.to_usize(),
            offset: read_primitive(bf, reader, 0i32)?.to_usize(),
        })
    }
}

/// Read one optional primitive field guarded by the next flag bit, returning
/// `default` when the bit is clear.
pub fn read_primitive<T: CaesarPrimitive>(
    bit_flag: &mut u32,
    reader: &mut Raf,
    default: T,
) -> Result<T> {
    T::read_bitflag(bit_flag, reader, default)
}

/// A numeric type that can be read as an optional bit-flagged CBF field.
pub trait CaesarPrimitive: Sized {
    /// Read this value if the next flag bit is set, else return `default`.
    fn read_bitflag(bit_flag: &mut u32, reader: &mut Raf, default: Self) -> Result<Self>;
    /// Widen to `usize` (used for offsets/counts; negatives saturate to 0).
    fn to_usize(&self) -> usize;
}

macro_rules! impl_caesar_primitive {
    ($t:ty, $read:ident) => {
        impl CaesarPrimitive for $t {
            fn read_bitflag(bit_flag: &mut u32, reader: &mut Raf, default: Self) -> Result<Self> {
                if check_and_advance(bit_flag) {
                    Ok(reader.$read().unwrap_or(default))
                } else {
                    Ok(default)
                }
            }
            fn to_usize(&self) -> usize {
                if *self < 0 as $t {
                    0
                } else {
                    *self as usize
                }
            }
        }
    };
}

impl_caesar_primitive!(f32, read_f32);
impl_caesar_primitive!(i32, read_i32);
impl_caesar_primitive!(u32, read_u32);
impl_caesar_primitive!(i16, read_i16);
impl_caesar_primitive!(u16, read_u16);
impl_caesar_primitive!(i8, read_i8);
impl_caesar_primitive!(u8, read_u8);

/// ISO-8859-10 (Latin-6) high half, code points for bytes `0xA0..=0xFF`.
#[rustfmt::skip]
const ISO_8859_10_HIGH: [char; 96] = [
    '\u{00A0}', '\u{0104}', '\u{0112}', '\u{0122}', '\u{012A}', '\u{0128}', '\u{0136}', '\u{00A7}',
    '\u{013B}', '\u{0110}', '\u{0160}', '\u{0166}', '\u{017D}', '\u{00AD}', '\u{016A}', '\u{014A}',
    '\u{00B0}', '\u{0105}', '\u{0113}', '\u{0123}', '\u{012B}', '\u{0129}', '\u{0137}', '\u{00B7}',
    '\u{013C}', '\u{0111}', '\u{0161}', '\u{0167}', '\u{017E}', '\u{2015}', '\u{016B}', '\u{014B}',
    '\u{0100}', '\u{00C1}', '\u{00C2}', '\u{00C3}', '\u{00C4}', '\u{00C5}', '\u{00C6}', '\u{012E}',
    '\u{010C}', '\u{00C9}', '\u{0118}', '\u{00CB}', '\u{0116}', '\u{00CD}', '\u{00CE}', '\u{00CF}',
    '\u{00D0}', '\u{0145}', '\u{014C}', '\u{00D3}', '\u{00D4}', '\u{00D5}', '\u{00D6}', '\u{0168}',
    '\u{00D8}', '\u{0172}', '\u{00DA}', '\u{00DB}', '\u{00DC}', '\u{00DD}', '\u{00DE}', '\u{00DF}',
    '\u{0101}', '\u{00E1}', '\u{00E2}', '\u{00E3}', '\u{00E4}', '\u{00E5}', '\u{00E6}', '\u{012F}',
    '\u{010D}', '\u{00E9}', '\u{0119}', '\u{00EB}', '\u{0117}', '\u{00ED}', '\u{00EE}', '\u{00EF}',
    '\u{00F0}', '\u{0146}', '\u{014D}', '\u{00F3}', '\u{00F4}', '\u{00F5}', '\u{00F6}', '\u{0169}',
    '\u{00F8}', '\u{0173}', '\u{00FA}', '\u{00FB}', '\u{00FC}', '\u{00FD}', '\u{00FE}', '\u{0138}',
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitflag_present_and_absent() {
        // flag 0b01: first field present (reads 7), second absent (default 99).
        let mut r = Raf::from_bytes(&[0x07, 0x00, 0x00, 0x00]);
        let mut flags = 0b01u32;
        assert_eq!(read_primitive(&mut flags, &mut r, 0i32).unwrap(), 7);
        assert_eq!(read_primitive(&mut flags, &mut r, 99i32).unwrap(), 99);
    }

    #[test]
    fn negative_to_usize_saturates() {
        assert_eq!((-1i32).to_usize(), 0);
        assert_eq!(5i32.to_usize(), 5);
    }

    #[test]
    fn latin6_ascii_and_high() {
        assert_eq!(decode_latin6(b"Sensor"), "Sensor");
        // 0xC4 -> U+00C4 'Ä' in Latin-6.
        assert_eq!(decode_latin6(&[0xC4]), "Ä");
    }

    #[test]
    fn decode_prefers_utf8_then_falls_back() {
        // Valid UTF-8 "Fühler" (ü = 0xC3 0xBC) decodes correctly, not mojibake.
        assert_eq!(decode_string(&[0x46, 0xC3, 0xBC, 0x68]), "Füh");
        // A lone 0xC4 is not valid UTF-8, so it falls back to Latin-6 'Ä'.
        assert_eq!(decode_string(&[0xC4, 0x20]), "Ä ");
        // Plain ASCII is unchanged either way.
        assert_eq!(decode_string(b"Overvoltage"), "Overvoltage");
    }
}
