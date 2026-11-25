//! K-line on the OpenPort 2.0: the cable's ISO 14230 (KWP2000) channel 4 and
//! ISO 9141 channel 3, used read-only.
//!
//! Unlike a dumb KKL cable, the OP2 does the bus timing itself: the wake-up
//! (fast init) and the five-baud address byte, the inter-byte and
//! inter-message times, and the half-duplex echo (dropped unless `LOOPBACK`
//! is on). The host sends whole messages and receives whole messages. This
//! module frames KWP2000 on top and exposes the result as a
//! [`DiagTransport`], so the core [`Kwp2000Client`] (and `identify`) run on it
//! unchanged, as they do over the `kkl` driver.
//!
//! Wire use (public sources, facts only; see `proto` for the citations):
//! 1. `ato<ch> <flags> 10400 0`: `flags` is 0 (the cable appends and checks
//!    the checksum: the J2534 default, and the vendor DLL trace
//!    `ato4 0 10400 0`) or `ISO9141_NO_CHECKSUM` (the host frames it);
//! 2. `ats<ch> <param> <value>` for each timing value the caller set;
//! 3. `atf<ch> 1 0 1` + `00` + `00`: a pass-all PASS filter (the shape
//!    measured on a cable, bisak §7.9); addressing is checked here;
//! 4. init: `aty<ch> <n> 0` + StartCommunication (fast) → `ary<ch> <n>` + the
//!    ECU's answer, or `atw<ch> <addr>` (five-baud) → `arw<ch> <kb1> <kb2>`; the
//!    sync byte, the inverted KB2 and the inverted address are exchanged by
//!    the cable (J2534 `FIVE_BAUD_INIT`, mode set by `FIVE_BAUD_MOD`);
//! 5. per request `att<ch> <n> 0 <budget_us>` + the message (no CAN id);
//!    answers arrive as K-line records (data without timestamp, optional
//!    START/END markers).
//!
//! Read path only: [`KLineLink::request`] refuses every service outside the
//! shared read-only list (`opentune_core::protocol::readonly`, narrowed for
//! K-line; see [`refuse_non_read`]) before anything is sent, and the
//! driver's `write_memory` refuses on K-line.

use crate::proto::{self, param, status, RawFrame, ISO9141_NO_CHECKSUM};
use crate::proto::{PROTO_ISO14230, PROTO_ISO9141};
use crate::{device_error, Session, CMD_TIMEOUT_MS, SETUP_TIMEOUT_MS, TX_REPLY_GRACE_MS};
use opentune_core::protocol::diag::{is_response_pending, DiagTransport};
use opentune_core::protocol::readonly;
use opentune_core::{ConnectConfig, DiagLayer, Error, Protocol, Result};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// The only K-line rate the sources report the cable running ISO 14230 at
/// (Mackanized `PROTOCOL.md`, roffe `RE_NOTES.md`: "10400 baud only"; bisak
/// §5: ISO 9141 opened at another rate reads back `DATA_RATE` 10400).
pub const KLINE_BAUD: u32 = 10_400;
/// Default tester (diagnostic source) address, as in the `kkl` driver.
pub const DEFAULT_TESTER: u8 = 0xF1;
/// Default per-message transmit budget on K-line: what the vendor DLL hands
/// the cable for every non-CAN channel when the caller gives none (bisak §4).
/// A full 260-byte frame at 10400 baud takes about 250 ms.
const DEFAULT_KLINE_TX_TIMEOUT_MS: u32 = 1000;
/// Reply wait for `aty`: ~108 ms measured, after up to W5 (300 ms) of quiet.
const FAST_INIT_TIMEOUT_MS: u64 = 3000;
/// Reply wait for `atw`: ~2.45 s measured, ~3.5 s with W1 = 1000 ms.
const FIVE_BAUD_TIMEOUT_MS: u64 = 8000;
/// ResponsePending (0x78) answers waited through per request, as in `kkl`.
const MAX_PENDING: usize = 40;

/// `Err` (nothing sent) unless `req` is a K-line read-path service: the
/// shared read-only list ([`opentune_core::protocol::readonly`]) narrowed for
/// KWP2000 K-line (no upload trio, nothing in the manufacturer range) plus
/// Start/StopCommunication, and never a programming session.
pub fn refuse_non_read(req: &[u8]) -> Result<()> {
    match req {
        [] => Err(Error::Protocol("empty K-line request".into())),
        [0x10, s, ..] if readonly::KLINE_REFUSED_SESSIONS.contains(s) => {
            Err(Error::Unsupported(format!(
                "openport2 K-line is read-only: startDiagnosticSession 0x{s:02X} (programming) \
                 was refused; nothing was sent"
            )))
        }
        [sid, ..] if readonly::kline_read_allowed(*sid) => Ok(()),
        [sid, ..] => Err(Error::Unsupported(format!(
            "openport2 K-line is read-only: service 0x{sid:02X} is not a read service and \
             was refused; nothing was sent"
        ))),
    }
}

