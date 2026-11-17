//! Block read/write (UDS on ISO-TP) over an ELM327's CAN mode.
//!
//! The ELM327 segments ISO-TP itself with `ATCAF1`, but it can only
//! **transmit** a single frame: every request has to fit in
//! [`elm::MAX_CAN_REQUEST`] (7) bytes. Replies can be multi-frame, and the ELM
//! answers the ECU's first frame with the flow control set by
//! `ATFCSH`/`ATFCSD`/`ATFCSM`. This module plans every request to fit that
//! budget **before** anything is sent, and refuses what does not fit:
//!
//! - `ReadMemoryByAddress` (0x23): `23 <alfid> <addr> <size>`, 5 bytes for
//!   address + size, so the default format is `0x14` (4-byte address, 1-byte
//!   size) and a read is chunked to at most 255 bytes per request.
//! - `RequestUpload` (0x35) / `RequestDownload` (0x34): `3x <dfi> <alfid>
//!   <addr> <size>`, only 4 bytes for address + size. The default format is
//!   `0x22` (16-bit address, 16-bit size) or `0x13` (24-bit address, 8-bit
//!   size); a region is transferred in windows the size field can express, one
//!   request/transfer/exit cycle per window. A 32-bit address cannot be
//!   expressed and is refused.
//! - `TransferData` (0x36) for a download: `36 <counter> <data>` leaves 5 data
//!   bytes per block ([`MAX_TX_BLOCK_DATA`]). Multi-frame transmit is **not**
//!   implemented, so a download is slow (one ELM round trip per 5 bytes) and
//!   gated behind an explicit bench opt-in (`elm_write=single-frame`).
//!
//! Security access (0x27) runs exactly once per operation with the caller's
//! [`SeedKey`] handler: no retries, no brute force. NRC 0x35/0x36 stop with
//! [`Error::SecurityDenied`]; NRC 0x37 (delay not expired) is reported with an
//! explanation and is never retried automatically.

use opentune_core::protocol::diag::DiagTransport;
use opentune_core::protocol::elm;
use opentune_core::protocol::seedkey::SeedKey;
use opentune_core::protocol::uds::{next_block_seq, seed_is_unlocked, sid, UdsClient};
use opentune_core::{ConnectConfig, Error, MemoryRegion, Phase, Progress, Result};

/// Largest `TransferData` payload one single-frame request can carry:
/// 7 bytes minus the service id and the block sequence counter.
pub const MAX_TX_BLOCK_DATA: usize = elm::MAX_CAN_REQUEST - 2;

/// Bytes left for `memoryAddress` + `memorySize` in a 0x23 request
/// (`23 <alfid>` + 5).
const RMBA_FIELD_BUDGET: usize = elm::MAX_CAN_REQUEST - 2;

/// Bytes left for `memoryAddress` + `memorySize` in a 0x34/0x35 request
/// (`3x <dfi> <alfid>` + 4).
const TRANSFER_FIELD_BUDGET: usize = elm::MAX_CAN_REQUEST - 3;

/// Largest ISO-TP payload, minus the 0x63 response id.
const MAX_RMBA_CHUNK: u32 = 4094;

/// Default P2 (first-response) budget for memory services on a slow clone:
/// longer than the 2 s UDS default because the ELM adds its own serial and
/// `ATST` latency to every exchange. The ELM itself answers `NO DATA` after
/// its `ATST` window, so this only bounds a stalled adapter.
pub const DEFAULT_MEMORY_P2_MS: u64 = 3000;

/// Default 0x23 chunk when the ELM paces the ECU's consecutive frames with a
/// user-set STmin (`ATFCSM 1` accepted at init).
pub const DEFAULT_CHUNK_PACED: u32 = 0x80;

/// Default 0x23 chunk when the adapter refused user flow control: the ECU then
/// sends consecutive frames back to back, and a clone's small receive buffer
/// overflows (`BUFFER FULL`) on long replies.
pub const DEFAULT_CHUNK_UNPACED: u32 = 0x20;

/// Which read service carries a block read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadMode {
    /// `ReadMemoryByAddress` (0x23), chunked.
    ByAddress,
    /// `RequestUpload` (0x35) + `TransferData` (0x36) + `RequestTransferExit` (0x37).
    Upload,
}

/// Which operation the options are parsed for (they differ in default session).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Block read: default session extended (0x03).
    Read,
    /// Block write: default session programming (0x02).
    Write,
}

