//! Cross-validation against **ZedSuite** (LeZed97/ZedSuite, GPL-3.0,
//! `src/lib/ecu/bosch/checksums/edc15p-checksum.ts` + `edc16-checksum.ts`, commit
//! 77d75fb), an independent implementation of the Bosch EDC15 (VAG TDI v4.1) and
//! EDC16 (FADECAFE/CAFEAFFE, sum to 0xD01FE500) checksums.
//!
//! ZedSuite's TypeScript was run unmodified on the owner's fixture dumps
//! (2026-11-18, see docs/BOSCH-CHECKSUM.md "Cross-validation"); this test keeps
//! only its **results**, so it runs without ZedSuite: for each dump, the regions
//! ZedSuite's EDC16 scan finds, and the CRC-32 of ZedSuite's corrected output
//! after a fixed corruption (16 bytes XOR 0x5A at `corrupt_at`, inside a region
//! both tools protect). OpenTune must find every ZedSuite region, see it verify,
//! and produce the byte-identical corrected file.
//!
//! The dumps are private, so the test is ignored by default:
//!
//! ```text
//! OPENTUNE_FIXTURES=/path/to/opentune-fixtures \
//!     cargo test -p opentune-checksum --test zedsuite_xval -- --ignored
//! ```

use opentune_checksum::{crc32, edc16::parse_directory, get};

struct Case {
    file: &'static str,
    corrupt_at: usize,
    zed_fixed_crc32: u32,
    /// EDC16 regions ZedSuite finds (file offsets, inclusive); empty for EDC15.
    zed_regions: &'static [(u32, u32)],
}

