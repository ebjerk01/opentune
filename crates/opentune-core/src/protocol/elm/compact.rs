//! Compact `addressAndLengthFormatIdentifier`s for an ELM327 on CAN.
//!
//! An ELM327 with `ATCAF1` can only transmit a single ISO-TP frame, so a UDS
//! memory request has to fit [`MAX_CAN_REQUEST`](super::MAX_CAN_REQUEST)
//! (7) bytes. That leaves:
//!
//! - `ReadMemoryByAddress` (0x23): `23 <alfid>` + **5** bytes for address and
//!   size ([`RMBA_FIELD_BUDGET`]); the default format is `0x14` (4-byte
//!   address, 1-byte size).
//! - `RequestUpload` (0x35) / `RequestDownload` (0x34): `3x <dfi> <alfid>` +
//!   **4** bytes ([`TRANSFER_FIELD_BUDGET`]); the default format is `0x22`
//!   (16-bit address and size) or `0x13` (24-bit address, 8-bit size). A
//!   32-bit address leaves no byte for the size and is refused.
//!
//! These helpers plan a format before anything is sent and refuse what cannot
//! be expressed. They are shared by the ELM drivers (`elm327obd`, `elm327`).

use crate::{Error, MemoryRegion, Result};

/// Bytes left for `memoryAddress` + `memorySize` in a 0x23 request
/// (`23 <alfid>` + 5).
pub const RMBA_FIELD_BUDGET: usize = super::MAX_CAN_REQUEST - 2;

/// Bytes left for `memoryAddress` + `memorySize` in a 0x34/0x35 request
/// (`3x <dfi> <alfid>` + 4).
pub const TRANSFER_FIELD_BUDGET: usize = super::MAX_CAN_REQUEST - 3;

/// The last byte address of a non-empty region; an empty region, or one that
/// runs past 0xFFFF_FFFF, is a configuration error.
pub fn last_address(region: MemoryRegion) -> Result<u32> {
    if region.length == 0 {
        return Err(Error::Config("empty memory region".into()));
    }
    region
        .address
        .checked_add(region.length - 1)
        .ok_or_else(|| {
            Error::Config(format!(
                "region 0x{:08X}+0x{:X} runs past the 32-bit address space",
                region.address, region.length
            ))
        })
}

/// Bytes needed to express `v`.
pub fn bytes_needed(v: u32) -> usize {
    match v {
        0..=0xFF => 1,
        0x100..=0xFFFF => 2,
        0x1_0000..=0xFF_FFFF => 3,
        _ => 4,
    }
}

/// Largest value an `n`-byte field holds.
pub fn field_max(n: u8) -> u32 {
    match n {
        0 => 0,
        1..=3 => (1u32 << (8 * u32::from(n))) - 1,
        _ => u32::MAX,
    }
}

/// Check that `alfid` is a valid format that fits `budget` request bytes and
/// can express every address up to `last`.
pub fn check_alfid(alfid: u8, last: u32, budget: usize, service: &str) -> Result<()> {
    let size_len = usize::from(alfid >> 4);
    let addr_len = usize::from(alfid & 0x0F);
    if !(1..=4).contains(&size_len) || !(1..=4).contains(&addr_len) {
        return Err(Error::Config(format!(
            "{service}: addressAndLengthFormatIdentifier 0x{alfid:02X} is not a valid 1..=4-byte format"
        )));
    }
    if size_len + addr_len > budget {
        return Err(Error::Unsupported(format!(
            "{service}: format 0x{alfid:02X} needs {} address+size bytes but an ELM327 \
             single-frame request leaves only {budget}; use a smaller format (alfid option) \
             or a raw-CAN interface (slcan/gs_usb/SocketCAN/J2534)",
            size_len + addr_len
        )));
    }
    if bytes_needed(last) > addr_len {
        return Err(Error::Config(format!(
            "{service}: address 0x{last:08X} does not fit the {addr_len}-byte address of format 0x{alfid:02X}"
        )));
    }
    Ok(())
}

/// Format for 0x34/0x35 within the 4-byte address+size budget: the override
/// if given, else `0x22` for 16-bit addresses or `0x13` for 24-bit ones. A
/// 32-bit address is refused (it leaves no byte for the size).
pub fn transfer_alfid(over: Option<u8>, last: u32, service: &str) -> Result<u8> {
    let alfid = match over {
        Some(a) => a,
        None => match bytes_needed(last) {
            1 | 2 => 0x22,
            3 => 0x13,
            _ => {
                return Err(Error::Unsupported(format!(
                    "{service}: address 0x{last:08X} needs a 4-byte address field, which leaves \
                     no room for the size in an ELM327 single-frame request (7 bytes); use \
                     read_mode=rmba for reads, or a raw-CAN interface (slcan/gs_usb/SocketCAN/J2534)"
                )))
            }
        },
    };
    check_alfid(alfid, last, TRANSFER_FIELD_BUDGET, service)?;
    Ok(alfid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budgets_follow_the_single_frame_limit() {
        assert_eq!(RMBA_FIELD_BUDGET, 5);
        assert_eq!(TRANSFER_FIELD_BUDGET, 4);
    }

    #[test]
    fn transfer_format_by_address_width() {
        assert_eq!(transfer_alfid(None, 0xFFFF, "x").unwrap(), 0x22);
        assert_eq!(transfer_alfid(None, 0x1_0000, "x").unwrap(), 0x13);
        assert!(matches!(
            transfer_alfid(None, 0x100_0000, "x"),
            Err(Error::Unsupported(m)) if m.contains("read_mode=rmba")
        ));
        // Overrides are checked against the budget and the address.
        assert_eq!(transfer_alfid(Some(0x12), 0xFFFF, "x").unwrap(), 0x12);
        assert!(matches!(
            transfer_alfid(Some(0x14), 0, "x"),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            transfer_alfid(Some(0x12), 0x1_0000, "x"),
            Err(Error::Config(_))
        ));
        assert!(matches!(
            transfer_alfid(Some(0x05), 0, "x"),
            Err(Error::Config(_))
        ));
    }

    #[test]
    fn rmba_formats_fit_five_bytes() {
        assert!(check_alfid(0x14, u32::MAX, RMBA_FIELD_BUDGET, "x").is_ok());
        assert!(check_alfid(0x23, 0xFF_FFFF, RMBA_FIELD_BUDGET, "x").is_ok());
        assert!(matches!(
            check_alfid(0x24, 0, RMBA_FIELD_BUDGET, "x"),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn region_and_field_helpers() {
        assert_eq!(last_address(MemoryRegion::new(0x10, 0x10)).unwrap(), 0x1F);
        assert!(last_address(MemoryRegion::new(0, 0)).is_err());
        assert!(last_address(MemoryRegion::new(0xFFFF_FFF0, 0x20)).is_err());
        assert_eq!(field_max(1), 0xFF);
        assert_eq!(field_max(2), 0xFFFF);
        assert_eq!(field_max(4), u32::MAX);
        assert_eq!(bytes_needed(0x1_0000), 3);
    }
}