/// Memory-access options from [`ConnectConfig::options`]. See the crate docs
/// for the option table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemOptions {
    /// `DiagnosticSessionControl` sub-function to enter first, if any.
    pub session: Option<u8>,
    /// The config marks the ECU as needing security access.
    pub security_required: bool,
    /// Request-seed access level (odd); send-key is `level + 1`.
    pub seed_level: u8,
    /// Read service.
    pub read_mode: ReadMode,
    /// Bytes per 0x23 request (`None` = adapter-dependent default).
    pub read_chunk: Option<u32>,
    /// `addressAndLengthFormatIdentifier` override.
    pub alfid: Option<u8>,
    /// `dataFormatIdentifier` for 0x34/0x35 (0x00 = no compression/encryption).
    pub data_format: u8,
    /// ISO-TP STmin (ms) the ELM asks the ECU for during a block read.
    pub fc_stmin: Option<u8>,
    /// Bench opt-in for the slow single-frame download path.
    pub bench_write: bool,
    /// P2 budget (ms) per memory request.
    pub timeout_ms: u64,
}

/// A validated read: the service and its request format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadPlan {
    /// 0x23 with `alfid`, `chunk` bytes per request.
    ByAddress {
        /// `addressAndLengthFormatIdentifier`.
        alfid: u8,
        /// Bytes per request.
        chunk: u32,
    },
    /// 0x35/0x36/0x37 with `alfid`, in windows of at most `window` bytes.
    Upload {
        /// `addressAndLengthFormatIdentifier`.
        alfid: u8,
        /// Largest size the format's size field can express.
        window: u32,
    },
}

fn opt<'a>(cfg: &'a ConnectConfig, key: &str) -> Option<&'a str> {
    cfg.options
        .get(key)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
}

/// Decimal, or hex with a `0x` prefix.
fn parse_num(s: &str) -> Option<u32> {
    let t = s.trim();
    match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(h) => u32::from_str_radix(h, 16).ok(),
        None => t.parse().ok(),
    }
}

/// A byte that is hex by convention (format identifiers): `14`, `0x14`.
fn parse_hex_byte(s: &str) -> Option<u8> {
    let t = s.trim();
    let h = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    u8::from_str_radix(h, 16).ok()
}

fn config_err(key: &str, value: &str, want: &str) -> Error {
    Error::Config(format!("elm327obd option {key}={value:?}: expected {want}"))
}

