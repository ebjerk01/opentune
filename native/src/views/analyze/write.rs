//! Flash a data file into an image FILE at an address (the webview app's
//! file-target "Flash": `desktop/src-tauri/src/flash.rs`).
//!
//! Only the in-memory patch and the read-back check live here (pure, run in
//! jobs). The write itself goes through the host's guarded path in the view:
//! `Host::confirm` → `Host::write_file`, which archives the target's current
//! contents into the vault BEFORE anything is written and replaces the file
//! atomically. A file target has no battery, so there is no voltage gate.

use std::path::{Path, PathBuf};

/// Largest image this accepts (raw ECU images are a few MiB).
pub(crate) const MAX_IMAGE: u64 = 64 << 20;

/// `0x…` hexadecimal or decimal, as the webview app's address field.
pub(crate) fn parse_addr(s: &str) -> Result<u32, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("enter the address to write at (e.g. 0x1C000)".into());
    }
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u32::from_str_radix(&h.replace('_', ""), 16).map_err(|e| format!("address: {e}"))
    } else {
        s.parse::<u32>()
            .map_err(|e| format!("address: {e} (prefix hexadecimal with 0x)"))
    }
}

/// `original` with `data` written at `addr`, and how many bytes differ.
/// Refused (nothing produced) when the data is empty or runs past the end of
/// the image.
pub(crate) fn patch(original: &[u8], addr: u32, data: &[u8]) -> Result<(Vec<u8>, usize), String> {
    if data.is_empty() {
        return Err("the data file is empty — nothing to write".into());
    }
    let end = addr as u64 + data.len() as u64;
    if end > original.len() as u64 {
        return Err(format!(
            "write past end of image: 0x{addr:08X} + {} bytes ends at 0x{end:08X}, the image is 0x{:08X} bytes",
            data.len(),
            original.len()
        ));
    }
    let a = addr as usize;
    let changed = original[a..a + data.len()]
        .iter()
        .zip(data)
        .filter(|(x, y)| x != y)
        .count();
    let mut out = original.to_vec();
    out[a..a + data.len()].copy_from_slice(data);
    Ok((out, changed))
}

/// A prepared write, waiting for the user's confirmation.
#[derive(Clone, Debug)]
pub(crate) struct Prepared {
    pub target: PathBuf,
    pub data_name: String,
    pub addr: u32,
    pub len: usize,
    pub image_len: usize,
    /// Bytes that actually change.
    pub changed: usize,
    /// The whole new image.
    pub patched: Vec<u8>,
}

pub(crate) fn read_limited(p: &Path, what: &str) -> Result<Vec<u8>, String> {
    let len = std::fs::metadata(p)
        .map_err(|e| format!("{what} {}: {e}", p.display()))?
        .len();
    if len > MAX_IMAGE {
        return Err(format!(
            "{what} {} is {len} bytes: larger than any raw ECU image (limit {MAX_IMAGE})",
            p.display()
        ));
    }
    std::fs::read(p).map_err(|e| format!("{what} {}: {e}", p.display()))
}

/// Read both files and build the patched image (blocking: run it in a job).
pub(crate) fn prepare(target: &Path, data_path: &Path, addr: u32) -> Result<Prepared, String> {
    let data = read_limited(data_path, "data file")?;
    let original = read_limited(target, "target image")?;
    let (patched, changed) = patch(&original, addr, &data)?;
    if changed == 0 {
        return Err(
            "the image already holds these bytes at that address — nothing to write".into(),
        );
    }
    Ok(Prepared {
        target: target.to_path_buf(),
        data_name: data_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
        addr,
        len: data.len(),
        image_len: original.len(),
        changed,
        patched,
    })
}

/// Read the target back and compare it with what was written (blocking).
pub(crate) fn read_back(target: &Path, expected: &[u8]) -> Result<(), String> {
    let now = std::fs::read(target).map_err(|e| format!("read-back {}: {e}", target.display()))?;
    if now == expected {
        Ok(())
    } else {
        let first = now
            .iter()
            .zip(expected)
            .position(|(a, b)| a != b)
            .unwrap_or(now.len().min(expected.len()));
        Err(format!(
            "read-back MISMATCH at 0x{first:08X} ({} bytes on disk, {} expected)",
            now.len(),
            expected.len()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(n: usize) -> Vec<u8> {
        (0..n).map(|i| i as u8).collect()
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "ot-native-analyze-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn addresses_parse_as_hex_or_decimal() {
        assert_eq!(parse_addr("0x10"), Ok(16));
        assert_eq!(parse_addr(" 0X1_C000 "), Ok(0x1C000));
        assert_eq!(parse_addr("32"), Ok(32));
        assert!(parse_addr("1C000").unwrap_err().contains("0x"));
        assert!(parse_addr("").is_err());
    }

    #[test]
    fn patch_writes_in_place_and_refuses_past_end() {
        let img = ramp(64);
        let (out, changed) = patch(&img, 0x10, &[0xA5; 16]).unwrap();
        assert_eq!(&out[0x10..0x20], &[0xA5; 16]);
        assert_eq!(&out[..0x10], &img[..0x10]);
        assert_eq!(&out[0x20..], &img[0x20..]);
        assert_eq!(changed, 16);
        // Same bytes: nothing changes.
        assert_eq!(patch(&img, 4, &img[4..8]).unwrap().1, 0);
        let e = patch(&img, 0x30, &[0; 32]).unwrap_err();
        assert!(e.contains("past end"), "{e}");
        assert!(patch(&img, 0, &[]).unwrap_err().contains("empty"));
        // Exactly to the end is fine.
        assert!(patch(&img, 0x30, &[0; 16]).is_ok());
    }

    #[test]
    fn prepare_reads_files_and_read_back_checks() {
        let dir = tmp("prep");
        let img = dir.join("target.bin");
        let data = dir.join("data.dat");
        std::fs::write(&img, ramp(256)).unwrap();
        std::fs::write(&data, [0xA5u8; 8]).unwrap();
        let p = prepare(&img, &data, 0x20).unwrap();
        assert_eq!((p.len, p.image_len, p.changed), (8, 256, 8));
        assert_eq!(p.data_name, "data.dat");
        // prepare never writes.
        assert_eq!(std::fs::read(&img).unwrap(), ramp(256));
        assert!(read_back(&img, &p.patched)
            .unwrap_err()
            .contains("0x00000020"));
        std::fs::write(&img, &p.patched).unwrap();
        assert_eq!(read_back(&img, &p.patched), Ok(()));
        // Writing the same data again is a no-op and refused.
        assert!(prepare(&img, &data, 0x20)
            .unwrap_err()
            .contains("nothing to write"));
        assert!(prepare(&dir.join("missing.bin"), &data, 0).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
