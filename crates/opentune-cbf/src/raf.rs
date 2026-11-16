//! Little-endian random-access byte reader.
//!
//! A trimmed, self-contained port of OpenVehicleDiag's `common::raf::Raf`
//! (GPL-3.0). CBF files are always little-endian, so unlike the upstream reader
//! this one hard-codes LE and only keeps the operations the CBF parser needs.

use crate::error::{CbfError, Result};

/// A cursor over an in-memory byte buffer with explicit `seek`/`pos`.
///
/// All multi-byte integers are read little-endian, matching the CBF on-disk
/// layout. Out-of-range reads return [`CbfError::UnexpectedEof`] rather than
/// panicking.
#[derive(Debug, Clone, Default)]
pub struct Raf {
    data: Vec<u8>,
    /// Current read position (byte offset from the start of the buffer).
    pub pos: usize,
}

impl Raf {
    /// Wrap a byte buffer. The buffer is copied so the reader owns its data.
    pub fn from_bytes(data: &[u8]) -> Self {
        Raf {
            data: data.to_vec(),
            pos: 0,
        }
    }

    /// Move the cursor to an absolute position. A position past the end is
    /// permitted; the error surfaces on the next read, matching the upstream
    /// reader's behaviour.
    pub fn seek(&mut self, pos: usize) {
        self.pos = pos;
    }

    /// Read `num_bytes` bytes, advancing the cursor.
    pub fn read_bytes(&mut self, num_bytes: usize) -> Result<Vec<u8>> {
        let end = self
            .pos
            .checked_add(num_bytes)
            .ok_or(CbfError::UnexpectedEof)?;
        if end > self.data.len() {
            return Err(CbfError::UnexpectedEof);
        }
        let res = self.data[self.pos..end].to_vec();
        self.pos = end;
        Ok(res)
    }

    /// Read a NUL-terminated byte string (the terminator is consumed but not
    /// returned).
    pub fn read_cstr_bytes(&mut self) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        loop {
            let b = self.read_u8()?;
            if b == 0 {
                return Ok(bytes);
            }
            bytes.push(b);
        }
    }

    fn read_array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let end = self.pos.checked_add(N).ok_or(CbfError::UnexpectedEof)?;
        if end > self.data.len() {
            return Err(CbfError::UnexpectedEof);
        }
        let arr: [u8; N] = self.data[self.pos..end]
            .try_into()
            .expect("slice length checked above");
        self.pos = end;
        Ok(arr)
    }

    /// Read one unsigned byte.
    pub fn read_u8(&mut self) -> Result<u8> {
        Ok(self.read_array::<1>()?[0])
    }

    /// Read one signed byte.
    pub fn read_i8(&mut self) -> Result<i8> {
        Ok(self.read_u8()? as i8)
    }

    /// Read a little-endian `u16`.
    pub fn read_u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.read_array::<2>()?))
    }

    /// Read a little-endian `i16`.
    pub fn read_i16(&mut self) -> Result<i16> {
        Ok(i16::from_le_bytes(self.read_array::<2>()?))
    }

    /// Read a little-endian `u32`.
    pub fn read_u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.read_array::<4>()?))
    }

    /// Read a little-endian `i32`.
    pub fn read_i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.read_array::<4>()?))
    }

    /// Read a little-endian `f32`.
    pub fn read_f32(&mut self) -> Result<f32> {
        Ok(f32::from_le_bytes(self.read_array::<4>()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_little_endian() {
        let mut r = Raf::from_bytes(&[0x01, 0x00, 0x00, 0x00, 0x02, 0x00]);
        assert_eq!(r.read_i32().unwrap(), 1);
        assert_eq!(r.read_u16().unwrap(), 2);
    }

    #[test]
    fn seek_and_cstr() {
        let mut r = Raf::from_bytes(b"AB\0CD\0");
        assert_eq!(r.read_cstr_bytes().unwrap(), b"AB");
        r.seek(3);
        assert_eq!(r.read_cstr_bytes().unwrap(), b"CD");
    }

    #[test]
    fn overflow_is_error() {
        let mut r = Raf::from_bytes(&[0x00, 0x01]);
        assert!(r.read_i32().is_err());
        r.seek(100);
        assert!(r.read_u8().is_err());
    }
}