const CASES: &[Case] = &[
    Case {
        file: "edc15/audi_a4_19tdi_352254_mod.bin",
        corrupt_at: 0x35ffc,
        zed_fixed_crc32: 0xB7E63002,
        zed_regions: &[],
    },
    Case {
        file: "edc15/audi_a4_19tdi_352254_ori.bin",
        corrupt_at: 0x35ffc,
        zed_fixed_crc32: 0x6879E55A,
        zed_regions: &[],
    },
    Case {
        file: "edc16-cr/MB_E220_2.2CDI_edc16_368503_mod.bin",
        corrupt_at: 0x3fffc,
        zed_fixed_crc32: 0x8E1A2291,
        zed_regions: &[(0x0, 0x7FFFF), (0x80000, 0xBFFFF), (0xC0000, 0xFDFFF)],
    },
    Case {
        file: "edc16-cr/MB_E220_2.2CDI_edc16_368503_ori.bin",
        corrupt_at: 0x3fffc,
        zed_fixed_crc32: 0x067F3885,
        zed_regions: &[(0x0, 0x7FFFF), (0x80000, 0xBFFFF), (0xC0000, 0xFDFFF)],
    },
    Case {
        file: "edc16-cr/audi_a6_30_374416_mod.bin",
        corrupt_at: 0xdfffc,
        zed_fixed_crc32: 0x8EE7820C,
        zed_regions: &[
            (0x40000, 0x17FFFF),
            (0x180000, 0x1BFFFF),
            (0x1C0000, 0x1FDFFF),
        ],
    },
    Case {
        file: "edc16-cr/audi_a6_30_374416_ori.bin",
        corrupt_at: 0xdfffc,
        zed_fixed_crc32: 0x85BB5C6D,
        zed_regions: &[
            (0x40000, 0x17FFFF),
            (0x180000, 0x1BFFFF),
            (0x1C0000, 0x1FDFFF),
        ],
    },
    Case {
        file: "edc16-cr/audi_a6_30_375256_mod.bin",
        corrupt_at: 0xdfffc,
        zed_fixed_crc32: 0x5B493235,
        zed_regions: &[
            (0x40000, 0x17FFFF),
            (0x180000, 0x1BFFFF),
            (0x1C0000, 0x1FDFFF),
        ],
    },
    Case {
        file: "edc16-cr/audi_a6_30_375256_ori.bin",
        corrupt_at: 0xdfffc,
        zed_fixed_crc32: 0x380E4B2A,
        zed_regions: &[
            (0x40000, 0x17FFFF),
            (0x180000, 0x1BFFFF),
            (0x1C0000, 0x1FDFFF),
        ],
    },
    Case {
        file: "edc16-cr/audi_q7_30_379749_mod.bin",
        corrupt_at: 0x19fffc,
        zed_fixed_crc32: 0xBBD34097,
        zed_regions: &[(0x180000, 0x1BFFFF), (0x1C0000, 0x1FDFFF)],
    },
    Case {
        file: "edc16-cr/audi_q7_30_379749_ori.bin",
        corrupt_at: 0x19fffc,
        zed_fixed_crc32: 0xCF115FA2,
        zed_regions: &[(0x180000, 0x1BFFFF), (0x1C0000, 0x1FDFFF)],
    },
    Case {
        file: "edc16-full/bmw_318d_376967_mod.bin",
        corrupt_at: 0x7fffc,
        zed_fixed_crc32: 0xCC35FBBF,
        zed_regions: &[(0x40000, 0xBFFFF)],
    },
    Case {
        file: "edc16-full/bmw_318d_376967_ori.bin",
        corrupt_at: 0x7fffc,
        zed_fixed_crc32: 0xA248803A,
        zed_regions: &[(0x40000, 0xBFFFF)],
    },
    Case {
        file: "edc16-full/kia_sorento_381743_mod.bin",
        corrupt_at: 0xefffc,
        zed_fixed_crc32: 0x0643756E,
        zed_regions: &[
            (0x40000, 0x19FFFF),
            (0x1A0000, 0x1BFFFF),
            (0x1C0000, 0x1FDFFF),
        ],
    },
    Case {
        file: "edc16-full/kia_sorento_381743_ori.bin",
        corrupt_at: 0xefffc,
        zed_fixed_crc32: 0x7E8C71FF,
        zed_regions: &[
            (0x40000, 0x19FFFF),
            (0x1A0000, 0x1BFFFF),
            (0x1C0000, 0x1FDFFF),
        ],
    },
    Case {
        file: "edc16-full/kia_sorento_388042_mod.bin",
        corrupt_at: 0xefffc,
        zed_fixed_crc32: 0x558C24CD,
        zed_regions: &[
            (0x40000, 0x19FFFF),
            (0x1A0000, 0x1BFFFF),
            (0x1C0000, 0x1FDFFF),
        ],
    },
    Case {
        file: "edc16-full/kia_sorento_388042_ori.bin",
        corrupt_at: 0xefffc,
        zed_fixed_crc32: 0xB151B3BA,
        zed_regions: &[
            (0x40000, 0x19FFFF),
            (0x1A0000, 0x1BFFFF),
            (0x1C0000, 0x1FDFFF),
        ],
    },
    Case {
        file: "edc16-full/kia_sorento_390051_mod.bin",
        corrupt_at: 0xefffc,
        zed_fixed_crc32: 0xB4BF4619,
        zed_regions: &[
            (0x40000, 0x19FFFF),
            (0x1A0000, 0x1BFFFF),
            (0x1C0000, 0x1FDFFF),
        ],
    },
    Case {
        file: "edc16-full/kia_sorento_390051_ori.bin",
        corrupt_at: 0xefffc,
        zed_fixed_crc32: 0x3F6F869C,
        zed_regions: &[
            (0x40000, 0x19FFFF),
            (0x1A0000, 0x1BFFFF),
            (0x1C0000, 0x1FDFFF),
        ],
    },
    Case {
        file: "edc16-full/kia_sorento_394237_mod.bin",
        corrupt_at: 0xefffc,
        zed_fixed_crc32: 0x1B394819,
        zed_regions: &[
            (0x40000, 0x19FFFF),
            (0x1A0000, 0x1BFFFF),
            (0x1C0000, 0x1FDFFF),
        ],
    },
    Case {
        file: "edc16-full/kia_sorento_394237_ori.bin",
        corrupt_at: 0xefffc,
        zed_fixed_crc32: 0x30DEF117,
        zed_regions: &[
            (0x40000, 0x19FFFF),
            (0x1A0000, 0x1BFFFF),
            (0x1C0000, 0x1FDFFF),
        ],
    },
    Case {
        file: "edc16-full/kia_sorento_394261_mod.bin",
        corrupt_at: 0xefffc,
        zed_fixed_crc32: 0xEB1D7572,
        zed_regions: &[
            (0x40000, 0x19FFFF),
            (0x1A0000, 0x1BFFFF),
            (0x1C0000, 0x1FDFFF),
        ],
    },
    Case {
        file: "edc16-full/kia_sorento_394261_ori.bin",
        corrupt_at: 0xefffc,
        zed_fixed_crc32: 0x90C926CA,
        zed_regions: &[
            (0x40000, 0x19FFFF),
            (0x1A0000, 0x1BFFFF),
            (0x1C0000, 0x1FDFFF),
        ],
    },
    Case {
        file: "edc16-pairs/audi_a4_371805_2m_mod.bin",
        corrupt_at: 0x6fffc,
        zed_fixed_crc32: 0x4CEC6DD5,
        zed_regions: &[
            (0x40000, 0x9FFFF),
            (0x180000, 0x1BFFFF),
            (0x1C0000, 0x1FDFFF),
        ],
    },
    Case {
        file: "edc16-pairs/audi_a4_371805_2m_ori.bin",
        corrupt_at: 0x6fffc,
        zed_fixed_crc32: 0x524076D0,
        zed_regions: &[
            (0x40000, 0x9FFFF),
            (0x180000, 0x1BFFFF),
            (0x1C0000, 0x1FDFFF),
        ],
    },
    Case {
        file: "edc16-pairs/audi_a4_372668_2m_mod.bin",
        corrupt_at: 0x6fffc,
        zed_fixed_crc32: 0xFE9C1DFF,
        zed_regions: &[
            (0x40000, 0x9FFFF),
            (0x180000, 0x1BFFFF),
            (0x1C0000, 0x1FDFFF),
        ],
    },
    Case {
        file: "edc16-pairs/audi_a4_372668_2m_ori.bin",
        corrupt_at: 0x6fffc,
        zed_fixed_crc32: 0xBD9C775C,
        zed_regions: &[
            (0x40000, 0x9FFFF),
            (0x180000, 0x1BFFFF),
            (0x1C0000, 0x1FDFFF),
        ],
    },
    Case {
        file: "edc16-pairs/audi_a6_376101_512k_mod.bin",
        corrupt_at: 0x1fffc,
        zed_fixed_crc32: 0x68B33148,
        zed_regions: &[(0x0, 0x3FFFF), (0x40000, 0x7DFFF)],
    },
    Case {
        file: "edc16-pairs/audi_a6_376101_512k_ori.bin",
        corrupt_at: 0x1fffc,
        zed_fixed_crc32: 0xF72960F9,
        zed_regions: &[(0x0, 0x3FFFF), (0x40000, 0x7DFFF)],
    },
    Case {
        file: "edc16-pairs/audi_a6_393546_2m_mod.bin",
        corrupt_at: 0xdfffc,
        zed_fixed_crc32: 0xA52060CA,
        zed_regions: &[
            (0x40000, 0x17FFFF),
            (0x180000, 0x1BFFFF),
            (0x1C0000, 0x1FDFFF),
        ],
    },
    Case {
        file: "edc16-pairs/audi_a6_393546_2m_ori.bin",
        corrupt_at: 0xdfffc,
        zed_fixed_crc32: 0xCDE678C4,
        zed_regions: &[
            (0x40000, 0x17FFFF),
            (0x180000, 0x1BFFFF),
            (0x1C0000, 0x1FDFFF),
        ],
    },
    Case {
        file: "edc16-pairs/bmw_e46_318d_376967_2m_mod.bin",
        corrupt_at: 0x7fffc,
        zed_fixed_crc32: 0xCC35FBBF,
        zed_regions: &[(0x40000, 0xBFFFF)],
    },
    Case {
        file: "edc16-pairs/bmw_e46_318d_376967_2m_ori.bin",
        corrupt_at: 0x7fffc,
        zed_fixed_crc32: 0xA248803A,
        zed_regions: &[(0x40000, 0xBFFFF)],
    },
    Case {
        file: "edc16-pairs/freelander_1037386593_512k_mod.bin",
        corrupt_at: 0x3e7fc,
        zed_fixed_crc32: 0x9C390EAA,
        zed_regions: &[(0x0, 0x7CFFF)],
    },
    Case {
        file: "edc16-pairs/freelander_1037386593_512k_ori.bin",
        corrupt_at: 0x3e7fc,
        zed_fixed_crc32: 0x6E67E95E,
        zed_regions: &[(0x0, 0x7CFFF)],
    },
    Case {
        file: "edc16-pairs/vw_jetta_371296_512k_mod.bin",
        corrupt_at: 0x1fffc,
        zed_fixed_crc32: 0x0F75D5CA,
        zed_regions: &[(0x0, 0x3FFFF), (0x40000, 0x7DFFF)],
    },
    Case {
        file: "edc16-pairs/vw_jetta_371296_512k_ori.bin",
        corrupt_at: 0x1fffc,
        zed_fixed_crc32: 0x766F9046,
        zed_regions: &[(0x0, 0x3FFFF), (0x40000, 0x7DFFF)],
    },
    Case {
        file: "edc16-pairs/vw_passat_370476_512k_mod.bin",
        corrupt_at: 0x1fffc,
        zed_fixed_crc32: 0x446CD834,
        zed_regions: &[(0x0, 0x3FFFF), (0x40000, 0x7DFFF)],
    },
    Case {
        file: "edc16-pairs/vw_passat_370476_512k_ori.bin",
        corrupt_at: 0x1fffc,
        zed_fixed_crc32: 0xE19409D3,
        zed_regions: &[(0x0, 0x3FFFF), (0x40000, 0x7DFFF)],
    },
    Case {
        file: "edc16-pd/golf19_374183_mod.bin",
        corrupt_at: 0x3fffc,
        zed_fixed_crc32: 0x5C0B056C,
        zed_regions: &[(0x0, 0x7FFFF), (0x80000, 0xCFFFF), (0xD0000, 0xFDFFF)],
    },
    Case {
        file: "edc16-pd/golf19_374183_ori.bin",
        corrupt_at: 0x3fffc,
        zed_fixed_crc32: 0xA1662091,
        zed_regions: &[(0x0, 0x7FFFF), (0x80000, 0xCFFFF), (0xD0000, 0xFDFFF)],
    },
    Case {
        file: "edc16-pd/golf19_375902_mod.bin",
        corrupt_at: 0x3fffc,
        zed_fixed_crc32: 0x732BB6AC,
        zed_regions: &[(0x0, 0x7FFFF), (0x80000, 0xCFFFF), (0xD0000, 0xFDFFF)],
    },
    Case {
        file: "edc16-pd/golf19_375902_ori.bin",
        corrupt_at: 0x3fffc,
        zed_fixed_crc32: 0xE26C1055,
        zed_regions: &[(0x0, 0x7FFFF), (0x80000, 0xCFFFF), (0xD0000, 0xFDFFF)],
    },
    Case {
        file: "edc16-pd/golf19_381814_mod.bin",
        corrupt_at: 0xdfffc,
        zed_fixed_crc32: 0x47BF8540,
        zed_regions: &[
            (0x40000, 0x17FFFF),
            (0x180000, 0x1BFFFF),
            (0x1C0000, 0x1FDFFF),
        ],
    },
    Case {
        file: "edc16-pd/golf19_381814_ori.bin",
        corrupt_at: 0xdfffc,
        zed_fixed_crc32: 0x3D057194,
        zed_regions: &[
            (0x40000, 0x17FFFF),
            (0x180000, 0x1BFFFF),
            (0x1C0000, 0x1FDFFF),
        ],
    },
    Case {
        file: "edc16-pd/golf19_389289_mod.bin",
        corrupt_at: 0xdfffc,
        zed_fixed_crc32: 0xFAEEC415,
        zed_regions: &[
            (0x40000, 0x17FFFF),
            (0x180000, 0x1BFFFF),
            (0x1C0000, 0x1FDFFF),
        ],
    },
    Case {
        file: "edc16-pd/golf19_389289_ori.bin",
        corrupt_at: 0xdfffc,
        zed_fixed_crc32: 0x28263C7B,
        zed_regions: &[
            (0x40000, 0x17FFFF),
            (0x180000, 0x1BFFFF),
            (0x1C0000, 0x1FDFFF),
        ],
    },
    Case {
        file: "edc16-pd/golf20_370223_mod.bin",
        corrupt_at: 0x3fffc,
        zed_fixed_crc32: 0xA52900BA,
        zed_regions: &[(0x0, 0x7FFFF), (0x80000, 0xCFFFF), (0xD0000, 0xFDFFF)],
    },
    Case {
        file: "edc16-pd/golf20_370223_ori.bin",
        corrupt_at: 0x3fffc,
        zed_fixed_crc32: 0x5FFE7A58,
        zed_regions: &[(0x0, 0x7FFFF), (0x80000, 0xCFFFF), (0xD0000, 0xFDFFF)],
    },
    Case {
        file: "edc16-pd/golf20_376971_mod.bin",
        corrupt_at: 0x3fffc,
        zed_fixed_crc32: 0x7C251549,
        zed_regions: &[(0x0, 0x7FFFF), (0x80000, 0xCFFFF), (0xD0000, 0xFDFFF)],
    },
    Case {
        file: "edc16-pd/golf20_376971_ori.bin",
        corrupt_at: 0x3fffc,
        zed_fixed_crc32: 0x6890FE00,
        zed_regions: &[(0x0, 0x7FFFF), (0x80000, 0xCFFFF), (0xD0000, 0xFDFFF)],
    },
    Case {
        file: "edc16/uaz_edc16c39_398273_ori.bin",
        corrupt_at: 0xafffc,
        zed_fixed_crc32: 0x7A01313C,
        zed_regions: &[(0x40000, 0x11FFFF), (0x1E0000, 0x1FFFFF)],
    },
    Case {
        file: "golf-real/stock.bin",
        corrupt_at: 0x19fffc,
        zed_fixed_crc32: 0xAB84B8BB,
        zed_regions: &[(0x180000, 0x1BFFFF), (0x1C0000, 0x1FDFFF)],
    },
    Case {
        file: "golf-real/tuned_fixed.bin",
        corrupt_at: 0x19fffc,
        zed_fixed_crc32: 0xA08C8B19,
        zed_regions: &[(0x180000, 0x1BFFFF), (0x1C0000, 0x1FDFFF)],
    },
    Case {
        file: "golf-real/tuned_uncorrected.bin",
        corrupt_at: 0x19fffc,
        zed_fixed_crc32: 0xA08C8B19,
        zed_regions: &[(0x180000, 0x1BFFFF), (0x1C0000, 0x1FDFFF)],
    },
];

