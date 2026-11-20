//! Generic UDS block download: `requestDownload` (0x34) → `transferData`
//! (0x36) per block → `requestTransferExit` (0x37), over an adapter job's
//! [`RawDiag`] through the core `UdsClient`.
//!
//! It is only the transfer core: the caller drives the session (`10 02`),
//! security access ([`crate::Unlocker`]) and any routines (erase, check)
//! around it. It runs only inside `adapter.mutate(ack, &backup, …)` — the
//! host's confirm + backup-first + confirmed-module path — and refuses
//! anywhere else before sending anything.
//!
//! * Block size: the ECU's `maxNumberOfBlockLength` from the 0x74 reply
//!   counts the whole `36 <counter> <data…>` message, so each block carries
//!   up to that minus 2 data bytes (optionally capped by the adapter).
//! * Counter: starts at 0x01, wraps 0xFF → 0x00; every 0x76 echo is checked
//!   by the client. Nothing is ever re-sent.
//! * Cancel: honoured before the download starts, and between blocks; never
//!   in the middle of one. Cancelling between blocks (or any failure after
//!   the first block) leaves the ECU partially written, and the error says so.

use crate::host::ProgressSink;
use crate::host::RawTransport;
use opentune_core::protocol::uds::next_block_seq;
use opentune_core::protocol::UdsClient;
use opentune_core::RawDiag;

/// Progress and cancellation for a transfer ([`ProgressSink`] implements it).
pub trait TransferProgress {
    fn report(&self, frac: f32, msg: String);
    fn cancelled(&self) -> bool;
}

impl TransferProgress for ProgressSink {
    fn report(&self, frac: f32, msg: String) {
        self.set(frac, msg);
    }
    fn cancelled(&self) -> bool {
        ProgressSink::cancelled(self)
    }
}

/// No progress display, never cancelled.
pub struct NoProgress;

impl TransferProgress for NoProgress {
    fn report(&self, _frac: f32, _msg: String) {}
    fn cancelled(&self) -> bool {
        false
    }
}

/// What to download.
#[derive(Clone, Debug)]
pub struct Download<'a> {
    /// Target memory address (as the ECU's programming spec names it).
    pub address: u32,
    pub data: &'a [u8],
    /// `dataFormatIdentifier` (0x00 = neither compressed nor encrypted).
    pub data_format: u8,
    /// `addressAndLengthFormatIdentifier` (0x44 = 4-byte address + size).
    pub alfid: u8,
    /// Extra cap on data bytes per block (an adapter's frame limit), on top
    /// of the ECU's own maximum.
    pub max_payload: Option<usize>,
}

impl<'a> Download<'a> {
    /// Uncompressed, unencrypted data at `address`, 4-byte address + size.
    pub fn new(address: u32, data: &'a [u8]) -> Self {
        Self {
            address,
            data,
            data_format: 0x00,
            alfid: 0x44,
            max_payload: None,
        }
    }
}

/// A completed download.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferReport {
    pub blocks: usize,
    pub bytes: usize,
    /// The ECU's `maxNumberOfBlockLength` (whole 0x36 message).
    pub max_block_len: usize,
    /// Data bytes per block actually used.
    pub payload_per_block: usize,
    /// `transferResponseParameterRecord` of the 0x77 reply (often empty).
    pub exit_record: Vec<u8>,
}

/// Why a download did not complete, by what it left in the ECU.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransferError {
    /// Nothing was written (refused before or at `requestDownload`, or
    /// cancelled before the first block).
    NotStarted { reason: String },
    /// A block was sent, then the transfer stopped (an error, or a cancel
    /// between blocks). `blocks_done` counts the blocks the ECU accepted; a
    /// refused block may still have been partly programmed.
    PartiallyWritten {
        blocks_done: usize,
        bytes_done: usize,
        total: usize,
        cancelled: bool,
        reason: String,
    },
    /// Every block was accepted but `requestTransferExit` failed.
    ExitFailed { bytes: usize, reason: String },
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotStarted { reason } => {
                write!(f, "download not started (nothing written): {reason}")
            }
            Self::PartiallyWritten {
                blocks_done,
                bytes_done,
                total,
                cancelled,
                reason,
            } => write!(
                f,
                "download {} after {blocks_done} block(s), {bytes_done} of {total} bytes: \
                 {reason}. WARNING: the ECU is left PARTIALLY WRITTEN; repeat the complete \
                 programming sequence before relying on it",
                if *cancelled { "cancelled" } else { "stopped" }
            ),
            Self::ExitFailed { bytes, reason } => write!(
                f,
                "all {bytes} bytes were transferred but requestTransferExit failed: {reason}. \
                 WARNING: the ECU may not have accepted the download"
            ),
        }
    }
}

