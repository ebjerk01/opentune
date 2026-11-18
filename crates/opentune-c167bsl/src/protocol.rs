//! Protocol facts: the on-chip bootstrap loader and the monitor-kernel
//! command set, plus decoders for the registers the presets program.
//!
//! Sources (all public; see `docs/PRIOR-ART.md`):
//! * Siemens *SAB 80C166 Family ApNotes — Bootstrap Loader C165/C167*
//!   (ed. 1.94): P0L.4 entry, zero byte, identification byte, 32 bytes to
//!   `00'FA40h..00'FA5Fh`, register state in BSL mode, baud calculation.
//! * Infineon C167CR derivatives user manual (ch. 14, as quoted publicly):
//!   the later identification bytes `C5h` / `D5h`.
//! * C167BootTool (GPL-3.0, `ME7BootTool.py`): the monitor command bytes,
//!   acknowledges, reply shapes, block checksum and SFR addresses.

/// The autobaud byte: start bit, eight `0` data bits, stop bit.
pub const HELLO: u8 = 0x00;
/// The BSL loads exactly this many bytes, then jumps to them.
pub const PRIMARY_LOADER_LEN: usize = 32;
/// Where the BSL stores the 32 bytes (internal RAM) and jumps.
pub const PRIMARY_LOADER_ADDR: u32 = 0x00_FA40;

/// Primary loader running (sent by the loader once it starts).
pub const I_LOADER_STARTED: u8 = 0x01;
/// Monitor kernel running (sent by the kernel once it starts).
pub const I_APPLICATION_STARTED: u8 = 0x03;
/// First acknowledge (to a command byte).
pub const ACK1: u8 = 0xAA;
/// Second acknowledge (ends a command's data phase).
pub const ACK2: u8 = 0xEA;

/// Negative acknowledge some kernels/cables return instead of `AA`/`EA`
/// (NefMotoOpenSource notes it with CH340 cables and marginal bauds).
pub const NAK: u8 = 0xFD;

/// Monitor command: write one word (`addr24 LE`, `word LE`).
pub const C_WRITE_WORD: u8 = 0x82;
/// Monitor command: read a block (`addr24 LE`, `len16 LE`).
pub const C_READ_BLOCK: u8 = 0x85;
/// Monitor command: XOR checksum of the previous block.
pub const C_GETCHECKSUM: u8 = 0x33;
/// Monitor command: test communication (`AA EA`).
pub const C_TEST_COMM: u8 = 0x93;
/// Monitor command: read one word (`addr24 LE` → `lo hi EA`).
pub const C_READ_WORD: u8 = 0xCD;

/// Largest block this client asks for in one read (the size the public
/// tools use with the MiniMon kernel).
pub const MAX_BLOCK: u16 = 0x200;

/// SFR addresses (C167 family) the presets write.
pub mod sfr {
    pub const SYSCON: u32 = 0x00_FF12;
    pub const BUSCON0: u32 = 0x00_FF0C;
    pub const BUSCON1: u32 = 0x00_FF14;
    pub const BUSCON2: u32 = 0x00_FF16;
    pub const BUSCON3: u32 = 0x00_FF18;
    pub const BUSCON4: u32 = 0x00_FF1A;
    pub const ADDRSEL1: u32 = 0x00_FE18;
    pub const ADDRSEL2: u32 = 0x00_FE1A;
    pub const ADDRSEL3: u32 = 0x00_FE1C;
    pub const ADDRSEL4: u32 = 0x00_FE1E;
}

/// The CPU family named by the BSL's identification byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipFamily {
    /// `55h`: 8xC166 (older ST10 parts answer the same).
    C166,
    /// `A5h`: early C167 (application note ed. 1.94).
    C167Early,
    /// `B5h`: early C165.
    C165,
    /// `C5h`: C167 derivatives.
    C167,
    /// `D5h`: devices with identification registers (the byte does not
    /// name the derivative; read the ID registers for that).
    WithIdRegisters,
}

impl ChipFamily {
    pub const ALL: [ChipFamily; 5] = [
        ChipFamily::C166,
        ChipFamily::C167Early,
        ChipFamily::C165,
        ChipFamily::C167,
        ChipFamily::WithIdRegisters,
    ];

    pub fn from_id(b: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.id_byte() == b)
    }

    pub fn id_byte(self) -> u8 {
        match self {
            ChipFamily::C166 => 0x55,
            ChipFamily::C167Early => 0xA5,
            ChipFamily::C165 => 0xB5,
            ChipFamily::C167 => 0xC5,
            ChipFamily::WithIdRegisters => 0xD5,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ChipFamily::C166 => "8xC166 / early ST10",
            ChipFamily::C167Early => "C167 (early)",
            ChipFamily::C165 => "C165 (early)",
            ChipFamily::C167 => "C167 derivative",
            ChipFamily::WithIdRegisters => "device with ID registers (C167CS/ST10F2xx class)",
        }
    }
}

