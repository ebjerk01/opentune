//! Caller-supplied-key cryptography for VAG flash containers.
//!
//! ## No keys are bundled
//!
//! This crate ships **no** key material of any kind. Every scheme below takes
//! its key/IV/table from the caller. In particular the FRF container key
//! (`frf.key`), the DSG substitution table, the ZF-8HP repeating-XOR string and
//! the Simos AES keys are all recovered material with legal/security concerns;
//! OpenTune never hardcodes them. Supply them at the API boundary.
//!
//! Algorithms ported from the open-source references (see crate root): the FRF
//! rolling-XOR and Simos-XOR from `bri3d/VW_Flash` (BSD-2); the DSG progressive
//! substitution from `VW_Flash` (BSD-2); AES-CBC usage and the repeating-XOR
//! from `dspl1236/vag-tcu-tools` (MIT).

use crate::error::{Error, Result};
use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;

/// The block-level cipher a flash block was encrypted with. `None` passes data
/// through unchanged (method encryption char `0`).
#[derive(Debug, Clone, Default)]
pub enum BlockCipher {
    /// No encryption.
    #[default]
    None,
    /// AES-128-CBC with a caller-supplied key and IV (Simos / DQ381-BL301 / DQ500).
    Aes128Cbc {
        /// 16-byte AES-128 key.
        key: [u8; 16],
        /// 16-byte CBC IV.
        iv: [u8; 16],
    },
    /// Repeating-key XOR (ZF-8HP method `0x22`). The key is the caller-supplied
    /// ASCII/byte string.
    RepeatingXor(Vec<u8>),
    /// Simos counter XOR: byte `n` is XORed with `n % 256`.
    SimosCounterXor,
    /// DSG progressive substitution cipher with a caller-supplied 256-byte table
    /// (`mqb_dsg_key.bin`).
    Dsg(Box<[u8; 256]>),
}

impl BlockCipher {
    /// Decrypt a block. CBC input that is not a multiple of 16 bytes is an error.
    pub fn decrypt(&self, data: &[u8]) -> Result<Vec<u8>> {
        match self {
            BlockCipher::None => Ok(data.to_vec()),
            BlockCipher::Aes128Cbc { key, iv } => aes128_cbc_decrypt(key, iv, data),
            BlockCipher::RepeatingXor(k) => repeating_xor(k, data),
            BlockCipher::SimosCounterXor => Ok(simos_counter_xor(data)),
            BlockCipher::Dsg(table) => Ok(dsg_decrypt(table, data)),
        }
    }

    /// Encrypt a block (repack). CBC zero-pads to a 16-byte boundary, matching
    /// the reference tooling.
    pub fn encrypt(&self, data: &[u8]) -> Result<Vec<u8>> {
        match self {
            BlockCipher::None => Ok(data.to_vec()),
            BlockCipher::Aes128Cbc { key, iv } => Ok(aes128_cbc_encrypt(key, iv, data)),
            BlockCipher::RepeatingXor(k) => repeating_xor(k, data),
            BlockCipher::SimosCounterXor => Ok(simos_counter_xor(data)),
            BlockCipher::Dsg(table) => dsg_encrypt(table, data),
        }
    }
}

/// The FRF container rolling-XOR stream cipher (port of `VW_Flash`
/// `decrypt_data`). The keystream depends only on the key material and byte
/// position, so the transform is symmetric: the same call encrypts and decrypts.
///
/// `key_material` is the caller-supplied `frf.key` bytes (never bundled).
pub fn frf_xor(key_material: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    if key_material.is_empty() {
        return Err(Error::MissingKey("frf key material is empty"));
    }
    let mut out = Vec::with_capacity(data.len());
    let mut key_index = 0usize;
    let mut first_seed: u32 = 0;
    let mut second_seed: u32 = 1;
    for &byte in data {
        let key_byte = key_material[key_index] as u32;
        first_seed = ((first_seed + key_byte) * 3) & 0xff;
        let decrypted = byte ^ ((first_seed ^ 0xff ^ second_seed ^ key_byte) as u8);
        out.push(decrypted);
        second_seed = ((second_seed + 1) * first_seed) & 0xff;
        key_index = (key_index + 1) % key_material.len();
    }
    Ok(out)
}

/// Repeating-key XOR (symmetric). Errors on an empty key.
pub fn repeating_xor(key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    if key.is_empty() {
        return Err(Error::MissingKey("repeating-xor key is empty"));
    }
    Ok(data
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ key[i % key.len()])
        .collect())
}

/// Simos counter XOR (symmetric): byte `n` XOR `n % 256`.
pub fn simos_counter_xor(data: &[u8]) -> Vec<u8> {
    data.iter()
        .enumerate()
        .map(|(i, b)| b ^ (i as u8))
        .collect()
}

/// AES-128-CBC decrypt. `data` must be a whole number of 16-byte blocks.
pub fn aes128_cbc_decrypt(key: &[u8; 16], iv: &[u8; 16], data: &[u8]) -> Result<Vec<u8>> {
    if data.len() % 16 != 0 {
        return Err(Error::Unsupported(format!(
            "AES-CBC input must be a multiple of 16 bytes, got {}",
            data.len()
        )));
    }
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut prev = *iv;
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks_exact(16) {
        let mut block = GenericArray::clone_from_slice(chunk);
        cipher.decrypt_block(&mut block);
        for (b, p) in block.iter_mut().zip(prev.iter()) {
            *b ^= *p;
        }
        out.extend_from_slice(&block);
        prev.copy_from_slice(chunk);
    }
    Ok(out)
}