/// How the bus is woken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KInit {
    /// ISO 14230 fast init: wake-up pattern + StartCommunication (`aty`).
    Fast,
    /// ISO 9141 / ISO 14230 five-baud init to an address byte (`atw`).
    FiveBaud,
    /// No wake-up (the caller handles it, or the ECU needs none).
    None,
}

/// One timing option: option key, J2534 parameter, whether the parameter
/// counts 0.5 ms (P-times) instead of 1 ms, the accepted range in ms, and the
/// ISO 14230 / J2534-1 default (what the cable uses when it is not set).
struct Timing {
    key: &'static str,
    param: u32,
    half_ms: bool,
    min_ms: u32,
    max_ms: u32,
    default_ms: u32,
}

/// The K-line timing the cable accepts (bisak §8.1). Ranges follow ISO
/// 14230-2's limits, widened where a slow ECU needs room (W1 to 1 s was
/// measured to work); out-of-range values are refused.
const TIMINGS: &[Timing] = &[
    Timing {
        key: "p1_max_ms",
        param: param::P1_MAX,
        half_ms: true,
        min_ms: 1,
        max_ms: 50,
        default_ms: 20,
    },
    Timing {
        key: "p3_min_ms",
        param: param::P3_MIN,
        half_ms: true,
        min_ms: 0,
        max_ms: 5000,
        default_ms: 55,
    },
    Timing {
        key: "p4_min_ms",
        param: param::P4_MIN,
        half_ms: true,
        min_ms: 0,
        max_ms: 20,
        default_ms: 5,
    },
    Timing {
        key: "w0_ms",
        param: param::W0,
        half_ms: false,
        min_ms: 2,
        max_ms: 1000,
        default_ms: 300,
    },
    Timing {
        key: "w1_ms",
        param: param::W1,
        half_ms: false,
        min_ms: 20,
        max_ms: 1000,
        default_ms: 300,
    },
    Timing {
        key: "w2_ms",
        param: param::W2,
        half_ms: false,
        min_ms: 1,
        max_ms: 1000,
        default_ms: 20,
    },
    Timing {
        key: "w3_ms",
        param: param::W3,
        half_ms: false,
        min_ms: 0,
        max_ms: 1000,
        default_ms: 20,
    },
    Timing {
        key: "w4_ms",
        param: param::W4,
        half_ms: false,
        min_ms: 1,
        max_ms: 1000,
        default_ms: 50,
    },
    Timing {
        key: "w5_ms",
        param: param::W5,
        half_ms: false,
        min_ms: 0,
        max_ms: 10_000,
        default_ms: 300,
    },
    Timing {
        key: "tidle_ms",
        param: param::TIDLE,
        half_ms: false,
        min_ms: 0,
        max_ms: 10_000,
        default_ms: 300,
    },
    Timing {
        key: "tinil_ms",
        param: param::TINIL,
        half_ms: false,
        min_ms: 1,
        max_ms: 100,
        default_ms: 25,
    },
    Timing {
        key: "twup_ms",
        param: param::TWUP,
        half_ms: false,
        min_ms: 2,
        max_ms: 200,
        default_ms: 50,
    },
];

/// Every option key the K-line link reads, for the docs and tests.
pub fn timing_keys() -> impl Iterator<Item = &'static str> {
    TIMINGS.iter().map(|t| t.key)
}

/// K-line settings resolved from a [`ConnectConfig`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KLineSettings {
    /// Channel / J2534 protocol id: 4 (ISO 14230) or 3 (ISO 9141).
    pub proto: u8,
    /// ECU address in the KWP2000 header.
    pub target: u8,
    /// Tester address in the KWP2000 header.
    pub tester: u8,
    pub init: KInit,
    /// Address byte of a five-baud init (defaults to `target`).
    pub init_addr: u8,
    /// `true`: `ISO9141_NO_CHECKSUM`, the host appends and checks the
    /// checksum. `false`: the cable does (J2534 default).
    pub host_checksum: bool,
    pub tx_timeout_ms: u32,
    pub layer: DiagLayer,
    /// `(param, value)` pairs written with `ats` before the init, in J2534
    /// units.
    pub config: Vec<(u32, u32)>,
}

/// Parse a byte given in hex, `0x` optional (`F1`, `0xF1`), as `--tx-id` is.
fn parse_hex_byte(key: &str, s: &str) -> Result<u8> {
    let t = s.trim();
    let h = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    u8::from_str_radix(h, 16).map_err(|_| {
        Error::Config(format!(
            "openport2: {key} '{s}' must be one byte in hex (e.g. F1 or 0x33)"
        ))
    })
}

