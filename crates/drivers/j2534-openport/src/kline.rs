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
//! Request header (ISO 14230-2 format byte, address mode bits A1A0): the
//! target is the `target` option, else the five-baud address after a
//! five-baud init, else `tx_id`; the mode is functional (A1A0 = 11, `0xC0 |
//! len`) when that target is the OBD functional address 0x33 and physical
//! (A1A0 = 10, `0x80 | len`) otherwise, unless `addressing=` says otherwise
//! (ISO 14230-4:2000 §4.2: functional requests to 33H, physical responses
//! to the request's source). The key bytes the init returns then decide the
//! link ([`decide_link`]): KB2 0x8F is KWP2000 and KB1 picks the header
//! form, a form KB1 rules out is refused, not guessed; key bytes 08 08 /
//! 94 94 mean the ECU woke as an ISO 9141-2 OBD participant, which gets OBD
//! requests (`68 6A F1 …`) only and refuses KWP2000; other key bytes are
//! refused unless `target`/`addressing` force a form. The fast-init
//! StartCommunication itself stays physical to `tx_id`. An optional `session=<hex>` sends startDiagnosticSession right
//! after the init, through the same read-only guard.
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
/// Reply wait for the `session=` startDiagnosticSession: the core KWP2000
/// client's default (≥ P2max).
const SESSION_TIMEOUT_MS: u64 = 2000;
/// ISO 14230-4 OBD functional target address (ISO 14230-4:2000 §4.2: "Functional
/// address 33H shall be used as target address for messages sent to the
/// vehicle"; §4.4: the five-baud address is 33H).
pub const OBD_FUNCTIONAL_ADDR: u8 = 0x33;
/// KB2 of every ISO 14230 keyword (2000–2031 = KB2 0x8F with KB1 0x50..0x6F,
/// parity aside): freediag `diag_l2_iso14230.c` ("ISO14230 uses KB2 of
/// 0x8F"); the EU tachograph rules' StartCommunication response (Reg. (EU)
/// 2016/799 Annex IC App. 8, Table 6: KB1 EA, KB2 8F).
pub const KB2_ISO14230: u8 = 0x8F;

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

/// Address mode of a request header: ISO 14230-2 format byte bits A1A0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Addressing {
    /// A1A0 = 10: with address information, physical addressing (`0x80`).
    Physical,
    /// A1A0 = 11: with address information, functional addressing (`0xC0`).
    Functional,
    /// A1A0 = 00: no address information, the 1-byte header (`0x00`).
    NoAddress,
}

impl Addressing {
    /// The A1A0 bits in place in the format byte.
    pub fn bits(self) -> u8 {
        match self {
            Addressing::Physical => 0x80,
            Addressing::Functional => 0xC0,
            Addressing::NoAddress => 0x00,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Addressing::Physical => "physical",
            Addressing::Functional => "functional",
            Addressing::NoAddress => "no address",
        }
    }
}

