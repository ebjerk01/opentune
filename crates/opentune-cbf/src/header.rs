//! Container headers: stub header, CFF header, CTF header and the CTF language
//! string pool.
//!
//! Ported from OpenVehicleDiag's `ctf` and `caesar::container` modules
//! (GPL-3.0). A CBF file begins with a fixed-size stub header, followed by the
//! CFF header (which locates the ECU table, the string-pool size and the CTF
//! block) and the CTF header (which holds one or more language string pools).

use crate::error::{CbfError, Result};
use crate::raf::Raf;
use crate::reader::{clamp_off, read_bitflag_string, read_primitive, MAX_PREALLOC};

/// Size of the fixed stub header at the very start of the file.
pub const STUB_HEADER_SIZE: usize = 0x410;

const FILE_HEADER: &[u8] = b"CBF-TRANSLATOR-VERSION:04.00";

/// Validate the fixed stub header. Returns the version string read from it.
pub fn check_stub_header(header: &[u8]) -> Result<String> {
    if header.len() < STUB_HEADER_SIZE {
        return Err(CbfError::UnexpectedEof);
    }
    if !header.starts_with(FILE_HEADER) {
        let got = String::from_utf8_lossy(&header[..FILE_HEADER.len().min(header.len())]);
        return Err(CbfError::BadMagic(format!(
            "expected CBF-TRANSLATOR-VERSION:04.00, found {got:?}"
        )));
    }
    if header[0x401] != 3 {
        return Err(CbfError::BadMagic(format!(
            "unexpected CBF magic id {} (expected 3)",
            header[0x401]
        )));
    }
    // The version banner is an ASCII run at the start of the stub header.
    let end = header[..0x40]
        .iter()
        .position(|&b| b == b'\n' || b == 0)
        .unwrap_or(0x1C);
    Ok(String::from_utf8_lossy(&header[..end]).trim().to_string())
}

/// The CFF header — the file's table of contents.
#[derive(Debug, Clone, Default)]
pub struct CffHeader {
    pub ecu_count: i32,
    pub ecu_offset: i32,
    pub ctf_offset: i32,
    pub string_pool_size: i32,
    pub cff_header_size: i32,
    pub base_addr: usize,
    pub xml_string: String,
}

impl CffHeader {
    /// Parse the CFF header. The reader is positioned anywhere on entry.
    pub fn new(reader: &mut Raf) -> Result<CffHeader> {
        reader.seek(STUB_HEADER_SIZE);

        let cff_header_size = reader.read_i32()?;
        let base_addr = reader.pos;
        let mut bitflags = reader.read_u16()? as u32;

        let _caesar_version = read_primitive(&mut bitflags, reader, 0i32)?;
        let _gpd_version = read_primitive(&mut bitflags, reader, 0i32)?;
        let ecu_count = read_primitive(&mut bitflags, reader, 0i32)?;
        let ecu_offset = read_primitive(&mut bitflags, reader, 0i32)?;
        let ctf_offset = read_primitive(&mut bitflags, reader, 0i32)?;
        let string_pool_size = read_primitive(&mut bitflags, reader, 0i32)?;
        let _dsc_offset = read_primitive(&mut bitflags, reader, 0i32)?;
        let _dsc_count = read_primitive(&mut bitflags, reader, 0i32)?;
        let _dsc_entry_size = read_primitive(&mut bitflags, reader, 0i32)?;

        let _cbf_version_string = read_bitflag_string(&mut bitflags, reader, base_addr)?;
        let _gpd_version_string = read_bitflag_string(&mut bitflags, reader, base_addr)?;
        let xml_string = read_bitflag_string(&mut bitflags, reader, base_addr)?;

        Ok(CffHeader {
            ecu_count,
            ecu_offset,
            ctf_offset,
            string_pool_size,
            cff_header_size,
            base_addr,
            xml_string,
        })
    }
}

/// The CTF header: a set of language string pools. Only the pools are needed by
/// the rest of the parser (descriptions index into one of them).
#[derive(Debug, Clone, Default)]
pub struct CtfHeader {
    pub languages: Vec<CtfLanguage>,
}

