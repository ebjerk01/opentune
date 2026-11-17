//! A bounds-checked little-endian cursor over the container bytes.
//!
//! WinOLS writes its project blocks with Microsoft MFC `CArchive` conventions:
//! everything is little-endian, and strings are length-prefixed. Two string
//! flavours appear and are kept as separate methods rather than one "smart"
//! reader, because confusing them is the easiest way to desynchronise a parse:
//!
//! * [`Cursor::carchive_string`] — the metadata-block flavour. A 4-byte signed
//!   length; for container format >= 439 the length can be a small negative
//!   sentinel standing in for a blank field, and a positive length is followed
//!   on disk by one trailing NUL byte (older `CArchive` string writer).
//! * [`Cursor::winols5_string`] — the map-record flavour (container format
//!   >= 800). Same sentinels, but no trailing NUL.
//!
//! Every read returns `None` on short input instead of panicking, so a
//! misaligned parse unwinds cleanly to "not a record / stop here" rather than
//! reading out of bounds.

/// Largest string length this reader will honour. A real Notes/name field is
/// far smaller; a larger declared length means the 4 bytes were misread from
/// ROM content, so the read is refused rather than allocating megabytes.
pub const MAX_STRING_LEN: usize = 8192;

/// A forward-only cursor over a byte slice. All multi-byte reads are
/// little-endian.
pub struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    /// New cursor positioned at `pos` within `data`.
    pub fn new(data: &'a [u8], pos: usize) -> Self {
        Self { data, pos }
    }

    /// Current byte offset.
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// Borrow and advance over `n` bytes, or `None` if they run past the end.
    pub fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let slice = self.data.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }

    /// Advance over `n` bytes without returning them.
    pub fn skip(&mut self, n: usize) -> Option<()> {
        self.bytes(n).map(|_| ())
    }

    /// One byte.
    pub fn u8(&mut self) -> Option<u8> {
        self.bytes(1).map(|b| b[0])
    }

    /// Little-endian `u16`.
    pub fn u16(&mut self) -> Option<u16> {
        let b = self.bytes(2)?;
        Some(u16::from_le_bytes([b[0], b[1]]))
    }

    /// Little-endian `u32`.
    pub fn u32(&mut self) -> Option<u32> {
        let b = self.bytes(4)?;
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Little-endian `i32` (used for the signed string length prefix).
    pub fn i32(&mut self) -> Option<i32> {
        self.u32().map(|v| v as i32)
    }

    /// Little-endian `f64`, rejecting non-finite values (a misread block of ROM
    /// rarely decodes to a finite double, so this is a useful sanity gate).
    pub fn f64(&mut self) -> Option<f64> {
        let b = self.bytes(8)?;
        let v = f64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
        v.is_finite().then_some(v)
    }

    /// Read the shared length prefix and resolve the small negative sentinels
    /// WinOLS uses for blank fields. Returns:
    /// * `Some(Ok(n))` — a real string body of `n` bytes follows,
    /// * `Some(Err(s))` — a sentinel: the field's whole value is `s`,
    /// * `None` — short input or an implausibly large length.
    fn string_len(&mut self) -> Option<std::result::Result<usize, &'static str>> {
        let len = self.i32()?;
        match len {
            -1 => Some(Err("-")),
            -2 => Some(Err("?")),
            -3 => Some(Err("%")),
            n if n <= 0 => Some(Err("")),
            n => {
                let n = n as usize;
                (n <= MAX_STRING_LEN).then_some(Ok(n))
            }
        }
    }

    /// A metadata-block string (see the module docs): sentinels, then for
    /// container `format_version >= 439` a bare body, otherwise a body followed
    /// by one trailing NUL byte that is consumed here.
    pub fn carchive_string(&mut self, format_version: u32) -> Option<String> {
        let n = match self.string_len()? {
            Err(sentinel) => return Some(sentinel.to_string()),
            Ok(n) => n,
        };
        let body = self.bytes(n)?.to_vec();
        if format_version < 439 {
            // The pre-439 CArchive writer appends a terminating NUL after the
            // counted body; it must be present or we are out of sync.
            if self.u8()? != 0 {
                return None;
            }
        }
        Some(decode_text(&body))
    }

    /// A map-record string (WinOLS 5 flavour): sentinels then a bare body, no
    /// trailing NUL. Returns `None` if the body is not plausible text, so a
    /// misaligned record is rejected at the first string field.
    pub fn winols5_string(&mut self) -> Option<String> {
        let n = match self.string_len()? {
            Err(sentinel) => return Some(sentinel.to_string()),
            Ok(n) => n,
        };
        let body = self.bytes(n)?;
        decode_printable(body)
    }
}

/// Decode a metadata text body. WinOLS writes ASCII almost always, sometimes
/// UTF-8, and — on old files, for accented characters — Windows-1252. This is a
/// best-effort, legible decoding: ASCII fast path, then UTF-8, then a lossy
/// byte-as-codepoint fallback (which happens to equal Latin-1 for the low 256).
pub fn decode_text(raw: &[u8]) -> String {
    if raw.is_ascii() {
        return String::from_utf8_lossy(raw).into_owned();
    }
    match std::str::from_utf8(raw) {
        Ok(s) => s.to_string(),
        Err(_) => raw.iter().map(|&b| b as char).collect(),
    }
}

/// Like [`decode_text`], but rejects a body containing ASCII control characters
/// (other than tab/CR/LF). A real map name/unit never contains them; their
/// presence means the length prefix landed on binary ROM data, so the caller
/// should treat the record as a misalignment.
fn decode_printable(raw: &[u8]) -> Option<String> {
    let text = decode_text(raw);
    let has_control = text
        .chars()
        .any(|c| (c as u32) < 0x20 && !matches!(c, '\t' | '\r' | '\n'));
    (!has_control).then_some(text)
}
