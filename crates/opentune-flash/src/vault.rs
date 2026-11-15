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
pub fn save_backup(
    dir: &Path,
    ecu: &EcuId,
    region: MemoryRegion,
    data: &[u8],
) -> Result<PathBuf> {
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
    let stem = format!("{}_{:08X}_{}", sanitize(&name), region.address, when);
    let bin = dir.join(format!("{stem}.bin"));
    std::fs::write(&bin, data)?;

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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(std::fs::read_to_string(meta).unwrap().contains("length = 64"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