/// AES-128-CBC encrypt, zero-padding to a 16-byte boundary.
pub fn aes128_cbc_encrypt(key: &[u8; 16], iv: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let pad = (16 - data.len() % 16) % 16;
    let mut buf = data.to_vec();
    buf.extend(std::iter::repeat(0u8).take(pad));
    let mut prev = *iv;
    let mut out = Vec::with_capacity(buf.len());
    for chunk in buf.chunks_exact(16) {
        let mut block = [0u8; 16];
        for i in 0..16 {
            block[i] = chunk[i] ^ prev[i];
        }
        let mut ga = GenericArray::clone_from_slice(&block);
        cipher.encrypt_block(&mut ga);
        prev.copy_from_slice(&ga);
        out.extend_from_slice(&ga);
    }
    out
}

/// DSG progressive substitution decrypt (port of `VW_Flash` `DSG.decrypt`).
/// `table` is the caller-supplied 256-byte substitution table.
pub fn dsg_decrypt(table: &[u8; 256], data: &[u8]) -> Vec<u8> {
    let mut offset: u32 = 0;
    let mut rolling: u32 = 0;
    let mut last: u32 = 0;
    let mut out = Vec::with_capacity(data.len());
    for &d in data {
        let cipher = table[((d as u32 + offset) & 0xff) as usize] as u32;
        offset = offset.wrapping_add(cipher).wrapping_add(last);
        rolling = rolling.wrapping_add(0x167);
        offset = offset.wrapping_add(table[((rolling >> 8) & 0xff) as usize] as u32);
        offset &= 0xff;
        last = cipher;
        out.push(cipher as u8);
    }
    out
}

/// DSG progressive substitution encrypt (inverse of [`dsg_decrypt`]).
pub fn dsg_encrypt(table: &[u8; 256], data: &[u8]) -> Result<Vec<u8>> {
    // Build the inverse of the substitution table.
    let mut inverse = [0u8; 256];
    let mut seen = [false; 256];
    for (i, &v) in table.iter().enumerate() {
        inverse[v as usize] = i as u8;
        seen[v as usize] = true;
    }
    if !seen.iter().all(|&s| s) {
        return Err(Error::MissingKey("DSG table is not a 256-byte permutation"));
    }
    let mut offset: u32 = 0;
    let mut rolling: u32 = 0;
    let mut last: u32 = 0;
    let mut out = Vec::with_capacity(data.len());
    for &plain in data {
        let match_index = inverse[plain as usize] as u32;
        let cipher = (match_index.wrapping_sub(offset)) & 0xff;
        offset = offset.wrapping_add(plain as u32).wrapping_add(last);
        rolling = rolling.wrapping_add(0x167);
        offset = offset.wrapping_add(table[((rolling >> 8) & 0xff) as usize] as u32);
        offset &= 0xff;
        last = plain as u32;
        out.push(cipher as u8);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frf_xor_is_symmetric() {
        // Arbitrary synthetic key material — NOT a real frf.key.
        let key: Vec<u8> = (0..251u16).map(|i| (i * 7 % 256) as u8).collect();
        let plain = b"PK\x03\x04 this pretends to be a zip payload \x00\x01\x02";
        let enc = frf_xor(&key, plain).unwrap();
        let dec = frf_xor(&key, &enc).unwrap();
        assert_eq!(dec, plain);
        assert_ne!(&enc[..], &plain[..]);
    }

    #[test]
    fn aes_cbc_roundtrip() {
        let key = [0x11u8; 16];
        let iv = [0x22u8; 16];
        let data: Vec<u8> = (0..64u8).collect();
        let enc = aes128_cbc_encrypt(&key, &iv, &data);
        let dec = aes128_cbc_decrypt(&key, &iv, &enc).unwrap();
        assert_eq!(dec, data);
    }

    #[test]
    fn aes_cbc_known_answer() {
        // NIST-style sanity: single zero block with zero key/iv.
        let key = [0u8; 16];
        let iv = [0u8; 16];
        let ct = aes128_cbc_encrypt(&key, &iv, &[0u8; 16]);
        // AES-128 encrypt of all-zero block with all-zero key (well-known vector).
        assert_eq!(
            ct,
            [
                0x66, 0xe9, 0x4b, 0xd4, 0xef, 0x8a, 0x2c, 0x3b, 0x88, 0x4c, 0xfa, 0x59, 0xca, 0x34,
                0x2b, 0x2e
            ]
        );
    }

    #[test]
    fn repeating_xor_roundtrip() {
        let key = b"CyA2008ZFVAGtcuxsam"; // shape of a ZF-8HP key; supplied by caller
        let data: Vec<u8> = (0..300u16).map(|i| i as u8).collect();
        let enc = repeating_xor(key, &data).unwrap();
        let dec = repeating_xor(key, &enc).unwrap();
        assert_eq!(dec, data);
    }

    #[test]
    fn simos_counter_xor_roundtrip() {
        let data: Vec<u8> = (0..1000u16).map(|i| (i * 3) as u8).collect();
        let enc = simos_counter_xor(&data);
        let dec = simos_counter_xor(&enc);
        assert_eq!(dec, data);
    }

    #[test]
    fn dsg_roundtrip() {
        // Synthetic permutation table — NOT the real mqb_dsg_key.bin.
        let mut table = [0u8; 256];
        for (i, t) in table.iter_mut().enumerate() {
            *t = ((i * 167 + 13) & 0xff) as u8;
        }
        // Ensure it is a permutation (167 is coprime with 256 so it already is).
        let data: Vec<u8> = (0..500u16).map(|i| (i ^ 0x5a) as u8).collect();
        let enc = dsg_encrypt(&table, &data).unwrap();
        let dec = dsg_decrypt(&table, &enc);
        assert_eq!(dec, data);
    }
}
