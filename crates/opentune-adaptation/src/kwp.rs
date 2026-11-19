//! The **KWP2000 path** of the adaptation engine: VAG KWP2000 (ISO 14230)
//! measuring blocks, basic settings and adaptation channels, over a VW TP2.0
//! channel on CAN or directly on the K-line. The engine only sees
//! request/response telegrams ([`RawDiag`]), so the same code runs over both:
//! the transport is whatever the caller's driver opened.
//!
//! ## Services (public sources, see `docs/PRIOR-ART.md`)
//!
//! | Operation | Requests | Source / confidence |
//! |---|---|---|
//! | diagnostic session | `10 89` | community (the DSG / Haldex paths, kw1281test) |
//! | SecurityAccess | `27 03` → seed, `27 04 <key>` | kw1281test (EDC16, K-line); **unconfirmed** elsewhere |
//! | measuring block | `21 <group>` → `61 <group> (formula a b)*` | ISO 14230-3 service; VAG cell layout community |
//! | basic settings start | `31 B8 00 <group>` (after `31 B8 00 00`) | kw1281test (EDC16); **unconfirmed** elsewhere |
//! | basic settings poll | `31 BA 00 <group>` → `71 BA 00 <group> <status> (formula a b)*` | same |
//! | basic settings stop | `32 B8 00 <group>` | same |
//! | adaptation enter | `31 B8 00 00`, `31 B8 01 03` | same |
//! | adaptation select | `31 B9 01 03 <channel>` | same |
//! | adaptation read | `31 BA 01 03` → `71 BA 01 03 <status> <fmt> <hi> <lo> [04 f a b]` | same |
//! | adaptation test | `31 B9 01 03 <hi> <lo>` | same |
//! | adaptation save | `31 BB 01 03 <hi> <lo> <workshop code: 6>` | same |
//! | adaptation exit | `32 B8 01 03` | same |
//!
//! Only the service ids are ISO 14230 (`startRoutineByLocalIdentifier` 0x31,
//! `stopRoutineByLocalIdentifier` 0x32, `readDataByLocalIdentifier` 0x21,
//! `securityAccess` 0x27, `startDiagnosticSession` 0x10). The routine local
//! identifiers (`B8`..`BB`), the selectors (`01 03`, `00 <group>`) and the
//! record layouts are **VAG-specific and community-sourced from one module
//! family**; they are the *defaults* of [`KwpAddressing`], so a channel map
//! overrides them per channel as captures confirm or correct them. The
//! adaptation record's `<fmt>` byte and the status bytes are recorded, not
//! interpreted.
//!
//! ## Login and the TP2.0 re-open
//!
//! SecurityAccess uses a caller-supplied [`SeedKey`]: **OpenTune ships no VAG
//! KWP login algorithm** (an embedding build such as Bench supplies one, tests
//! use a synthetic one). Every flow starts its own session (and, when
//! configured, SecurityAccess) — never relying on one left over from an
//! earlier operation — because a TP2.0 channel that broke (failed exchange,
//! module `A8`) is set up again on the next request **without** the module's
//! KWP session.
//!
//! * **Reads** that fail with a session-loss symptom ([`is_session_loss`]: a
//!   transport failure, or NRC 0x80 / 0x7E / 0x7F) re-establish the session and
//!   security once and retry ([`GroupReading::reestablished`]).
//! * **Writes** never retry: the first failure after the first mutating request
//!   ends the flow with [`KwpWriteOutcome::Aborted`]. Nothing more is sent
//!   after a session-loss symptom (the module dropped the mode with the
//!   session); after a plain negative response a best-effort exit is sent.
//!
//! ## Safety gates
//!
//! Adaptation test / save and basic-settings start are writes. They are
//! refused **before any I/O** ([`AdaptationError::NotConfirmed`]) unless
//! [`KwpWriteOptions::confirmed`] is set, a session is given, and a seed/key
//! handler is given for SecurityAccess. [`KwpWriteOptions::dry_run`] returns the
//! request plan and sends nothing. Values are range-checked against the
//! channel's `min`/`max`, a save needs the 6-byte workshop code, and a save or
//! test is read back.
use crate::{AdaptationDef, AdaptationError, ChannelKind, RawDiag, Result, Scaling};
use opentune_core::protocol::kwp2000::kwp_nrc_name;
use opentune_core::protocol::seedkey::SeedKey;
use opentune_core::protocol::vagmeas;
use serde::{Deserialize, Serialize};

/// ISO 14230-3 service ids this path sends.
pub mod sid {
    /// `startDiagnosticSession`.
    pub const START_DIAGNOSTIC_SESSION: u8 = 0x10;
    /// `readDataByLocalIdentifier` (VAG measuring block).
    pub const READ_DATA_BY_LOCAL_ID: u8 = 0x21;
    /// `securityAccess`.
    pub const SECURITY_ACCESS: u8 = 0x27;
    /// `startRoutineByLocalIdentifier`.
    pub const START_ROUTINE_BY_LOCAL_ID: u8 = 0x31;
    /// `stopRoutineByLocalIdentifier`.
    pub const STOP_ROUTINE_BY_LOCAL_ID: u8 = 0x32;
}

/// Every service id this path can send.
pub const SERVICES: [u8; 5] = [
    sid::START_DIAGNOSTIC_SESSION,
    sid::READ_DATA_BY_LOCAL_ID,
    sid::SECURITY_ACCESS,
    sid::START_ROUTINE_BY_LOCAL_ID,
    sid::STOP_ROUTINE_BY_LOCAL_ID,
];

/// Driver ids that open a VW TP2.0 channel (raw-CAN adapters).
pub const TP20_DRIVERS: &[&str] = &["socketcan", "slcan", "gsusb"];
/// Driver ids that speak KWP2000 on the K-line.
pub const KLINE_DRIVERS: &[&str] = &["kkl", "kline-bridge", "elm327"];

