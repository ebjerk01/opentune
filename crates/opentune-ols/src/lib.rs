//! Read **WinOLS `.ols`** project files into OpenTune.
//!
//! A `.ols` file is the proprietary project container written by EVC/WinOLS. It
//! is **not** a raw ECU dump: it bundles one or more saved ROM *versions* of the
//! same ECU (e.g. "Original" + "Stage 1", kept together so WinOLS can diff them)
//! behind a small metadata header, plus — in WinOLS 5 projects — the project's
//! own map/table definitions. This crate opens such a container and produces:
//!
//! * the embedded **ROM image** ([`RomImage`]) for a chosen version,
//! * the project's **map definitions** as OpenTune [`MapDef`]s
//!   ([`opentune_core::ecudef::MapDef`]),
//! * the project **metadata** ([`Metadata`]) and the per-version labels.
//!
//! # What is recoverable, and what is not
//!
//! WinOLS `.ols` is only *partly* a plaintext format, and this crate is honest
//! about the boundary:
//!
//! * **Plaintext / parsed.** The 24-byte header and signature; the
//!   identification metadata (make, model, ECU/manufacturer, HW/SW numbers,
//!   revision tag); the version directory (count, offsets, per-version size);
//!   the raw ROM bytes of each saved version (stored uncompressed, extracted
//!   byte-for-byte); and, for container format version >= 800 (WinOLS 5), the
//!   map records — name, comment, unit, grid, cell type/endianness, linear
//!   factor/offset, map address and both axes' addresses/scaling.
//!
//! * **Opaque / not recovered.** Older WinOLS 4.x containers (format < 800) use
//!   a different map-record layout that this crate does not decode: the ROM
//!   still extracts, but [`OlsProject::maps`] comes back empty with a warning.
//!   **Encrypted / "protected" projects**, where WinOLS obfuscates the header
//!   and body, are not decrypted — such a file fails [`parse`](OlsProject::parse)
//!   with [`Error::NotContainer`] because the plaintext layout is absent.
//!   WinOLS's internal **checksum-region definitions** are not located in the
//!   metadata this crate reads, so they are reported as unrecovered (a warning)
//!   rather than guessed. Non-linear value scaling and 64-bit cell types have
//!   no OpenTune equivalent and are flagged, never fabricated.
//!
//! Addresses (map and axis) are **image-relative offsets** into the extracted
//! ROM image; WinOLS stores the image starting at offset 0, so [`RomImage::base`]
//! is `0` and the absolute flash base address (ECU-specific, not stored in the
//! container) is intentionally left for the caller to apply.
//!
//! # Provenance
//!
//! The `.ols` container and record layout implemented here was learned from the
//! structure of two GPL-3.0 open-source readers — `LeZed97/ZedSuite` and the
//! `ctabuyo/romHEX14-community` project it cross-references — and
//! **reimplemented** with this crate's own types and decomposition. No source
//! text is copied; see `NOTICE`/`LICENSE` for attribution. OpenTune is GPL-3.0,
//! compatible with both.
//!
//! # Example
//!
//! ```no_run
//! let bytes = std::fs::read("project.ols").unwrap();
//! match opentune_ols::OlsProject::parse(&bytes) {
//!     Ok(project) => {
//!         println!("{} ({})", project.metadata.ecu_name, project.metadata.manufacturer);
//!         println!("ROM: {} bytes", project.rom.bytes.len());
//!         println!("{} map(s) recovered", project.maps.len());
//!         for w in &project.warnings {
//!             eprintln!("warning: {w}");
//!         }
//!     }
//!     Err(e) => eprintln!("not an importable .ols: {e}"),
//! }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod container;
mod error;
mod maps;
mod reader;
mod versions;

pub use container::Metadata;
pub use error::{Error, Result};
pub use versions::VersionInfo;

use opentune_core::ecudef::MapDef;