/// The K-line channel the config selects, or `None` for the CAN path.
///
/// K-line is chosen by `cfg.protocol` (`Iso14230Kline` → 4, `Iso9141` → 3,
/// as an ECU definition sets it) or by the `bus` option (`kline`, `k`,
/// `iso14230` → 4; `iso9141` → 3; the CLI's `--bus`). Any other `bus` value
/// (or none) keeps the CAN path exactly as before; `bus=can` with a K-line
/// protocol is a contradiction and refused.
pub fn selected_channel(cfg: &ConnectConfig) -> Result<Option<u8>> {
    let from_protocol = match cfg.protocol {
        Protocol::Iso14230Kline => Some(PROTO_ISO14230),
        Protocol::Iso9141 => Some(PROTO_ISO9141),
        _ => None,
    };
    let bus = cfg
        .options
        .get("bus")
        .map(|s| s.trim().to_ascii_lowercase());
    let from_bus = match bus.as_deref() {
        Some("kline" | "k" | "k-line" | "iso14230") => Some(PROTO_ISO14230),
        Some("iso9141") => Some(PROTO_ISO9141),
        _ => None,
    };
    match (from_protocol, from_bus) {
        (Some(p), Some(b)) if p != b => Err(Error::Config(format!(
            "openport2: bus '{}' contradicts the connection's protocol {:?}",
            bus.unwrap_or_default(),
            cfg.protocol
        ))),
        (Some(_), None) if bus.as_deref() == Some("can") => Err(Error::Config(format!(
            "openport2: bus 'can' contradicts the connection's protocol {:?}",
            cfg.protocol
        ))),
        (p, b) => Ok(p.or(b)),
    }
}

impl KLineSettings {
    /// Resolve and validate the K-line settings for channel `proto`.
    pub fn from_cfg(cfg: &ConnectConfig, proto: u8) -> Result<Self> {
        let o = &cfg.options;
        if cfg.tx_id > 0xFF {
            return Err(Error::Config(format!(
                "openport2: K-line needs the ECU address (one byte) in tx_id, got 0x{:X} \
                 (CLI: --tx-id <addr> --rx-id F1, e.g. --tx-id 01)",
                cfg.tx_id
            )));
        }
        let target = cfg.tx_id as u8;
        let tester = match o.get("tester") {
            Some(s) => parse_hex_byte("tester", s)?,
            None => DEFAULT_TESTER,
        };
        let init_addr = match o.get("init_addr") {
            Some(s) => parse_hex_byte("init_addr", s)?,
            None => target,
        };

        // Baud: `baud`, else a K-line-range `bitrate`, else 10400 (as kkl).
        let baud = match o.get("baud") {
            Some(s) => s
                .trim()
                .parse::<u32>()
                .map_err(|_| Error::Config(format!("openport2: baud '{s}' is not a number")))?,
            None if (1_200..=125_000).contains(&cfg.bitrate) => cfg.bitrate,
            None => KLINE_BAUD,
        };
        if baud != KLINE_BAUD {
            return Err(Error::Config(format!(
                "openport2: K-line baud {baud} not supported: the cable runs ISO 14230 at \
                 {KLINE_BAUD} baud only"
            )));
        }

        let init = match o.get("init").map(|s| s.trim()) {
            None if proto == PROTO_ISO9141 => KInit::FiveBaud,
            None | Some("fast") => KInit::Fast,
            Some("5baud" | "slow" | "five-baud") => KInit::FiveBaud,
            Some("none") => KInit::None,
            Some(other) => {
                return Err(Error::Config(format!(
                    "openport2: unknown init '{other}' (expected fast, 5baud or none)"
                )))
            }
        };
        if init == KInit::Fast && proto == PROTO_ISO9141 {
            return Err(Error::Config(
                "openport2: ISO 9141 has no fast init; use init=5baud (or bus=kline for \
                 ISO 14230)"
                    .into(),
            ));
        }

        let host_checksum = match o.get("checksum").map(|s| s.trim()) {
            None | Some("cable") => false,
            Some("host") => true,
            Some(other) => {
                return Err(Error::Config(format!(
                    "openport2: checksum '{other}' (expected cable or host)"
                )))
            }
        };

        let tx_timeout_ms = match o.get("tx_timeout_ms") {
            Some(s) => s
                .trim()
                .parse::<u32>()
                .ok()
                .filter(|v| (1..=5000).contains(v))
                .ok_or_else(|| {
                    Error::Config(format!("openport2: tx_timeout_ms '{s}' must be 1..=5000"))
                })?,
            None => DEFAULT_KLINE_TX_TIMEOUT_MS,
        };

        // Layer: `layer`, else the definition's (when the protocol named
        // K-line), else KWP2000 (the CLI leaves the UDS default in place).
        let layer = match o.get("layer").map(|s| s.trim().to_ascii_lowercase()) {
            Some(l) if l == "kwp2000" || l == "kwp" => DiagLayer::Kwp2000,
            Some(l) if l == "uds" => DiagLayer::Uds,
            Some(l) => {
                return Err(Error::Config(format!(
                    "openport2: layer '{l}' (expected kwp2000 or uds)"
                )))
            }
            None if matches!(cfg.protocol, Protocol::Iso14230Kline | Protocol::Iso9141) => {
                cfg.diag_layer
            }
            None => DiagLayer::Kwp2000,
        };

        let mut config = Vec::new();
        let mut ms = |t: &Timing| -> Result<u32> {
            match o.get(t.key) {
                None => Ok(t.default_ms),
                Some(s) => s
                    .trim()
                    .parse::<u32>()
                    .ok()
                    .filter(|v| (t.min_ms..=t.max_ms).contains(v))
                    .inspect(|&v| {
                        config.push((t.param, if t.half_ms { v * 2 } else { v }));
                    })
                    .ok_or_else(|| {
                        Error::Config(format!(
                            "openport2: {} '{s}' must be a whole number of ms in {}..={}",
                            t.key, t.min_ms, t.max_ms
                        ))
                    }),
            }
        };
        let mut values = Vec::with_capacity(TIMINGS.len());
        for t in TIMINGS {
            values.push((t.key, ms(t)?));
        }
        let get = |k: &str| values.iter().find(|(key, _)| *key == k).map(|(_, v)| *v);
        if let (Some(tinil), Some(twup)) = (get("tinil_ms"), get("twup_ms")) {
            if twup <= tinil {
                return Err(Error::Config(format!(
                    "openport2: twup_ms ({twup}) must be longer than tinil_ms ({tinil}): the \
                     wake-up pattern is TiniL low, then high until TWUP"
                )));
            }
        }
        if let Some(s) = o.get("five_baud_mod") {
            let v = s
                .trim()
                .parse::<u32>()
                .ok()
                .filter(|v| *v <= 3)
                .ok_or_else(|| {
                    Error::Config(format!(
                        "openport2: five_baud_mod '{s}' must be 0..=3 (J2534 FIVE_BAUD_MOD)"
                    ))
                })?;
            config.push((param::FIVE_BAUD_MOD, v));
        }

        Ok(Self {
            proto,
            target,
            tester,
            init,
            init_addr,
            host_checksum,
            tx_timeout_ms,
            layer,
            config,
        })
    }

