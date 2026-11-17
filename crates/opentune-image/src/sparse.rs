//! The addressed sparse-image model: ordered, non-overlapping [`Segment`]s with
//! explicit holes between them.
//!
//! A flash image is rarely one contiguous blob — a container exposes separate
//! code/calibration/boot segments, a bench read covers only the pages it was
//! asked for, and a HEX/SREC tune file describes just the bytes it changes. This
//! model keeps those gaps *explicit* rather than papering over them with a flat
//! buffer full of implicit `0xFF`. Flattening to a contiguous buffer is a
//! deliberate, separate step ([`SparseImage::flatten`]) that names the fill byte
//! and reports exactly which ranges it invented.

use crate::error::{ImageError, Result};

/// One contiguous run of bytes placed at an absolute address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// Absolute start address of the first byte.
    pub address: u64,
    /// The bytes, in address order. Never empty in a [`SparseImage`].
    pub bytes: Vec<u8>,
}

impl Segment {
    /// One past the last address covered by this segment.
    #[must_use]
    pub fn end(&self) -> u64 {
        self.address + self.bytes.len() as u64
    }

    /// Number of bytes in the segment.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether the segment carries no bytes (never true inside a [`SparseImage`]).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

/// How [`SparseImage::insert`] resolves bytes that collide with existing data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlap {
    /// Any byte-level overlap aborts the insert with [`ImageError::Overlap`].
    /// Merely touching an existing segment (no shared byte) is allowed and the
    /// segments coalesce.
    Error,
    /// New bytes win: they replace existing bytes in the overlapping range.
    Overwrite,
    /// Existing bytes win: new bytes only fill the parts that were holes, and
    /// the collision is reported (never silently dropped) in [`InsertReport`].
    Reject,
}

/// A half-open address range `[start, end)`.
pub type Range = (u64, u64);

/// What an [`SparseImage::insert`] actually did, so no resolution is hidden.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InsertReport {
    /// Ranges where the new bytes overlapped existing data. Under
    /// [`Overlap::Overwrite`] these were overwritten with the new bytes; under
    /// [`Overlap::Reject`] the existing bytes were kept.
    pub conflicts: Vec<Range>,
}

impl InsertReport {
    /// Whether the insert collided with any existing data.
    #[must_use]
    pub fn had_conflicts(&self) -> bool {
        !self.conflicts.is_empty()
    }
}

/// An ordered set of non-overlapping [`Segment`]s.
///
/// Invariants, upheld by every method:
/// * segments are sorted by `address`,
/// * no two segments overlap,
/// * no two segments are *contiguous* — touching runs are always coalesced, so
///   a gap between consecutive segments is a real hole.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SparseImage {
    segments: Vec<Segment>,
}

impl SparseImage {
    /// An image with no segments.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert `bytes` starting at `address`, resolving collisions per `policy`.
    ///
    /// Returns an [`InsertReport`] describing any overlap that occurred. Under
    /// [`Overlap::Error`] a byte-level overlap instead returns
    /// [`ImageError::Overlap`] and leaves the image unchanged. Inserting an
    /// empty slice is a no-op.
    ///
    /// # Errors
    /// [`ImageError::Overlap`] when `policy` is [`Overlap::Error`] and the new
    /// bytes share an address with existing data.
    pub fn insert(&mut self, address: u64, bytes: &[u8], policy: Overlap) -> Result<InsertReport> {
        if bytes.is_empty() {
            return Ok(InsertReport::default());
        }
        let new_lo = address;
        let new_hi = address + bytes.len() as u64;

        // Find the run of existing segments that overlap OR touch the new span;
        // these are the ones that merge with it. `touch` means share a boundary
        // with no gap (seg.end == new_lo or seg.address == new_hi).
        let mut first = self.segments.len();
        let mut last = 0usize; // exclusive
        let mut conflicts: Vec<Range> = Vec::new();
        for (i, seg) in self.segments.iter().enumerate() {
            let touches = seg.end() >= new_lo && seg.address <= new_hi;
            if !touches {
                continue;
            }
            if first == self.segments.len() {
                first = i;
            }
            last = i + 1;
            // Real byte overlap (not mere adjacency).
            let ov_lo = seg.address.max(new_lo);
            let ov_hi = seg.end().min(new_hi);
            if ov_lo < ov_hi {
                conflicts.push((ov_lo, ov_hi));
            }
        }

        // Normalise to an empty, valid range when nothing was touched.
        if first > last {
            first = 0;
            last = 0;
        }

        if policy == Overlap::Error && !conflicts.is_empty() {
            let start = conflicts.iter().map(|c| c.0).min().unwrap();
            let end = conflicts.iter().map(|c| c.1).max().unwrap();
            return Err(ImageError::Overlap { start, end });
        }

        // Build the coalesced segment spanning the new data and every touched
        // existing segment.
        let merged_lo = if first < last {
            self.segments[first].address.min(new_lo)
        } else {
            new_lo
        };
        let merged_hi = if first < last {
            self.segments[first..last]
                .iter()
                .map(Segment::end)
                .max()
                .unwrap()
                .max(new_hi)
        } else {
            new_hi
        };

        let mut buf = vec![0u8; (merged_hi - merged_lo) as usize];
        match policy {
            Overlap::Overwrite => {
                // Existing first, then new bytes win on top.
                for seg in &self.segments[first..last] {
                    let off = (seg.address - merged_lo) as usize;
                    buf[off..off + seg.bytes.len()].copy_from_slice(&seg.bytes);
                }
                let off = (new_lo - merged_lo) as usize;
                buf[off..off + bytes.len()].copy_from_slice(bytes);
            }
            Overlap::Reject | Overlap::Error => {
                // New bytes first (fills holes), then existing wins on top. With
                // the `Error` policy there are no conflicts here by construction.
                let off = (new_lo - merged_lo) as usize;
                buf[off..off + bytes.len()].copy_from_slice(bytes);
                for seg in &self.segments[first..last] {
                    let off = (seg.address - merged_lo) as usize;
                    buf[off..off + seg.bytes.len()].copy_from_slice(&seg.bytes);
                }
            }
        }

        let merged = Segment {
            address: merged_lo,
            bytes: buf,
        };
        if first < last {
            self.segments.splice(first..last, core::iter::once(merged));
        } else {
            // No touched segments: insert at the sorted position.
            let pos = self.segments.partition_point(|s| s.address < merged_lo);
            self.segments.insert(pos, merged);
        }

        conflicts.sort_unstable();
        Ok(InsertReport { conflicts })
    }

