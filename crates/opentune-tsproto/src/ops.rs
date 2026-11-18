//! Whole-tune operations built on [`Client`]: plan and perform a verified
//! write.
use crate::client::Client;
use crate::crc::crc32;
use crate::port::Port;
use crate::{ProtoError, Result, WriteAuth};
use opentune_tsini::Tune;

/// Bytes that differ in one page, as `(start, end)` ranges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageChange {
    pub page: usize,
    pub ranges: Vec<(usize, usize)>,
    /// Bytes that actually differ (ranges may include a few equal bytes).
    pub changed_bytes: usize,
}

/// What a write would change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WritePlan {
    pub pages: Vec<PageChange>,
}

impl WritePlan {
    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    pub fn changed_bytes(&self) -> usize {
        self.pages.iter().map(|p| p.changed_bytes).sum()
    }
}

/// Ranges closer than this are merged into one request (the bytes between
/// are rewritten with their current, unchanged value).
const MERGE_GAP: usize = 8;

/// Compare `current` (what the ECU holds) with `target`.
pub fn plan_write(current: &Tune, target: &Tune) -> WritePlan {
    let mut plan = WritePlan::default();
    for (page, (a, b)) in current.pages.iter().zip(&target.pages).enumerate() {
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        let mut changed = 0;
        for i in 0..a.len().min(b.len()) {
            if a[i] == b[i] {
                continue;
            }
            changed += 1;
            match ranges.last_mut() {
                Some(r) if i <= r.1 + MERGE_GAP => r.1 = i + 1,
                _ => ranges.push((i, i + 1)),
            }
        }
        if !ranges.is_empty() {
            plan.pages.push(PageChange {
                page,
                ranges,
                changed_bytes: changed,
            });
        }
    }
    plan
}

/// Options for [`write_tune`].
#[derive(Debug, Clone, Copy)]
pub struct WriteOptions {
    /// Burn verified pages to permanent storage (otherwise they stay in the
    /// ECU's RAM and are lost at power-off).
    pub burn: bool,
    /// After burning, compare the ECU's page CRC32 with the target's when
    /// the INI has a `crc32CheckCommand`.
    pub check_crc: bool,
}

impl Default for WriteOptions {
    fn default() -> Self {
        Self {
            burn: true,
            check_crc: true,
        }
    }
}

/// Phase reported to the progress callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteStage {
    Write,
    Verify,
    Burn,
    CrcCheck,
}

/// What [`write_tune`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WriteReport {
    /// Pages written and verified (0-based).
    pub pages: Vec<usize>,
    pub bytes_written: usize,
    pub burned: Vec<usize>,
    /// Verified pages the INI gives no burn command for (RAM only).
    pub not_burnable: Vec<usize>,
    pub crc_checked: Vec<usize>,
}

/// Write `target` to an ECU currently holding `current` (read it first; it
/// is the backup the caller must save before calling this).
///
/// 1. write only the changed ranges of each page to working memory;
/// 2. read every written page back and compare it with `target` — any
///    difference stops here with [`ProtoError::Verify`] and nothing is burned;
/// 3. if `opts.burn`, burn each written page;
/// 4. if `opts.check_crc`, compare the ECU's page CRC32 with the target's.
pub fn write_tune<P: Port>(
    client: &mut Client<'_, P>,
    auth: &WriteAuth,
    current: &Tune,
    target: &Tune,
    opts: WriteOptions,
    progress: &mut dyn FnMut(WriteStage, usize, usize, usize),
) -> Result<WriteReport> {
    let ini = client.ini();
    if target.pages.len() != ini.pages.len() || current.pages.len() != ini.pages.len() {
        return Err(ProtoError::Definition(
            "tune does not match the INI's pages".into(),
        ));
    }
    let plan = plan_write(current, target);
    let mut rep = WriteReport::default();
    for pc in &plan.pages {
        let total: usize = pc.ranges.iter().map(|(a, b)| b - a).sum();
        let mut done = 0;
        for &(a, b) in &pc.ranges {
            client.write_chunk(auth, pc.page, a, &target.pages[pc.page][a..b])?;
            done += b - a;
            rep.bytes_written += b - a;
            progress(WriteStage::Write, pc.page, done, total);
        }
    }
    for pc in &plan.pages {
        let back = client.read_page(pc.page, &mut |d, t| {
            progress(WriteStage::Verify, pc.page, d, t)
        })?;
        let want = &target.pages[pc.page];
        if let Some(i) = (0..want.len()).find(|&i| back.get(i) != Some(&want[i])) {
            return Err(ProtoError::Verify {
                page: pc.page + 1,
                offset: i,
                wrote: want[i],
                read: back.get(i).copied().unwrap_or(0),
            });
        }
        rep.pages.push(pc.page);
    }
    if opts.burn {
        for &p in &rep.pages {
            progress(WriteStage::Burn, p, 0, 1);
            if client.burn(auth, p)? {
                rep.burned.push(p);
            } else {
                rep.not_burnable.push(p);
            }
            progress(WriteStage::Burn, p, 1, 1);
        }
    }
    if opts.check_crc {
        for &p in &rep.pages {
            progress(WriteStage::CrcCheck, p, 0, 1);
            if let Some(ecu) = client.page_crc32(p)? {
                let local = crc32(&target.pages[p]);
                if ecu != local {
                    return Err(ProtoError::CrcCheck {
                        page: p + 1,
                        ecu,
                        local,
                    });
                }
                rep.crc_checked.push(p);
            }
        }
    }
    Ok(rep)
}
