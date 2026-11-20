//! SA2 script text / file decoding shared by the SA2 panel and the
//! `sa2:@<file>` seed/key ids: hex or assembler text, raw bytes, size caps.

use opentune_sa2::{assemble_text, parse_hex};
use std::path::Path;

/// Longest script the panel accepts. Real SA2 scripts are tens of bytes.
pub const MAX_SCRIPT: usize = 4096;
/// Largest script file read (raw bytes, hex text or assembler source).
pub const MAX_SCRIPT_FILE: u64 = 1 << 20;

const MNEMONICS: [&str; 11] = [
    "rsl", "rsr", "add", "sub", "eor", "loop", "next", "bcc", "bra", "finish", ".byte",
];

/// Whether `text` has an SA2 mnemonic as a word. Hex alone cannot tell
/// `add 5` (assembler) from the bytes `AD D5`, so a mnemonic marks assembler.
fn has_mnemonic(text: &str) -> bool {
    text.split(|c: char| c.is_whitespace() || c == ',')
        .any(|w| MNEMONICS.contains(&w.to_ascii_lowercase().as_str()))
}

/// Decode script text: hex, or assembler source (any mnemonic word, or text
/// that is not hex). Returns the bytes and which form it was.
pub fn script_from_text(text: &str) -> Result<(Vec<u8>, &'static str), String> {
    if text.trim().is_empty() {
        return Err("enter a script: hex bytes (e.g. 87 5A 5A 5A 5A 4C) or SA2 assembler".into());
    }
    let asm = has_mnemonic(text);
    if !asm {
        if let Ok(b) = parse_hex(text) {
            return Ok((b, "hex"));
        }
    }
    assemble_text(text).map(|b| (b, "assembler")).map_err(|e| {
        if asm {
            format!("SA2 assembler: {e}")
        } else {
            format!("not hex or SA2 assembler: {e}")
        }
    })
}

/// Decode a script file: raw bytes when it is not text, else hex or assembler.
pub fn script_from_file_bytes(raw: Vec<u8>) -> Result<(Vec<u8>, &'static str), String> {
    let text = std::str::from_utf8(&raw)
        .ok()
        .filter(|t| t.chars().all(|c| !c.is_control() || c.is_whitespace()));
    match text {
        None => Ok((raw, "raw bytes")),
        Some(t) => script_from_text(t),
    }
}

/// Read at most `cap` bytes of `path`, refusing larger files.
pub fn read_capped(path: &str, cap: u64, what: &str) -> Result<Vec<u8>, String> {
    let p = Path::new(path.trim());
    let len = std::fs::metadata(p)
        .map_err(|e| format!("{what} {}: {e}", p.display()))?
        .len();
    if len > cap {
        return Err(format!(
            "{what} {} is {len} bytes — larger than the {cap}-byte limit",
            p.display()
        ));
    }
    std::fs::read(p).map_err(|e| format!("{what} {}: {e}", p.display()))
}

pub fn check_len(bytes: &[u8]) -> Result<(), String> {
    if bytes.is_empty() {
        return Err("the script is empty".into());
    }
    if bytes.len() > MAX_SCRIPT {
        return Err(format!(
            "{} bytes is longer than any SA2 script ({MAX_SCRIPT}-byte limit) — is this the right field?",
            bytes.len()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_hex_or_assembler() {
        assert_eq!(script_from_text("0x87,0xFF ff:ff FF 4c").unwrap().1, "hex");
        let (b, f) = script_from_text("87 FF FF FF FF 4C").unwrap();
        assert_eq!((b.len(), f), (6, "hex"));
        // "add 5" would be the hex bytes AD D5; it is read as assembler.
        let (b, f) = script_from_text("add 5\nfinish").unwrap();
        assert_eq!((b, f), (vec![0x93, 0, 0, 0, 5, 0x4C], "assembler"));
        assert!(script_from_text("  ").is_err());
        assert!(script_from_text("zz").unwrap_err().contains("not hex"));
        assert!(script_from_text("frobnicate 1 ; x")
            .unwrap_err()
            .contains("assembler"));
        assert!(script_from_text("eor 1 2")
            .unwrap_err()
            .starts_with("SA2 assembler"));
    }
}