/// VAG KWP2000 diagnostic session (community; the DSG and Haldex paths use it).
pub const VAG_SESSION: u8 = 0x89;
/// SecurityAccess level for the requestSeed (`27 03`; sendKey is `27 04`).
/// Seen on EDC16 (kw1281test); **unconfirmed** for other modules.
pub const VAG_SECURITY_LEVEL: u8 = 0x03;
/// Routine local id: enter adaptation / basic settings (and its stop).
pub const ROUTINE_ENTER: u8 = 0xB8;
/// Routine local id: select an adaptation channel, or set a test value.
pub const ROUTINE_SELECT: u8 = 0xB9;
/// Routine local id: read the current adaptation / basic-settings record.
pub const ROUTINE_READ: u8 = 0xBA;
/// Routine local id: save an adaptation value.
pub const ROUTINE_SAVE: u8 = 0xBB;
/// Sub-selector of the adaptation routines.
pub const ADAPTATION_SELECTOR: [u8; 2] = [0x01, 0x03];
/// Length of the workshop code a save carries.
pub const WORKSHOP_CODE_LEN: usize = 6;

// ---------------------------------------------------------------------------
// Addressing (data)
// ---------------------------------------------------------------------------

fn d_enter() -> u8 {
    ROUTINE_ENTER
}
fn d_select() -> u8 {
    ROUTINE_SELECT
}
fn d_read() -> u8 {
    ROUTINE_READ
}
fn d_save() -> u8 {
    ROUTINE_SAVE
}
fn d_selector() -> Vec<u8> {
    ADAPTATION_SELECTOR.to_vec()
}
fn d_true() -> bool {
    true
}

/// The `kwp` addressing object of a KWP channel: which routine local
/// identifiers and selectors the module uses. Every field defaults to the
/// community-sourced VAG values (see the module docs), so `{}` or an absent
/// `kwp` means "the defaults".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KwpAddressing {
    /// Enter (start) adaptation / basic settings; its stop exits.
    #[serde(default = "d_enter")]
    pub enter: u8,
    /// Select an adaptation channel / set a test value.
    #[serde(default = "d_select")]
    pub select: u8,
    /// Read the current record.
    #[serde(default = "d_read")]
    pub read: u8,
    /// Save an adaptation value.
    #[serde(default = "d_save")]
    pub save: u8,
    /// Sub-selector sent with every adaptation routine (1..=4 bytes).
    #[serde(default = "d_selector")]
    pub adaptation_selector: Vec<u8>,
    /// First selector byte of the basic-settings routines (`<this> <group>`).
    #[serde(default)]
    pub basic_settings_selector: u8,
    /// Send the enumerate request (`<enter> 00 00`) before entering.
    #[serde(default = "d_true")]
    pub enumerate: bool,
}

impl Default for KwpAddressing {
    fn default() -> Self {
        KwpAddressing {
            enter: ROUTINE_ENTER,
            select: ROUTINE_SELECT,
            read: ROUTINE_READ,
            save: ROUTINE_SAVE,
            adaptation_selector: ADAPTATION_SELECTOR.to_vec(),
            basic_settings_selector: 0x00,
            enumerate: true,
        }
    }
}

impl KwpAddressing {
    /// Semantic checks: the four routine ids are distinct and the adaptation
    /// selector is 1..=4 bytes.
    pub fn validate(&self, channel: &str) -> Result<()> {
        let ids = [self.enter, self.select, self.read, self.save];
        for (i, a) in ids.iter().enumerate() {
            if ids[i + 1..].contains(a) {
                return Err(AdaptationError::InvalidDef(format!(
                    "`{channel}` kwp routine id 0x{a:02X} is used twice"
                )));
            }
        }
        if self.adaptation_selector.is_empty() || self.adaptation_selector.len() > 4 {
            return Err(AdaptationError::InvalidDef(format!(
                "`{channel}` kwp adaptation_selector must be 1..=4 bytes"
            )));
        }
        Ok(())
    }

    fn basic_selector(&self, group: u8) -> [u8; 2] {
        [self.basic_settings_selector, group]
    }
}

// ---------------------------------------------------------------------------
// Login
// ---------------------------------------------------------------------------

/// How a flow logs in: the diagnostic session to start and, optionally,
/// SecurityAccess with a caller-supplied seed/key handler.
#[derive(Clone, Copy)]
pub struct KwpLogin<'a> {
    /// Session started first (`10 <session>`); `None` skips it (reads only:
    /// every write needs one).
    pub session: Option<u8>,
    /// requestSeed level (sendKey is `level + 1`).
    pub security_level: u8,
    /// The seed→key transform. `None` = no SecurityAccess (reads only).
    pub seed_key: Option<&'a dyn SeedKey>,
}

impl std::fmt::Debug for KwpLogin<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KwpLogin")
            .field("session", &self.session)
            .field("security_level", &self.security_level)
            .field("seed_key", &self.seed_key.map(|s| s.id().to_string()))
            .finish()
    }
}

impl<'a> KwpLogin<'a> {
    /// Start `session`, no SecurityAccess.
    pub fn session(session: u8) -> Self {
        KwpLogin {
            session: Some(session),
            security_level: VAG_SECURITY_LEVEL,
            seed_key: None,
        }
    }

    /// The VAG defaults: session [`VAG_SESSION`], no SecurityAccess.
    pub fn vag() -> Self {
        Self::session(VAG_SESSION)
    }

    /// No session, no SecurityAccess (a module already in the right state).
    pub fn none() -> Self {
        KwpLogin {
            session: None,
            security_level: VAG_SECURITY_LEVEL,
            seed_key: None,
        }
    }

    /// Add SecurityAccess at `level` with `seed_key`.
    pub fn with_security(mut self, level: u8, seed_key: &'a dyn SeedKey) -> Self {
        self.security_level = level;
        self.seed_key = Some(seed_key);
        self
    }
}

/// What [`establish`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct LoginReport {
    /// The session started, if any.
    pub session: Option<u8>,
    /// The SecurityAccess level unlocked, if any.
    pub security_level: Option<u8>,
    /// The module sent an all-zero seed: already unlocked, no key sent.
    pub already_unlocked: bool,
    /// Id of the seed/key handler used.
    pub seed_key: Option<String>,
}