impl CtfHeader {
    /// Parse the CTF header at `base_addr`. `header_size` is the CFF header size
    /// (needed to locate the shared string table).
    pub fn new(reader: &mut Raf, base_addr: usize, header_size: usize) -> Result<Self> {
        reader.seek(base_addr);
        let mut bit_flag = reader.read_u16()? as u32;

        let _unk1 = read_primitive(&mut bit_flag, reader, 0i32)?;
        let _qualifier = read_bitflag_string(&mut bit_flag, reader, base_addr)?;
        let _unk3 = read_primitive(&mut bit_flag, reader, 0i16)?;
        let _unk4 = read_primitive(&mut bit_flag, reader, 0i32)?;
        let language_count = read_primitive(&mut bit_flag, reader, 0i32)?;
        let language_table_offset = read_primitive(&mut bit_flag, reader, 0i32)?;
        let _unk7 = read_bitflag_string(&mut bit_flag, reader, base_addr)?;

        let lang_table_offset_relative = clamp_off(language_table_offset).saturating_add(base_addr);
        // Real CBF files declare a small handful of languages. Clamp the count so
        // a hostile value cannot drive an unbounded loop / allocation.
        let lang_count = clamp_off(language_count).min(MAX_PREALLOC);
        let mut languages = Vec::with_capacity(lang_count);
        for i in 0..lang_count {
            reader.seek(lang_table_offset_relative.saturating_add(i.saturating_mul(4)));
            let entry_addr =
                clamp_off(reader.read_i32()?).saturating_add(lang_table_offset_relative);
            languages.push(CtfLanguage::new(reader, entry_addr, header_size)?);
        }
        Ok(CtfHeader { languages })
    }

    /// Borrow the language at `idx` (CBF files always have at least one).
    pub fn language(&self, idx: usize) -> Result<&CtfLanguage> {
        self.languages
            .get(idx)
            .ok_or_else(|| CbfError::Malformed(format!("no CTF language at index {idx}")))
    }
}

/// One language's string pool. Descriptions elsewhere in the file are indices
/// into [`CtfLanguage::strings`].
#[derive(Debug, Clone, Default)]
pub struct CtfLanguage {
    pub strings: Vec<String>,
}

impl CtfLanguage {
    fn new(reader: &mut Raf, base_addr: usize, header_size: usize) -> Result<Self> {
        reader.seek(base_addr);
        let mut flags = reader.read_u16()? as u32;

        let _qualifier = read_bitflag_string(&mut flags, reader, base_addr)?;
        let _language_index = read_primitive(&mut flags, reader, 0i16)?;
        let _string_pool_size = read_primitive(&mut flags, reader, 0i32)?;
        let _offset_string_pool_base = read_primitive(&mut flags, reader, 0i32)?;
        let string_count = clamp_off(read_primitive(&mut flags, reader, 0i32)?);

        let mut res = CtfLanguage {
            // Bound the eager reservation; the loop in `load_strings` stops at the
            // first out-of-range read, so a false count cannot over-allocate.
            strings: Vec::with_capacity(string_count.min(MAX_PREALLOC)),
        };
        res.load_strings(reader, string_count, header_size)?;
        Ok(res)
    }

    fn load_strings(
        &mut self,
        reader: &mut Raf,
        string_count: usize,
        header_size: usize,
    ) -> Result<()> {
        let table_offset = header_size
            .saturating_add(STUB_HEADER_SIZE)
            .saturating_add(4);
        self.strings.clear();
        for i in 0..string_count {
            reader.seek(table_offset.saturating_add(i.saturating_mul(4)));
            let string_offset = clamp_off(reader.read_i32()?);
            reader.seek(table_offset.saturating_add(string_offset));
            let bytes = reader.read_cstr_bytes()?;
            self.strings.push(crate::reader::decode_string(&bytes));
        }
        Ok(())
    }

    /// Resolve a (possibly negative / sentinel) string index to text.
    ///
    /// A negative index means "no string" and yields `None`.
    pub fn get_string(&self, idx: i32) -> Option<String> {
        if idx < 0 {
            return None;
        }
        self.strings.get(idx as usize).cloned()
    }
}