/// 24-bit address, little-endian, as the monitor commands take it.
pub fn addr24(addr: u32) -> [u8; 3] {
    [addr as u8, (addr >> 8) as u8, (addr >> 16) as u8]
}

/// `C_WRITE_WORD` data phase.
pub fn write_word_args(addr: u32, value: u16) -> Vec<u8> {
    let mut v = addr24(addr).to_vec();
    v.extend_from_slice(&value.to_le_bytes());
    v
}

/// `C_READ_BLOCK` data phase.
pub fn read_block_args(addr: u32, len: u16) -> Vec<u8> {
    let mut v = addr24(addr).to_vec();
    v.extend_from_slice(&len.to_le_bytes());
    v
}

/// The monitor's block checksum: XOR of every byte.
pub fn xor_checksum(data: &[u8]) -> u8 {
    data.iter().fold(0, |a, b| a ^ b)
}

/// Space-separated lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// ------------------------------------------------------------- autobaud

/// What the BSL will make of a host baud rate at a given CPU clock.
///
/// The BSL times the zero byte (9 bit times) with Timer 6 at `fCPU/4`:
/// `T6 = 9/4 · fCPU / Bhost`, then sets `S0BRL = T6/72 − 1` (integer
/// division), giving `B = fCPU / (32 · (S0BRL + 1))`. The deviation
/// `FB = (B − Bhost) / B` must stay within ±2.5 %.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BaudCheck {
    pub fcpu_hz: u32,
    pub host_baud: u32,
    /// `None` if the zero byte is too long for 16-bit Timer 6 (baud too
    /// low) or too short to produce a reload value (baud too high).
    pub s0brl: Option<u32>,
    pub bsl_baud: f64,
    /// Signed deviation in percent.
    pub deviation_pct: f64,
    pub ok: bool,
}

/// Tolerance the application note gives for the BSL baud deviation.
pub const BAUD_TOLERANCE_PCT: f64 = 2.5;

pub fn baud_check(fcpu_hz: u32, host_baud: u32) -> BaudCheck {
    let t6 = (9u64 * fcpu_hz as u64) / (4 * host_baud.max(1) as u64);
    let s0brl = if t6 > 0xFFFF || t6 / 72 == 0 {
        None
    } else {
        Some((t6 / 72) as u32 - 1)
    };
    match s0brl {
        Some(r) => {
            let b = fcpu_hz as f64 / (32.0 * (r as f64 + 1.0));
            let dev = (b - host_baud as f64) / b * 100.0;
            BaudCheck {
                fcpu_hz,
                host_baud,
                s0brl,
                bsl_baud: b,
                deviation_pct: dev,
                ok: dev.abs() <= BAUD_TOLERANCE_PCT,
            }
        }
        None => BaudCheck {
            fcpu_hz,
            host_baud,
            s0brl: None,
            bsl_baud: 0.0,
            deviation_pct: f64::NAN,
            ok: false,
        },
    }
}

// ------------------------------------------------------ register decode

/// `ADDRSELx`: window start (`RGSAD` = bits 15..4 → A23..A12) and size
/// (`RGSZ` = bits 3..0 → 4 KiB · 2^RGSZ).
pub fn addrsel_window(v: u16) -> (u32, u32) {
    let start = ((v & 0xFFF0) as u32) << 8;
    let size = 4096u32 << (v & 0x000F);
    (start, size)
}

/// A short human description of a register value the presets write.
pub fn describe_register(addr: u32, v: u16) -> String {
    match addr {
        sfr::SYSCON => {
            let bit = |n: u16| (v >> n) & 1;
            format!(
                "ROMS1={} (int. ROM {}), ROMEN={} (int. ROM {}), BYTDIS={}, XPEN={}",
                bit(12),
                if bit(12) == 1 {
                    "mapped to segment 1, 01'0000h"
                } else {
                    "at segment 0"
                },
                bit(10),
                if bit(10) == 1 { "enabled" } else { "disabled" },
                bit(9),
                bit(2),
            )
        }
        sfr::ADDRSEL1 | sfr::ADDRSEL2 | sfr::ADDRSEL3 | sfr::ADDRSEL4 => {
            if v == 0 {
                "window off".into()
            } else {
                let (s, z) = addrsel_window(v);
                format!(
                    "window {s:#08x}..{:#08x} ({} KiB)",
                    s as u64 + z as u64 - 1,
                    z / 1024
                )
            }
        }
        sfr::BUSCON0 | sfr::BUSCON1 | sfr::BUSCON2 | sfr::BUSCON3 | sfr::BUSCON4 => {
            if v == 0 {
                return "bus off".into();
            }
            let btyp = match (v >> 6) & 3 {
                0 => "8-bit demux",
                1 => "8-bit mux",
                2 => "16-bit demux",
                _ => "16-bit mux",
            };
            format!(
                "BUSACT={}, {btyp}, {} wait state(s){}{}",
                (v >> 10) & 1,
                15 - (v & 0xF),
                if v & 0x4000 != 0 { ", CS on read" } else { "" },
                if v & 0x8000 != 0 { ", CS on write" } else { "" },
            )
        }
        _ => String::new(),
    }
}