/// Start the session and run SecurityAccess as `login` says. A refused key
/// (NRC 0x35 / 0x36 / 0x37) or a failing handler is
/// [`AdaptationError::SecurityDenied`].
pub fn establish(diag: &mut dyn RawDiag, login: &KwpLogin<'_>) -> Result<LoginReport> {
    let mut rep = LoginReport::default();
    if let Some(s) = login.session {
        let body = call(diag, &[sid::START_DIAGNOSTIC_SESSION, s])?;
        if let Some(&echo) = body.first() {
            if echo != s {
                return Err(AdaptationError::Protocol(format!(
                    "KWP2000 startDiagnosticSession: response echoes session 0x{echo:02X}, \
                     expected 0x{s:02X}"
                )));
            }
        }
        rep.session = Some(s);
    }
    if let Some(sk) = login.seed_key {
        let level = login.security_level;
        let body = call(diag, &[sid::SECURITY_ACCESS, level]).map_err(security)?;
        match body.split_first() {
            Some((&echo, seed)) if echo == level && !seed.is_empty() => {
                rep.seed_key = Some(sk.id().to_string());
                if seed.iter().all(|&b| b == 0) {
                    rep.already_unlocked = true;
                } else {
                    let key = sk.compute(seed, level).map_err(|e| {
                        AdaptationError::SecurityDenied(format!("seed/key '{}': {e}", sk.id()))
                    })?;
                    let mut req = vec![sid::SECURITY_ACCESS, level.wrapping_add(1)];
                    req.extend_from_slice(&key);
                    let body = call(diag, &req).map_err(security)?;
                    if body.first() != Some(&level.wrapping_add(1)) {
                        return Err(AdaptationError::Protocol(format!(
                            "KWP2000 securityAccess sendKey: response does not echo level \
                             0x{:02X}",
                            level.wrapping_add(1)
                        )));
                    }
                }
                rep.security_level = Some(level);
            }
            Some((&echo, _)) if echo != level => {
                return Err(AdaptationError::Protocol(format!(
                    "KWP2000 securityAccess requestSeed: response echoes level 0x{echo:02X}, \
                     expected 0x{level:02X}"
                )))
            }
            _ => {
                return Err(AdaptationError::Protocol(
                    "KWP2000 securityAccess requestSeed: response carries no seed".into(),
                ))
            }
        }
    }
    Ok(rep)
}

