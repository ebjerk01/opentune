//! Memory-region presets as data: where to read and which bus /
//! chip-select registers to program first.
//!
//! Register values come from C167BootTool (GPL-3.0): its internal-ROM read
//! setup and its EDC15 ("29Fx00BT, EDC15 variant") register values, in the
//! order NefMotoOpenSource (GPL-3.0, derived from C167BootTool) programs
//! them for an EDC15 external-flash read at `0x400000`. Sizes: VAG EDC15VM+/P+ reads are
//! publicly 512 KiB; the EDC15C6 size is the 1 MiB read this repository's
//! EDC15C6 checksum and definition were built on. **None of these reads has
//! been run on hardware by OpenTune** (every region has `verified: false`).
use crate::protocol::sfr;

/// One register write in a region's setup (written, then read back).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegWrite {
    pub name: &'static str,
    pub addr: u32,
    pub value: u16,
}

const fn w(name: &'static str, addr: u32, value: u16) -> RegWrite {
    RegWrite { name, addr, value }
}

/// A readable memory region.
#[derive(Debug, Clone, Copy)]
pub struct Region {
    pub id: &'static str,
    pub description: &'static str,
    /// CPU address the read starts from (offset 0).
    pub base: u32,
    /// Length read when none is given.
    pub default_len: u32,
    /// Size of the address window: no read may go past `base + max_len`.
    pub max_len: u32,
    /// Registers to program, in order, before reading.
    pub setup: &'static [RegWrite],
    /// Named SFRs whose values must be supplied by the board operator.
    pub unknown_setup: &'static [(&'static str, u32)],
    pub source: &'static str,
    /// Has this exact read been confirmed on a real ECU? (all `false`)
    pub verified: bool,
}

/// A board family.
#[derive(Debug, Clone, Copy)]
pub struct Preset {
    pub id: &'static str,
    pub name: &'static str,
    /// Identification bytes accepted from the BSL.
    pub accept_ids: &'static [u8],
    pub regions: &'static [Region],
    pub notes: &'static str,
}

impl Preset {
    pub fn region(&self, id: &str) -> Option<&'static Region> {
        self.regions.iter().find(|r| r.id.eq_ignore_ascii_case(id))
    }
}

/// C167 family identification bytes (early, derivative, with ID registers).
const C167_IDS: &[u8] = &[0xA5, 0xC5, 0xD5];

/// Internal ROM/flash mapped to segment 1 (`ROMS1=1`, `ROMEN=1`), every
/// external window off.
const INT_ROM_SETUP: &[RegWrite] = &[
    w("SYSCON", sfr::SYSCON, 0xF604),
    w("BUSCON0", sfr::BUSCON0, 0x0000),
    w("ADDRSEL1", sfr::ADDRSEL1, 0x0000),
    w("ADDRSEL2", sfr::ADDRSEL2, 0x0000),
    w("ADDRSEL3", sfr::ADDRSEL3, 0x0000),
    w("ADDRSEL4", sfr::ADDRSEL4, 0x0000),
    w("BUSCON1", sfr::BUSCON1, 0x0000),
    w("BUSCON2", sfr::BUSCON2, 0x0000),
    w("BUSCON3", sfr::BUSCON3, 0x0000),
    w("BUSCON4", sfr::BUSCON4, 0x0000),
];

/// EDC15 external flash: internal ROM off, CS3 window 1 MiB at `0x400000`,
/// 16-bit demultiplexed bus. Order and values as NefMotoOpenSource's EDC15
/// external-flash read setup (C167BootTool's values).
const EDC15_EXT_SETUP: &[RegWrite] = &[
    w("SYSCON", sfr::SYSCON, 0xE204),
    w("BUSCON0", sfr::BUSCON0, 0x04AD),
    w("ADDRSEL3", sfr::ADDRSEL3, 0x4008),
    w("ADDRSEL1", sfr::ADDRSEL1, 0x0000),
    w("ADDRSEL2", sfr::ADDRSEL2, 0x0000),
    w("ADDRSEL4", sfr::ADDRSEL4, 0x0000),
    w("BUSCON1", sfr::BUSCON1, 0x040D),
    w("BUSCON3", sfr::BUSCON3, 0x848E),
    w("BUSCON2", sfr::BUSCON2, 0x0000),
    w("BUSCON4", sfr::BUSCON4, 0x0000),
];