    /// The `ato` connect flags.
    pub fn flags(&self) -> u32 {
        if self.host_checksum {
            ISO9141_NO_CHECKSUM
        } else {
            0
        }
    }
}

// --- KWP2000 framing (ISO 14230-2) ------------------------------------------

/// 8-bit additive checksum.
pub fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |a, &b| a.wrapping_add(b))
}

/// A KWP2000 request with physical addressing: `[fmt][target][source]
/// [len?][data…]`, plus the checksum when `with_checksum`. Payloads up to 63
/// bytes carry their length in the format byte, longer ones (to 255) in a
/// separate length byte.
pub fn build_frame(target: u8, source: u8, data: &[u8], with_checksum: bool) -> Result<Vec<u8>> {
    if data.is_empty() || data.len() > 255 {
        return Err(Error::Protocol(format!(
            "K-line message of {} bytes (1..=255)",
            data.len()
        )));
    }
    let mut f = Vec::with_capacity(data.len() + 5);
    if data.len() <= 0x3F {
        f.extend_from_slice(&[0x80 | data.len() as u8, target, source]);
    } else {
        f.extend_from_slice(&[0x80, target, source, data.len() as u8]);
    }
    f.extend_from_slice(data);
    if with_checksum {
        f.push(checksum(&f));
    }
    Ok(f)
}

/// A decoded KWP2000 frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KFrame {
    /// Target and source addresses, when the format byte carries them.
    pub target: Option<u8>,
    pub source: Option<u8>,
    /// Service id and data.
    pub payload: Vec<u8>,
}

/// Result of looking for one frame at the start of `buf`.
#[derive(Debug, PartialEq, Eq)]
pub enum Split {
    /// A frame, and how many bytes of `buf` it used.
    Frame(KFrame, usize),
    /// Not enough bytes yet.
    Need,
    /// Not a valid frame: the error and how many bytes to drop.
    Bad(String, usize),
}

/// Find one KWP2000 frame at the start of `buf`. With `checksum_required`
/// the checksum byte must follow and match. Without it (the cable checks and
/// may strip it), a following byte is taken as the checksum only when it
/// matches; the sources do not say whether a cable in that mode delivers it.
pub fn split_frame(buf: &[u8], checksum_required: bool) -> Split {
    let Some(&fmt) = buf.first() else {
        return Split::Need;
    };
    let has_addr = fmt & 0xC0 != 0;
    let mut hdr = 1 + if has_addr { 2 } else { 0 };
    if buf.len() < hdr {
        return Split::Need;
    }
    let mut len = (fmt & 0x3F) as usize;
    if len == 0 {
        let Some(&l) = buf.get(hdr) else {
            return Split::Need;
        };
        len = l as usize;
        hdr += 1;
        if len == 0 {
            return Split::Bad("K-line frame with no data".into(), hdr);
        }
    }
    let core = hdr + len;
    if buf.len() < core {
        return Split::Need;
    }
    let cs = checksum(&buf[..core]);
    let used = if checksum_required {
        let Some(&got) = buf.get(core) else {
            return Split::Need;
        };
        if got != cs {
            return Split::Bad(
                format!("K-line checksum mismatch: computed 0x{cs:02X}, frame carries 0x{got:02X}"),
                core + 1,
            );
        }
        core + 1
    } else if buf.get(core) == Some(&cs) {
        core + 1
    } else {
        core
    };
    Split::Frame(
        KFrame {
            target: has_addr.then(|| buf[1]),
            source: has_addr.then(|| buf[2]),
            payload: buf[hdr..core].to_vec(),
        },
        used,
    )
}