/// Map a refused unlock to [`AdaptationError::SecurityDenied`].
fn security(e: AdaptationError) -> AdaptationError {
    match e {
        AdaptationError::NegativeResponse {
            code: c @ (0x33 | 0x35 | 0x36 | 0x37),
            name,
            ..
        } => AdaptationError::SecurityDenied(format!("NRC 0x{c:02X} ({name})")),
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Telegrams
// ---------------------------------------------------------------------------

/// Send one KWP2000 request; return the positive response **after** the
/// service id. A `7F` echoing the service is [`AdaptationError::NegativeResponse`]
/// (a leaked `0x78` included); a truncated or mis-correlated one, an empty
/// response or a wrong positive SID is [`AdaptationError::Protocol`]; a
/// transport failure is [`AdaptationError::Transport`].
pub fn call(diag: &mut dyn RawDiag, req: &[u8]) -> Result<Vec<u8>> {
    let sid = *req
        .first()
        .ok_or_else(|| AdaptationError::Protocol("empty KWP2000 request".into()))?;
    let resp = diag.request(req).map_err(AdaptationError::Transport)?;
    match resp.first() {
        None => Err(AdaptationError::Protocol(format!(
            "KWP2000 service 0x{sid:02X}: empty response"
        ))),
        Some(0x7F) => {
            if resp.len() < 3 {
                return Err(AdaptationError::Protocol(format!(
                    "KWP2000 service 0x{sid:02X}: truncated negative response"
                )));
            }
            if resp[1] != sid {
                return Err(AdaptationError::Protocol(format!(
                    "KWP2000 negative response echoes service 0x{:02X}, expected 0x{sid:02X}",
                    resp[1]
                )));
            }
            Err(AdaptationError::NegativeResponse {
                service: sid,
                code: resp[2],
                name: kwp_nrc_name(resp[2]),
            })
        }
        Some(&b) if b == sid.wrapping_add(0x40) => Ok(resp[1..].to_vec()),
        Some(&b) => Err(AdaptationError::Protocol(format!(
            "KWP2000 service 0x{sid:02X}: unexpected response SID 0x{b:02X}"
        ))),
    }
}

/// `31`/`32 <rlid> <data>`: the response must echo `rlid`; returns the bytes
/// after it.
fn routine(diag: &mut dyn RawDiag, service: u8, rlid: u8, data: &[u8]) -> Result<Vec<u8>> {
    let mut req = Vec::with_capacity(2 + data.len());
    req.extend_from_slice(&[service, rlid]);
    req.extend_from_slice(data);
    let body = call(diag, &req)?;
    match body.split_first() {
        Some((&echo, rest)) if echo == rlid => Ok(rest.to_vec()),
        Some((&echo, _)) => Err(AdaptationError::Protocol(format!(
            "KWP2000 service 0x{service:02X}: response echoes routine 0x{echo:02X}, expected \
             0x{rlid:02X}"
        ))),
        None => Err(AdaptationError::Protocol(format!(
            "KWP2000 service 0x{service:02X}: response without the routine echo"
        ))),
    }
}

/// A routine response that must start with `sel`; returns what follows.
fn expect_selector(rest: Vec<u8>, sel: &[u8], what: &str) -> Result<Vec<u8>> {
    if rest.len() < sel.len() || &rest[..sel.len()] != sel {
        return Err(AdaptationError::Protocol(format!(
            "KWP2000 {what}: response does not echo selector {}",
            hex(sel)
        )));
    }
    Ok(rest[sel.len()..].to_vec())
}

/// `21 <id>` with the echo checked; the record after it.
fn read_local(diag: &mut dyn RawDiag, id: u8) -> Result<Vec<u8>> {
    let body = call(diag, &[sid::READ_DATA_BY_LOCAL_ID, id])?;
    match body.split_first() {
        Some((&echo, rec)) if echo == id => Ok(rec.to_vec()),
        Some((&echo, _)) => Err(AdaptationError::Protocol(format!(
            "KWP2000 readDataByLocalIdentifier echoed 0x{echo:02X}, expected 0x{id:02X}"
        ))),
        None => Err(AdaptationError::Protocol(
            "empty KWP2000 readDataByLocalIdentifier response".into(),
        )),
    }
}

/// Whether `e` is what a lost KWP session looks like: the transport failed
/// (a TP2.0 channel that broke is re-opened on the next request, without the
/// session), or the module refuses the service in its current session (NRC
/// 0x80, or the UDS-style 0x7E / 0x7F some modules send).
pub fn is_session_loss(e: &AdaptationError) -> bool {
    match e {
        AdaptationError::Transport(_) => true,
        AdaptationError::NegativeResponse { code, .. } => matches!(code, 0x7E..=0x80),
        _ => false,
    }
}

/// Uppercase space-separated hex.
pub fn hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// Cells
// ---------------------------------------------------------------------------

/// One measuring-block cell, scaled with the shared VAG formula table
/// ([`vagmeas`]); a formula not in the table stays raw.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KwpCell {
    /// 0-based position in the record.
    pub index: u8,
    /// Formula byte.
    pub formula: u8,
    /// First data byte.
    pub a: u8,
    /// Second data byte.
    pub b: u8,
    /// Scaled value (`None` = raw only).
    pub value: Option<f64>,
    /// Unit of `value`.
    pub unit: &'static str,
}

/// Most cells [`cells`] decodes: [`KwpCell::index`] is a `u8`.
pub const MAX_CELLS: usize = 256;

/// Split a record into cells (a partial tail cell is dropped, and cells past
/// [`MAX_CELLS`] are left undecoded; callers keep the raw record).
pub fn cells(rec: &[u8]) -> Vec<KwpCell> {
    vagmeas::cells(rec)
        .into_iter()
        .take(MAX_CELLS)
        .enumerate()
        .map(|(i, c)| KwpCell {
            index: i as u8,
            formula: c.formula,
            a: c.a,
            b: c.b,
            value: c.value,
            unit: c.unit,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Channel lookup
// ---------------------------------------------------------------------------

fn lookup<'a>(def: &'a AdaptationDef, name: &str) -> Result<&'a ChannelKind> {
    def.validate()?;
    def.channel(name)
        .map(|c| &c.kind)
        .ok_or_else(|| AdaptationError::UnknownChannel(name.to_string()))
}

fn mismatch(name: &str, expected: &'static str) -> AdaptationError {
    AdaptationError::ChannelKindMismatch {
        channel: name.to_string(),
        expected,
    }
}

struct AdaptChan<'a> {
    number: u8,
    scaling: &'a Scaling,
    min: Option<u16>,
    max: Option<u16>,
    kwp: &'a KwpAddressing,
}

fn adapt_channel<'a>(def: &'a AdaptationDef, name: &str) -> Result<AdaptChan<'a>> {
    match lookup(def, name)? {
        ChannelKind::KwpAdaptation {
            channel,
            scaling,
            min,
            max,
            kwp,
        } => Ok(AdaptChan {
            number: *channel,
            scaling,
            min: *min,
            max: *max,
            kwp,
        }),
        _ => Err(mismatch(name, "kwp_adaptation")),
    }
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// Run `f`, and once more after re-establishing the login when it failed with
/// a session-loss symptom. Returns whether the retry happened.
fn with_reestablish<T>(
    diag: &mut dyn RawDiag,
    login: &KwpLogin<'_>,
    f: &mut dyn FnMut(&mut dyn RawDiag) -> Result<T>,
) -> Result<(T, bool)> {
    match f(diag) {
        Ok(v) => Ok((v, false)),
        Err(e) if is_session_loss(&e) => {
            establish(diag, login)?;
            f(diag).map(|v| (v, true))
        }
        Err(e) => Err(e),
    }
}

/// A measuring-block read.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GroupReading {
    /// Channel name.
    pub channel: String,
    /// Group number.
    pub group: u8,
    /// Decoded cells.
    pub cells: Vec<KwpCell>,
    /// The record after the echoed group.
    pub raw: Vec<u8>,
    /// The first attempt hit a session-loss symptom and the read was retried
    /// after re-establishing the session.
    pub reestablished: bool,
}

/// Read a `kwp_group` channel (`21 <group>`) after starting `login`'s session
/// (and SecurityAccess, when given).
pub fn read_group(
    diag: &mut dyn RawDiag,
    def: &AdaptationDef,
    channel: &str,
    login: &KwpLogin<'_>,
) -> Result<GroupReading> {
    let group = match lookup(def, channel)? {
        ChannelKind::KwpGroup { group } => *group,
        _ => return Err(mismatch(channel, "kwp_group")),
    };
    establish(diag, login)?;
    let (raw, reestablished) =
        with_reestablish(diag, login, &mut |d: &mut dyn RawDiag| read_local(d, group))?;
    Ok(GroupReading {
        channel: channel.to_string(),
        group,
        cells: cells(&raw),
        raw,
        reestablished,
    })
}

/// A decoded adaptation record.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AdaptationReading {
    /// Channel name.
    pub channel: String,
    /// Adaptation channel number.
    pub adaptation_channel: u8,
    /// Raw 16-bit value.
    pub raw: u16,
    /// Scaled value.
    pub engineering: f64,
    /// Unit of `engineering`.
    pub unit: String,
    /// Status byte (not interpreted).
    pub status: u8,
    /// The byte before the value (not interpreted).
    pub value_format: u8,
    /// The measured value the module reports with the channel, when present
    /// (cell type `0x04`).
    pub measured: Option<KwpCell>,
    /// The record after the routine and selector echo.
    pub record: Vec<u8>,
    /// Read again after re-establishing a lost session.
    pub reestablished: bool,
}

