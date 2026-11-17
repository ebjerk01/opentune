//! The container layer: header, project metadata, the version directory, and
//! slicing one saved ROM version out of the file.
//!
//! See the crate docs for provenance. The byte layout here (24-byte header,
//! `CArchive` metadata fields, the `M1` version-directory anchor) is format
//! knowledge reimplemented from the structure described by the GPL-3.0 ZedSuite
//! and romHEX14 readers; it is not a line-for-line translation.

use crate::reader::Cursor;

/// First 4 bytes of every `.ols` file: an MFC schema/version word.
const HEADER_MAGIC: u32 = 0x0000_000B;
/// The ASCII signature that follows the magic.
const SIGNATURE: &[u8] = b"WinOLS File";
/// Bytes from file start to the end of the fixed header (magic + signature +
/// NUL + a 4-byte format-version word + a 4-byte field).
const HEADER_LEN: usize = 24;

/// A marker WinOLS writes immediately after the 12-byte version-directory
/// header. We locate the directory by finding this anchor near the file start.
const VERSION_DIR_ANCHOR: u32 = 0x4200_7899;
/// The anchor is only trusted within this many bytes of the file start; a
/// chance 4-byte match deep inside the ROM image does not count.
const ANCHOR_SCAN_LIMIT: usize = 4096;

/// Container format version and where its metadata block begins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// WinOLS container format version (e.g. 288 ~ WinOLS 4, 804 ~ WinOLS 5.84).
    pub format_version: u32,
}

impl Header {
    /// Byte offset of the first metadata field. Modern (>= 200) files carry the
    /// full 24-byte header; older ones start the metadata 4 bytes earlier.
    pub fn metadata_offset(self) -> usize {
        if self.format_version >= 200 {
            0x18
        } else {
            0x14
        }
    }
}

/// Validate the fixed header and read the format version. `None` when the bytes
/// are not a (plaintext) WinOLS container.
pub fn parse_header(data: &[u8]) -> Option<Header> {
    if data.len() < HEADER_LEN {
        return None;
    }
    let magic = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    if magic != HEADER_MAGIC {
        return None;
    }
    if &data[4..15] != SIGNATURE || data[15] != 0x00 {
        return None;
    }
    let format_version = u32::from_le_bytes([data[16], data[17], data[18], data[19]]);
    Some(Header { format_version })
}

/// Project-level identification strings. Every field is plaintext in a normal
/// WinOLS project; a field left empty on disk (or carrying a blank sentinel)
/// comes back as an empty / placeholder string, never fabricated.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Metadata {
    /// Vehicle make, e.g. `"Seat"`.
    pub make: String,
    /// Vehicle model, e.g. `"Leon"`.
    pub model: String,
    /// ECU manufacturer, e.g. `"Bosch"`.
    pub manufacturer: String,
    /// ECU name, e.g. `"EDC15P"` / `"MED17.5.20"`.
    pub ecu_name: String,
    /// Hardware part number, if recorded.
    pub hw_number: String,
    /// Software part number, if recorded.
    pub sw_number: String,
    /// Free-text revision tag. On many files it encodes one label per saved
    /// version inside parentheses; [`crate::versions`] splits that when it can.
    pub revision_tag: String,
}

/// Walk the metadata block far enough to read the identification fields, in
/// their real on-disk order, stopping at the first unreadable field rather than
/// guessing past it (the remaining fields keep their defaults).
pub fn parse_metadata(data: &[u8], header: Header) -> Metadata {
    let fv = header.format_version;
    let mut c = Cursor::new(data, header.metadata_offset());
    let mut m = Metadata::default();

    // Read one string into `$slot`; bail out of the whole walk on short input.
    macro_rules! take {
        ($slot:expr) => {
            match c.carchive_string(fv) {
                Some(v) => $slot = v,
                None => return m,
            }
        };
    }
    // Read and discard one string field.
    macro_rules! drop_str {
        () => {
            if c.carchive_string(fv).is_none() {
                return m;
            }
        };
    }

    take!(m.make);
    take!(m.model);
    drop_str!(); // vehicle type
    drop_str!(); // year
    drop_str!(); // fuel type
    drop_str!(); // engine capacity
    drop_str!(); // power output
    drop_str!(); // drivetrain
    drop_str!(); // memory type
    take!(m.manufacturer);
    take!(m.ecu_name);
    take!(m.hw_number);
    take!(m.sw_number);
    drop_str!(); // production number
    drop_str!(); // engine code
    drop_str!(); // transmission
    if c.skip(8).is_none() {
        return m; // last-write-time (8-byte FILETIME-like field)
    }
    drop_str!(); // original file name
    drop_str!(); // OLS version string
    drop_str!(); // reserved
    take!(m.revision_tag);

    m
}

/// The version directory: where the saved ROM copies live and how large each
/// one is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionDirectory {
    /// Number of saved ROM versions in the container (1 for a plain project).
    pub count: u32,
    /// File offset where the version data region starts.
    pub data_start: u32,
    /// Byte size of each saved version (they are all the same ROM size).
    pub record_size: u32,
}

impl VersionDirectory {
    /// File offset of saved version `index`'s first ROM byte. The region opens
    /// with a 4-byte field, then the fixed-size records back to back.
    fn version_offset(self, index: u32) -> usize {
        self.data_start as usize + 4 + self.record_size as usize * index as usize
    }
}

/// Locate the version directory by scanning for its anchor near the file start
/// and reading the 12-byte header that sits immediately before the anchor:
/// `[count-1: u32][data_start: u32][record_size: u32]`.
///
/// The computed layout is cross-checked against the real file size; if it does
/// not fit exactly, the match is rejected (better no directory than one that
/// slices the ROM at the wrong offsets).
pub fn find_version_directory(data: &[u8]) -> Option<VersionDirectory> {
    let scan_end = data.len().min(ANCHOR_SCAN_LIMIT);
    let needle = VERSION_DIR_ANCHOR.to_le_bytes();
    let anchor = data.get(..scan_end)?.windows(4).position(|w| w == needle)?;
    let hdr = anchor.checked_sub(12)?;
    let fields = data.get(hdr..hdr + 12)?;

    let count_minus_1 = u32::from_le_bytes([fields[0], fields[1], fields[2], fields[3]]);
    let data_start = u32::from_le_bytes([fields[4], fields[5], fields[6], fields[7]]);
    let record_size = u32::from_le_bytes([fields[8], fields[9], fields[10], fields[11]]);

    let total = data.len() as u64;
    if record_size == 0 || u64::from(data_start) >= total {
        return None;
    }
    let count = count_minus_1.wrapping_add(1);
    let expected_end = u64::from(data_start) + 4 + u64::from(record_size) * u64::from(count);
    if expected_end != total {
        return None;
    }

    Some(VersionDirectory {
        count,
        data_start,
        record_size,
    })
}

/// Copy one saved version's raw ROM bytes out of the container.
pub fn extract_version(data: &[u8], dir: VersionDirectory, index: u32) -> crate::Result<Vec<u8>> {
    if index >= dir.count {
        return Err(crate::Error::VersionOutOfRange {
            requested: index,
            available: dir.count,
        });
    }
    let start = dir.version_offset(index);
    let end = start
        .checked_add(dir.record_size as usize)
        .ok_or(crate::Error::Truncated)?;
    let slice = data.get(start..end).ok_or(crate::Error::Truncated)?;
    Ok(slice.to_vec())
}