// --- Session side ------------------------------------------------------------

/// K-line receive state kept in the [`Session`].
#[derive(Debug)]
pub(crate) struct KLineState {
    pub settings: KLineSettings,
    /// Bytes of a message between its START marker and its END.
    cur: Option<Vec<u8>>,
    /// Messages as the cable delimited them (a frame may still be split
    /// over two, or two frames share one; [`Session::kline_next_frame`]
    /// copes with both).
    msgs: VecDeque<Vec<u8>>,
    /// Key bytes from the init (fast: StartCommunication; five-baud: KB1/KB2).
    pub key_bytes: Option<(u8, u8)>,
}

impl KLineState {
    pub fn new(settings: KLineSettings) -> Self {
        Self {
            settings,
            cur: None,
            msgs: VecDeque::new(),
            key_bytes: None,
        }
    }

    fn flush(&mut self) {
        if let Some(c) = self.cur.take() {
            if !c.is_empty() {
                self.msgs.push_back(c);
            }
        }
    }
}

impl Session {
    /// File a K-line record (the caller checked the channel). Echoes and
    /// transmit indications are dropped; START/END markers delimit a
    /// message; every other record is message data.
    pub(crate) fn accept_kline(&mut self, f: RawFrame) {
        let st = f.status;
        if st & (status::LOOPBACK | status::TX_INDICATION) != 0 {
            self.stats.indications += 1;
            return;
        }
        let Some(k) = self.kline.as_mut() else {
            self.stats.foreign_frames += 1;
            return;
        };
        if f.is_kline_marker() {
            if st & status::START != 0 {
                k.flush();
                k.cur = Some(Vec::new());
            }
            if st & status::END != 0 {
                k.flush();
            }
            return;
        }
        if st & status::START != 0 {
            k.flush();
            k.cur = Some(Vec::new());
        }
        match k.cur.as_mut() {
            Some(c) => c.extend_from_slice(&f.body),
            None if !f.body.is_empty() => k.msgs.push_back(f.body),
            None => {}
        }
        if st & status::END != 0 {
            k.flush();
        }
    }

    fn kline(&self) -> Result<&KLineState> {
        self.kline.as_ref().ok_or(Error::NotConnected)
    }

    /// Open the K-line channel, configure it, filter and wake the ECU.
    pub(crate) fn open_kline(&mut self) -> Result<()> {
        let k = self.kline()?.settings.clone();
        let body = proto::open_channel(k.proto, k.flags(), KLINE_BAUD);
        self.command(&body, &[], SETUP_TIMEOUT_MS)?;
        for &(p, v) in &k.config {
            self.command(&proto::set_config(k.proto, p, v), &[], CMD_TIMEOUT_MS)?;
        }
        // Pass-all: mask 00, pattern 00 (one byte each). Addressing is
        // checked on each frame instead.
        let (body, payload) = proto::filter(k.proto, proto::filter::PASS, 0, &[0], &[0], None)
            .ok_or_else(|| Error::Protocol("invalid filter".into()))?;
        self.command(&body, &payload, CMD_TIMEOUT_MS)?;
        let keys = match k.init {
            KInit::Fast => Some(self.kline_fast_init(&k)?),
            KInit::FiveBaud => Some(self.kline_five_baud(&k)?),
            KInit::None => None,
        };
        if let Some(st) = self.kline.as_mut() {
            st.key_bytes = keys;
            st.cur = None;
            st.msgs.clear();
        }
        Ok(())
    }

    /// Send an init command and wait for its result: `ary`/`arw` with or
    /// without the sequence number (bisak §3: the one `arw` seen on a cable
    /// answered an unnumbered `atw`), `are` with it. A bare `aro` means the
    /// cable acknowledged and no ECU answered.
    fn init_command(
        &mut self,
        body: &str,
        payload: &[u8],
        timeout_ms: u64,
        verb: u8,
    ) -> Result<proto::Reply> {
        let seq = self.next_seq();
        self.port.write_all(&proto::numbered(body, seq, payload))?;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        // `ary<ch> <n>` / `arw<ch> <kb1> <kb2>`, then the optional number.
        let bare = if verb == b'y' { 2 } else { 3 };
        loop {
            match self.read_msg(deadline)? {
                None => {
                    return Err(Error::Protocol(format!(
                        "OpenPort 2.0 did not answer `{}` within {timeout_ms} ms",
                        crate::cmd_name(body)
                    )))
                }
                Some(proto::Msg::Frame(f)) => self.accept_frame(f),
                Some(proto::Msg::Reply(r))
                    if r.verb == verb
                        && (r.fields.len() == bare
                            || (r.fields.len() == bare + 1 && r.seq() == Some(seq))) =>
                {
                    return Ok(r)
                }
                Some(proto::Msg::Reply(r)) if r.verb != b'i' && r.seq() == Some(seq) => {
                    return Err(match r.verb {
                        b'e' => device_error(body, &r),
                        b'o' => Error::Protocol(format!(
                            "K-line init `{}`: the cable acknowledged but no ECU answered",
                            crate::cmd_name(body)
                        )),
                        _ => Error::Protocol(format!(
                            "unexpected reply to `{}`: ar{} {}",
                            crate::cmd_name(body),
                            r.verb as char,
                            r.text
                        )),
                    });
                }
                Some(proto::Msg::Reply(_)) => self.stats.stale_replies += 1,
            }
        }
    }