/// Parse a read record (after `<read> <selector>`): `status fmt hi lo [04 f a b]`.
fn parse_adaptation(name: &str, ch: &AdaptChan<'_>, rec: Vec<u8>) -> Result<AdaptationReading> {
    if rec.len() < 4 {
        return Err(AdaptationError::Protocol(format!(
            "KWP2000 adaptation read of `{name}`: record {} is too short",
            hex(&rec)
        )));
    }
    let raw = u16::from_be_bytes([rec[2], rec[3]]);
    let measured = (rec.len() >= 8 && rec[4] == 0x04)
        .then(|| cells(&rec[5..8]).into_iter().next())
        .flatten();
    Ok(AdaptationReading {
        channel: name.to_string(),
        adaptation_channel: ch.number,
        raw,
        engineering: ch.scaling.apply(raw as u64),
        unit: ch.scaling.unit.clone(),
        status: rec[0],
        value_format: rec[1],
        measured,
        record: rec,
        reestablished: false,
    })
}

fn enter_adaptation(d: &mut dyn RawDiag, k: &KwpAddressing) -> Result<()> {
    if k.enumerate {
        routine(d, sid::START_ROUTINE_BY_LOCAL_ID, k.enter, &[0x00, 0x00])?;
    }
    routine(
        d,
        sid::START_ROUTINE_BY_LOCAL_ID,
        k.enter,
        &k.adaptation_selector,
    )?;
    Ok(())
}

fn select_channel(d: &mut dyn RawDiag, k: &KwpAddressing, number: u8) -> Result<()> {
    let mut data = k.adaptation_selector.clone();
    data.push(number);
    routine(d, sid::START_ROUTINE_BY_LOCAL_ID, k.select, &data).map(|_| ())
}

fn read_record(d: &mut dyn RawDiag, k: &KwpAddressing) -> Result<Vec<u8>> {
    let rest = routine(
        d,
        sid::START_ROUTINE_BY_LOCAL_ID,
        k.read,
        &k.adaptation_selector,
    )?;
    expect_selector(rest, &k.adaptation_selector, "adaptation read")
}

fn value_data(k: &KwpAddressing, raw: u16) -> Vec<u8> {
    let mut data = k.adaptation_selector.clone();
    data.extend_from_slice(&raw.to_be_bytes());
    data
}

fn exit_adaptation(d: &mut dyn RawDiag, k: &KwpAddressing) -> Result<()> {
    routine(
        d,
        sid::STOP_ROUTINE_BY_LOCAL_ID,
        k.enter,
        &k.adaptation_selector,
    )
    .map(|_| ())
}

/// Enter, select, read; exit best-effort unless the session is gone.
fn read_adaptation_flow(
    d: &mut dyn RawDiag,
    name: &str,
    ch: &AdaptChan<'_>,
) -> Result<AdaptationReading> {
    enter_adaptation(d, ch.kwp)?;
    let r = select_channel(d, ch.kwp, ch.number)
        .and_then(|_| read_record(d, ch.kwp))
        .and_then(|rec| parse_adaptation(name, ch, rec));
    match &r {
        Err(e) if is_session_loss(e) => {}
        _ => {
            let _ = exit_adaptation(d, ch.kwp);
        }
    }
    r
}

/// Read a `kwp_adaptation` channel: session (+ SecurityAccess when `login`
/// carries a handler — some modules refuse the channel select without it),
/// enter adaptation, select the channel, read the record, exit. Retried once
/// after a session-loss symptom.
pub fn read_adaptation(
    diag: &mut dyn RawDiag,
    def: &AdaptationDef,
    channel: &str,
    login: &KwpLogin<'_>,
) -> Result<AdaptationReading> {
    let ch = adapt_channel(def, channel)?;
    establish(diag, login)?;
    let (mut r, again) = with_reestablish(diag, login, &mut |d: &mut dyn RawDiag| {
        read_adaptation_flow(d, channel, &ch)
    })?;
    r.reestablished = again;
    Ok(r)
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

/// Options for the KWP writes (adaptation test / save, basic settings).
#[derive(Debug, Clone, Default)]
pub struct KwpWriteOptions {
    /// Return the request plan; send nothing.
    pub dry_run: bool,
    /// The operator confirmed this write. Without it the write is refused
    /// before any I/O.
    pub confirmed: bool,
    /// The 6-byte workshop code a save carries (required for save).
    pub workshop_code: Option<[u8; WORKSHOP_CODE_LEN]>,
}

/// Refuse a write before any I/O unless it is confirmed and has an explicit
/// session and SecurityAccess.
fn gate(opts: &KwpWriteOptions, login: &KwpLogin<'_>, what: &str) -> Result<()> {
    if opts.dry_run {
        return Ok(());
    }
    if !opts.confirmed {
        return Err(AdaptationError::NotConfirmed(format!(
            "{what} writes to the module: it must be confirmed explicitly"
        )));
    }
    if login.session.is_none() {
        return Err(AdaptationError::NotConfirmed(format!(
            "{what} needs an explicit diagnostic session (e.g. 0x{VAG_SESSION:02X})"
        )));
    }
    if login.seed_key.is_none() {
        return Err(AdaptationError::NotConfirmed(format!(
            "{what} needs SecurityAccess with a seed/key handler; OpenTune ships no VAG KWP \
             login algorithm, an embedding build (e.g. Bench) supplies one"
        )));
    }
    Ok(())
}

fn login_plan(login: &KwpLogin<'_>) -> Vec<String> {
    let mut p = Vec::new();
    if let Some(s) = login.session {
        p.push(format!("10 {s:02X}  startDiagnosticSession"));
    }
    let l = login.security_level;
    p.push(format!("27 {l:02X}  securityAccess requestSeed"));
    p.push(format!(
        "27 {:02X} <key>  securityAccess sendKey (key from the seed/key handler{})",
        l.wrapping_add(1),
        login
            .seed_key
            .map(|s| format!(" '{}'", s.id()))
            .unwrap_or_default()
    ));
    p
}

fn line(service: u8, rlid: u8, data: &[u8], what: &str) -> String {
    let mut b = vec![service, rlid];
    b.extend_from_slice(data);
    format!("{}  {what}", hex(&b))
}

/// Test (temporary) or save (persistent).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdaptMode {
    /// Apply the value temporarily (`<select> <sel> hi lo`), read it back,
    /// exit — the module is expected to discard it on exit (KWP1281
    /// semantics; unconfirmed for KWP2000).
    Test,
    /// Test, then store it (`<save> <sel> hi lo <workshop code>`), read back.
    Save,
}