/// One saved ROM image extracted from a `.ols` container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RomImage {
    /// Image-relative base address. Always `0`: WinOLS stores the ROM starting
    /// at offset 0 and records map/axis addresses as offsets into it. The
    /// absolute flash base address is ECU-specific and not held in the
    /// container, so the caller applies it if needed.
    pub base: u32,
    /// The raw ROM bytes, exactly as stored (uncompressed, unmodified).
    pub bytes: Vec<u8>,
    /// Which saved version these bytes came from.
    pub version_index: u32,
}

impl RomImage {
    /// Size of the image in bytes.
    pub fn size(&self) -> usize {
        self.bytes.len()
    }
}

/// A parsed WinOLS `.ols` project: ROM image, metadata, versions, maps, and any
/// fidelity warnings collected along the way.
#[derive(Debug, Clone, PartialEq)]
pub struct OlsProject {
    /// WinOLS container format version (e.g. 804 ~ WinOLS 5.84).
    pub format_version: u32,
    /// Project identification strings.
    pub metadata: Metadata,
    /// The chosen version's extracted ROM image.
    pub rom: RomImage,
    /// Every saved version in the container, with labels where recoverable.
    pub versions: Vec<VersionInfo>,
    /// Map definitions recovered from the project (empty for WinOLS 4.x
    /// containers, whose record layout this crate does not decode).
    pub maps: Vec<MapDef>,
    /// Non-fatal fidelity notes: anything that did not round-trip cleanly (a
    /// skipped map, a dropped axis address, an unread map section, an
    /// unrecovered checksum region). An empty list means nothing lossy was
    /// detected; it never means the import is guaranteed complete.
    pub warnings: Vec<String>,
}

impl OlsProject {
    /// Parse a `.ols` container, extracting saved **version 0** as the ROM image.
    ///
    /// Returns [`Error::NotContainer`] when the bytes are not a plaintext WinOLS
    /// project, or [`Error::NoVersionDirectory`] when the version directory
    /// cannot be located (so no ROM can be sliced out safely).
    pub fn parse(data: &[u8]) -> Result<Self> {
        Self::parse_version(data, 0)
    }

