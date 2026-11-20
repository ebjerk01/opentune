//! Backup vault: every ECU read is archived to disk with a metadata sidecar,
//! so an original is never a single unsaved buffer. Dependency-free.
use opentune_core::{EcuId, MemoryRegion, Result};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// CRC-32 (IEEE) integrity tag for the sidecar.
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Save `data` read from `region` as a backup in `dir`. Returns the image path.
/// Writes `<stem>.bin` and a human-readable `<stem>.meta` sidecar.
///
/// An existing backup is never overwritten: when `<stem>.bin` already exists
/// (two backups of the same ECU within one second) the new one gets a
/// `_2`, `_3`, … suffix. The image file is created exclusively, so even two
/// concurrent saves cannot replace each other.
pub fn save_backup(dir: &Path, ecu: &EcuId, region: MemoryRegion, data: &[u8]) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let when = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let name = ecu
        .ecu_name
        .clone()
        .or_else(|| ecu.sw_number.clone())
        .unwrap_or_else(|| "ecu".into());
    let base = format!("{}_{:08X}_{}", sanitize(&name), region.address, when);
    let (stem, bin) = create_unique(dir, &base, data)?;

    let meta = format!(
        "ecu_name = {}\nsw_number = {}\nhw_number = {}\nvin = {}\n\
         address = 0x{:08X}\nlength = {}\ncrc32 = 0x{:08X}\nunix_time = {}\n",
        ecu.ecu_name.as_deref().unwrap_or(""),
        ecu.sw_number.as_deref().unwrap_or(""),
        ecu.hw_number.as_deref().unwrap_or(""),
        ecu.vin.as_deref().unwrap_or(""),
        region.address,
        data.len(),
        crc32(data),
        when,
    );
    std::fs::write(dir.join(format!("{stem}.meta")), meta)?;
    Ok(bin)
}

/// Write `data` to `<base>.bin`, or `<base>_N.bin` for the first N ≥ 2 that
/// does not exist yet, creating the file exclusively (never replacing one).
fn create_unique(dir: &Path, base: &str, data: &[u8]) -> Result<(String, PathBuf)> {
    use std::io::Write;
    for n in 1u32.. {
        let stem = if n == 1 {
            base.to_string()
        } else {
            format!("{base}_{n}")
        };
        let bin = dir.join(format!("{stem}.bin"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&bin)
        {
            Ok(mut f) => {
                f.write_all(data)?;
                f.sync_all()?;
                return Ok((stem, bin));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    unreachable!("u32 suffixes exhausted")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_second_backups_never_overwrite() {
        let dir = std::env::temp_dir().join(format!("opentune_vault_dup_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ecu = EcuId {
            ecu_name: Some("before write".into()),
            ..Default::default()
        };
        let region = MemoryRegion::new(0, 4);
        // Same ECU, same address, back to back: same second in practice.
        let a = save_backup(&dir, &ecu, region, b"ORIG").unwrap();
        let b = save_backup(&dir, &ecu, region, b"EDIT").unwrap();
        let c = save_backup(&dir, &ecu, region, b"MORE").unwrap();
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_eq!(
            std::fs::read(&a).unwrap(),
            b"ORIG",
            "the first backup survives"
        );
        assert_eq!(std::fs::read(&b).unwrap(), b"EDIT");
        assert_eq!(std::fs::read(&c).unwrap(), b"MORE");
        for p in [&a, &b, &c] {
            assert!(
                p.with_extension("meta").exists(),
                "{} has its sidecar",
                p.display()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn crc32_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn writes_bin_and_meta() {
        let dir = std::env::temp_dir().join(format!("opentune_vault_{}", std::process::id()));
        let ecu = EcuId {
            ecu_name: Some("test-edc16".into()),
            ..Default::default()
        };
        let data = vec![0xABu8; 64];
        let p = save_backup(&dir, &ecu, MemoryRegion::new(0, 64), &data).unwrap();
        assert!(p.exists());
        assert_eq!(std::fs::read(&p).unwrap(), data);
        let meta = p.with_extension("meta");
        assert!(meta.exists());
        assert!(std::fs::read_to_string(meta)
            .unwrap()
            .contains("length = 64"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