/// What a KWP write did.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum KwpWriteOutcome {
    /// `dry_run`: nothing sent.
    Planned,
    /// The test value was applied and read back equal (then exited).
    Tested,
    /// The value was saved and read back equal.
    Verified,
    /// The read-back after the test/save differs from the requested value.
    VerifyFailed {
        /// What the module reports now.
        read_back: u16,
    },
    /// A request after the first mutating one failed; the module state is
    /// **unknown** — read the channel again before anything else. Nothing was
    /// sent after a session-loss symptom.
    Aborted {
        /// The step that failed.
        step: String,
        /// Why.
        message: String,
        /// The failure looks like a lost session (TP2.0 re-open / NRC 0x80).
        session_lost: bool,
    },
}

/// The report of an adaptation test / save.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KwpAdaptReport {
    /// Channel name.
    pub channel: String,
    /// Adaptation channel number.
    pub adaptation_channel: u8,
    /// Test or save.
    pub mode: AdaptMode,
    /// The requested raw value.
    pub new_raw: u16,
    /// Its scaled value.
    pub new_engineering: f64,
    /// The channel before (read after selecting it).
    pub before: Option<AdaptationReading>,
    /// The channel after the test / save.
    pub after: Option<AdaptationReading>,
    /// The requests of the flow, in order (the plan for a dry run).
    pub plan: Vec<String>,
    /// What the login did.
    pub login: Option<LoginReport>,
    /// The exit (`<stop> <enter> <sel>`) was acknowledged.
    pub exited: bool,
    /// What happened.
    pub outcome: KwpWriteOutcome,
}

impl KwpAdaptReport {
    /// True for [`KwpWriteOutcome::Verified`] (save) or
    /// [`KwpWriteOutcome::Tested`] (test).
    pub fn is_ok(&self) -> bool {
        matches!(
            self.outcome,
            KwpWriteOutcome::Verified | KwpWriteOutcome::Tested
        )
    }
}

/// End a write after a failure: a best-effort exit unless the session is
/// gone, and the [`KwpWriteOutcome::Aborted`] outcome.
fn abort_write(
    diag: &mut dyn RawDiag,
    k: &KwpAddressing,
    rep: &mut KwpAdaptReport,
    step: &str,
    e: AdaptationError,
) {
    let session_lost = is_session_loss(&e);
    if !session_lost {
        rep.exited = exit_adaptation(diag, k).is_ok();
    }
    rep.outcome = KwpWriteOutcome::Aborted {
        step: step.to_string(),
        message: e.to_string(),
        session_lost,
    };
}

fn check_range(name: &str, ch: &AdaptChan<'_>, raw: u16) -> Result<()> {
    if let Some(lo) = ch.min {
        if raw < lo {
            return Err(AdaptationError::InvalidValue {
                channel: name.to_string(),
                value: raw as u64,
                reason: format!("below minimum {lo}"),
            });
        }
    }
    if let Some(hi) = ch.max {
        if raw > hi {
            return Err(AdaptationError::InvalidValue {
                channel: name.to_string(),
                value: raw as u64,
                reason: format!("above maximum {hi}"),
            });
        }
    }
    Ok(())
}