    /// The byte stored at `address`, or `None` if it falls in a hole.
    #[must_use]
    pub fn byte(&self, address: u64) -> Option<u8> {
        let idx = self.segment_containing(address)?;
        let seg = &self.segments[idx];
        Some(seg.bytes[(address - seg.address) as usize])
    }

    /// The `len` bytes starting at `address`, or `None` if any of them fall in a
    /// hole. Because contiguous runs are always coalesced, a present range lives
    /// entirely inside one segment.
    #[must_use]
    pub fn range(&self, address: u64, len: usize) -> Option<Vec<u8>> {
        if len == 0 {
            return Some(Vec::new());
        }
        let end = address + len as u64;
        let idx = self.segment_containing(address)?;
        let seg = &self.segments[idx];
        if end > seg.end() {
            return None;
        }
        let off = (address - seg.address) as usize;
        Some(seg.bytes[off..off + len].to_vec())
    }

    /// Iterate the segments in address order.
    pub fn segments(&self) -> impl Iterator<Item = &Segment> {
        self.segments.iter()
    }

    /// Number of segments.
    #[must_use]
    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    /// Whether the image holds no data at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    /// The holes between segments, as half-open `[start, end)` ranges, in
    /// address order. Space before the first segment and after the last is not a
    /// hole (the image simply does not extend there).
    #[must_use]
    pub fn gaps(&self) -> Vec<Range> {
        self.segments
            .windows(2)
            .map(|w| (w[0].end(), w[1].address))
            .collect()
    }

    /// The overall `[start, end)` the image covers, or `None` when empty. This
    /// spans holes: `end - start` can exceed the number of stored bytes.
    #[must_use]
    pub fn span(&self) -> Option<Range> {
        let first = self.segments.first()?;
        let last = self.segments.last()?;
        Some((first.address, last.end()))
    }

    /// Total number of bytes actually stored (excluding holes).
    #[must_use]
    pub fn stored_len(&self) -> usize {
        self.segments.iter().map(Segment::len).sum()
    }

    /// Materialise one contiguous buffer covering the whole [`span`], filling
    /// every hole with `fill`.
    ///
    /// The returned [`Flattened`] reports the base address, the bytes, and the
    /// exact ranges that were invented, so a caller can never mistake a fill
    /// byte for real data.
    ///
    /// [`span`]: SparseImage::span
    ///
    /// # Errors
    /// [`ImageError::Empty`] if the image has no segments.
    pub fn flatten(&self, fill: u8) -> Result<Flattened> {
        let (base, end) = self.span().ok_or(ImageError::Empty)?;
        let mut bytes = vec![fill; (end - base) as usize];
        for seg in &self.segments {
            let off = (seg.address - base) as usize;
            bytes[off..off + seg.bytes.len()].copy_from_slice(&seg.bytes);
        }
        let fills = self.gaps();
        Ok(Flattened { base, bytes, fills })
    }

    /// Binary-search for the index of the segment that contains `address`.
    fn segment_containing(&self, address: u64) -> Option<usize> {
        // `partition_point` gives the first segment starting after `address`;
        // the candidate is therefore the one just before it.
        let pp = self.segments.partition_point(|s| s.address <= address);
        if pp == 0 {
            return None;
        }
        let idx = pp - 1;
        if address < self.segments[idx].end() {
            Some(idx)
        } else {
            None
        }
    }
}

/// The result of [`SparseImage::flatten`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flattened {
    /// Address of `bytes[0]`.
    pub base: u64,
    /// The contiguous buffer, holes filled.
    pub bytes: Vec<u8>,
    /// The `[start, end)` ranges that were filled rather than read from data.
    pub fills: Vec<Range>,
}