impl MemOptions {
    /// Parse and validate the memory options for `op`. Nothing is sent.
    pub fn from_config(cfg: &ConnectConfig, op: Op) -> Result<Self> {
        let session = match opt(cfg, "session") {
            None => Some(match op {
                Op::Read => 0x03,
                Op::Write => 0x02,
            }),
            Some(v) => match v.to_ascii_lowercase().as_str() {
                "none" | "off" | "skip" => None,
                "default" => Some(0x01),
                "programming" => Some(0x02),
                "extended" => Some(0x03),
                other => Some(
                    parse_num(other)
                        .and_then(|n| u8::try_from(n).ok())
                        .filter(|&n| n != 0)
                        .ok_or_else(|| {
                            config_err(
                                "session",
                                v,
                                "none/default/programming/extended or a session byte",
                            )
                        })?,
                ),
            },
        };
        let security_required = cfg.options.contains_key("seedkey")
            || matches!(
                opt(cfg, "security"),
                Some("required" | "true" | "1" | "yes")
            );
        let seed_level = match opt(cfg, "seedkey_level") {
            None => 0x11,
            Some(v) => parse_num(v)
                .and_then(|n| u8::try_from(n).ok())
                .filter(|n| n % 2 == 1)
                .ok_or_else(|| config_err("seedkey_level", v, "an odd request-seed level"))?,
        };
        let read_mode = match opt(cfg, "read_mode")
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            None | Some("rmba" | "23" | "0x23" | "read_memory_by_address") => ReadMode::ByAddress,
            Some("upload" | "35" | "0x35" | "request_upload") => ReadMode::Upload,
            Some(_) => {
                let v = opt(cfg, "read_mode").unwrap_or_default();
                return Err(config_err("read_mode", v, "rmba or upload"));
            }
        };
        let read_chunk = match opt(cfg, "read_chunk") {
            None => None,
            Some(v) => Some(
                parse_num(v)
                    .filter(|n| (1..=MAX_RMBA_CHUNK).contains(n))
                    .ok_or_else(|| config_err("read_chunk", v, "1..=4094 bytes"))?,
            ),
        };
        let alfid = match opt(cfg, "alfid") {
            None => None,
            Some(v) => Some(
                parse_hex_byte(v).ok_or_else(|| config_err("alfid", v, "a hex byte like 14"))?,
            ),
        };
        let data_format = match opt(cfg, "data_format") {
            None => 0x00,
            Some(v) => {
                parse_hex_byte(v).ok_or_else(|| config_err("data_format", v, "a hex byte"))?
            }
        };
        let fc_stmin = match opt(cfg, "fc_stmin") {
            None => None,
            Some(v) => Some(
                parse_num(v)
                    .and_then(|n| u8::try_from(n).ok())
                    .filter(|n| *n <= 0x7F)
                    .ok_or_else(|| config_err("fc_stmin", v, "0..=127 ms"))?,
            ),
        };
        let bench_write = matches!(
            opt(cfg, "elm_write")
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("single-frame" | "single_frame" | "bench")
        );
        let timeout_ms = match opt(cfg, "timeout_ms") {
            None => DEFAULT_MEMORY_P2_MS,
            Some(v) => parse_num(v)
                .filter(|n| (500..=30_000).contains(n))
                .map(u64::from)
                .ok_or_else(|| config_err("timeout_ms", v, "500..=30000 ms"))?,
        };
        Ok(Self {
            timeout_ms,
            session,
            security_required,
            seed_level,
            read_mode,
            read_chunk,
            alfid,
            data_format,
            fc_stmin,
            bench_write,
        })
    }

    /// Plan a read of `region`. `paced` = the adapter accepted user flow
    /// control at init, so long multi-frame replies can be slowed down.
    pub fn plan_read(&self, region: MemoryRegion, paced: bool) -> Result<ReadPlan> {
        let last = last_address(region)?;
        match self.read_mode {
            ReadMode::ByAddress => {
                let alfid = self.alfid.unwrap_or(0x14);
                check_alfid(alfid, last, RMBA_FIELD_BUDGET, "ReadMemoryByAddress (0x23)")?;
                let size_max = field_max(alfid >> 4).min(MAX_RMBA_CHUNK);
                let default = if paced {
                    DEFAULT_CHUNK_PACED
                } else {
                    DEFAULT_CHUNK_UNPACED
                };
                let chunk = self.read_chunk.unwrap_or(default).min(size_max);
                Ok(ReadPlan::ByAddress { alfid, chunk })
            }
            ReadMode::Upload => {
                let alfid = transfer_alfid(self.alfid, last, "RequestUpload (0x35)")?;
                Ok(ReadPlan::Upload {
                    alfid,
                    window: field_max(alfid >> 4),
                })
            }
        }
    }

    /// Plan a download of `region`: the `RequestDownload` format and window.
    pub fn plan_write(&self, region: MemoryRegion) -> Result<(u8, u32)> {
        let last = last_address(region)?;
        let alfid = transfer_alfid(self.alfid, last, "RequestDownload (0x34)")?;
        Ok((alfid, field_max(alfid >> 4)))
    }
}

/// The last byte address of a non-empty region; a region that wraps past
/// 0xFFFF_FFFF is a configuration error.
fn last_address(region: MemoryRegion) -> Result<u32> {
    if region.length == 0 {
        return Err(Error::Config("empty memory region".into()));
    }
    region
        .address
        .checked_add(region.length - 1)
        .ok_or_else(|| {
            Error::Config(format!(
                "region 0x{:08X}+0x{:X} runs past the 32-bit address space",
                region.address, region.length
            ))
        })
}

/// Bytes needed to express `v`.
fn bytes_needed(v: u32) -> usize {
    match v {
        0..=0xFF => 1,
        0x100..=0xFFFF => 2,
        0x1_0000..=0xFF_FFFF => 3,
        _ => 4,
    }
}

/// Largest value an `n`-byte field holds.
fn field_max(n: u8) -> u32 {
    match n {
        0 => 0,
        1..=3 => (1u32 << (8 * u32::from(n))) - 1,
        _ => u32::MAX,
    }
}