/// Test or save a raw value on a `kwp_adaptation` channel.
///
/// Returns `Err` only when **nothing was written**: invalid definition, wrong
/// channel kind, out-of-range value, missing workshop code (save), the gate
/// ([`AdaptationError::NotConfirmed`]), a refused login, or a failure while
/// entering / selecting / reading the channel before the value is sent. From
/// the first mutating request on, the result is `Ok(report)` whose
/// [`KwpAdaptReport::outcome`] records success, a verify failure, or where it
/// stopped.
pub fn write_adaptation(
    diag: &mut dyn RawDiag,
    def: &AdaptationDef,
    channel: &str,
    new_raw: u16,
    mode: AdaptMode,
    login: &KwpLogin<'_>,
    opts: &KwpWriteOptions,
) -> Result<KwpAdaptReport> {
    let ch = adapt_channel(def, channel)?;
    check_range(channel, &ch, new_raw)?;
    let wsc = match (mode, opts.workshop_code) {
        (AdaptMode::Save, None) => {
            return Err(AdaptationError::NotConfirmed(format!(
                "saving `{channel}` needs the module's {WORKSHOP_CODE_LEN}-byte workshop code"
            )))
        }
        (_, w) => w,
    };
    let what = match mode {
        AdaptMode::Test => "an adaptation test",
        AdaptMode::Save => "an adaptation save",
    };
    gate(opts, login, what)?;

    let k = ch.kwp;
    let start = sid::START_ROUTINE_BY_LOCAL_ID;
    let mut plan = login_plan(login);
    if k.enumerate {
        plan.push(line(start, k.enter, &[0, 0], "enumerate"));
    }
    plan.push(line(
        start,
        k.enter,
        &k.adaptation_selector,
        "enter adaptation",
    ));
    let mut sel_ch = k.adaptation_selector.clone();
    sel_ch.push(ch.number);
    plan.push(line(start, k.select, &sel_ch, "select channel"));
    plan.push(line(start, k.read, &k.adaptation_selector, "read (before)"));
    plan.push(line(start, k.select, &value_data(k, new_raw), "test value"));
    if let (AdaptMode::Save, Some(w)) = (mode, wsc) {
        let mut d = value_data(k, new_raw);
        d.extend_from_slice(&w);
        plan.push(line(start, k.save, &d, "save value + workshop code"));
    }
    plan.push(line(start, k.read, &k.adaptation_selector, "read back"));
    plan.push(line(
        sid::STOP_ROUTINE_BY_LOCAL_ID,
        k.enter,
        &k.adaptation_selector,
        "exit adaptation",
    ));

    let mut rep = KwpAdaptReport {
        channel: channel.to_string(),
        adaptation_channel: ch.number,
        mode,
        new_raw,
        new_engineering: ch.scaling.apply(new_raw as u64),
        before: None,
        after: None,
        plan,
        login: None,
        exited: false,
        outcome: KwpWriteOutcome::Planned,
    };
    if opts.dry_run {
        return Ok(rep);
    }

    // Nothing mutating yet: any failure is an Err (nothing written).
    rep.login = Some(establish(diag, login)?);
    enter_adaptation(diag, k)?;
    let before = select_channel(diag, k, ch.number)
        .and_then(|_| read_record(diag, k))
        .and_then(|rec| parse_adaptation(channel, &ch, rec));
    match before {
        Ok(b) => rep.before = Some(b),
        Err(e) => {
            if !is_session_loss(&e) {
                let _ = exit_adaptation(diag, k);
            }
            return Err(e);
        }
    }

    // From here on the module may hold the new value: never retry, report.
    if let Err(e) = routine(diag, start, k.select, &value_data(k, new_raw)) {
        abort_write(diag, k, &mut rep, "test value", e);
        return Ok(rep);
    }
    if let (AdaptMode::Save, Some(w)) = (mode, wsc) {
        let mut d = value_data(k, new_raw);
        d.extend_from_slice(&w);
        if let Err(e) = routine(diag, start, k.save, &d) {
            abort_write(diag, k, &mut rep, "save", e);
            return Ok(rep);
        }
    }
    let after = match read_record(diag, k).and_then(|rec| parse_adaptation(channel, &ch, rec)) {
        Ok(a) => a,
        Err(e) => {
            abort_write(diag, k, &mut rep, "read back", e);
            return Ok(rep);
        }
    };
    let read_back = after.raw;
    rep.after = Some(after);
    rep.exited = exit_adaptation(diag, k).is_ok();
    rep.outcome = match (read_back == new_raw, mode) {
        (true, AdaptMode::Test) => KwpWriteOutcome::Tested,
        (true, AdaptMode::Save) => KwpWriteOutcome::Verified,
        (false, _) => KwpWriteOutcome::VerifyFailed { read_back },
    };
    Ok(rep)
}

/// One basic-settings poll.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BasicSettingsSample {
    /// Poll number (0-based).
    pub index: u32,
    /// Status byte (not interpreted).
    pub status: u8,
    /// Decoded cells.
    pub cells: Vec<KwpCell>,
    /// The record after the selector echo (status included).
    pub raw: Vec<u8>,
}

/// How a basic-settings run ended.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum BasicSettingsOutcome {
    /// `dry_run`: nothing sent.
    Planned,
    /// Started, polled, stopped.
    Completed,
    /// Polled, but the stop was refused: the module may still be in basic
    /// settings until its session times out.
    StopFailed {
        /// Why.
        message: String,
    },
    /// A poll failed, or the start's answer was lost or malformed (step
    /// `start`: the module may have started). With `session_lost` nothing
    /// more was sent (the module leaves basic settings with the session);
    /// otherwise a stop was sent ([`BasicSettingsReport::stopped`]).
    Aborted {
        /// The step that failed.
        step: String,
        /// Why.
        message: String,
        /// The failure looks like a lost session.
        session_lost: bool,
    },
}

/// The report of a basic-settings run.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BasicSettingsReport {
    /// Channel name.
    pub channel: String,
    /// Group number.
    pub group: u8,
    /// The requests of the run (the plan for a dry run).
    pub plan: Vec<String>,
    /// What the login did.
    pub login: Option<LoginReport>,
    /// The start was acknowledged.
    pub started: bool,
    /// The stop was acknowledged.
    pub stopped: bool,
    /// The measuring-block polls in basic-settings mode.
    pub samples: Vec<BasicSettingsSample>,
    /// What happened.
    pub outcome: BasicSettingsOutcome,
}