// --------------------------------------------------- known payload files

/// A loader/kernel file recognised by SHA-256 (no bytes ship with OpenTune).
#[derive(Debug, Clone, Copy)]
pub struct KnownPayload {
    pub sha256: &'static str,
    pub name: &'static str,
    pub len: usize,
    /// For a primary loader: the kernel length it expects to receive.
    pub expects_kernel_len: Option<usize>,
    pub note: &'static str,
}

/// Files the public C167 bootmode tools use. Infineon MiniMon is freeware
/// without source and is **not** redistributed by OpenTune.
pub const KNOWN_PAYLOADS: &[KnownPayload] = &[
    KnownPayload {
        sha256: "486fa0681ef24ebc07fdfe8ef93138fc02fecdd07b43cec06c15a5f6ee5b2bf3",
        name: "MiniMon K-line primary loader (LOADK.bin)",
        len: 32,
        expects_kernel_len: Some(394),
        note: "sends 01, loads 394 bytes to 00'FA60h, jumps there",
    },
    KnownPayload {
        sha256: "b462c8ece301d3f60c8a27e2958e0a104a07be62edb4b5bf0febd9763cbda2e8",
        name: "MiniMon K-line monitor kernel (MINIMONK.bin)",
        len: 394,
        expects_kernel_len: None,
        note: "sends 03 when running; answers the 0x33/0x82/0x85/0x93/0xCD commands used here",
    },
];

/// Look a file up by its SHA-256 (lowercase hex).
pub fn recognise(sha256_hex: &str) -> Option<&'static KnownPayload> {
    KNOWN_PAYLOADS
        .iter()
        .find(|k| k.sha256.eq_ignore_ascii_case(sha256_hex))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_bytes_round_trip() {
        for f in ChipFamily::ALL {
            assert_eq!(ChipFamily::from_id(f.id_byte()), Some(f));
        }
        assert_eq!(ChipFamily::from_id(0x12), None);
        assert_eq!(ChipFamily::from_id(ACK1), None);
    }

    #[test]
    fn baud_check_matches_the_application_note_example() {
        // ApNote 4.5.3: 20 MHz, host 40300 → S0BRL 14, 41666 Bd, ≈ 3.2 %.
        let a = baud_check(20_000_000, 40_300);
        assert_eq!(a.s0brl, Some(14));
        assert_eq!(a.bsl_baud as u32, 41_666);
        assert!((a.deviation_pct - 3.28).abs() < 0.05, "{a:?}");
        assert!(!a.ok);
        // Host 41500 → ≈ 0.4 %.
        let b = baud_check(20_000_000, 41_500);
        assert_eq!(b.s0brl, Some(14));
        assert!((b.deviation_pct - 0.4).abs() < 0.05, "{b:?}");
        assert!(b.ok);
    }

    #[test]
    fn baud_check_limits() {
        // 20 MHz: lowest usable host baud is ≈ 687.
        assert_eq!(baud_check(20_000_000, 600).s0brl, None);
        assert!(baud_check(20_000_000, 1200).s0brl.is_some());
        // Far too fast for the generator.
        assert_eq!(baud_check(20_000_000, 2_000_000).s0brl, None);
    }

    #[test]
    fn addrsel_decodes_window() {
        assert_eq!(addrsel_window(0x4008), (0x40_0000, 0x10_0000));
        assert_eq!(addrsel_window(0x3803), (0x38_0000, 0x8000));
        assert!(describe_register(sfr::ADDRSEL3, 0x4008).contains("1024 KiB"));
    }

    #[test]
    fn buscon_and_syscon_decode() {
        // ApNote example: 06BFh = external bus on, 16-bit non-multiplexed.
        let d = describe_register(sfr::BUSCON0, 0x06BF);
        assert!(d.contains("BUSACT=1") && d.contains("16-bit demux") && d.contains("0 wait"));
        let d = describe_register(sfr::BUSCON3, 0x848E);
        assert!(d.contains("1 wait") && d.contains("CS on write"), "{d}");
        let s = describe_register(sfr::SYSCON, 0xF604);
        assert!(s.contains("ROMS1=1") && s.contains("ROMEN=1"), "{s}");
    }

    #[test]
    fn arg_encoding_is_little_endian() {
        assert_eq!(
            write_word_args(0xFF12, 0xF604),
            [0x12, 0xFF, 0x00, 0x04, 0xF6]
        );
        assert_eq!(
            read_block_args(0x40_0200, 0x200),
            [0x00, 0x02, 0x40, 0x00, 0x02]
        );
        assert_eq!(xor_checksum(&[0x01, 0x02, 0x04]), 0x07);
    }

    #[test]
    fn recognise_is_case_insensitive() {
        let k = recognise(&KNOWN_PAYLOADS[0].sha256.to_uppercase()).unwrap();
        assert_eq!(k.expects_kernel_len, Some(394));
        assert!(recognise("00").is_none());
    }
}