/// Check that `alfid` is a valid format that fits `budget` request bytes and
/// can express every address up to `last`.
fn check_alfid(alfid: u8, last: u32, budget: usize, service: &str) -> Result<()> {
    let size_len = usize::from(alfid >> 4);
    let addr_len = usize::from(alfid & 0x0F);
    if !(1..=4).contains(&size_len) || !(1..=4).contains(&addr_len) {
        return Err(Error::Config(format!(
            "{service}: addressAndLengthFormatIdentifier 0x{alfid:02X} is not a valid 1..=4-byte format"
        )));
    }
    if size_len + addr_len > budget {
        return Err(Error::Unsupported(format!(
            "{service}: format 0x{alfid:02X} needs {} address+size bytes but an ELM327 \
             single-frame request leaves only {budget}; use a smaller format (alfid option) \
             or a raw-CAN interface (slcan/gs_usb/SocketCAN/J2534)",
            size_len + addr_len
        )));
    }
    if bytes_needed(last) > addr_len {
        return Err(Error::Config(format!(
            "{service}: address 0x{last:08X} does not fit the {addr_len}-byte address of format 0x{alfid:02X}"
        )));
    }
    Ok(())
}

/// Format for 0x34/0x35 within the 4-byte address+size budget: the override
/// if given, else `0x22` for 16-bit addresses or `0x13` for 24-bit ones. A
/// 32-bit address is refused (it leaves no byte for the size).
fn transfer_alfid(over: Option<u8>, last: u32, service: &str) -> Result<u8> {
    let alfid = match over {
        Some(a) => a,
        None => match bytes_needed(last) {
            1 | 2 => 0x22,
            3 => 0x13,
            _ => {
                return Err(Error::Unsupported(format!(
                    "{service}: address 0x{last:08X} needs a 4-byte address field, which leaves \
                     no room for the size in an ELM327 single-frame request (7 bytes); use \
                     read_mode=rmba for reads, or a raw-CAN interface (slcan/gs_usb/SocketCAN/J2534)"
                )))
            }
        },
    };
    check_alfid(alfid, last, TRANSFER_FIELD_BUDGET, service)?;
    Ok(alfid)
}

/// A UDS client with the memory P2 budget.
fn client(t: &mut dyn DiagTransport, p2_ms: u64) -> UdsClient<'_> {
    let mut uds = UdsClient::new(t);
    uds.timeout_ms = p2_ms;
    uds.p2_star_ms = uds.p2_star_ms.max(p2_ms);
    uds
}

/// Enter `session` (0x10) if one is configured. NRC 0x22/0x12/0x7E get a hint.
pub fn enter_session(t: &mut dyn DiagTransport, session: Option<u8>, p2_ms: u64) -> Result<()> {
    if let Some(s) = session {
        client(t, p2_ms).session(s).map_err(|e| match e {
            Error::NegativeResponse {
                service: sid::SESSION_CONTROL,
                code: code @ (0x12 | 0x22 | 0x7E),
                name,
            } => Error::Protocol(format!(
                "DiagnosticSessionControl 0x{s:02X} refused (NRC 0x{code:02X} {name}); check \
                 ignition on / engine off, or pick another session with option \
                 session=extended|default|none"
            )),
            e => e,
        })?;
    }
    Ok(())
}

/// One request a block operation would send, for a dry run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedRequest {
    /// Exactly what is written to the adapter: an `AT` command (adapter-local,
    /// never transmitted on the bus) or the request's hex bytes. A send-key's
    /// key bytes are unknown until the seed arrives and shown as `<key>`; an
    /// upload's block counters are shown as a range.
    pub line: String,
    /// What the request is for.
    pub purpose: String,
}

fn planned(line: impl Into<String>, purpose: impl Into<String>) -> PlannedRequest {
    PlannedRequest {
        line: line.into(),
        purpose: purpose.into(),
    }
}