/// Run basic settings on a `kwp_basic_settings` channel: session and
/// SecurityAccess, (enumerate,) start `<enter> <bs> <group>`, poll the
/// measuring block `polls` times (`<read> <bs> <group>`, `interval_ms`
/// apart), then **always** stop (`32 <enter> <bs> <group>`) unless the session
/// was lost.
///
/// `Err` only when nothing was started (pre-flight, the gate, the login, the
/// enumerate failed, or the module refused the start with a negative
/// response). A start whose answer was lost or malformed may have started the
/// module: that is `Ok` with [`BasicSettingsOutcome::Aborted`] at step
/// `start` (and [`BasicSettingsReport::started`] false).
pub fn run_basic_settings(
    diag: &mut dyn RawDiag,
    def: &AdaptationDef,
    channel: &str,
    polls: u32,
    interval_ms: u64,
    login: &KwpLogin<'_>,
    opts: &KwpWriteOptions,
) -> Result<BasicSettingsReport> {
    let (group, k) = match lookup(def, channel)? {
        ChannelKind::KwpBasicSettings { group, kwp } => (*group, kwp),
        _ => return Err(mismatch(channel, "kwp_basic_settings")),
    };
    gate(opts, login, "basic settings")?;
    let start = sid::START_ROUTINE_BY_LOCAL_ID;
    let sel = k.basic_selector(group);
    let mut plan = login_plan(login);
    if k.enumerate {
        plan.push(line(start, k.enter, &[0, 0], "enumerate"));
    }
    plan.push(line(start, k.enter, &sel, "start basic settings"));
    plan.push(line(
        start,
        k.read,
        &sel,
        &format!("read measuring block in basic settings (x{polls})"),
    ));
    plan.push(line(
        sid::STOP_ROUTINE_BY_LOCAL_ID,
        k.enter,
        &sel,
        "stop basic settings",
    ));
    let mut rep = BasicSettingsReport {
        channel: channel.to_string(),
        group,
        plan,
        login: None,
        started: false,
        stopped: false,
        samples: Vec::new(),
        outcome: BasicSettingsOutcome::Planned,
    };
    if opts.dry_run {
        return Ok(rep);
    }

    rep.login = Some(establish(diag, login)?);
    if k.enumerate {
        // Some modules refuse the enumerate; only a lost session stops us.
        if let Err(e) = routine(diag, start, k.enter, &[0x00, 0x00]) {
            if is_session_loss(&e) {
                return Err(e);
            }
        }
    }
    if let Err(e) = routine(diag, start, k.enter, &sel) {
        // A negative response started nothing. A lost or malformed answer
        // may belong to a start the module executed: its state is unknown,
        // so this is reported like a failed poll, never as "nothing done".
        if matches!(e, AdaptationError::NegativeResponse { .. }) {
            return Err(e);
        }
        let session_lost = is_session_loss(&e);
        if !session_lost {
            rep.stopped = routine(diag, sid::STOP_ROUTINE_BY_LOCAL_ID, k.enter, &sel).is_ok();
        }
        rep.outcome = BasicSettingsOutcome::Aborted {
            step: "start".into(),
            message: e.to_string(),
            session_lost,
        };
        return Ok(rep);
    }
    rep.started = true;

    let mut failure: Option<(String, AdaptationError)> = None;
    for i in 0..polls {
        if i > 0 && interval_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(interval_ms));
        }
        let r = routine(diag, start, k.read, &sel)
            .and_then(|rest| expect_selector(rest, &sel, "basic settings read"));
        match r {
            Ok(rec) => {
                let (status, body) = match rec.split_first() {
                    Some((&s, body)) => (s, body),
                    None => {
                        failure = Some((
                            format!("poll {i}"),
                            AdaptationError::Protocol("basic settings record is empty".into()),
                        ));
                        break;
                    }
                };
                rep.samples.push(BasicSettingsSample {
                    index: i,
                    status,
                    cells: cells(body),
                    raw: rec.clone(),
                });
            }
            Err(e) => {
                failure = Some((format!("poll {i}"), e));
                break;
            }
        }
    }

    match failure {
        Some((step, e)) if is_session_loss(&e) => {
            rep.outcome = BasicSettingsOutcome::Aborted {
                step,
                message: e.to_string(),
                session_lost: true,
            };
        }
        failure => {
            let stop = routine(diag, sid::STOP_ROUTINE_BY_LOCAL_ID, k.enter, &sel);
            rep.stopped = stop.is_ok();
            rep.outcome = match (failure, stop) {
                (Some((step, e)), _) => BasicSettingsOutcome::Aborted {
                    step,
                    message: e.to_string(),
                    session_lost: false,
                },
                (None, Ok(_)) => BasicSettingsOutcome::Completed,
                (None, Err(e)) => BasicSettingsOutcome::StopFailed {
                    message: e.to_string(),
                },
            };
        }
    }
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addressing_defaults_and_validation() {
        let k = KwpAddressing::default();
        assert_eq!(
            (k.enter, k.select, k.read, k.save),
            (0xB8, 0xB9, 0xBA, 0xBB)
        );
        assert_eq!(k.adaptation_selector, vec![0x01, 0x03]);
        assert!(k.validate("x").is_ok());
        let parsed: KwpAddressing = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed, k);
        let dup = KwpAddressing {
            save: 0xB8,
            ..KwpAddressing::default()
        };
        assert!(dup.validate("x").is_err());
        let empty = KwpAddressing {
            adaptation_selector: vec![],
            ..KwpAddressing::default()
        };
        assert!(empty.validate("x").is_err());
    }

    #[test]
    fn cell_indices_never_wrap() {
        // Regression: the index was `i as u8`, so an oversized record had two
        // cells numbered 0 (and two of every other number after 255).
        let rec: Vec<u8> = (0..300 * 3).map(|i| i as u8).collect();
        let c = cells(&rec);
        assert_eq!(c.len(), MAX_CELLS);
        assert!(c.iter().enumerate().all(|(i, x)| x.index as usize == i));
    }

    #[test]
    fn call_checks_echo_truncation_and_names_nrcs() {
        let mut d = |_: &[u8]| -> std::result::Result<Vec<u8>, String> { Ok(vec![0x7F, 0x31]) };
        assert!(matches!(
            call(&mut d, &[0x31, 0xB8]),
            Err(AdaptationError::Protocol(_))
        ));
        let mut d =
            |_: &[u8]| -> std::result::Result<Vec<u8>, String> { Ok(vec![0x7F, 0x21, 0x31]) };
        assert!(matches!(
            call(&mut d, &[0x31, 0xB8]),
            Err(AdaptationError::Protocol(_))
        ));
        let mut d =
            |_: &[u8]| -> std::result::Result<Vec<u8>, String> { Ok(vec![0x7F, 0x31, 0x80]) };
        let e = call(&mut d, &[0x31, 0xB8]).unwrap_err();
        assert!(is_session_loss(&e));
        assert!(e
            .to_string()
            .contains("serviceNotSupportedInActiveDiagnosticSession"));
        let mut d = |_: &[u8]| -> std::result::Result<Vec<u8>, String> { Ok(vec![0x72, 0xB8]) };
        assert!(matches!(
            routine(&mut d, 0x31, 0xB8, &[]),
            Err(AdaptationError::Protocol(_))
        ));
        let mut d = |_: &[u8]| -> std::result::Result<Vec<u8>, String> { Ok(vec![0x71, 0xB9]) };
        assert!(matches!(
            routine(&mut d, 0x31, 0xB8, &[]),
            Err(AdaptationError::Protocol(_))
        ));
    }
}