    /// ISO 14230 fast init: the cable sends the wake-up pattern and our
    /// StartCommunication and returns the ECU's answer.
    fn kline_fast_init(&mut self, k: &KLineSettings) -> Result<(u8, u8)> {
        let req = build_frame(k.target, k.tester, &[0x81], k.host_checksum)?;
        let r = self.init_command(
            &proto::fast_init(k.proto, req.len()),
            &req,
            FAST_INIT_TIMEOUT_MS,
            b'y',
        )?;
        let frame = match split_frame(&r.data, k.host_checksum) {
            Split::Frame(f, _) => f,
            Split::Need => {
                return Err(Error::Protocol(format!(
                    "fast init: truncated StartCommunication answer {:02X?}",
                    r.data
                )))
            }
            Split::Bad(e, _) => return Err(Error::Protocol(format!("fast init: {e}"))),
        };
        match frame.payload.as_slice() {
            [0xC1, kb1, kb2, ..] => Ok((*kb1, *kb2)),
            [0x7F, 0x81, nrc, ..] => Err(Error::Protocol(format!(
                "ECU refused StartCommunication (NRC 0x{nrc:02X})"
            ))),
            other => Err(Error::Protocol(format!(
                "unexpected StartCommunication answer {other:02X?}"
            ))),
        }
    }

    /// Five-baud init: the cable shifts the address out at 5 bit/s, reads the
    /// 0x55 sync and the key bytes, sends the inverted KB2 and takes the
    /// inverted address back (per `FIVE_BAUD_MOD`), and reports KB1/KB2.
    fn kline_five_baud(&mut self, k: &KLineSettings) -> Result<(u8, u8)> {
        let r = self.init_command(
            &proto::five_baud_init(k.proto, k.init_addr),
            &[],
            FIVE_BAUD_TIMEOUT_MS,
            b'w',
        )?;
        let byte = |i: usize| -> Result<u8> {
            r.fields
                .get(i)
                .and_then(|s| s.parse::<u8>().ok())
                .ok_or_else(|| Error::Protocol(format!("bad five-baud result `arw{}`", r.text)))
        };
        Ok((byte(1)?, byte(2)?))
    }

    /// Transmit one already-framed K-line message.
    fn kline_transmit(&mut self, frame: &[u8]) -> Result<()> {
        let k = &self.kline()?.settings;
        let (proto, budget_ms) = (k.proto, k.tx_timeout_ms);
        let body = format!("att{proto} {} 0 {}", frame.len(), budget_ms * 1000);
        self.command(&body, frame, budget_ms as u64 + TX_REPLY_GRACE_MS)
            .map(|_| ())
    }

    /// Drop received messages nobody asked for (a late answer to an earlier,
    /// timed-out request must not be read as this one's).
    fn kline_clear_rx(&mut self) {
        if let Some(k) = self.kline.as_mut() {
            k.cur = None;
            k.msgs.clear();
        }
    }

    /// The next KWP2000 frame, or [`Error::Timeout`] after `timeout_ms`.
    fn kline_next_frame(&mut self, timeout_ms: u64) -> Result<KFrame> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if let Some(k) = self.kline.as_mut() {
                let host = k.settings.host_checksum;
                if let Some(front) = k.msgs.front() {
                    match split_frame(front, host) {
                        Split::Frame(f, used) => {
                            let rest = k.msgs.pop_front().unwrap_or_default()[used..].to_vec();
                            if !rest.is_empty() {
                                k.msgs.push_front(rest);
                            }
                            return Ok(f);
                        }
                        Split::Need if k.msgs.len() >= 2 => {
                            // A frame the cable split over two messages.
                            let a = k.msgs.pop_front().unwrap_or_default();
                            if let Some(b) = k.msgs.front_mut() {
                                let mut joined = a;
                                joined.extend_from_slice(b);
                                *b = joined;
                            }
                            continue;
                        }
                        Split::Need => {}
                        Split::Bad(e, n) => {
                            let front = k.msgs.pop_front().unwrap_or_default();
                            let rest = front.get(n..).unwrap_or_default().to_vec();
                            if !rest.is_empty() {
                                k.msgs.push_front(rest);
                            }
                            self.stats.garbage_bytes += n as u64;
                            return Err(Error::Protocol(e));
                        }
                    }
                }
            }
            match self.read_msg(deadline)? {
                None => return Err(Error::Timeout(timeout_ms)),
                Some(proto::Msg::Frame(f)) => self.accept_frame(f),
                Some(proto::Msg::Reply(_)) => self.stats.stale_replies += 1,
            }
        }
    }
}