    /// Parse a `.ols` container, extracting the saved version at `index` as the
    /// ROM image. The map definitions and metadata are the project's and do not
    /// depend on `index`.
    pub fn parse_version(data: &[u8], index: u32) -> Result<Self> {
        let header = container::parse_header(data).ok_or(Error::NotContainer)?;
        let metadata = container::parse_metadata(data, header);
        let dir = container::find_version_directory(data).ok_or(Error::NoVersionDirectory)?;
        let bytes = container::extract_version(data, dir, index)?;

        let labels = versions::split_labels(&metadata.revision_tag, dir.count);
        let version_list = (0..dir.count)
            .map(|i| VersionInfo {
                index: i,
                size: dir.record_size,
                label: labels.as_ref().and_then(|l| l.get(i as usize).cloned()),
            })
            .collect();

        // Map records live between the header/metadata and the version data.
        let region_end = dir.data_start as usize;
        let (maps, mut warnings) = maps::to_map_defs(data, region_end);

        if header.format_version < 800 {
            warnings.push(format!(
                "container format version {} predates WinOLS 5; map records are in an \
                 unread layout, so no maps were recovered (ROM image is unaffected)",
                header.format_version
            ));
        }
        warnings.push(
            "WinOLS internal checksum-region definitions are not present in the plaintext \
             metadata this parser reads and were not recovered"
                .to_string(),
        );

        Ok(Self {
            format_version: header.format_version,
            metadata,
            rom: RomImage {
                base: 0,
                bytes,
                version_index: index,
            },
            versions: version_list,
            maps,
            warnings,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentune_core::ecudef::{DataType, Endian};

    // ---- fixture builders (a minimal `.ols`-shaped buffer per this crate's
    // own implemented layout; ships no real WinOLS file) ----

    const FORMAT_VERSION: u32 = 804;

    /// A metadata-block string as a format >= 439 container writes it: 4-byte LE
    /// length then the bare body (no trailing NUL). Empty uses a 0 length.
    fn meta_str(s: &str) -> Vec<u8> {
        let mut v = (s.len() as i32).to_le_bytes().to_vec();
        v.extend_from_slice(s.as_bytes());
        v
    }

    /// A WinOLS 5 record string: 4-byte LE length then the bare body.
    fn rec_str(s: &str) -> Vec<u8> {
        let mut v = (s.len() as i32).to_le_bytes().to_vec();
        v.extend_from_slice(s.as_bytes());
        v
    }

    fn u32b(v: u32) -> [u8; 4] {
        v.to_le_bytes()
    }

    fn folder_ref(name: &str) -> Vec<u8> {
        let mut v = rec_str(name);
        v.extend_from_slice(&u32b(2));
        v.extend_from_slice(&u32b(2));
        v.extend_from_slice(&u32b(0)); // entry count
        v
    }

    fn axis(
        name: &str,
        unit: &str,
        factor: f64,
        address: u32,
        type_code: u32,
        signed: bool,
    ) -> Vec<u8> {
        let mut v = folder_ref(name);
        v.extend(rec_str(unit));
        v.extend_from_slice(&factor.to_le_bytes());
        v.extend_from_slice(&0f64.to_le_bytes()); // offset
        v.extend_from_slice(&u32b(1)); // axis kind
        v.extend_from_slice(&u32b(address));
        v.extend_from_slice(&u32b(type_code));
        v.extend_from_slice(&u32b(0));
        v.extend_from_slice(&u32b(16)); // cell bits
        v.extend_from_slice(&0u16.to_le_bytes()); // backwards
        v.extend_from_slice(&[0u8; 16]); // skip 8 + 4 + 4
        v.push(signed as u8);
        v.extend_from_slice(&u32b(0)); // inline byte count
        v.extend_from_slice(&u32b(0));
        v.extend_from_slice(&u32b(0)); // header word
        v.extend(rec_str("")); // id-name
        v.extend_from_slice(&u32b(0)); // folder count
        v.extend_from_slice(&u32b(0));
        v
    }

    /// One full map record (including the 0xFFFFFFFF introducer), laid out
    /// exactly as `maps::read_record` walks it.
    #[allow(clippy::too_many_arguments)]
    fn record(
        name: &str,
        comment: &str,
        unit: &str,
        rows: u32,
        cols: u32,
        start: u32,
        base: u32,
        type_code: u32,
        factor: f64,
        signed: bool,
    ) -> Vec<u8> {
        let cell_bytes = match type_code {
            2 | 3 => 2,
            4..=7 => 4,
            10..=13 => 8,
            _ => 1,
        };
        let end = start + rows * cols * cell_bytes;

        let mut v = vec![0xFF, 0xFF, 0xFF, 0xFF];
        v.extend(folder_ref(comment));
        v.push(0);
        v.extend(rec_str(name));
        v.extend_from_slice(&u32b(0));
        v.extend_from_slice(&u32b(0));
        v.extend_from_slice(&u32b(0));
        v.extend_from_slice(&u32b(0)); // kennfeld type
        v.extend_from_slice(&u32b(0));
        v.extend_from_slice(&u32b(type_code));
        v.extend_from_slice(&u32b(0));
        v.extend_from_slice(&u32b(16)); // cell bits
        v.extend_from_slice(&u32b(0));
        v.extend(rec_str("")); // id-name
        v.extend_from_slice(&u32b(0));
        v.extend_from_slice(&u32b(0));
        v.extend_from_slice(&u32b(0));
        v.push(0);
        v.extend_from_slice(&u32b(0));
        v.extend_from_slice(&[0u8; 48]);
        v.extend_from_slice(&[0u8; 48]);
        v.push(0); // cell inverse
        v.push(signed as u8);
        v.push(0);
        v.push(0);
        v.extend_from_slice(&u32b(cols));
        v.extend_from_slice(&u32b(rows));
        v.extend_from_slice(&u32b(0));
        v.extend_from_slice(&u32b(0));
        v.extend_from_slice(&u32b(0)); // precision
        v.extend(rec_str("")); // label
        v.extend(rec_str(unit));
        v.extend_from_slice(&factor.to_le_bytes());
        v.extend_from_slice(&0f64.to_le_bytes()); // offset
        v.extend_from_slice(&u32b(start));
        v.extend_from_slice(&u32b(end));
        v.extend_from_slice(&u32b(base));
        v.extend_from_slice(&0f64.to_le_bytes());
        v.extend_from_slice(&[0u8; 20]); // 5 x u32
        v.extend(axis("rpm", "rpm", 1.0, 0x200, type_code, false)); // X
        v.extend(axis("load", "%", 0.5, 0x220, type_code, false)); // Y
        v
    }

    fn metadata_block(ecu: &str, revision_tag: &str) -> Vec<u8> {
        let mut v = Vec::new();
        for s in [
            "Seat", "Leon", "", "", "", "", "", "", "", // make,model,+7
            "Bosch", ecu, "hw1", "sw1", // manufacturer, ecu, hw, sw
            "", "", "", // production, engine code, transmission
        ] {
            v.extend(meta_str(s));
        }
        v.extend_from_slice(&0u64.to_le_bytes()); // last write time
        for s in ["", "", ""] {
            v.extend(meta_str(s)); // original filename, ols version, reserved
        }
        v.extend(meta_str(revision_tag));
        v
    }

    /// Assemble a single-version `.ols` with the given records and ROM size.
    fn build(ecu: &str, revision_tag: &str, records: &[Vec<u8>], rom: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&0x0000_000Bu32.to_le_bytes());
        buf.extend_from_slice(b"WinOLS File\0");
        buf.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        buf.extend_from_slice(&u32b(0)); // reserved header word (24 bytes total)

        buf.extend(metadata_block(ecu, revision_tag));
        for r in records {
            buf.extend_from_slice(r);
        }

        // Version directory header + anchor, then the single ROM version.
        let version_data_start = (buf.len() + 16) as u32;
        buf.extend_from_slice(&u32b(0)); // count - 1
        buf.extend_from_slice(&u32b(version_data_start));
        buf.extend_from_slice(&u32b(rom.len() as u32));
        buf.extend_from_slice(&0x4200_7899u32.to_le_bytes()); // anchor
        buf.extend_from_slice(&u32b(0)); // the +4 field before version 0
        buf.extend_from_slice(rom);
        buf
    }

    // ---- tests ----

    #[test]
    fn rejects_non_container() {
        let junk = vec![0x00, 0x01, 0x02, 0x03, 0xFF, 0xFE, 0x12, 0x34];
        assert_eq!(OlsProject::parse(&junk), Err(Error::NotContainer));
        assert_eq!(OlsProject::parse(&[]), Err(Error::NotContainer));
    }

    #[test]
    fn extracts_rom_and_metadata() {
        let rom: Vec<u8> = (0u8..=255).cycle().take(512).collect();
        let file = build("EDC15P", "0 (Original)", &[], &rom);
        let p = OlsProject::parse(&file).expect("parse");
        assert_eq!(p.format_version, 804);
        assert_eq!(p.metadata.make, "Seat");
        assert_eq!(p.metadata.model, "Leon");
        assert_eq!(p.metadata.manufacturer, "Bosch");
        assert_eq!(p.metadata.ecu_name, "EDC15P");
        assert_eq!(p.metadata.hw_number, "hw1");
        assert_eq!(p.metadata.sw_number, "sw1");
        assert_eq!(p.rom.base, 0);
        assert_eq!(p.rom.bytes, rom);
        assert_eq!(p.rom.version_index, 0);
        assert_eq!(p.versions.len(), 1);
        assert_eq!(p.versions[0].size, 512);
    }

    #[test]
    fn recovers_a_map_as_a_mapdef() {
        let rom = vec![0u8; 0x200];
        // 2 rows x 3 cols, u16 little-endian (type 3), at 0x100, factor 0.1.
        let rec = record(
            "EGR setpoint",
            "EGR comment",
            "mg",
            2,
            3,
            0x100,
            0x200,
            3,
            0.1,
            false,
        );
        let file = build("EDC15P", "0 (Original)", &[rec], &rom);
        let p = OlsProject::parse(&file).expect("parse");
        assert_eq!(p.maps.len(), 1, "warnings: {:?}", p.warnings);
        let m = &p.maps[0];
        assert_eq!(m.name, "EGR setpoint");
        assert_eq!(m.notes, "EGR comment");
        assert_eq!(m.address, 0x100);
        assert_eq!((m.rows, m.cols), (2, 3));
        assert_eq!(m.data_type, DataType::U16);
        assert_eq!(m.endian, Endian::Little);
        assert_eq!(m.unit, "mg");
        assert!((m.scaling.factor - 0.1).abs() < 1e-12);
        assert_eq!(m.byte_len(), 2 * 3 * 2);

        let x = m.x_axis.as_ref().expect("x axis");
        assert_eq!(x.name, "rpm");
        assert_eq!(x.count, 3);
        assert_eq!(x.address, Some(0x200));
        let y = m.y_axis.as_ref().expect("y axis");
        assert_eq!(y.name, "load");
        assert_eq!(y.count, 2);
        assert_eq!(y.address, Some(0x220));
        assert!((y.scaling.factor - 0.5).abs() < 1e-12);
    }

    #[test]
    fn signed_big_endian_32bit_map() {
        let rom = vec![0u8; 0x400];
        // type 4 = 32-bit big-endian, signed.
        let rec = record("Torque", "", "Nm", 4, 4, 0x100, 0x300, 4, 1.0, true);
        let file = build("EDC17", "0 (Original)", &[rec], &rom);
        let p = OlsProject::parse(&file).expect("parse");
        assert_eq!(p.maps.len(), 1, "warnings: {:?}", p.warnings);
        assert_eq!(p.maps[0].data_type, DataType::S32);
        assert_eq!(p.maps[0].endian, Endian::Big);
    }

    #[test]
    fn multiple_versions_get_labels() {
        let rom = vec![0xABu8; 128];
        let file = build_multi("2 (Original, Stage 1, Stage 2)", 3, &rom);
        let p = OlsProject::parse(&file).expect("parse");
        assert_eq!(p.versions.len(), 3);
        assert_eq!(p.versions[0].label.as_deref(), Some("Original"));
        assert_eq!(p.versions[1].label.as_deref(), Some("Stage 1"));
        assert_eq!(p.versions[2].label.as_deref(), Some("Stage 2"));
    }

    /// A multi-version container: `count` copies of `rom` back to back.
    fn build_multi(revision_tag: &str, count: u32, rom: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&0x0000_000Bu32.to_le_bytes());
        buf.extend_from_slice(b"WinOLS File\0");
        buf.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        buf.extend_from_slice(&u32b(0));
        buf.extend(metadata_block("EDC15P", revision_tag));

        let version_data_start = (buf.len() + 16) as u32;
        buf.extend_from_slice(&u32b(count - 1));
        buf.extend_from_slice(&u32b(version_data_start));
        buf.extend_from_slice(&u32b(rom.len() as u32));
        buf.extend_from_slice(&0x4200_7899u32.to_le_bytes());
        buf.extend_from_slice(&u32b(0));
        for _ in 0..count {
            buf.extend_from_slice(rom);
        }
        buf
    }

    #[test]
    fn parse_version_out_of_range_errors() {
        let rom = vec![0u8; 64];
        let file = build("EDC15P", "0 (Original)", &[], &rom);
        assert_eq!(
            OlsProject::parse_version(&file, 5),
            Err(Error::VersionOutOfRange {
                requested: 5,
                available: 1
            })
        );
    }

    #[test]
    fn old_format_has_no_maps_but_keeps_rom() {
        let rom = vec![0x11u8; 100];
        let rec = record("EGR", "", "mg", 2, 2, 0x10, 0x100, 3, 0.1, false);
        let mut file = build("EDC15", "0 (Original)", &[rec], &rom);
        // Downgrade the format version to a WinOLS 4-era value.
        file[16..20].copy_from_slice(&288u32.to_le_bytes());
        let p = OlsProject::parse(&file).expect("parse");
        assert!(p.maps.is_empty());
        assert_eq!(p.rom.bytes, rom);
        assert!(p.warnings.iter().any(|w| w.contains("predates WinOLS 5")));
    }

    #[test]
    fn malformed_record_is_skipped_not_fatal() {
        let rom = vec![0u8; 0x200];
        // A record whose end address does not match rows*cols*cell_bytes.
        let mut bad = record("Broken", "", "mg", 4, 4, 0x100, 0x200, 3, 0.1, false);
        // Corrupt the `end` u32: locate the correct end and clobber it.
        let good_end = (0x100u32 + 4 * 4 * 2).to_le_bytes();
        let pos = bad.windows(4).position(|w| w == good_end).unwrap();
        bad[pos..pos + 4].copy_from_slice(&(0x100u32 + 3).to_le_bytes());
        let file = build("EDC15P", "0 (Original)", &[bad], &rom);
        let p = OlsProject::parse(&file).expect("parse still succeeds");
        assert!(p.maps.is_empty());
        // ROM still recovered.
        assert_eq!(p.rom.bytes.len(), 0x200);
    }

    #[test]
    fn always_warns_about_unrecovered_checksum_regions() {
        let rom = vec![0u8; 64];
        let file = build("EDC15P", "0 (Original)", &[], &rom);
        let p = OlsProject::parse(&file).expect("parse");
        assert!(p.warnings.iter().any(|w| w.contains("checksum-region")));
    }

    // ---- coverage: malformed / minimal / edge inputs ------------------------

    #[test]
    fn truncated_container_errors_without_panicking() {
        // A real container cut off partway through: the parser must return an
        // error (not a valid project, and above all not a panic / slice OOB).
        let rom = vec![0u8; 256];
        let file = build("EDC15P", "0 (Original)", &[], &rom);
        for cut in [8usize, 24, file.len() / 2, file.len() - 1] {
            let r = OlsProject::parse(&file[..cut]);
            assert!(
                r.is_err(),
                "truncation to {cut} bytes should error, got {r:?}"
            );
        }
    }

    #[test]
    fn minimal_single_byte_rom_parses() {
        // The smallest useful container: one saved version holding a single ROM
        // byte. It must parse, recover that exact byte, and report one version.
        let file = build("MINI", "0 (Original)", &[], &[0x42]);
        let p = OlsProject::parse(&file).expect("parse minimal");
        assert_eq!(p.rom.bytes, vec![0x42]);
        assert_eq!(p.rom.size(), 1);
        assert_eq!(p.versions.len(), 1);
        assert_eq!(p.versions[0].size, 1);
    }

    #[test]
    fn unrepresentable_64bit_cell_type_is_skipped_with_warning() {
        // Cell type code 10 is a 64-bit cell, which OpenTune cannot store. The
        // map must be skipped with an explanatory warning (never emitted with a
        // wrong width), while the ROM image is still recovered intact.
        let rom = vec![0u8; 0x400];
        let rec = record("Wide Map", "", "mg", 2, 2, 0x100, 0x200, 10, 1.0, false);
        let file = build("EDC17", "0 (Original)", &[rec], &rom);
        let p = OlsProject::parse(&file).expect("parse");
        assert!(p.maps.is_empty(), "64-bit map must not be emitted");
        assert!(
            p.warnings
                .iter()
                .any(|w| w.contains("no OpenTune equivalent")),
            "warnings: {:?}",
            p.warnings
        );
        assert_eq!(p.rom.bytes.len(), 0x400);
    }
}
