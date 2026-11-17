//! Synthetic fixture generator: minimal fake images carrying one family's
//! markers, so recognition can be unit-tested without real dumps.
//!
//! Every identifier written here is **synthetic** (`0261209999`,
//! `1037999999`, `06A906099X`, …) — no real calibration data, part numbers of
//! real units or vehicle identifiers. The layouts only reproduce *where* the
//! public markers sit (descriptor at 0x10000 for ME7, DECAFE at active+0x3D for
//! EDC16, TSW at 0x8000 for EDC15, …), surrounded by erased-flash fill.

/// The families the generator can build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SynthFamily {
    Me7,
    Me9,
    Med9,
    Med17,
    Edc17,
    Edc16,
    Edc15,
    Simos,
    Sid,
    Ppd,
    Ms43,
    Simtec,
    DelphiMt,
    DelphiDcm,
    MarelliMjd,
    MarelliIaw,
    DensoSh,
}

/// Every buildable family.
pub const ALL: &[SynthFamily] = &[
    SynthFamily::Me7,
    SynthFamily::Me9,
    SynthFamily::Med9,
    SynthFamily::Med17,
    SynthFamily::Edc17,
    SynthFamily::Edc16,
    SynthFamily::Edc15,
    SynthFamily::Simos,
    SynthFamily::Sid,
    SynthFamily::Ppd,
    SynthFamily::Ms43,
    SynthFamily::Simtec,
    SynthFamily::DelphiMt,
    SynthFamily::DelphiDcm,
    SynthFamily::MarelliMjd,
    SynthFamily::MarelliIaw,
    SynthFamily::DensoSh,
];

/// What recognition must report for a synthetic image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Expected {
    pub supplier: &'static str,
    pub family: &'static str,
    pub variant: Option<&'static str>,
}

fn put(img: &mut [u8], off: usize, bytes: &[u8]) {
    img[off..off + bytes.len()].copy_from_slice(bytes);
}

fn blank(len: usize, fill: u8) -> Vec<u8> {
    vec![fill; len]
}