/// List, without sending anything, every request a block read of `region`
/// would make: flow-control pacing, session, security, each 0x23 chunk (or
/// each upload window), and the pacing restore. Fails with the same error the
/// real read would raise before touching the bus (bad options, a format that
/// does not fit a single frame, security required without a handler).
pub fn dry_run_read(
    opts: &MemOptions,
    region: MemoryRegion,
    paced: bool,
    stmin: u8,
    has_handler: bool,
) -> Result<Vec<PlannedRequest>> {
    let mut out = Vec::new();
    if region.length == 0 {
        return Ok(out);
    }
    let plan = opts.plan_read(region, paced)?;
    if opts.security_required && !has_handler {
        return Err(missing_handler());
    }
    if paced {
        out.push(planned(
            format!("ATFCSD 30 00 {stmin:02X}"),
            format!("adapter: ask the ECU for {stmin} ms between consecutive frames"),
        ));
    }
    if let Some(s) = opts.session {
        out.push(planned(
            elm::request_hex(&[sid::SESSION_CONTROL, s]),
            format!("DiagnosticSessionControl 0x{s:02X}"),
        ));
    }
    if has_handler {
        let lvl = opts.seed_level;
        out.push(planned(
            elm::request_hex(&[sid::SECURITY_ACCESS, lvl]),
            format!("SecurityAccess requestSeed level 0x{lvl:02X}"),
        ));
        out.push(planned(
            format!(
                "{}<key>",
                elm::request_hex(&[sid::SECURITY_ACCESS, lvl + 1])
            ),
            "SecurityAccess sendKey (handler's key; skipped on an all-zero seed; one \
             attempt, stops on NRC 0x35/0x36, reports 0x37)",
        ));
    }
    let total = region.length;
    let mut done = 0u32;
    match plan {
        ReadPlan::ByAddress { alfid, chunk } => {
            while done < total {
                let n = chunk.min(total - done);
                let addr = region.address + done;
                let mut req = vec![sid::READ_MEMORY_BY_ADDRESS, alfid];
                req.extend(opentune_core::protocol::uds::encode_address_and_length(
                    alfid, addr, n,
                )?);
                out.push(planned(
                    elm::request_hex(&req),
                    format!("ReadMemoryByAddress 0x{addr:08X} +{n}"),
                ));
                done += n;
            }
        }
        ReadPlan::Upload { alfid, window } => {
            while done < total {
                let n = window.min(total - done);
                let addr = region.address + done;
                let mut req = vec![sid::REQUEST_UPLOAD, opts.data_format, alfid];
                req.extend(opentune_core::protocol::uds::encode_address_and_length(
                    alfid, addr, n,
                )?);
                out.push(planned(
                    elm::request_hex(&req),
                    format!("RequestUpload 0x{addr:08X} +{n}"),
                ));
                out.push(planned(
                    "3601..",
                    format!(
                        "TransferData blocks from counter 01 (wrapping FF->00) until {n} bytes \
                         arrive; the count depends on the ECU's maxNumberOfBlockLength"
                    ),
                ));
                out.push(planned("37", "RequestTransferExit"));
                done += n;
            }
        }
    }
    if paced {
        out.push(planned(
            "ATFCSD 30 00 00",
            "adapter: restore the init flow control",
        ));
    }
    Ok(out)
}

/// Add where a transfer failed to a transport-level error (timeout, adapter
/// error line, truncated reply). NRCs, security and adapter-reset errors stay
/// structured.
fn at_address(e: Error, what: &str, addr: u32, done: usize, total: u32) -> Error {
    match e {
        Error::Timeout(ms) => Error::Protocol(format!(
            "{what} at 0x{addr:08X}: no answer within {ms} ms after {done} of {total} bytes \
             (ECU asleep, wrong rx id, or the adapter stalled)"
        )),
        Error::Protocol(m) => Error::Protocol(format!(
            "{what} at 0x{addr:08X} after {done} of {total} bytes: {m}"
        )),
        e => e,
    }
}

/// Turn NRC 0x37 into an explanation; everything else passes through.
fn explain_delay(e: Error) -> Error {
    match e {
        Error::NegativeResponse {
            service: sid::SECURITY_ACCESS,
            code: 0x37,
            ..
        } => Error::Protocol(
            "security access: the ECU answered NRC 0x37 (requiredTimeDelayNotExpired) - \
             its anti-scan delay timer is running after an earlier failed unlock. Wait for \
             it to expire (often 10 s or more; some ECUs need ignition off/on) and start \
             again; OpenTune does not retry automatically"
                .into(),
        ),
        e => e,
    }
}