fn fixtures() -> Option<String> {
    std::env::var("OPENTUNE_FIXTURES").ok()
}

#[test]
#[ignore = "needs the owner's EDC15/EDC16 dumps via OPENTUNE_FIXTURES"]
fn corrected_output_matches_zedsuite() {
    let Some(dir) = fixtures() else { return };
    let mut checked = 0;
    for c in CASES {
        let Ok(mut img) = std::fs::read(format!("{dir}/{}", c.file)) else {
            continue;
        };
        let algo = if c.file.starts_with("edc15/") {
            "edc15c6"
        } else {
            "edc16"
        };
        let algo = get(algo).unwrap();
        // Every dump verifies as dumped, except the deliberately uncorrected tune.
        assert!(
            c.file.contains("uncorrected") || algo.verify(&img).unwrap().iter().all(|b| b.ok),
            "{}: verifies as dumped",
            c.file
        );
        if !c.zed_regions.is_empty() {
            let ours = parse_directory(&img);
            for &(s, e) in c.zed_regions {
                assert!(
                    ours.iter().any(|b| b.start == s && b.end == e),
                    "{}: ZedSuite region {s:#x}..={e:#x} missing from our directory",
                    c.file
                );
            }
        }
        for b in &mut img[c.corrupt_at..c.corrupt_at + 16] {
            *b ^= 0x5A;
        }
        algo.correct(&mut img).unwrap();
        assert_eq!(
            crc32(&img),
            c.zed_fixed_crc32,
            "{}: corrected output differs from ZedSuite's",
            c.file
        );
        checked += 1;
    }
    assert!(
        checked > 0,
        "no EDC15/EDC16 fixtures under OPENTUNE_FIXTURES"
    );
}

/// ZedSuite's `tdi41_2002` variant verifies all 11 checksums of the classic
/// (V4.1 code blocks at 0x50001/0x60001/0x70001) VW Golf stock read; OpenTune
/// does not implement that variant yet and must keep refusing the file rather
/// than apply the compact-layout table to it.
#[test]
#[ignore = "needs the owner's EDC15 dumps via OPENTUNE_FIXTURES"]
fn classic_tdi41_is_refused_where_zedsuite_uses_tdi41_2002() {
    let Some(dir) = fixtures() else { return };
    let Ok(mut img) = std::fs::read(format!("{dir}/edc15/vw_golf_classic_tdi41_ori.bin")) else {
        return;
    };
    let before = img.clone();
    assert!(get("edc15").unwrap().correct(&mut img).is_err());
    assert_eq!(img, before);
}