/// Build the synthetic image for `f`.
pub fn build(f: SynthFamily) -> Vec<u8> {
    use SynthFamily::*;
    match f {
        Me7 => {
            let mut b = blank(0x8_0000, 0xFF);
            put(
                &mut b,
                0x1_0000,
                b"ZZ\xFF\xFF41/1/ME7.5/3/4000.00//24C/Dst01o/010199//",
            );
            put(&mut b, 0x1_4000, b"0261209999\x001037999999\x00");
            put(
                &mut b,
                0x1_4100,
                b"06A906099X  1.8l R4/5VT  MOTRONIC ME7.5    0001\x00",
            );
            b
        }
        Me9 | Med9 => {
            let mut b = blank(0x20_0000, 0xFF);
            put(&mut b, 0x2600, b"0261209999\x00");
            put(&mut b, 0x461D, b"\x221037999998\x01\x01//1037999999\x00");
            put(&mut b, 0x1_594C, b"Bosch.Common.RamLoader.Me9.0001\x00");
            if f == Med9 {
                put(
                    &mut b,
                    0x1C_21F0,
                    b"\x0057/1/MED91/5/4400.01//D9100_X00Z000/D9100_X00Z000/010101/\x00",
                );
            }
            b
        }
        Med17 => {
            let mut b = blank(0x20_0000, 0xFF);
            put(&mut b, 0x4_0000, b"SB_V18.00.02/1793\x00");
            put(
                &mut b,
                0x7_FF08,
                b"\x0045/1/MED17/5/MED17.5//D175X0XX_M00X00/Dst00//\x00",
            );
            b
        }
        Edc17 => {
            let mut b = blank(0x20_0000, 0xFF);
            put(&mut b, 0x4_0000, b"SB_V18.00.02/1793\x00");
            put(
                &mut b,
                0x1A_0A10,
                b"\x0035/1/EDC17_CP14/5/P598//C598X0X0///\x00",
            );
            b
        }
        Edc16 => {
            let mut b = blank(0x10_0000, 0xFF);
            put(&mut b, 0x4_0010, b"1037999999");
            put(&mut b, 0x4_003D, b"\xDE\xCA\xFE");
            put(&mut b, 0xE_054B, b"EDC16C8/009/C000/ /110000_000/\x00");
            b
        }
        Edc15 => {
            let mut b = blank(0x8_0000, 0xC3);
            put(&mut b, 0x8000, b"TSW V2.40 280700 1718 C7/ESB/G40\x00");
            put(&mut b, 0x7_C000, b"\xC30281099999\xC3\xC31037999999\xC3");
            b
        }
        Simos => {
            let mut b = blank(0x8_0000, 0xFF);
            put(&mut b, 0x7F80, b"5WP49999----\x00");
            put(
                &mut b,
                0x4_8071,
                b"047906099   1,4l SIMOS3PB      XX99   \x00",
            );
            b
        }
        Sid => {
            let mut b = blank(0x20_0000, 0xFF);
            put(&mut b, 0x4218, b"SID206\x00\x00");
            put(&mut b, 0x6300, b"5WS49999X-T     00000010110049  \x00");
            b
        }
        Ppd => {
            let mut b = blank(0x20_0000, 0xFF);
            put(&mut b, 0x4_0074, b"    03G906099XX R4 2.0l PPD1.2     \x00");
            put(&mut b, 0x4_1000, b"CASN0X00.DAT\x00");
            b
        }
        Ms43 => {
            let mut b = blank(0x8_0000, 0xFF);
            put(
                &mut b,
                0x3F80,
                b"\x00\x00\x00\x005WK99999--0000000000MS43000000000000006577999999\x00",
            );
            b
        }
        Simtec => {
            let mut b = blank(0x10_0000, 0xFF);
            put(&mut b, 0x3D36, b"SIMTEC76  \x00\x00\x00\x00");
            put(&mut b, 0x3E00, b"6577999999\x00");
            b
        }
        DelphiMt => {
            let mut b = blank(0x10_0000, 0xFF);
            put(
                &mut b,
                0x2_01A1,
                b"\x00\x00XXXXMT80.1    D00000XX000000  \x00",
            );
            put(
                &mut b,
                0x3_C821,
                b"Copyright (c) 1999 Delphi Technologies, Inc.\x00",
            );
            b
        }
        DelphiDcm => {
            let mut b = blank(0x20_0000, 0xFF);
            put(&mut b, 0x1_0000, b"\x00DCM6.2\x00");
            put(&mut b, 0x1_0100, b"Copyright (c) Delphi Technologies\x00");
            b
        }
        MarelliMjd => {
            let mut b = blank(0x7_0000, 0xFF);
            put(&mut b, 0x6_007D, b"\x00MAG  01246J000D\x00");
            put(&mut b, 0x6_00E4, b"\x00MJD6JFHW01X    \x006JF\x00");
            b
        }
        MarelliIaw => {
            let mut b = blank(0x1_0000, 0xFF);
            put(&mut b, 0xF030, b"\x00\x00\x00iaw16\x00\x00\x00");
            b
        }
        DensoSh => {
            let mut b = blank(0x10_0000, 0xFF);
            put(
                &mut b,
                0,
                &[
                    0, 0, 0x0B, 0x68, 0xFF, 0xFF, 0xBF, 0xA0, 0, 0, 0x0B, 0x68, 0xFF, 0xFF, 0xBF,
                    0xA0,
                ],
            );
            put(&mut b, 0x2028, b"Copr.DENSO2000 \x00");
            b
        }
    }
}

/// The recognition result `build(f)` must produce.
pub fn expected(f: SynthFamily) -> Expected {
    use crate::recognize::supplier::*;
    use SynthFamily::*;
    let (supplier, family, variant) = match f {
        Me7 => (BOSCH, "ME7", Some("ME7.5")),
        Me9 => (BOSCH, "ME9", None),
        Med9 => (BOSCH, "MED9", Some("MED91")),
        Med17 => (BOSCH, "MED17", Some("MED17.5")),
        Edc17 => (BOSCH, "EDC17", Some("EDC17CP14")),
        Edc16 => (BOSCH, "EDC16", Some("EDC16C8")),
        Edc15 => (BOSCH, "EDC15", Some("EDC15C7")),
        Simos => (CONTINENTAL, "SIMOS", Some("SIMOS3PB")),
        Sid => (CONTINENTAL, "SID", Some("SID206")),
        Ppd => (CONTINENTAL, "PPD", Some("PPD1.2")),
        Ms43 => (CONTINENTAL, "MS4x", Some("MS43")),
        Simtec => (CONTINENTAL, "SIMTEC", Some("SIMTEC76")),
        DelphiMt => (DELPHI, "MT", Some("MT80.1")),
        DelphiDcm => (DELPHI, "DCM", Some("DCM6.2")),
        MarelliMjd => (MARELLI, "MJD", Some("MJD6JF")),
        MarelliIaw => (MARELLI, "IAW", Some("IAW16")),
        DensoSh => (DENSO, "Denso SH", None),
    };
    Expected {
        supplier,
        family,
        variant,
    }
}

/// A deterministic pseudo-random image (no markers) for false-positive tests.
pub fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            // xorshift64*
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8
        })
        .collect()
}