/// UDS security access (0x27) with the caller's handler, exactly once.
///
/// Without a handler: an error when `required`, else a no-op (open ECU).
/// An all-zero seed means "already unlocked" and skips the key. NRC 0x35
/// (invalid key) and 0x36 (attempts exceeded) stop with
/// [`Error::SecurityDenied`]; 0x37 is reported, never retried. Returns whether
/// an unlock was performed.
pub fn security_access(
    t: &mut dyn DiagTransport,
    seed_key: Option<&dyn SeedKey>,
    level: u8,
    required: bool,
    p2_ms: u64,
) -> Result<bool> {
    let Some(sk) = seed_key else {
        return if required {
            Err(missing_handler())
        } else {
            Ok(false)
        };
    };
    let mut uds = client(t, p2_ms);
    let seed = uds.request_seed(level).map_err(explain_delay)?;
    if seed_is_unlocked(&seed) {
        return Ok(true);
    }
    if seed.is_empty() {
        return Err(Error::Protocol(format!(
            "security access level 0x{level:02X}: the ECU sent an empty seed"
        )));
    }
    let key = sk.compute(&seed, level)?;
    if key.is_empty() || 2 + key.len() > elm::MAX_CAN_REQUEST {
        return Err(Error::Unsupported(format!(
            "security access: a {}-byte key does not fit an ELM327 single-frame request \
             (max {MAX_TX_BLOCK_DATA} key bytes); use a raw-CAN interface",
            key.len()
        )));
    }
    uds.send_key(level.wrapping_add(1), &key)
        .map_err(explain_delay)?;
    Ok(true)
}

/// The error for "security is required but no handler was supplied".
pub fn missing_handler() -> Error {
    Error::Config(
        "this ECU requires security access (0x27) but no seed/key handler is set: \
         supply one for this ECU (set_seed_key / a plugin); OpenTune ships no OEM algorithms"
            .into(),
    )
}

/// Hints for a refused `ReadMemoryByAddress`.
fn explain_rmba(e: Error, unlocked: bool) -> Error {
    match e {
        Error::NegativeResponse {
            service: sid::READ_MEMORY_BY_ADDRESS,
            code: 0x33,
            ..
        } if !unlocked => Error::Config(
            "the ECU answered ReadMemoryByAddress with NRC 0x33 (securityAccessDenied): this \
             region needs security access; supply a seed/key handler (set_seed_key) and mark \
             it required (option seedkey=<id> or security=required)"
                .into(),
        ),
        Error::NegativeResponse {
            service: sid::READ_MEMORY_BY_ADDRESS,
            code: code @ (0x11 | 0x7F),
            name,
        } => Error::Unsupported(format!(
            "the ECU refuses ReadMemoryByAddress (NRC 0x{code:02X} {name}); try \
             read_mode=upload (RequestUpload 0x35/0x36/0x37) or another session"
        )),
        e => e,
    }
}

/// Read `region` with `plan`. Progress is reported per chunk/block.
pub fn read(
    t: &mut dyn DiagTransport,
    plan: ReadPlan,
    opts: &MemOptions,
    unlocked: bool,
    region: MemoryRegion,
    progress: &mut dyn FnMut(Progress),
) -> Result<Vec<u8>> {
    let data_format = opts.data_format;
    let total = region.length;
    let report = |progress: &mut dyn FnMut(Progress), done: usize| {
        progress(Progress {
            phase: Phase::Read,
            done: done as u64,
            total: u64::from(total),
        })
    };
    let mut out = Vec::with_capacity(total as usize);
    report(progress, 0);
    let mut uds = client(t, opts.timeout_ms);
    match plan {
        ReadPlan::ByAddress { alfid, chunk } => {
            while (out.len() as u32) < total {
                let addr = region.address + out.len() as u32;
                let n = chunk.min(total - out.len() as u32);
                let data = uds
                    .read_memory_by_address(alfid, addr, n)
                    .map_err(|e| explain_rmba(e, unlocked))
                    .map_err(|e| at_address(e, "ReadMemoryByAddress", addr, out.len(), total))?;
                out.extend_from_slice(&data);
                report(progress, out.len());
            }
        }
        ReadPlan::Upload { alfid, window } => {
            while (out.len() as u32) < total {
                let addr = region.address + out.len() as u32;
                let win = window.min(total - out.len() as u32);
                let max_block = uds
                    .request_upload_with_format(data_format, alfid, addr, win)
                    .map_err(|e| at_address(e, "RequestUpload", addr, out.len(), total))?;
                if max_block < 3 {
                    return Err(Error::Protocol(format!(
                        "RequestUpload: maxNumberOfBlockLength {max_block} leaves no room for data"
                    )));
                }
                let mut got = 0u32;
                let mut seq = 1u8;
                while got < win {
                    let at = addr + got;
                    let body = uds
                        .transfer_data(seq, &[])
                        .map_err(|e| at_address(e, "TransferData", at, out.len(), total))?;
                    let data = &body[1..];
                    if data.is_empty() {
                        return Err(Error::Protocol(format!(
                            "TransferData block 0x{seq:02X}: empty upload block"
                        )));
                    }
                    let take = data.len().min((win - got) as usize);
                    out.extend_from_slice(&data[..take]);
                    got += take as u32;
                    seq = next_block_seq(seq);
                    report(progress, out.len());
                }
                uds.transfer_exit()
                    .map_err(|e| at_address(e, "RequestTransferExit", addr, out.len(), total))?;
            }
        }
    }
    Ok(out)
}