/// [`DiagTransport`] over the OP2's K-line channel: KWP2000 framing, the
/// read-only allow-list and responsePending handling.
pub(crate) struct KLineLink {
    pub session: Rc<RefCell<Session>>,
}

impl DiagTransport for KLineLink {
    fn request(&mut self, req: &[u8], timeout_ms: u64) -> Result<Vec<u8>> {
        refuse_non_read(req)?;
        let mut s = self.session.borrow_mut();
        let k = s.kline()?.settings.clone();
        let frame = build_frame(k.target, k.tester, req, k.host_checksum)?;
        s.kline_clear_rx();
        s.kline_transmit(&frame)?;
        // Frames for another tester (a second tool on the bus) are skipped;
        // responsePending for this request is waited through, bounded.
        for _ in 0..MAX_PENDING {
            let f = s.kline_next_frame(timeout_ms)?;
            if f.target.is_some_and(|t| t != k.tester) {
                s.stats.foreign_frames += 1;
                continue;
            }
            if is_response_pending(req, &f.payload) {
                continue;
            }
            return Ok(f.payload);
        }
        Err(Error::Timeout(timeout_ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(opts: &[(&str, &str)]) -> ConnectConfig {
        let mut c = ConnectConfig {
            tx_id: 0x01,
            rx_id: 0xF1,
            ..ConnectConfig::default()
        };
        c.options.insert("bus".into(), "kline".into());
        for (k, v) in opts {
            c.options.insert((*k).into(), (*v).into());
        }
        c
    }

    #[test]
    fn frames_build_and_split_both_ways() {
        let f = build_frame(0x01, 0xF1, &[0x1A, 0x90], true).unwrap();
        // 0x82 + 0x01 + 0xF1 + 0x1A + 0x90 = 0x21E.
        assert_eq!(f, vec![0x82, 0x01, 0xF1, 0x1A, 0x90, 0x1E]);
        assert_eq!(
            build_frame(0x01, 0xF1, &[0x81], false).unwrap(),
            vec![0x81, 0x01, 0xF1, 0x81]
        );
        // Long payloads use the separate length byte.
        let long = vec![0x21; 70];
        let f = build_frame(0x10, 0xF1, &long, true).unwrap();
        assert_eq!(&f[..4], &[0x80, 0x10, 0xF1, 70]);
        let Split::Frame(k, used) = split_frame(&f, true) else {
            panic!()
        };
        assert_eq!((k.payload, used), (long, f.len()));
        assert!(build_frame(1, 0xF1, &[], true).is_err());
        assert!(build_frame(1, 0xF1, &[0; 256], true).is_err());

        // Host checksum: required and checked.
        let resp = [0x83, 0xF1, 0x01, 0xC1, 0xEF, 0x8F];
        let mut with_cs = resp.to_vec();
        with_cs.push(checksum(&resp));
        let Split::Frame(k, 7) = split_frame(&with_cs, true) else {
            panic!()
        };
        assert_eq!(
            (k.target, k.source, k.payload),
            (Some(0xF1), Some(0x01), vec![0xC1, 0xEF, 0x8F])
        );
        assert_eq!(split_frame(&resp, true), Split::Need);
        let mut bad = with_cs.clone();
        bad[6] ^= 1;
        assert!(matches!(split_frame(&bad, true), Split::Bad(_, 7)));
        // Cable checksum: optional; a non-matching trailing byte is the next
        // frame's.
        assert!(matches!(split_frame(&resp, false), Split::Frame(_, 6)));
        assert!(matches!(split_frame(&with_cs, false), Split::Frame(_, 7)));
        assert!(matches!(split_frame(&bad, false), Split::Frame(_, 6)));
        // No address bytes.
        assert!(matches!(
            split_frame(&[0x02, 0x7E, 0x00], false),
            Split::Frame(KFrame { target: None, .. }, 3)
        ));
        assert_eq!(split_frame(&[0x80, 0xF1], false), Split::Need);
    }

    #[test]
    fn the_allow_list_admits_reads_only() {
        for ok in [
            &[0x1A, 0x90][..],
            &[0x21, 0x01],
            &[0x23, 0, 0, 0, 4],
            &[0x3E, 0x01],
            &[0x10, 0x89],
            &[0x27, 0x01],
            &[0x81],
        ] {
            refuse_non_read(ok).unwrap();
        }
        for refused in [
            &[0x3B, 0x01, 0x02][..],
            &[0x34, 0, 0, 0, 0],
            &[0x36, 1],
            &[0x31, 0x01],
            &[0x2E, 0xF1, 0x90],
            &[0x14, 0xFF, 0x00],
            &[0x04],
            &[0x11, 0x01],
            &[0x10, 0x85],
            &[0x10, 0x02],
            &[0x3D, 0, 0, 0],
        ] {
            let e = refuse_non_read(refused).unwrap_err();
            assert!(matches!(e, Error::Unsupported(_)), "{refused:02X?}: {e}");
        }
        assert!(refuse_non_read(&[]).is_err());
    }

    #[test]
    fn channel_selection() {
        let mut c = ConnectConfig::default();
        assert_eq!(selected_channel(&c).unwrap(), None);
        for (v, want) in [
            ("kline", Some(4)),
            ("K", Some(4)),
            ("iso14230", Some(4)),
            ("iso9141", Some(3)),
            ("can", None),
            ("hs", None),
        ] {
            c.options.insert("bus".into(), v.into());
            assert_eq!(selected_channel(&c).unwrap(), want, "{v}");
        }
        c.options.clear();
        c.protocol = Protocol::Iso14230Kline;
        assert_eq!(selected_channel(&c).unwrap(), Some(4));
        c.protocol = Protocol::Iso9141;
        assert_eq!(selected_channel(&c).unwrap(), Some(3));
        c.options.insert("bus".into(), "kline".into());
        assert!(selected_channel(&c).is_err(), "9141 vs bus=kline");
        c.options.insert("bus".into(), "can".into());
        assert!(selected_channel(&c).is_err(), "9141 vs bus=can");
    }

    #[test]
    fn settings_defaults_and_validation() {
        let s = KLineSettings::from_cfg(&cfg(&[]), 4).unwrap();
        assert_eq!(
            (s.target, s.tester, s.init, s.init_addr, s.host_checksum),
            (0x01, 0xF1, KInit::Fast, 0x01, false)
        );
        assert_eq!(
            (s.tx_timeout_ms, s.layer, s.flags()),
            (1000, DiagLayer::Kwp2000, 0)
        );
        assert!(s.config.is_empty(), "nothing set, nothing sent");

        let s = KLineSettings::from_cfg(
            &cfg(&[
                ("init", "5baud"),
                ("tester", "0xF0"),
                ("init_addr", "33"),
                ("checksum", "host"),
                ("p1_max_ms", "10"),
                ("p3_min_ms", "60"),
                ("w1_ms", "1000"),
                ("tinil_ms", "25"),
                ("twup_ms", "50"),
                ("five_baud_mod", "3"),
                ("layer", "uds"),
                ("baud", "10400"),
            ]),
            4,
        )
        .unwrap();
        assert_eq!(
            (s.init, s.tester, s.init_addr, s.flags(), s.layer),
            (
                KInit::FiveBaud,
                0xF0,
                0x33,
                ISO9141_NO_CHECKSUM,
                DiagLayer::Uds
            )
        );
        assert_eq!(
            s.config,
            vec![
                (param::P1_MAX, 20),
                (param::P3_MIN, 120),
                (param::W1, 1000),
                (param::TINIL, 25),
                (param::TWUP, 50),
                (param::FIVE_BAUD_MOD, 3),
            ]
        );

        // ISO 9141 defaults to five-baud and refuses fast init.
        let s = KLineSettings::from_cfg(&cfg(&[]), 3).unwrap();
        assert_eq!(s.init, KInit::FiveBaud);
        assert!(KLineSettings::from_cfg(&cfg(&[("init", "fast")]), 3).is_err());

        // An ECU definition's layer is kept; the CLI's UDS default is not.
        let mut c = cfg(&[]);
        c.options.remove("bus");
        c.protocol = Protocol::Iso14230Kline;
        c.diag_layer = DiagLayer::Uds;
        assert_eq!(
            KLineSettings::from_cfg(&c, 4).unwrap().layer,
            DiagLayer::Uds
        );

        for bad in [
            ("baud", "9600"),
            ("baud", "fast"),
            ("init", "turbo"),
            ("tester", "F1F"),
            ("init_addr", "zz"),
            ("checksum", "none"),
            ("tx_timeout_ms", "0"),
            ("p1_max_ms", "0"),
            ("p1_max_ms", "51"),
            ("p4_min_ms", "21"),
            ("w1_ms", "5000"),
            ("tinil_ms", "abc"),
            ("five_baud_mod", "4"),
            ("layer", "j1850"),
        ] {
            let e = KLineSettings::from_cfg(&cfg(&[bad]), 4).unwrap_err();
            assert!(matches!(e, Error::Config(_)), "{bad:?}: {e}");
        }
        let e = KLineSettings::from_cfg(&cfg(&[("tinil_ms", "60"), ("twup_ms", "50")]), 4);
        assert!(e.is_err(), "TWUP must exceed TiniL");
        let mut c = cfg(&[]);
        c.tx_id = 0x7E0;
        let e = KLineSettings::from_cfg(&c, 4).unwrap_err().to_string();
        assert!(e.contains("--tx-id"), "{e}");
        assert_eq!(timing_keys().count(), TIMINGS.len());
    }
}
