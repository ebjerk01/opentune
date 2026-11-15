//! The flash orchestrator: the safety-enforcing sequence around any write.
//!
//! No driver writes to an ECU directly from the app. Writes go through
//! [`flash_calibration`], which enforces: voltage gate → verified backup →
//! write → read-back verify. These rules live here so no driver can skip them.
use opentune_core::{Error, Interface, MemoryRegion, Phase, Progress, Result};

/// Safety thresholds and switches for a flash operation.
#[derive(Debug, Clone)]
pub struct SafetyConfig {
    /// Refuse to flash below this battery voltage (mV). 0 disables the gate.
    pub min_millivolts: u32,
    /// Require a successful backup read before writing.
    pub require_backup: bool,
    /// Read back and compare after writing.
    pub verify_after_write: bool,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            min_millivolts: 12_000,
            require_backup: true,
            verify_after_write: true,
        }
    }
}

/// Outcome of a flash operation.
#[derive(Debug, Clone)]
pub struct FlashReport {
    pub region: MemoryRegion,
    pub battery_millivolts: Option<u32>,
    pub backup: Vec<u8>,
    pub bytes_written: usize,
    pub verified: bool,
}

/// Read a region into a backup buffer (always do this before writing).
pub fn backup(
    iface: &mut dyn Interface,
    region: MemoryRegion,
    progress: &mut dyn FnMut(Progress),
) -> Result<Vec<u8>> {
    iface.read_memory(region, progress)
}

/// Write `new_data` to `region` with full safety enforcement.
pub fn flash_calibration(
    iface: &mut dyn Interface,
    region: MemoryRegion,
    new_data: &[u8],
    safety: &SafetyConfig,
    progress: &mut dyn FnMut(Progress),
) -> Result<FlashReport> {
    if new_data.len() != region.length as usize {
        return Err(Error::Safety(format!(
            "data length {} != region length {}",
            new_data.len(),
            region.length
        )));
    }

    // 1. Voltage gate.
    let mv = iface.battery_millivolts()?;
    if safety.min_millivolts > 0 {
        match mv {
            Some(v) if v < safety.min_millivolts => {
                return Err(Error::Safety(format!(
                    "battery {v} mV below minimum {} mV",
                    safety.min_millivolts
                )));
            }
            None if iface.capabilities().battery_voltage => {
                return Err(Error::Safety("battery voltage unreadable".into()));
            }
            _ => {}
        }
    }

    // 2. Verified backup.
    let backup_buf = if safety.require_backup {
        let b = backup(iface, region, progress)?;
        if b.len() != region.length as usize {
            return Err(Error::Safety("backup read short — aborting".into()));
        }
        b
    } else {
        Vec::new()
    };

    // 3. Write.
    iface.write_memory(region, new_data, progress)?;

    // 4. Read-back verify.
    let mut verified = false;
    if safety.verify_after_write {
        let readback = iface.read_memory(region, progress)?;
        if readback != new_data {
            return Err(Error::Verify(
                "read-back does not match written data".into(),
            ));
        }
        progress(Progress {
            phase: Phase::Verify,
            done: region.length as u64,
            total: region.length as u64,
        });
        verified = true;
    }

    Ok(FlashReport {
        region,
        battery_millivolts: mv,
        backup: backup_buf,
        bytes_written: new_data.len(),
        verified,
    })
}