/// K-line settings resolved from a [`ConnectConfig`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KLineSettings {
    /// Channel / J2534 protocol id: 4 (ISO 14230) or 3 (ISO 9141).
    pub proto: u8,
    /// The ECU's physical address (`tx_id`): the fast-init
    /// StartCommunication goes to it.
    pub ecu: u8,
    /// Target address of every request header after the init: `target`,
    /// else the five-baud address after a five-baud init, else `ecu`.
    pub target: u8,
    /// Requested address mode: `addressing`, else functional for target
    /// 0x33 and physical otherwise. The key bytes may still refuse it.
    pub addressing: Addressing,
    /// Whether `target` or `addressing` was given explicitly (then the key
    /// bytes may refuse the form but never replace it).
    pub header_explicit: bool,
    /// startDiagnosticSession sent right after the init (`session`).
    pub session: Option<u8>,
    /// Tester address in the KWP2000 header.
    pub tester: u8,
    pub init: KInit,
    /// Address byte of a five-baud init (defaults to `target`).
    pub init_addr: u8,
    /// `true` (the default): `ISO9141_NO_CHECKSUM`, the host appends and
    /// checks the checksum; on a real cable the sum byte was seen on the
    /// wire in this mode. `false` (`checksum=cable`): the cable does (the
    /// J2534 default), not yet confirmed on hardware.
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
        let ecu = cfg.tx_id as u8;
        let tester = match o.get("tester") {
            Some(s) => parse_hex_byte("tester", s)?,
            None => DEFAULT_TESTER,
        };
        let init_addr = match o.get("init_addr") {
            Some(s) => parse_hex_byte("init_addr", s)?,
            None => ecu,
        };
        let target_opt = o
            .get("target")
            .map(|s| parse_hex_byte("target", s))
            .transpose()?;
        let addressing_opt = match o.get("addressing").map(|s| s.trim().to_ascii_lowercase()) {
            None => None,
            Some(a) if a == "physical" || a == "phys" => Some(Addressing::Physical),
            Some(a) if a == "functional" || a == "func" => Some(Addressing::Functional),
            Some(a) if a == "none" => Some(Addressing::NoAddress),
            Some(a) => {
                return Err(Error::Config(format!(
                    "openport2: addressing '{a}' (expected physical, functional or none)"
                )))
            }
        };
        if addressing_opt == Some(Addressing::NoAddress) && target_opt.is_some() {
            return Err(Error::Config(
                "openport2: target= contradicts addressing=none (a header without address \
                 information has no target byte)"
                    .into(),
            ));
        }
        let session = match o.get("session") {
            None => None,
            Some(s) => {
                let v = parse_hex_byte("session", s)?;
                // The shared read-only guard decides, before the port is
                // touched: programming sessions are refused.
                refuse_non_read(&[0x10, v])?;
                Some(v)
            }
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

        // Host framing by default: proven on a cable (the echo carries the
        // appended sum); cable-checksum mode is not yet confirmed there.
        let host_checksum = match o.get("checksum").map(|s| s.trim()) {
            None | Some("host") => true,
            Some("cable") => false,
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

        // Request target: explicit, else the five-baud address after a
        // five-baud init (an ECU woken at the OBD functional address 0x33
        // expects functional requests to 0x33), else the physical address.
        let target = match (target_opt, init) {
            (Some(t), _) => t,
            (None, KInit::FiveBaud) => init_addr,
            (None, _) => ecu,
        };
        let addressing = addressing_opt.unwrap_or(if target == OBD_FUNCTIONAL_ADDR {
            Addressing::Functional
        } else {
            Addressing::Physical
        });

        Ok(Self {
            proto,
            ecu,
            target,
            addressing,
            header_explicit: target_opt.is_some() || addressing_opt.is_some(),
            session,
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
/// separate length byte. (The fast-init StartCommunication uses this.)
pub fn build_frame(target: u8, source: u8, data: &[u8], with_checksum: bool) -> Result<Vec<u8>> {
    HeaderForm::unchecked(Addressing::Physical, target, source).build(data, with_checksum)
}

/// The request header form used after the init (ISO 14230-2 format byte:
/// A1A0 address mode, L5..L0 length; optional target/source and length
/// bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeaderForm {
    pub addressing: Addressing,
    /// Target and source bytes (unused without address information).
    pub target: u8,
    pub source: u8,
    /// The length may go in the format byte (KB1 AL0).
    pub len_in_fmt: bool,
    /// A separate length byte may follow the header (KB1 AL1).
    pub len_byte: bool,
    /// KB1 bits TP1 TP0 as received (`None` without key bytes); reported
    /// only, the P-times are not changed from them.
    pub timing_bits: Option<u8>,
    /// Forced by `target`/`addressing` over key bytes that are not ISO
    /// 14230 ones (unchecked).
    pub forced: bool,
}

impl HeaderForm {
    /// A form not checked against key bytes (no init, or the fast-init
    /// StartCommunication): length in the format byte up to 63, a length
    /// byte above, as before.
    pub fn unchecked(addressing: Addressing, target: u8, source: u8) -> Self {
        Self {
            addressing,
            target,
            source,
            len_in_fmt: true,
            len_byte: true,
            timing_bits: None,
            forced: false,
        }
    }

    /// Frame `data` (service id + data), plus the checksum when
    /// `with_checksum`. The length goes in the format byte when it fits (1 to
    /// 63) and KB1 allows it, else in a length byte when KB1 allows that;
    /// otherwise the message is refused unsent.
    pub fn build(&self, data: &[u8], with_checksum: bool) -> Result<Vec<u8>> {
        let n = data.len();
        if n == 0 || n > 255 {
            return Err(Error::Protocol(format!(
                "K-line message of {n} bytes (1..=255)"
            )));
        }
        let in_fmt = n <= 0x3F && self.len_in_fmt;
        if !in_fmt && !self.len_byte {
            return Err(Error::Unsupported(format!(
                "K-line: a {n}-byte message needs the additional length byte, which the \
                 ECU's key byte KB1 does not announce (AL1 = 0); nothing was sent"
            )));
        }
        let a = self.addressing.bits();
        let mut f = Vec::with_capacity(n + 5);
        f.push(if in_fmt { a | n as u8 } else { a });
        if self.addressing != Addressing::NoAddress {
            f.extend_from_slice(&[self.target, self.source]);
        }
        if !in_fmt {
            f.push(n as u8);
        }
        f.extend_from_slice(data);
        if with_checksum {
            f.push(checksum(&f));
        }
        Ok(f)
    }

    /// One line for `info()`: e.g. `header C0 33 F1 (functional), length in
    /// format byte, normal timing`.
    pub fn describe(&self) -> String {
        let a = self.addressing.bits();
        let fmt = if self.len_in_fmt {
            format!("{:X}n", a >> 4)
        } else {
            format!("{a:02X}")
        };
        let head = match self.addressing {
            Addressing::NoAddress => fmt,
            _ => format!("{fmt} {:02X} {:02X}", self.target, self.source),
        };
        let len = match (self.len_in_fmt, self.len_byte) {
            (true, true) => "length in format byte (length byte above 63)",
            (true, false) => "length in format byte",
            _ => "length byte",
        };
        let timing = match self.timing_bits {
            None => String::new(),
            Some(0b10) => ", normal timing".into(),
            Some(t) => format!(", KB1 TP1 TP0 = {t:02b} (not the normal set; P-times unchanged)"),
        };
        let forced = if self.forced {
            ", forced by target/addressing (key bytes not ISO 14230: unchecked)"
        } else {
            ""
        };
        format!(
            "header {head} ({}), {len}{timing}{forced}",
            self.addressing.name()
        )
    }
}

/// What the link speaks after the init, decided from the key bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkMode {
    /// KWP2000 (ISO 14230) in this request header form.
    Kwp(HeaderForm),
    /// The ECU woke as a legislated ISO 9141-2 OBD participant (key bytes
    /// 08 08 or 94 94): OBD requests `68 6A <tester> <mode> … <cs>`, answers
    /// `48 6B <ecu> <mode + 40> … <cs>`; every KWP2000 service is refused.
    Iso9141Obd { tester: u8 },
}

impl LinkMode {
    /// One line for `info()`; `obd_source` is the ECU address learned from
    /// the first ISO 9141-2 answer.
    pub fn describe(&self, obd_source: Option<u8>) -> String {
        match self {
            LinkMode::Kwp(h) => h.describe(),
            LinkMode::Iso9141Obd { tester } => format!(
                "ISO 9141-2 OBD: header 68 6A {tester:02X}, KWP2000 refused{}",
                obd_source
                    .map(|s| format!(", ECU source address {s:02X}"))
                    .unwrap_or_default()
            ),
        }
    }
}

/// ISO 9141-2 / SAE J1979 OBD request header bytes 1–2 and response header
/// bytes 1–2 (freediag `diag_l2_iso9141.c`: `0x68`, `0x6A` "defined by spec",
/// then the tester address; answers checked for `0x48 0x6B`, `data[2]` =
/// "Originating ECU").
pub const ISO9141_REQ_HEADER: [u8; 2] = [0x68, 0x6A];
pub const ISO9141_RESP_HEADER: [u8; 2] = [0x48, 0x6B];
/// ISO 9141-2 / J1979 limits a message to 7 data bytes.
const ISO9141_MAX_DATA: usize = 7;

/// The two ISO 9141-2 key byte pairs (freediag `diag_l2_iso9141.c`: "these
/// can be 0x08 0x08 or 0x94 0x94").
pub fn is_iso9141_2_keywords(kb1: u8, kb2: u8) -> bool {
    kb1 == kb2 && (kb1 == 0x08 || kb1 == 0x94)
}

/// Decide what the link speaks from the settings and the key bytes the init
/// returned (`None` with `init=none`: the requested KWP2000 form, unchecked).
///
/// Keyword decoding first:
/// - KB2 0x8F: ISO 14230 (KWP2000); KB1 decides the header form (below).
///   KB2 is 0x8F for every ISO 14230 keyword (freediag `diag_l2_iso14230.c`;
///   Reg. (EU) 2016/799 Annex IC App. 8 Table 6).
/// - 08 08 or 94 94: ISO 9141-2 (freediag `diag_l2_iso9141.c`): the ECU woke
///   as a legislated OBD participant, [`LinkMode::Iso9141Obd`].
/// - Anything else (e.g. KW1281's 01 8A): no header is guessed; refused.
///
/// An explicit `target`/`addressing` still wins over the last two: the
/// forced KWP2000 form is used unchecked (KB1 is not an ISO 14230 KB1 then).
///
/// ISO 14230-2 KB1, as public sources give it:
/// - KB1, parity bit 7 dropped (freediag reads 7O1 key bytes "and ignore[s]
///   parity"): bit 0 AL0 = length in the format byte supported, bit 1 AL1 =
///   additional length byte supported, bit 2 HB0 = 1-byte header (no address
///   information) supported, bit 3 HB1 = header with target/source address
///   supported (freediag `dl2p_14230_startcomms`, "iso14230 5.2.4.1 & Table
///   8"; cross-checked by ISO 14230-4:2000 §4.4: keyword 2025 = KB1 0x69 =
///   "3 byte header, no additional length byte, normal timing").
/// - KB1 bits 5..4 (TP1 TP0) = `10` is the normal timing set (keyword 2025,
///   same clause). Other values are reported, not acted on.
///
/// The requested address mode is kept when KB1 allows it. A *defaulted*
/// address header becomes the 1-byte header when KB1 announces only that
/// (HB1 = 0, HB0 = 1); an *explicit* `target`/`addressing` is never replaced.
/// Everything KB1 rules out is refused.
pub fn decide_link(s: &KLineSettings, key_bytes: Option<(u8, u8)>) -> Result<LinkMode> {
    let mut form = HeaderForm::unchecked(s.addressing, s.target, s.tester);
    let Some((kb1, kb2)) = key_bytes else {
        return Ok(LinkMode::Kwp(form));
    };
    if kb2 != KB2_ISO14230 {
        if s.header_explicit {
            form.forced = true;
            return Ok(LinkMode::Kwp(form));
        }
        if is_iso9141_2_keywords(kb1, kb2) {
            return Ok(LinkMode::Iso9141Obd { tester: s.tester });
        }
        return Err(Error::Unsupported(format!(
            "K-line keybytes {kb1:02X} {kb2:02X} are neither ISO 14230 (KWP2000: KB2 8F) nor \
             ISO 9141-2 (08 08 / 94 94) key bytes, so no header form is guessed; force one with \
             target=/addressing= if you know it; nothing was sent after the init"
        )));
    }
    let kb = kb1 & 0x7F;
    let bit = |n: u8| kb & (1 << n) != 0;
    let (al0, al1, hb0, hb1) = (bit(0), bit(1), bit(2), bit(3));
    form.len_in_fmt = al0;
    form.len_byte = al1;
    form.timing_bits = Some((kb >> 4) & 0x03);
    let refuse = |why: String| {
        Err(Error::Unsupported(format!(
            "K-line keybytes {kb1:02X} {kb2:02X}: {why}; nothing was sent after the init"
        )))
    };
    if !al0 && !al1 {
        return refuse(
            "KB1 announces no length form (AL0 = AL1 = 0), so no request can be framed".into(),
        );
    }
    match s.addressing {
        Addressing::NoAddress if !hb0 => {
            return refuse(
                "addressing=none needs the 1-byte header, which KB1 does not announce \
                 (HB0 = 0)"
                    .into(),
            )
        }
        Addressing::NoAddress => {}
        a if hb1 => form.addressing = a,
        _ if hb0 && !s.header_explicit => form.addressing = Addressing::NoAddress,
        a => {
            return refuse(format!(
                "KB1 announces no header with address information (HB1 = 0), so the {} \
                 header to {:02X} is refused{}",
                a.name(),
                s.target,
                if hb0 {
                    " (KB1 allows the 1-byte header: addressing=none)"
                } else {
                    ""
                }
            ))
        }
    }
    Ok(LinkMode::Kwp(form))
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
    /// What the link speaks, decided after the init ([`decide_link`]).
    pub mode: Option<LinkMode>,
    /// ISO 9141-2 mode: the ECU address from the first `48 6B <src>` answer.
    pub obd_source: Option<u8>,
}

impl KLineState {
    pub fn new(settings: KLineSettings) -> Self {
        Self {
            settings,
            cur: None,
            msgs: VecDeque::new(),
            key_bytes: None,
            mode: None,
            obd_source: None,
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
        let mode = decide_link(&k, keys)?;
        log::debug!(
            target: "openport2",
            "K-line {}",
            mode.describe(None)
        );
        if let Some(st) = self.kline.as_mut() {
            st.key_bytes = keys;
            st.mode = Some(mode);
            st.obd_source = None;
            st.cur = None;
            st.msgs.clear();
        }
        if let Some(sess) = k.session {
            self.kline_start_session(sess)?;
        }
        Ok(())
    }

    /// `session=`: startDiagnosticSession right after the init, before any
    /// other request, through the same read-only guard as every request.
    fn kline_start_session(&mut self, sess: u8) -> Result<()> {
        let resp = self
            .kline_request(&[0x10, sess], SESSION_TIMEOUT_MS)
            .map_err(|e| match e {
                Error::Timeout(ms) => Error::Protocol(format!(
                    "K-line startDiagnosticSession 0x{sess:02X}: no answer within {ms} ms"
                )),
                other => other,
            })?;
        match resp.as_slice() {
            [0x50, ..] => Ok(()),
            [0x7F, 0x10, nrc, ..] => Err(Error::Protocol(format!(
                "ECU refused startDiagnosticSession 0x{sess:02X} (NRC 0x{nrc:02X})"
            ))),
            other => Err(Error::Protocol(format!(
                "unexpected startDiagnosticSession answer {other:02X?}"
            ))),
        }
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
        // Physical, to the ECU's own address, whatever `target` says.
        let req = build_frame(k.ecu, k.tester, &[0x81], k.host_checksum)?;
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

    /// One KWP2000 request/answer: the read-only guard first (nothing sent
    /// on refusal), then the decided header form. Answers in any header form
    /// are accepted (address mode 10 or 11 with target/source, 00 without,
    /// length in the format byte or a length byte: [`split_frame`]); frames
    /// for another tester are skipped; responsePending is waited through.
    fn kline_request(&mut self, req: &[u8], timeout_ms: u64) -> Result<Vec<u8>> {
        refuse_non_read(req)?;
        let (mode, tester, host_cs) = {
            let k = self.kline()?;
            let s = &k.settings;
            let mode = k.mode.unwrap_or_else(|| {
                LinkMode::Kwp(HeaderForm::unchecked(s.addressing, s.target, s.tester))
            });
            (mode, s.tester, s.host_checksum)
        };
        let form = match mode {
            LinkMode::Kwp(form) => form,
            LinkMode::Iso9141Obd { .. } => return self.iso9141_request(req, timeout_ms),
        };
        let frame = form.build(req, host_cs)?;
        self.kline_clear_rx();
        self.kline_transmit(&frame)?;
        for _ in 0..MAX_PENDING {
            let f = self.kline_next_frame(timeout_ms)?;
            if f.target.is_some_and(|t| t != tester) {
                self.stats.foreign_frames += 1;
                continue;
            }
            if is_response_pending(req, &f.payload) {
                continue;
            }
            return Ok(f.payload);
        }
        Err(Error::Timeout(timeout_ms))
    }

    /// The next message as the cable delimited it, or [`Error::Timeout`].
    /// ISO 9141-2 frames carry no length: the bus timing (P1 inter-byte,
    /// P2 between frames) delimits them, which the cable does.
    fn kline_next_msg(&mut self, timeout_ms: u64) -> Result<Vec<u8>> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if let Some(m) = self.kline.as_mut().and_then(|k| k.msgs.pop_front()) {
                return Ok(m);
            }
            match self.read_msg(deadline)? {
                None => return Err(Error::Timeout(timeout_ms)),
                Some(proto::Msg::Frame(f)) => self.accept_frame(f),
                Some(proto::Msg::Reply(_)) => self.stats.stale_replies += 1,
            }
        }
    }

    /// An ISO 9141-2 OBD request (the ECU woke with key bytes 08 08 or
    /// 94 94): only OBD modes 0x01..=0x0A that passed the shared read-only
    /// guard go out, as `68 6A <tester> <mode> …` (+ the checksum in host
    /// mode). Every KWP2000 service is refused unsent. The answer is
    /// `48 6B <ecu> <data> <cs>`; the first one's ECU address is kept.
    fn iso9141_request(&mut self, req: &[u8], timeout_ms: u64) -> Result<Vec<u8>> {
        let (tester, host_cs, keys, src) = {
            let k = self.kline()?;
            (
                k.settings.tester,
                k.settings.host_checksum,
                k.key_bytes.unwrap_or((0x08, 0x08)),
                k.obd_source,
            )
        };
        let sid = req.first().copied().unwrap_or_default();
        if !(0x01..=0x0A).contains(&sid) {
            let ecu = src.map_or_else(
                || "<ecu>; an OBD request (mode 01 PID 00) reports the ECU's address".into(),
                |s| format!("{s:02X}"),
            );
            return Err(Error::Unsupported(format!(
                "ECU woke as ISO 9141-2 OBD (key bytes {:02X} {:02X}); KWP2000 needs its \
                 physical init address (init_addr={ecu}). Service 0x{sid:02X} was refused; \
                 nothing was sent",
                keys.0, keys.1
            )));
        }
        if req.len() > ISO9141_MAX_DATA {
            return Err(Error::Protocol(format!(
                "ISO 9141-2 OBD request of {} bytes (at most {ISO9141_MAX_DATA}); nothing was \
                 sent",
                req.len()
            )));
        }
        let mut frame = vec![ISO9141_REQ_HEADER[0], ISO9141_REQ_HEADER[1], tester];
        frame.extend_from_slice(req);
        if host_cs {
            frame.push(checksum(&frame));
        }
        self.kline_clear_rx();
        self.kline_transmit(&frame)?;
        for _ in 0..MAX_PENDING {
            let msg = self.kline_next_msg(timeout_ms)?;
            let Some((src, payload)) = split_iso9141_answer(&msg, host_cs)? else {
                self.stats.foreign_frames += 1;
                continue;
            };
            if let Some(k) = self.kline.as_mut() {
                if k.obd_source.is_none() {
                    log::debug!(
                        target: "openport2",
                        "K-line ISO 9141-2 OBD: ECU source address {src:02X}"
                    );
                    k.obd_source = Some(src);
                }
            }
            log::debug!(target: "openport2", "K-line ISO 9141-2 OBD: answer from {src:02X}");
            if is_response_pending(req, &payload) {
                continue;
            }
            self.log_more_obd_responders();
            return Ok(payload);
        }
        Err(Error::Timeout(timeout_ms))
    }

    /// After the first OBD answer (the one used), log the source address of
    /// every other ECU that answers the same request within a short window
    /// (ISO 9141-2 responders answer within P2), so a multi-ECU bus is
    /// visible in the trace. Their answers are discarded.
    fn log_more_obd_responders(&mut self) {
        for _ in 0..8 {
            let Ok(msg) = self.kline_next_msg(OBD_EXTRA_RESPONDER_MS) else {
                return;
            };
            let host = self
                .kline
                .as_ref()
                .is_some_and(|k| k.settings.host_checksum);
            match split_iso9141_answer(&msg, host) {
                Ok(Some((src, _))) => log::debug!(
                    target: "openport2",
                    "K-line ISO 9141-2 OBD: another responder {src:02X} (answer not used)"
                ),
                _ => self.stats.foreign_frames += 1,
            }
        }
    }
}

/// How long to listen for further ISO 9141-2 responders after the first.
const OBD_EXTRA_RESPONDER_MS: u64 = 60;

/// One ISO 9141-2 OBD answer `48 6B <src> <data…> [cs]`: `Ok(None)` for a
/// message with another header (skipped), the source address and data
/// otherwise. With `checksum_required` the last byte must be the 8-bit sum
/// of the rest; without it (the cable checks), a last byte is dropped only
/// when it matches, as for KWP2000 frames.
pub fn split_iso9141_answer(msg: &[u8], checksum_required: bool) -> Result<Option<(u8, Vec<u8>)>> {
    if msg.len() < 4 || msg[..2] != ISO9141_RESP_HEADER {
        return Ok(None);
    }
    let (body, last) = msg.split_at(msg.len() - 1);
    let cs = checksum(body);
    let end = if checksum_required {
        if last[0] != cs {
            return Err(Error::Protocol(format!(
                "ISO 9141-2 checksum mismatch: computed 0x{cs:02X}, frame carries 0x{:02X}",
                last[0]
            )));
        }
        body.len()
    } else if last[0] == cs {
        body.len()
    } else {
        msg.len()
    };
    let data = msg[3..end].to_vec();
    Ok((!data.is_empty()).then_some((msg[2], data)))
}

/// [`DiagTransport`] over the OP2's K-line channel: KWP2000 framing, the
/// read-only allow-list and responsePending handling.
pub(crate) struct KLineLink {
    pub session: Rc<RefCell<Session>>,
}

impl DiagTransport for KLineLink {
    fn request(&mut self, req: &[u8], timeout_ms: u64) -> Result<Vec<u8>> {
        // The guard runs before the session is even borrowed.
        refuse_non_read(req)?;
        self.session.borrow_mut().kline_request(req, timeout_ms)
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
            (
                s.ecu,
                s.target,
                s.tester,
                s.init,
                s.init_addr,
                s.host_checksum
            ),
            (0x01, 0x01, 0xF1, KInit::Fast, 0x01, true),
            "host checksum is the default (proven on a cable)"
        );
        let s = KLineSettings::from_cfg(&cfg(&[("checksum", "cable")]), 4).unwrap();
        assert!(!s.host_checksum, "checksum=cable still selectable");
        assert_eq!(
            (s.addressing, s.header_explicit, s.session),
            (Addressing::Physical, false, None)
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
            ("target", "133"),
            ("addressing", "broadcast"),
            ("session", "x"),
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

    #[test]
    fn the_request_target_and_address_mode_follow_the_init() {
        let s = |o: &[(&str, &str)]| KLineSettings::from_cfg(&cfg(o), 4).unwrap();
        // Five-baud at the OBD functional address: functional to 0x33.
        let k = s(&[("init", "5baud"), ("init_addr", "33")]);
        assert_eq!(
            (k.ecu, k.target, k.addressing, k.header_explicit),
            (0x01, 0x33, Addressing::Functional, false)
        );
        // Five-baud at the physical address: physical to it.
        let k = s(&[("init", "5baud")]);
        assert_eq!((k.target, k.addressing), (0x01, Addressing::Physical));
        // Fast init: tx_id, whatever init_addr says.
        let k = s(&[("init_addr", "33")]);
        assert_eq!((k.target, k.addressing), (0x01, Addressing::Physical));
        // tx_id 0x33 is functional by default.
        let mut c = cfg(&[]);
        c.tx_id = 0x33;
        let k = KLineSettings::from_cfg(&c, 4).unwrap();
        assert_eq!((k.target, k.addressing), (0x33, Addressing::Functional));
        // Explicit options win.
        let k = s(&[("init", "5baud"), ("init_addr", "33"), ("target", "01")]);
        assert_eq!(
            (k.target, k.addressing, k.header_explicit),
            (0x01, Addressing::Physical, true)
        );
        let k = s(&[
            ("init", "5baud"),
            ("init_addr", "33"),
            ("addressing", "physical"),
        ]);
        assert_eq!((k.target, k.addressing), (0x33, Addressing::Physical));
        let k = s(&[("target", "0x10"), ("addressing", "functional")]);
        assert_eq!((k.target, k.addressing), (0x10, Addressing::Functional));
        let k = s(&[("addressing", "none")]);
        assert_eq!(k.addressing, Addressing::NoAddress);
        let e = KLineSettings::from_cfg(&cfg(&[("addressing", "none"), ("target", "33")]), 4);
        assert!(matches!(e, Err(Error::Config(_))));
    }

    #[test]
    fn the_session_option_passes_the_read_only_guard() {
        let s = KLineSettings::from_cfg(&cfg(&[("session", "81")]), 4).unwrap();
        assert_eq!(s.session, Some(0x81));
        let s = KLineSettings::from_cfg(&cfg(&[("session", "0x89")]), 4).unwrap();
        assert_eq!(s.session, Some(0x89));
        for prog in ["85", "02"] {
            let e = KLineSettings::from_cfg(&cfg(&[("session", prog)]), 4).unwrap_err();
            assert!(matches!(e, Error::Unsupported(_)), "{prog}: {e}");
            assert!(e.to_string().contains("programming"), "{e}");
        }
    }

    #[test]
    fn header_forms_frame_as_iso_14230_2_says() {
        let f = |a, len_in_fmt, len_byte| HeaderForm {
            len_in_fmt,
            len_byte,
            ..HeaderForm::unchecked(a, 0x33, 0xF1)
        };
        // Functional, length in the format byte: C2 33 F1 1A 80 + cs.
        let fr = f(Addressing::Functional, true, true)
            .build(&[0x1A, 0x80], true)
            .unwrap();
        // 0xC2 + 0x33 + 0xF1 + 0x1A + 0x80 = 0x280.
        assert_eq!(fr, vec![0xC2, 0x33, 0xF1, 0x1A, 0x80, 0x80]);
        // Physical.
        let fr = f(Addressing::Physical, true, false)
            .build(&[0x1A, 0x80], false)
            .unwrap();
        assert_eq!(fr, vec![0x82, 0x33, 0xF1, 0x1A, 0x80]);
        // No address information: format byte + data only.
        let fr = f(Addressing::NoAddress, true, false)
            .build(&[0x1A, 0x80], false)
            .unwrap();
        assert_eq!(fr, vec![0x02, 0x1A, 0x80]);
        // Length byte only (AL0 = 0): format byte length 0, then the length.
        let fr = f(Addressing::Physical, false, true)
            .build(&[0x1A, 0x80], false)
            .unwrap();
        assert_eq!(fr, vec![0x80, 0x33, 0xF1, 0x02, 0x1A, 0x80]);
        let fr = f(Addressing::NoAddress, false, true)
            .build(&[0x3E], false)
            .unwrap();
        assert_eq!(fr, vec![0x00, 0x01, 0x3E]);
        // 64+ bytes without AL1: refused unsent.
        let e = f(Addressing::Functional, true, false)
            .build(&[0x21; 64], false)
            .unwrap_err();
        assert!(matches!(e, Error::Unsupported(_)), "{e}");
        // Every form reads back through the tolerant splitter.
        for form in [
            f(Addressing::Functional, true, true),
            f(Addressing::Physical, false, true),
            f(Addressing::NoAddress, true, false),
            f(Addressing::NoAddress, false, true),
        ] {
            let fr = form.build(&[0x5A, 0x80, 0x41], true).unwrap();
            let Split::Frame(k, used) = split_frame(&fr, true) else {
                panic!("{form:?}")
            };
            assert_eq!((k.payload, used), (vec![0x5A, 0x80, 0x41], fr.len()));
            let addr = form.addressing != Addressing::NoAddress;
            assert_eq!(k.target, addr.then_some(0x33), "{form:?}");
        }
        assert_eq!(
            f(Addressing::Functional, true, false).describe(),
            "header Cn 33 F1 (functional), length in format byte"
        );
    }

    /// [`decide_link`] for KWP2000 outcomes.
    fn decide_header(s: &KLineSettings, kb: Option<(u8, u8)>) -> Result<HeaderForm> {
        decide_link(s, kb).map(|m| match m {
            LinkMode::Kwp(h) => h,
            other => panic!("not KWP2000: {other:?}"),
        })
    }

    #[test]
    fn keywords_decide_the_link_before_the_header() {
        let s = |o: &[(&str, &str)]| KLineSettings::from_cfg(&cfg(o), 4).unwrap();
        let obd = s(&[("init", "5baud"), ("init_addr", "33")]);
        // ISO 9141-2 key bytes: OBD mode, whatever the defaulted header.
        for kb in [(0x08, 0x08), (0x94, 0x94)] {
            let m = decide_link(&obd, Some(kb)).unwrap();
            assert_eq!(m, LinkMode::Iso9141Obd { tester: 0xF1 });
            assert_eq!(
                m.describe(Some(0x10)),
                "ISO 9141-2 OBD: header 68 6A F1, KWP2000 refused, ECU source address 10"
            );
            assert!(decide_link(&s(&[]), Some(kb)).is_ok());
        }
        // Not ISO 9141-2: the pair must match and be 08 or 94.
        for kb in [(0x08, 0x94), (0x94, 0x08), (0x01, 0x8A), (0x55, 0x55)] {
            let e = decide_link(&obd, Some(kb)).unwrap_err().to_string();
            assert!(e.contains("neither ISO 14230"), "{kb:02X?}: {e}");
            assert!(e.contains(&format!("{:02X} {:02X}", kb.0, kb.1)), "{e}");
            assert!(e.contains("nothing was sent"), "{e}");
        }
        // Explicit target/addressing still win, unchecked.
        for kb in [(0x08, 0x08), (0x01, 0x8A)] {
            for o in [&[("target", "01")][..], &[("addressing", "physical")]] {
                let h = decide_header(&s(o), Some(kb)).unwrap();
                assert!(h.forced && h.len_in_fmt && h.len_byte, "{o:?}");
                assert_eq!(h.addressing, Addressing::Physical);
                assert!(h.describe().contains("forced"), "{}", h.describe());
            }
        }
        // KB2 8F is never forced: KB1 still rules.
        let h = decide_header(&s(&[("target", "01")]), Some((0xEF, 0x8F))).unwrap();
        assert!(!h.forced);
    }

    #[test]
    fn iso9141_answers_split_with_and_without_the_checksum() {
        let body = [0x48, 0x6B, 0x10, 0x41, 0x00, 0xBE, 0x1F, 0xA8, 0x13];
        let mut with_cs = body.to_vec();
        with_cs.push(checksum(&body));
        let want = Some((0x10, vec![0x41, 0x00, 0xBE, 0x1F, 0xA8, 0x13]));
        assert_eq!(split_iso9141_answer(&with_cs, true).unwrap(), want);
        assert_eq!(split_iso9141_answer(&with_cs, false).unwrap(), want);
        // Cable mode, checksum stripped: the last byte is data.
        assert_eq!(split_iso9141_answer(&body, false).unwrap(), want);
        let mut bad = with_cs.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(split_iso9141_answer(&bad, true).is_err());
        // Another header (a KWP2000 frame, an echo): skipped.
        assert_eq!(
            split_iso9141_answer(&[0x83, 0xF1, 0x10, 0xC1, 0xEF, 0x8F], false).unwrap(),
            None
        );
        assert_eq!(
            split_iso9141_answer(&[0x48, 0x6B, 0x10], false).unwrap(),
            None
        );
    }

    #[test]
    fn key_byte_1_decides_or_refuses_the_header_form() {
        let s = |o: &[(&str, &str)]| KLineSettings::from_cfg(&cfg(o), 4).unwrap();
        let obd = s(&[("init", "5baud"), ("init_addr", "33")]);
        // No key bytes (init=none): the requested form, unchecked.
        let h = decide_header(&obd, None).unwrap();
        assert_eq!(h, HeaderForm::unchecked(Addressing::Functional, 0x33, 0xF1));

        // Keywords 2025/2027/2029/2031 (KB1 E9/6B/6D/EF, KB2 8F): all have
        // HB1 and AL0, the normal timing set; AL1 and HB0 vary.
        for (kb1, al1, hb0) in [
            (0xE9, false, false),
            (0x6B, true, false),
            (0x6D, false, true),
            (0xEF, true, true),
        ] {
            let h = decide_header(&obd, Some((kb1, 0x8F))).unwrap();
            assert_eq!(h.addressing, Addressing::Functional, "{kb1:02X}");
            assert_eq!((h.len_in_fmt, h.len_byte), (true, al1), "{kb1:02X}");
            assert_eq!(h.timing_bits, Some(0b10));
            assert!(h.describe().ends_with("normal timing"), "{}", h.describe());
            let none = s(&[("addressing", "none")]);
            assert_eq!(decide_header(&none, Some((kb1, 0x8F))).is_ok(), hb0);
        }
        // KB1 EA (2026: HB1 + AL1 only): length byte, address header.
        let h = decide_header(&obd, Some((0xEA, 0x8F))).unwrap();
        assert_eq!(
            (h.addressing, h.len_in_fmt, h.len_byte),
            (Addressing::Functional, false, true)
        );
        // KB1 E5 (2021: HB0 + AL0 only): a defaulted address header becomes
        // the 1-byte header; an explicit one is refused.
        let h = decide_header(&obd, Some((0xE5, 0x8F))).unwrap();
        assert_eq!(h.addressing, Addressing::NoAddress);
        for explicit in [
            s(&[("target", "01")]),
            s(&[("addressing", "functional"), ("init", "5baud")]),
        ] {
            let e = decide_header(&explicit, Some((0xE5, 0x8F))).unwrap_err();
            assert!(matches!(e, Error::Unsupported(_)), "{e}");
            assert!(e.to_string().contains("HB1 = 0"), "{e}");
            assert!(e.to_string().contains("addressing=none"), "{e}");
        }
        // Neither header form, neither length form.
        for (kb1, kb2, why) in [(0x61, 0x8F, "HB1 = 0"), (0x6C, 0x8F, "AL0 = AL1 = 0")] {
            let e = decide_header(&obd, Some((kb1, kb2)))
                .unwrap_err()
                .to_string();
            assert!(e.contains(why), "{kb1:02X} {kb2:02X}: {e}");
            assert!(e.contains("nothing was sent"), "{e}");
        }
        // An unusual timing set is reported, not acted on.
        let h = decide_header(&obd, Some((0x5F, 0x8F))).unwrap();
        assert_eq!(h.timing_bits, Some(0b01));
        assert!(h.describe().contains("not the normal set"));
    }
}