impl std::error::Error for TransferError {}

impl From<TransferError> for String {
    fn from(e: TransferError) -> String {
        e.to_string()
    }
}

/// Run one download (see the module docs). Call it inside an
/// `adapter.mutate(…)` closure; anywhere else it refuses, sending nothing.
pub fn download(
    d: &mut dyn RawDiag,
    job: &Download,
    progress: &dyn TransferProgress,
) -> Result<TransferReport, TransferError> {
    let not_started = |reason: String| TransferError::NotStarted { reason };
    if !crate::host::in_mutation() {
        return Err(not_started(
            "a download must run inside adapter.mutate (confirm + backup first)".into(),
        ));
    }
    if job.data.is_empty() {
        return Err(not_started("no data to download".into()));
    }
    let total = job.data.len();
    let size = u32::try_from(total).map_err(|_| not_started("data larger than 4 GiB".into()))?;
    if progress.cancelled() {
        return Err(not_started("cancelled before the download started".into()));
    }

    let mut t = RawTransport::new(d);
    let mut uds = UdsClient::new(&mut t);
    progress.report(0.0, format!("requesting download of {total} bytes"));
    let max_block_len = uds
        .request_download_with_format(job.data_format, job.alfid, job.address, size)
        .map_err(|e| not_started(format!("requestDownload: {e}")))?;
    // The ECU's maximum counts `36 <counter>` too.
    let mut payload = max_block_len.saturating_sub(2);
    if let Some(cap) = job.max_payload {
        payload = payload.min(cap);
    }
    if payload == 0 {
        return Err(not_started(format!(
            "no room for data in a block (ECU maxNumberOfBlockLength {max_block_len}, cap {:?})",
            job.max_payload
        )));
    }

    let blocks = total.div_ceil(payload);
    let mut seq = 0x01u8;
    let mut done = 0usize;
    for (i, chunk) in job.data.chunks(payload).enumerate() {
        // Between blocks only: a block already on the wire is never cut off.
        if i > 0 && progress.cancelled() {
            return Err(TransferError::PartiallyWritten {
                blocks_done: i,
                bytes_done: done,
                total,
                cancelled: true,
                reason: "cancelled by the user".into(),
            });
        }
        if let Err(e) = uds.transfer_data(seq, chunk) {
            // Even a refused first block may have been partly programmed
            // (e.g. NRC 0x72 generalProgrammingFailure): never claim
            // "nothing written" once a block was on the wire.
            return Err(TransferError::PartiallyWritten {
                blocks_done: i,
                bytes_done: done,
                total,
                cancelled: false,
                reason: format!("transferData block {} (counter 0x{seq:02X}): {e}", i + 1),
            });
        }
        done += chunk.len();
        seq = next_block_seq(seq);
        progress.report(
            done as f32 / total as f32,
            format!("block {}/{blocks} · {done}/{total} bytes", i + 1),
        );
    }

    progress.report(1.0, "transfer exit".into());
    let exit = uds.transfer_exit().map_err(|e| TransferError::ExitFailed {
        bytes: total,
        reason: e.to_string(),
    })?;
    Ok(TransferReport {
        blocks,
        bytes: total,
        max_block_len,
        payload_per_block: payload,
        exit_record: exit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    /// A scripted ECU: 0x34 → maxNumberOfBlockLength `max`, 0x36 echoes the
    /// counter (or the scripted NRC at block `fail_at`), 0x37 → `77`.
    struct Ecu {
        max: u16,
        fail_at: Option<usize>,
        /// The NRC a refused block gets (default 0x72).
        fail_nrc: u8,
        /// Block (0-based) whose 0x76 echoes the wrong counter.
        bad_echo_at: Option<usize>,
        /// Block (0-based) answered with a bare responsePending.
        pending_at: Option<usize>,
        /// Replaces the whole 0x34 reply.
        dl_reply: Option<Vec<u8>>,
        exit_nrc: Option<u8>,
        sent: Vec<Vec<u8>>,
    }
    impl Ecu {
        fn new(max: u16) -> Self {
            Self {
                max,
                fail_at: None,
                fail_nrc: 0x72,
                bad_echo_at: None,
                pending_at: None,
                dl_reply: None,
                exit_nrc: None,
                sent: Vec::new(),
            }
        }
        fn blocks(&self) -> Vec<&Vec<u8>> {
            self.sent.iter().filter(|r| r[0] == 0x36).collect()
        }
    }
    impl RawDiag for Ecu {
        fn request(&mut self, req: &[u8]) -> Result<Vec<u8>, String> {
            self.sent.push(req.to_vec());
            Ok(match req[0] {
                0x34 => match &self.dl_reply {
                    Some(r) => r.clone(),
                    None => vec![0x74, 0x20, (self.max >> 8) as u8, self.max as u8],
                },
                0x36 => {
                    let n = self.blocks().len() - 1; // 0-based index of this block
                    if self.fail_at == Some(n) {
                        vec![0x7F, 0x36, self.fail_nrc]
                    } else if self.bad_echo_at == Some(n) {
                        vec![0x76, req[1].wrapping_add(1)]
                    } else if self.pending_at == Some(n) {
                        vec![0x7F, 0x36, 0x78]
                    } else {
                        vec![0x76, req[1]]
                    }
                }
                0x37 => match self.exit_nrc {
                    Some(nrc) => vec![0x7F, 0x37, nrc],
                    None => vec![0x77],
                },
                s => vec![0x7F, s, 0x11],
            })
        }
    }

    /// Cancels once `after` progress reports have been made.
    struct CancelAfter {
        after: usize,
        seen: Cell<usize>,
        log: RefCell<Vec<String>>,
    }
    impl CancelAfter {
        fn new(after: usize) -> Self {
            Self {
                after,
                seen: Cell::new(0),
                log: RefCell::new(Vec::new()),
            }
        }
    }
    impl TransferProgress for CancelAfter {
        fn report(&self, _f: f32, msg: String) {
            self.seen.set(self.seen.get() + 1);
            self.log.borrow_mut().push(msg);
        }
        fn cancelled(&self) -> bool {
            self.seen.get() >= self.after
        }
    }

    fn data(n: usize) -> Vec<u8> {
        (0..n).map(|i| i as u8).collect()
    }

    /// Run `f` as if inside adapter.mutate.
    fn in_mutation<R>(f: impl FnOnce() -> R) -> R {
        let _s = crate::host::MutationScope::enter();
        f()
    }

    #[test]
    fn downloads_in_ecu_sized_blocks_and_exits() {
        let mut ecu = Ecu::new(10); // 8 data bytes per block
        let d = data(20);
        let p = CancelAfter::new(usize::MAX);
        let r = in_mutation(|| download(&mut ecu, &Download::new(0x8000, &d), &p)).unwrap();
        assert_eq!(
            r,
            TransferReport {
                blocks: 3,
                bytes: 20,
                max_block_len: 10,
                payload_per_block: 8,
                exit_record: vec![],
            }
        );
        assert_eq!(
            ecu.sent[0],
            vec![0x34, 0x00, 0x44, 0, 0, 0x80, 0, 0, 0, 0, 20]
        );
        let b = ecu.blocks();
        assert_eq!((b[0][1], b[1][1], b[2][1]), (1, 2, 3));
        assert_eq!(b[2].len(), 2 + 4);
        let mut back = Vec::new();
        for blk in &b {
            back.extend_from_slice(&blk[2..]);
        }
        assert_eq!(back, d);
        assert_eq!(ecu.sent.last().unwrap(), &vec![0x37]);
        assert!(p
            .log
            .borrow()
            .iter()
            .any(|m| m == "block 3/3 · 20/20 bytes"));
    }

    #[test]
    fn counter_wraps_and_adapter_cap_applies() {
        let mut ecu = Ecu::new(4095);
        let d = data(300);
        let job = Download {
            max_payload: Some(1),
            ..Download::new(0, &d)
        };
        let r = in_mutation(|| download(&mut ecu, &job, &NoProgress)).unwrap();
        assert_eq!((r.blocks, r.payload_per_block), (300, 1));
        let b = ecu.blocks();
        assert_eq!(b[254][1], 0xFF);
        assert_eq!(b[255][1], 0x00, "256th block wraps to 0x00");
        assert_eq!(b[256][1], 0x01);
    }

    #[test]
    fn refused_outside_mutate_or_before_start() {
        let d = data(4);
        let mut ecu = Ecu::new(10);
        let e = download(&mut ecu, &Download::new(0, &d), &NoProgress).unwrap_err();
        assert!(matches!(e, TransferError::NotStarted { .. }), "{e}");
        assert!(e.to_string().contains("adapter.mutate"));
        // Cancelled before anything: nothing sent.
        let e = in_mutation(|| download(&mut ecu, &Download::new(0, &d), &CancelAfter::new(0)))
            .unwrap_err();
        assert!(matches!(e, TransferError::NotStarted { .. }));
        let e =
            in_mutation(|| download(&mut ecu, &Download::new(0, &[]), &NoProgress)).unwrap_err();
        assert!(e.to_string().contains("no data"));
        assert!(ecu.sent.is_empty());
        // An ECU block size with no room for data: no 0x36 sent.
        let mut tiny = Ecu::new(2);
        let e =
            in_mutation(|| download(&mut tiny, &Download::new(0, &d), &NoProgress)).unwrap_err();
        assert!(matches!(e, TransferError::NotStarted { .. }));
        assert!(tiny.blocks().is_empty());
    }

    #[test]
    fn cancel_only_between_blocks_and_warns() {
        let mut ecu = Ecu::new(10);
        let d = data(30); // 4 blocks
                          // Reports: "requesting" (1), block 1 (2) -> cancelled before block 2.
        let p = CancelAfter::new(2);
        let e = in_mutation(|| download(&mut ecu, &Download::new(0, &d), &p)).unwrap_err();
        assert_eq!(
            e,
            TransferError::PartiallyWritten {
                blocks_done: 1,
                bytes_done: 8,
                total: 30,
                cancelled: true,
                reason: "cancelled by the user".into(),
            }
        );
        assert!(e.to_string().contains("PARTIALLY WRITTEN"));
        assert_eq!(ecu.blocks().len(), 1, "the first block went whole");
        assert!(
            !ecu.sent.iter().any(|r| r[0] == 0x37),
            "no exit after a cancel"
        );
    }

    /// Through the real `read` / `mutate` wrappers: an adapter that runs the
    /// job on another thread over one scripted ECU.
    #[test]
    fn only_a_mutation_may_download() {
        use crate::host::{Ack, AdapterAccess, BackupReceipt, DiagJob, DiagTarget, LinkInfo};
        use std::any::Any;
        use std::sync::{Arc, Mutex};
        struct OneEcu(Mutex<Ecu>);
        impl AdapterAccess for OneEcu {
            fn link(&self) -> LinkInfo {
                LinkInfo::default()
            }
            fn read_job(
                &self,
                _op: &str,
                _t: DiagTarget,
                job: DiagJob,
            ) -> Result<Box<dyn Any + Send>, String> {
                let mut ecu = self.0.lock().unwrap();
                let ecu: &mut Ecu = &mut ecu;
                std::thread::scope(|s| s.spawn(move || job(ecu)).join().unwrap())
            }
            fn mutate_job(
                &self,
                op: &str,
                ack: Ack,
                _b: &BackupReceipt,
                job: DiagJob,
            ) -> Result<Box<dyn Any + Send>, String> {
                let t = ack.ecu_target()?;
                self.read_job(op, t, job)
            }
        }
        let ecu = Arc::new(OneEcu(Mutex::new(Ecu::new(10))));
        let a: Arc<dyn AdapterAccess> = ecu.clone();
        let d = data(12);
        let d2 = d.clone();
        let e = a
            .read("sneaky write", DiagTarget::ENGINE, move |raw| {
                download(raw, &Download::new(0, &d2), &NoProgress).map_err(String::from)
            })
            .unwrap_err();
        assert!(e.contains("adapter.mutate"), "{e}");
        assert!(
            ecu.0.lock().unwrap().sent.is_empty(),
            "nothing sent from a read"
        );

        let dir = std::env::temp_dir().join(format!("ot-transfer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let backup = crate::host::test_backup(&dir, b"OLD");
        let ack = crate::host::test_ack(DiagTarget::ENGINE);
        let r = a
            .mutate("download", ack, &backup, move |raw| {
                download(raw, &Download::new(0, &d), &NoProgress).map_err(String::from)
            })
            .unwrap();
        assert_eq!((r.blocks, r.bytes), (2, 12));
        assert!(
            !crate::host::in_mutation(),
            "the scope ends with the closure"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failures_say_what_was_left() {
        let d = data(30);
        // Block 1 refused: none accepted, but it was on the wire.
        let mut ecu = Ecu {
            fail_at: Some(0),
            ..Ecu::new(10)
        };
        let e = in_mutation(|| download(&mut ecu, &Download::new(0, &d), &NoProgress)).unwrap_err();
        assert!(
            matches!(e, TransferError::PartiallyWritten { blocks_done: 0, .. }),
            "{e}"
        );
        // Block 3 refused: partially written, the refused block not re-sent.
        let mut ecu = Ecu {
            fail_at: Some(2),
            ..Ecu::new(10)
        };
        let e = in_mutation(|| download(&mut ecu, &Download::new(0, &d), &NoProgress)).unwrap_err();
        assert!(
            matches!(
                e,
                TransferError::PartiallyWritten {
                    blocks_done: 2,
                    bytes_done: 16,
                    cancelled: false,
                    ..
                }
            ),
            "{e}"
        );
        assert_eq!(ecu.blocks().len(), 3);
        // requestDownload refused.
        struct No;
        impl RawDiag for No {
            fn request(&mut self, _r: &[u8]) -> Result<Vec<u8>, String> {
                Ok(vec![0x7F, 0x34, 0x70])
            }
        }
        let e = in_mutation(|| download(&mut No, &Download::new(0, &d), &NoProgress)).unwrap_err();
        assert!(e.to_string().contains("requestDownload"), "{e}");
        // Exit refused after all data.
        let mut ecu = Ecu {
            exit_nrc: Some(0x24),
            ..Ecu::new(10)
        };
        let e = in_mutation(|| download(&mut ecu, &Download::new(0, &d), &NoProgress)).unwrap_err();
        assert!(
            matches!(e, TransferError::ExitFailed { bytes: 30, .. }),
            "{e}"
        );
    }

    // -----------------------------------------------------------------------
    // Edge cases
    // -----------------------------------------------------------------------

    /// Records every progress report; never cancels.
    #[derive(Default)]
    struct Record(RefCell<Vec<(f32, String)>>);
    impl TransferProgress for Record {
        fn report(&self, frac: f32, msg: String) {
            self.0.borrow_mut().push((frac, msg));
        }
        fn cancelled(&self) -> bool {
            false
        }
    }

    /// The data the ECU received, block by block, in order.
    fn received(ecu: &Ecu) -> Vec<u8> {
        ecu.blocks()
            .iter()
            .flat_map(|b| b[2..].iter().copied())
            .collect()
    }

    fn counters(ecu: &Ecu) -> Vec<u8> {
        ecu.blocks().iter().map(|b| b[1]).collect()
    }

    #[test]
    fn data_an_exact_multiple_of_the_payload() {
        let mut ecu = Ecu::new(10);
        let d = data(24);
        let r =
            in_mutation(|| download(&mut ecu, &Download::new(0x1000, &d), &NoProgress)).unwrap();
        assert_eq!((r.blocks, r.bytes, r.payload_per_block), (3, 24, 8));
        assert!(ecu.blocks().iter().all(|b| b.len() == 10), "full blocks");
        assert_eq!(received(&ecu), d);
        assert_eq!(counters(&ecu), vec![1, 2, 3]);
        assert_eq!(
            ecu.sent.len(),
            1 + 3 + 1,
            "34, three 36, 37; no empty block"
        );
    }

    #[test]
    fn a_single_byte() {
        let mut ecu = Ecu::new(0xFFFF);
        let r =
            in_mutation(|| download(&mut ecu, &Download::new(0x8000_0000, &[0xA5]), &NoProgress))
                .unwrap();
        assert_eq!((r.blocks, r.bytes), (1, 1));
        assert_eq!(
            ecu.sent,
            vec![
                vec![0x34, 0x00, 0x44, 0x80, 0, 0, 0, 0, 0, 0, 1],
                vec![0x36, 0x01, 0xA5],
                vec![0x37],
            ]
        );
    }

    #[test]
    fn the_ecu_maximum_wins_over_a_larger_adapter_cap() {
        let mut ecu = Ecu::new(10);
        let d = data(20);
        let job = Download {
            max_payload: Some(100),
            ..Download::new(0, &d)
        };
        let r = in_mutation(|| download(&mut ecu, &job, &NoProgress)).unwrap();
        assert_eq!((r.payload_per_block, r.blocks, r.max_block_len), (8, 3, 10));
        assert!(ecu.blocks().iter().all(|b| b.len() <= 10));
        // A cap equal to the ECU's room changes nothing either.
        let mut ecu = Ecu::new(10);
        let job = Download {
            max_payload: Some(8),
            ..Download::new(0, &d)
        };
        let r = in_mutation(|| download(&mut ecu, &job, &NoProgress)).unwrap();
        assert_eq!(r.payload_per_block, 8);
    }

    #[test]
    fn a_four_byte_length_field_in_the_0x74_reply() {
        let mut ecu = Ecu {
            dl_reply: Some(vec![0x74, 0x40, 0x00, 0x00, 0x01, 0x02]),
            ..Ecu::new(0)
        };
        let d = data(600);
        let r = in_mutation(|| download(&mut ecu, &Download::new(0, &d), &NoProgress)).unwrap();
        assert_eq!(
            (r.max_block_len, r.payload_per_block, r.blocks),
            (0x102, 0x100, 3)
        );
        assert_eq!(received(&ecu), d);
        // A one-byte field (and the smallest block that carries data).
        let mut ecu = Ecu {
            dl_reply: Some(vec![0x74, 0x10, 0x03]),
            ..Ecu::new(0)
        };
        let r =
            in_mutation(|| download(&mut ecu, &Download::new(0, &d[..3]), &NoProgress)).unwrap();
        assert_eq!((r.payload_per_block, r.blocks), (1, 3));
    }

    #[test]
    fn a_malformed_0x74_reply_or_format_sends_no_block() {
        let d = data(16);
        for reply in [
            vec![0x74],                   // no lengthFormatIdentifier
            vec![0x74, 0x00],             // zero length bytes
            vec![0x74, 0x20, 0x01],       // truncated length field
            vec![0x74, 0x20, 0x00, 0x00], // a zero block length
            vec![0x76, 0x20, 0x00, 0x10], // another service's reply
        ] {
            let mut ecu = Ecu {
                dl_reply: Some(reply.clone()),
                ..Ecu::new(0)
            };
            let e =
                in_mutation(|| download(&mut ecu, &Download::new(0, &d), &NoProgress)).unwrap_err();
            assert!(
                matches!(e, TransferError::NotStarted { .. }),
                "{reply:02X?}: {e}"
            );
            assert!(e.to_string().contains("requestDownload"), "{e}");
            assert!(ecu.blocks().is_empty(), "{reply:02X?}");
            assert!(!ecu.sent.iter().any(|r| r[0] == 0x37));
        }
        // A format the address or size does not fit: nothing at all is sent.
        for (alfid, addr) in [(0x55u8, 0u32), (0x00, 0), (0x22, 0x1_0000), (0x41, 0x100)] {
            let mut ecu = Ecu::new(10);
            let job = Download {
                alfid,
                ..Download::new(addr, &d)
            };
            let e = in_mutation(|| download(&mut ecu, &job, &NoProgress)).unwrap_err();
            assert!(
                matches!(e, TransferError::NotStarted { .. }),
                "0x{alfid:02X}: {e}"
            );
            assert!(ecu.sent.is_empty(), "0x{alfid:02X}");
        }
        // An adapter cap of zero: the request went out, no block did.
        let mut ecu = Ecu::new(10);
        let job = Download {
            max_payload: Some(0),
            ..Download::new(0, &d)
        };
        let e = in_mutation(|| download(&mut ecu, &job, &NoProgress)).unwrap_err();
        assert!(e.to_string().contains("no room"), "{e}");
        assert!(ecu.blocks().is_empty());
    }

    #[test]
    fn wrong_counter_nrc_mid_way_is_partial_and_nothing_is_resent() {
        let mut ecu = Ecu {
            fail_at: Some(2),
            fail_nrc: 0x73, // wrongBlockSequenceCounter
            ..Ecu::new(10)
        };
        let d = data(40); // 5 blocks
        let e = in_mutation(|| download(&mut ecu, &Download::new(0, &d), &NoProgress)).unwrap_err();
        match &e {
            TransferError::PartiallyWritten {
                blocks_done,
                bytes_done,
                total,
                cancelled,
                reason,
            } => {
                assert_eq!((*blocks_done, *bytes_done, *total), (2, 16, 40));
                assert!(!cancelled);
                assert!(
                    reason.contains("block 3") && reason.contains("0x03"),
                    "{reason}"
                );
                assert!(reason.contains("0x73"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
        assert!(e.to_string().contains("PARTIALLY WRITTEN"));
        assert_eq!(counters(&ecu), vec![1, 2, 3], "each counter sent once");
        assert!(!ecu.sent.iter().any(|r| r[0] == 0x37), "no exit");
    }

    #[test]
    fn a_wrong_echo_or_an_unresolved_pending_stops_without_resending() {
        let d = data(32); // 4 blocks
        let mut ecu = Ecu {
            bad_echo_at: Some(1),
            ..Ecu::new(10)
        };
        let e = in_mutation(|| download(&mut ecu, &Download::new(0, &d), &NoProgress)).unwrap_err();
        assert!(
            matches!(
                e,
                TransferError::PartiallyWritten {
                    blocks_done: 1,
                    bytes_done: 8,
                    ..
                }
            ),
            "{e}"
        );
        assert_eq!(counters(&ecu), vec![1, 2]);
        // The adapter stopped waiting while the ECU still said 0x78.
        let mut ecu = Ecu {
            pending_at: Some(0),
            ..Ecu::new(10)
        };
        let e = in_mutation(|| download(&mut ecu, &Download::new(0, &d), &NoProgress)).unwrap_err();
        assert!(
            matches!(e, TransferError::PartiallyWritten { blocks_done: 0, .. }),
            "{e}"
        );
        assert!(e.to_string().contains("not re-sent"), "{e}");
        assert_eq!(counters(&ecu), vec![1], "the block went out once");
    }

    #[test]
    fn progress_is_monotonic_and_ends_at_one() {
        for (len, max) in [(1usize, 10u16), (24, 10), (25, 10), (1000, 130)] {
            let mut ecu = Ecu::new(max);
            let d = data(len);
            let p = Record::default();
            let r = in_mutation(|| download(&mut ecu, &Download::new(0, &d), &p)).unwrap();
            let log = p.0.borrow();
            assert_eq!(log.len(), r.blocks + 2, "request, every block, exit");
            assert_eq!(log[0].0, 0.0);
            assert!(
                log.windows(2).all(|w| w[0].0 <= w[1].0),
                "{len}/{max}: {log:?}"
            );
            assert!(log.iter().all(|(f, _)| (0.0..=1.0).contains(f)));
            assert_eq!(log[log.len() - 2].0, 1.0, "the last block reports 1.0");
            assert_eq!(log.last().unwrap(), &(1.0, "transfer exit".to_string()));
            let last_block = &log[log.len() - 2].1;
            assert_eq!(
                last_block,
                &format!("block {0}/{0} · {len}/{len} bytes", r.blocks)
            );
        }
    }

    #[test]
    fn a_cancel_after_the_last_block_still_exits() {
        // Reports: requesting (1), block 1 (2), block 2 (3): the cancel comes
        // after the last block, when there is no "between blocks" left; the
        // exit runs so the ECU is not left mid-transfer.
        let mut ecu = Ecu::new(10);
        let d = data(16);
        let p = CancelAfter::new(3);
        let r = in_mutation(|| download(&mut ecu, &Download::new(0, &d), &p)).unwrap();
        assert_eq!(r.blocks, 2);
        assert_eq!(ecu.sent.last().unwrap(), &vec![0x37]);
    }

    #[test]
    fn the_exit_record_is_returned() {
        struct Rec(Vec<Vec<u8>>);
        impl RawDiag for Rec {
            fn request(&mut self, req: &[u8]) -> Result<Vec<u8>, String> {
                self.0.push(req.to_vec());
                Ok(match req[0] {
                    0x34 => vec![0x74, 0x20, 0x00, 0x05],
                    0x36 => vec![0x76, req[1]],
                    _ => vec![0x77, 0xC0, 0xFF, 0xEE],
                })
            }
        }
        let mut ecu = Rec(Vec::new());
        let r =
            in_mutation(|| download(&mut ecu, &Download::new(0, &data(7)), &NoProgress)).unwrap();
        assert_eq!(r.exit_record, vec![0xC0, 0xFF, 0xEE]);
        assert_eq!(r.blocks, 3, "3 data bytes per block");
    }
}