/// Download `data` to `region` with 0x34/0x36/0x37 in single-frame blocks of
/// at most [`MAX_TX_BLOCK_DATA`] bytes. Progress is reported per block.
pub fn write(
    t: &mut dyn DiagTransport,
    (alfid, window): (u8, u32),
    opts: &MemOptions,
    region: MemoryRegion,
    data: &[u8],
    progress: &mut dyn FnMut(Progress),
) -> Result<()> {
    let data_format = opts.data_format;
    let total = data.len() as u64;
    let mut done = 0usize;
    progress(Progress {
        phase: Phase::Write,
        done: 0,
        total,
    });
    let mut uds = client(t, opts.timeout_ms);
    for win in data.chunks(window as usize) {
        let addr = region.address + done as u32;
        let max_block =
            uds.request_download_with_format(data_format, alfid, addr, win.len() as u32)?;
        if max_block < 3 {
            return Err(Error::Protocol(format!(
                "RequestDownload: maxNumberOfBlockLength {max_block} leaves no room for data"
            )));
        }
        let block = (max_block - 2).min(MAX_TX_BLOCK_DATA);
        let mut seq = 1u8;
        for chunk in win.chunks(block) {
            uds.transfer_data(seq, chunk)?;
            seq = next_block_seq(seq);
            done += chunk.len();
            progress(Progress {
                phase: Phase::Write,
                done: done as u64,
                total,
            });
        }
        uds.transfer_exit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(pairs: &[(&str, &str)]) -> ConnectConfig {
        let mut c = ConnectConfig::default();
        for (k, v) in pairs {
            c.options.insert((*k).into(), (*v).into());
        }
        c
    }

    #[test]
    fn defaults_per_operation() {
        let r = MemOptions::from_config(&cfg(&[]), Op::Read).unwrap();
        assert_eq!(r.session, Some(0x03));
        assert_eq!(r.read_mode, ReadMode::ByAddress);
        assert_eq!(r.seed_level, 0x11);
        assert!(!r.security_required && !r.bench_write);
        let w = MemOptions::from_config(&cfg(&[]), Op::Write).unwrap();
        assert_eq!(w.session, Some(0x02));
    }

    #[test]
    fn options_parse_and_reject() {
        let o = MemOptions::from_config(
            &cfg(&[
                ("session", "none"),
                ("read_mode", "upload"),
                ("alfid", "13"),
                ("read_chunk", "0x40"),
                ("seedkey_level", "0x01"),
                ("security", "required"),
                ("fc_stmin", "8"),
                ("elm_write", "single-frame"),
            ]),
            Op::Read,
        )
        .unwrap();
        assert_eq!(o.session, None);
        assert_eq!(o.read_mode, ReadMode::Upload);
        assert_eq!(o.alfid, Some(0x13));
        assert_eq!(o.read_chunk, Some(0x40));
        assert_eq!(o.seed_level, 0x01);
        assert!(o.security_required && o.bench_write);
        assert_eq!(o.fc_stmin, Some(8));
        assert_eq!(o.timeout_ms, DEFAULT_MEMORY_P2_MS);
        for bad in [
            ("timeout_ms", "10"),
            ("session", "bogus"),
            ("read_mode", "dma"),
            ("seedkey_level", "2"),
            ("read_chunk", "0"),
            ("fc_stmin", "200"),
            ("alfid", "zz"),
        ] {
            assert!(
                matches!(
                    MemOptions::from_config(&cfg(&[bad]), Op::Read),
                    Err(Error::Config(_))
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn read_plans_fit_a_single_frame() {
        let o = MemOptions::from_config(&cfg(&[]), Op::Read).unwrap();
        let r = MemoryRegion::new(0x8000_0000, 0x1000);
        assert_eq!(
            o.plan_read(r, true).unwrap(),
            ReadPlan::ByAddress {
                alfid: 0x14,
                chunk: DEFAULT_CHUNK_PACED
            }
        );
        assert_eq!(
            o.plan_read(r, false).unwrap(),
            ReadPlan::ByAddress {
                alfid: 0x14,
                chunk: DEFAULT_CHUNK_UNPACED
            }
        );
        // A 1-byte size field caps the chunk at 255 whatever is configured.
        let big = MemOptions::from_config(&cfg(&[("read_chunk", "1000")]), Op::Read).unwrap();
        assert_eq!(
            big.plan_read(r, true).unwrap(),
            ReadPlan::ByAddress {
                alfid: 0x14,
                chunk: 0xFF
            }
        );
        // 0x24 = 4-byte address + 2-byte size = 8-byte request: refused.
        let wide = MemOptions::from_config(&cfg(&[("alfid", "24")]), Op::Read).unwrap();
        assert!(matches!(
            wide.plan_read(r, true),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn transfer_formats_pick_the_compact_one_or_refuse() {
        let o = MemOptions::from_config(&cfg(&[("read_mode", "upload")]), Op::Read).unwrap();
        assert_eq!(
            o.plan_read(MemoryRegion::new(0x8000, 0x100), true).unwrap(),
            ReadPlan::Upload {
                alfid: 0x22,
                window: 0xFFFF
            }
        );
        assert_eq!(
            o.plan_read(MemoryRegion::new(0x01_0000, 0x100), true)
                .unwrap(),
            ReadPlan::Upload {
                alfid: 0x13,
                window: 0xFF
            }
        );
        assert!(matches!(
            o.plan_read(MemoryRegion::new(0x0100_0000, 0x10), true),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            o.plan_write(MemoryRegion::new(0xFFFF_FFF0, 0x20)),
            Err(Error::Config(_))
        ));
        // An override that cannot express the address is a config error.
        let narrow = MemOptions::from_config(&cfg(&[("alfid", "31")]), Op::Write).unwrap();
        assert!(matches!(
            narrow.plan_write(MemoryRegion::new(0x1000, 4)),
            Err(Error::Config(_))
        ));
    }

    #[test]
    fn dry_run_lists_exact_requests() {
        let o = MemOptions::from_config(&cfg(&[("read_chunk", "0x80")]), Op::Read).unwrap();
        let steps = dry_run_read(&o, MemoryRegion::new(0x8000_0000, 0x100), true, 6, true).unwrap();
        let lines: Vec<&str> = steps.iter().map(|s| s.line.as_str()).collect();
        assert_eq!(
            lines,
            vec![
                "ATFCSD 30 00 06",
                "1003",
                "2711",
                "2712<key>",
                "23148000000080",
                "23148000008080",
                "ATFCSD 30 00 00",
            ]
        );
        // Every bus request fits one ELM single frame.
        for l in lines
            .iter()
            .filter(|l| !l.starts_with("AT") && !l.contains('<'))
        {
            assert!(l.len() <= 2 * elm::MAX_CAN_REQUEST, "{l}");
        }
        // Required security without a handler fails the dry run too.
        let req = MemOptions::from_config(&cfg(&[("security", "required")]), Op::Read).unwrap();
        assert!(matches!(
            dry_run_read(&req, MemoryRegion::new(0, 4), true, 6, false),
            Err(Error::Config(_))
        ));
    }

    #[test]
    fn dry_run_upload_windows() {
        let o = MemOptions::from_config(
            &cfg(&[("read_mode", "upload"), ("session", "none")]),
            Op::Read,
        )
        .unwrap();
        let steps = dry_run_read(&o, MemoryRegion::new(0x01_0000, 0x180), false, 6, false).unwrap();
        let lines: Vec<&str> = steps.iter().map(|s| s.line.as_str()).collect();
        assert_eq!(
            lines,
            vec![
                "350013010000FF",
                "3601..",
                "37",
                "3500130100FF81",
                "3601..",
                "37"
            ]
        );
    }

    #[test]
    fn field_sizes() {
        assert_eq!(field_max(1), 0xFF);
        assert_eq!(field_max(2), 0xFFFF);
        assert_eq!(field_max(3), 0xFF_FFFF);
        assert_eq!(field_max(4), u32::MAX);
        assert_eq!(bytes_needed(0), 1);
        assert_eq!(bytes_needed(0x1_0000), 3);
        assert_eq!(bytes_needed(0x0100_0000), 4);
        assert_eq!(MAX_TX_BLOCK_DATA, 5);
    }
}
