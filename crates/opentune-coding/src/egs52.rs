//! Candidate CRC for the 42-byte EGS52 coding record. Unconfirmed on real EGS units.

/// Width-16 CRC parameters in normal (MSB-first polynomial) notation.
#[derive(Debug, Clone, Copy)]
pub struct Crc16 {
    /// Polynomial without the implicit x^16 term.
    pub poly: u16,
    /// Initial register.
    pub init: u16,
    /// Reflect each input byte.
    pub refin: bool,
    /// Reflect the final register.
    pub refout: bool,
    /// Final XOR mask.
    pub xorout: u16,
}

impl Default for Crc16 {
    fn default() -> Self {
        Self {
            poly: 0x8005,
            init: 0,
            refin: true,
            refout: true,
            xorout: 0,
        }
    }
}

impl Crc16 {
    /// Calculate the CRC of a byte slice.
    pub fn calculate(self, bytes: &[u8]) -> u16 {
        let mut crc = self.init;
        for &byte in bytes {
            let byte = if self.refin {
                byte.reverse_bits()
            } else {
                byte
            };
            crc ^= u16::from(byte) << 8;
            for _ in 0..8 {
                crc = if crc & 0x8000 != 0 {
                    (crc << 1) ^ self.poly
                } else {
                    crc << 1
                };
            }
        }
        (if self.refout { crc.reverse_bits() } else { crc }) ^ self.xorout
    }

    /// Return the stored and candidate CRC, respectively. Equality verifies the candidate.
    pub fn verify(self, record: &[u8; 42]) -> (u16, u16) {
        (
            u16::from_le_bytes([record[40], record[41]]),
            self.calculate(&record[..40]),
        )
    }

    /// Replace just the final little-endian CRC after explicit acknowledgement.
    pub fn recompute(
        self,
        record: &mut [u8; 42],
        allow_unconfirmed: bool,
    ) -> Result<(), &'static str> {
        if !allow_unconfirmed {
            return Err("EGS52 coding CRC is unconfirmed on a real EGS; recompute needs --allow-unconfirmed");
        }
        let (_, expected) = self.verify(record);
        record[40..].copy_from_slice(&expected.to_le_bytes());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_vectors() {
        let arc = Crc16::default();
        assert_eq!(arc.calculate(b"123456789"), 0xbb3d);
        let genibus = Crc16 {
            poly: 0x1021,
            init: 0xffff,
            refin: false,
            refout: false,
            xorout: 0xffff,
        };
        assert_eq!(genibus.calculate(b"123456789"), 0xd64e);
    }

    #[test]
    fn trailing_crc_and_opt_in() {
        let crc = Crc16::default();
        // Leading zero bytes leave ARC's zero initial state unchanged.
        let mut record = [0; 42];
        record[31..40].copy_from_slice(b"123456789");
        let before = record;
        assert!(crc.recompute(&mut record, false).is_err());
        assert_eq!(before, record);
        crc.recompute(&mut record, true).unwrap();
        assert_eq!(&record[..40], &before[..40]);
        assert_eq!(&record[40..], &[0x3d, 0xbb]);
        assert_eq!(crc.verify(&record), (0xbb3d, 0xbb3d));
        record[0] ^= 1;
        let (stored, expected) = crc.verify(&record);
        assert_ne!(stored, expected);
    }
}