const INT_ROM: Region = Region {
    id: "int-rom",
    description: "internal ROM/flash, mapped to 01'0000h (32 KiB window; undefined data on ROMless -LM parts)",
    base: 0x01_0000,
    default_len: 0x8000,
    max_len: 0x8000,
    setup: INT_ROM_SETUP,
    unknown_setup: &[],
    source: "C167BootTool -readInt (SYSCON F604h, buses off, read from 01'0000h)",
    verified: false,
};

const fn edc15_ext(default_len: u32) -> Region {
    Region {
        id: "ext-flash",
        description: "external flash on CS3, 1 MiB window at 40'0000h",
        base: 0x40_0000,
        default_len,
        max_len: 0x10_0000,
        setup: EDC15_EXT_SETUP,
        unknown_setup: &[],
        source: "NefMotoOpenSource EDC15 external-flash read setup / C167BootTool values (ADDRSEL3 4008h, BUSCON3 848Eh, BUSCON0 04ADh)",
        verified: false,
    }
}

pub const PRESETS: &[Preset] = &[
    Preset {
        id: "egs52",
        name: "Mercedes 722.6 EGS52 (C167)",
        accept_ids: C167_IDS,
        regions: &[Region {
            id: "ext-flash",
            description: "external AM29F400BB flash; board chip select and bus timings unknown",
            base: 0x08_0000,
            default_len: 0x8_0000,
            max_len: 0x8_0000,
            setup: &[],
            unknown_setup: &[("SYSCON", sfr::SYSCON), ("BUSCON0", sfr::BUSCON0), ("ADDRSEL1", sfr::ADDRSEL1), ("ADDRSEL2", sfr::ADDRSEL2), ("ADDRSEL3", sfr::ADDRSEL3), ("ADDRSEL4", sfr::ADDRSEL4), ("BUSCON1", sfr::BUSCON1), ("BUSCON2", sfr::BUSCON2), ("BUSCON3", sfr::BUSCON3), ("BUSCON4", sfr::BUSCON4)],
            source: "rnd-ash/EGS52_emu flash mapping (emulator); register values unverified",
            verified: false,
        }],
        notes: "95320 SPI EEPROM (4 KiB) is outside the flash window and out of scope.",
    },
    Preset {
        id: "edc15vm-p",
        name: "Bosch EDC15VM+ / EDC15P+ (VAG TDI)",
        accept_ids: C167_IDS,
        regions: &[edc15_ext(0x8_0000), INT_ROM],
        notes: "512 KiB external flash (29F400 class). The board EEPROM (24C0x over I2C) is not read by this tool.",
    },
    Preset {
        id: "edc15c6",
        name: "Bosch EDC15C6 (Mercedes CDI, CR1)",
        accept_ids: C167_IDS,
        regions: &[edc15_ext(0x10_0000), INT_ROM],
        notes: "1 MiB external flash (the read size the EDC15C6 checksum and definition use). Same bus setup as EDC15VM+/P+ — assumed, not confirmed on a C6 board.",
    },
    Preset {
        id: "c167",
        name: "Generic C167 (internal ROM/flash only)",
        accept_ids: C167_IDS,
        regions: &[INT_ROM],
        notes: "No external bus setup; reads the internal ROM/flash window only.",
    },
];

pub fn find_preset(id: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.id.eq_ignore_ascii_case(id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::addrsel_window;

    #[test]
    fn presets_are_consistent() {
        for p in PRESETS {
            assert!(!p.regions.is_empty(), "{}", p.id);
            for r in p.regions {
                assert!(
                    r.default_len > 0 && r.default_len <= r.max_len,
                    "{}/{}",
                    p.id,
                    r.id
                );
                assert!(!r.verified, "nothing is hardware-verified yet");
                if r.unknown_setup.is_empty() {
                    assert_eq!(r.setup[0].addr, sfr::SYSCON, "SYSCON first");
                } else {
                    assert!(r.setup.is_empty());
                }
            }
        }
        assert!(find_preset("EDC15C6").is_some());
        assert!(find_preset("edc15c6")
            .unwrap()
            .region("ext-flash")
            .is_some());
    }

    #[test]
    fn ext_flash_window_covers_the_region() {
        let r = find_preset("edc15c6").unwrap().region("ext-flash").unwrap();
        let sel = r.setup.iter().find(|w| w.name == "ADDRSEL3").unwrap();
        let (start, size) = addrsel_window(sel.value);
        assert_eq!(start, r.base);
        assert_eq!(size, r.max_len);
    }
}
