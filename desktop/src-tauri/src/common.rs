//! Small shared helpers: hex parsing / formatting, number parsing and
//! user-facing file-write errors.

/// Uppercase space-separated hex.
pub(crate) fn hexs(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parse a whitespace-tolerant hex string.
pub(crate) fn parse_hex(s: &str) -> Result<Vec<u8>, String> {
    let clean: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if !clean.len().is_multiple_of(2) {
        return Err(format!("hex string '{s}' has an odd number of digits"));
    }
    (0..clean.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&clean[i..i + 2], 16).map_err(|_| format!("invalid hex in '{s}'"))
        })
        .collect()
}

pub(crate) fn parse_u32(s: &str) -> Result<u32, String> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u32::from_str_radix(h, 16).map_err(|e| e.to_string())
    } else {
        s.parse::<u32>().map_err(|e| e.to_string())
    }
}

/// Map a filesystem error to a clear, user-facing message, flagging the common
/// read-only / permission case so the UI can offer a "Save As…" fallback.
pub(crate) fn write_error(path: &str, e: &std::io::Error) -> String {
    use std::io::ErrorKind::*;
    match e.kind() {
        PermissionDenied => format!(
            "permission denied writing {path} — the file is read-only or owned by another user. Use Save As… to write a copy you own.",
        ),
        _ => format!("could not write {path}: {e}"),
    }
}

/// Parse a hex blob: ignores spaces, `0x`, `-`, `:`; errors on odd length / junk.
pub(crate) fn parse_hex_bytes(s: &str) -> Result<Vec<u8>, String> {
    let cleaned: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if cleaned.is_empty() {
        return Err("empty seed".into());
    }
    if !cleaned.len().is_multiple_of(2) {
        return Err("hex has an odd number of digits".into());
    }
    (0..cleaned.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&cleaned[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_parsing() {
        assert_eq!(parse_hex_bytes("AA BB CC").unwrap(), vec![0xAA, 0xBB, 0xCC]);
        assert_eq!(
            parse_hex_bytes("de:ad:be:ef").unwrap(),
            vec![0xDE, 0xAD, 0xBE, 0xEF]
        );
        assert!(parse_hex_bytes("ABC").is_err()); // odd
        assert!(parse_hex_bytes("").is_err());
    }
}

/// Test helpers shared by the file-writing tests (scratch dirs, vault listing).
#[cfg(test)]
pub(crate) mod testutil {
    use std::path::PathBuf;

    /// A per-test scratch directory under the system temp dir, removed on drop.
    pub(crate) struct TmpDir(PathBuf);

    impl TmpDir {
        pub(crate) fn new(tag: &str) -> Self {
            let mut p = std::env::temp_dir();
            let uniq = format!(
                "otproj-test-{tag}-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            p.push(uniq);
            std::fs::create_dir_all(&p).unwrap();
            TmpDir(p)
        }
        pub(crate) fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub(crate) fn vault_bins(dir: &TmpDir) -> Vec<PathBuf> {
        let v = dir.join("opentune-vault");
        let mut out: Vec<PathBuf> = std::fs::read_dir(&v)
            .map(|rd| rd.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        out.retain(|p| p.extension().and_then(|s| s.to_str()) == Some("bin"));
        out
    }

    pub(crate) fn ramp(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 7 + 3) as u8).collect()
    }
}
