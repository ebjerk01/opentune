//! **Subaru SSM2 logging**: pick the `logger.xml` parameters a connected
//! module supports and poll them with batched `A8` address reads.
//!
//! Everything runs over the same [`RawDiag`] seam as the UDS paths, carrying
//! SSM *payloads* (`A8 00 …` in, `E8 …` out). The application supplies the
//! wire: [`SsmKLine`](opentune_core::protocol::ssm::SsmKLine) over a raw
//! K-line, or an ISO-TP transport on `7E0`/`7E8` for SSM over CAN.
//!
//! 1. [`identify`] sends `BF` and decodes the SSM ID, ROM ID and capability
//!    bitmap.
//! 2. [`select`] keeps the definition's SSM parameters this module can
//!    answer: a standard `<parameter>`/`<switch>` only when its
//!    `ecubyteindex`/`ecubit` capability bit is set, an `<ecuparam>` only for
//!    the matching `<ecu id>` (the ROM ID), and only those whose `target`
//!    includes the module ([`SsmTarget`]). Derived parameters (`<depends>`,
//!    no address of their own) are not evaluated and are left out.
//! 3. A [`LiveSession`](crate::LiveSession) with
//!    [`FastLogConfig::ssm`](crate::FastLogConfig::ssm) set reads the
//!    selection in [`LogMode::Ssm`](crate::LogMode::Ssm): every byte address
//!    the channels cover, de-duplicated, packed into `A8` requests under the
//!    packet limit, then sliced back into channels with the same decoding as
//!    every other path.
//!
//! Nothing here writes: `B8` is never sent.
use crate::live::{blank_sample, decode_memory, Sample};
use crate::model::{Addressing, LogDef, LogParam, ProtocolKind};
use opentune_core::protocol::ssm::{self as wire, SsmInit};
use opentune_core::RawDiag;
use serde::{Deserialize, Serialize};
use std::ops::Range;

/// Which SSM module is being logged.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SsmTarget {
    /// The engine ECU (K-line address `10`, CAN `7E0`).
    #[default]
    Ecu,
    /// The transmission control unit (K-line address `18`, CAN `7E1`).
    Tcu,
}

impl SsmTarget {
    /// The module's K-line address.
    pub fn address(self) -> u8 {
        match self {
            SsmTarget::Ecu => wire::ECU,
            SsmTarget::Tcu => wire::TCU,
        }
    }

    /// The module's ISO-TP request/response ids for SSM over CAN.
    pub fn can_ids(self) -> (u32, u32) {
        match self {
            SsmTarget::Ecu => (0x7E0, 0x7E8),
            SsmTarget::Tcu => (0x7E1, 0x7E9),
        }
    }

    /// The `logger.xml` `target` bit (1 = ECU, 2 = TCU; 3 = both).
    fn target_bit(self) -> u8 {
        match self {
            SsmTarget::Ecu => 1,
            SsmTarget::Tcu => 2,
        }
    }
}

/// How an SSM session reads (see [`FastLogConfig::ssm`](crate::FastLogConfig::ssm)).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SsmPollConfig {
    /// Largest request or reply packet in bytes; `A8` reads split to stay
    /// under it ([`wire::addresses_per_read`]).
    pub max_packet_len: usize,
}

impl Default for SsmPollConfig {
    fn default() -> Self {
        Self {
            max_packet_len: wire::DEFAULT_MAX_PACKET_LEN,
        }
    }
}

impl SsmPollConfig {
    /// Addresses per `A8` request under [`max_packet_len`](Self::max_packet_len).
    pub fn addresses_per_read(&self) -> usize {
        wire::addresses_per_read(self.max_packet_len)
    }

    /// The limit must leave room for at least one address.
    pub fn validate(&self) -> Result<(), String> {
        if self.addresses_per_read() == 0 {
            return Err(format!(
                "an SSM packet limit of {} bytes cannot carry one address (minimum 10)",
                self.max_packet_len
            ));
        }
        Ok(())
    }
}

/// Send `BF` and decode the module's identity and capability bitmap.
pub fn identify(diag: &mut dyn RawDiag) -> Result<SsmInit, String> {
    let resp = diag.request(&[wire::CMD_INIT])?;
    SsmInit::from_reply(&resp).map_err(|e| e.to_string())
}

/// The SSM parameters of a definition that one module can answer.
#[derive(Debug, Clone, PartialEq)]
pub struct SsmSelection {
    /// The definition reduced to those parameters (its SSM protocols only).
    pub def: LogDef,
    /// What was left out and why, one line per reason.
    pub notes: Vec<String>,
}

impl SsmSelection {
    /// Parameters kept.
    pub fn len(&self) -> usize {
        self.def.param_count()
    }

    /// `true` when nothing can be logged.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Does a `<ecu id="A,B,…">` group apply to this ROM ID?
fn rom_matches(group_id: &str, rom: &str) -> bool {
    group_id
        .split(',')
        .any(|id| id.trim().eq_ignore_ascii_case(rom))
}

/// Why a parameter is left out of an SSM selection.
fn drop_reason(p: &LogParam, init: &SsmInit, target: SsmTarget) -> Option<&'static str> {
    if let Some(t) = p.ssm.and_then(|s| s.target) {
        if t & target.target_bit() == 0 {
            return Some("for the other module (target)");
        }
    }
    if let Some((byte, bit)) = p.ssm.and_then(|s| s.capability()) {
        if !init.supports(byte, bit) {
            return Some("not supported by this module (capability bit clear)");
        }
    }
    match &p.addressing {
        Addressing::Memory {
            address, length, ..
        } if *length > 0
            && u64::from(*address) + u64::from(*length) - 1 <= u64::from(wire::MAX_ADDRESS) =>
        {
            None
        }
        Addressing::Memory { .. } => Some("address past the 3-byte SSM range"),
        // Kept for now; `select` drops it again unless everything it
        // references is logged too.
        Addressing::Unknown if crate::derived::is_computable(p) => None,
        _ if !p.depends_on.is_empty() => Some("derived from other parameters (not evaluated)"),
        _ => Some("no SSM address"),
    }
}

/// Keep the definition's SSM parameters that `init`'s module answers for
/// `target`: standard parameters by capability bit, per-ROM `<ecuparam>`s by
/// ROM ID. Other protocols are dropped.
pub fn select(def: &LogDef, init: &SsmInit, target: SsmTarget) -> SsmSelection {
    let rom = init.rom_id_hex();
    let mut out = def.clone();
    let mut dropped: Vec<(&'static str, Vec<String>)> = Vec::new();
    let mut note_drop = |why: &'static str, id: &str| match dropped.iter_mut().find(|d| d.0 == why)
    {
        Some(d) => d.1.push(id.to_string()),
        None => dropped.push((why, vec![id.to_string()])),
    };
    let mut other_protocols = Vec::new();
    let mut other_roms = 0usize;

    out.protocols.retain(|p| {
        let ssm = p.kind == ProtocolKind::SubaruSsm;
        if !ssm {
            other_protocols.push(p.id.clone());
        }
        ssm
    });
    for proto in &mut out.protocols {
        proto.ecus.retain(|g| match &g.id {
            Some(id) if !rom_matches(id, &rom) => {
                other_roms += g.params.len();
                false
            }
            _ => true,
        });
        for group in &mut proto.ecus {
            group.params.retain(|p| match drop_reason(p, init, target) {
                Some(why) => {
                    note_drop(why, &p.id);
                    false
                }
                None => true,
            });
        }
        // Calculated parameters need every parameter they reference; drop
        // the ones whose inputs this module does not log (to a fixed point,
        // for calculated-on-calculated chains).
        loop {
            let kept: std::collections::HashSet<String> = proto
                .ecus
                .iter()
                .flat_map(|g| g.params.iter().map(|p| p.id.clone()))
                .collect();
            let mut changed = false;
            for group in &mut proto.ecus {
                group.params.retain(|p| {
                    let ok = crate::derived::references(p)
                        .iter()
                        .all(|id| kept.contains(id));
                    if !ok {
                        note_drop("derived from parameters this module does not log", &p.id);
                        changed = true;
                    }
                    ok
                });
            }
            if !changed {
                break;
            }
        }
        proto.ecus.retain(|g| !g.params.is_empty());
    }

    let mut notes = Vec::new();
    if !other_protocols.is_empty() {
        notes.push(format!(
            "skipped non-SSM protocol(s): {}",
            other_protocols.join(", ")
        ));
    }
    if other_roms > 0 {
        notes.push(format!(
            "{other_roms} per-ROM parameter(s) for other ROM IDs (this module is {rom})"
        ));
    }
    for (why, ids) in dropped {
        notes.push(format!(
            "{} parameter(s) {why}: {}",
            ids.len(),
            id_list(&ids)
        ));
    }
    SsmSelection { def: out, notes }
}

/// `a, b, c, … (+n more)`.
fn id_list(ids: &[String]) -> String {
    const SHOW: usize = 8;
    let mut s = ids
        .iter()
        .take(SHOW)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if ids.len() > SHOW {
        s.push_str(&format!(", … (+{} more)", ids.len() - SHOW));
    }
    s
}

/// The address layout of an SSM session: every byte address its channels
/// cover (once each, in first-use order) and the `A8` batches over them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SsmPlan {
    addresses: Vec<u32>,
    batches: Vec<Range<usize>>,
    /// Per channel: positions in `addresses` of its bytes, in order; `None`
    /// for a channel with no SSM address.
    slots: Vec<Option<Vec<usize>>>,
    /// Planning caveats (also surfaced in [`LiveStatus::notes`](crate::LiveStatus::notes)).
    pub(crate) notes: Vec<String>,
}

/// Lay out `params` for batched `A8` reads. Batches fill in channel order and
/// a channel's bytes stay in one request where they fit, so a multi-byte
/// value is never assembled from two different moments.
pub(crate) fn plan(params: &[LogParam], cfg: &SsmPollConfig) -> SsmPlan {
    let per = cfg.addresses_per_read().max(1);
    let mut addresses: Vec<u32> = Vec::new();
    let mut index = std::collections::HashMap::new();
    let mut batches: Vec<Range<usize>> = Vec::new();
    let mut notes = Vec::new();
    let mut slots = Vec::with_capacity(params.len());
    for p in params {
        let (address, length) = match &p.addressing {
            Addressing::Memory {
                address, length, ..
            } if *length > 0
                && u64::from(*address) + u64::from(*length) - 1 <= u64::from(wire::MAX_ADDRESS) =>
            {
                (*address, u32::from(*length))
            }
            Addressing::Unknown if crate::derived::is_calculated(p) => {
                // Computed from the other channels after each read.
                slots.push(None);
                continue;
            }
            _ => {
                notes.push(format!("{}: no SSM address; not read", p.id));
                slots.push(None);
                continue;
            }
        };
        let fresh = (0..length)
            .filter(|i| !index.contains_key(&(address + i)))
            .count();
        let open = batches.last().map_or(per, |b| b.len());
        if open + fresh > per && fresh <= per {
            // Start a new request rather than split this channel.
            batches.push(addresses.len()..addresses.len());
        } else if fresh > per {
            notes.push(format!(
                "{}: {length} bytes exceed one A8 read ({per} addresses); read in parts",
                p.id
            ));
        }
        let mut positions = Vec::with_capacity(length as usize);
        for a in (0..length).map(|i| address + i) {
            let pos = *index.entry(a).or_insert_with(|| {
                if !matches!(batches.last(), Some(b) if b.len() < per) {
                    batches.push(addresses.len()..addresses.len());
                }
                addresses.push(a);
                if let Some(b) = batches.last_mut() {
                    b.end += 1;
                }
                addresses.len() - 1
            });
            positions.push(pos);
        }
        slots.push(Some(positions));
    }
    notes.push(format!(
        "SSM: {} address(es) in {} A8 read(s) per sample (≤{per} per read)",
        addresses.len(),
        batches.len()
    ));
    SsmPlan {
        addresses,
        batches,
        slots,
        notes,
    }
}

/// Why an `A8` reply cannot be used.
fn check_a8(resp: &[u8], n: usize) -> Result<(), String> {
    match resp.first() {
        Some(&0xE8) if resp.len() == n + 1 => Ok(()),
        Some(&0xE8) => Err(format!(
            "A8 of {n} address(es) answered with {} byte(s)",
            resp.len() - 1
        )),
        Some(&0x7F) if resp.len() >= 3 => Err(format!(
            "negative response 0x{:02X} ({})",
            resp[2],
            opentune_core::protocol::nrc_name(resp[2])
        )),
        _ => Err(format!("unexpected response {resp:02X?}")),
    }
}

/// Read every planned batch once and decode each channel. Always one
/// [`Sample`] per channel, in `params` order; a failed batch is noted on the
/// channels that need it.
pub(crate) fn tick(diag: &mut dyn RawDiag, plan: &SsmPlan, params: &[LogParam]) -> Vec<Sample> {
    let mut values: Vec<Result<u8, usize>> = vec![Err(0); plan.addresses.len()];
    let mut errors: Vec<String> = Vec::new();
    for range in &plan.batches {
        let batch = &plan.addresses[range.clone()];
        let outcome = wire::read_addresses_payload(batch)
            .map_err(|e| e.to_string())
            .and_then(|req| diag.request(&req))
            .and_then(|resp| check_a8(&resp, batch.len()).map(|()| resp));
        match outcome {
            Ok(resp) => {
                for (slot, b) in values[range.clone()].iter_mut().zip(&resp[1..]) {
                    *slot = Ok(*b);
                }
            }
            Err(e) => {
                errors.push(e);
                let id = errors.len() - 1;
                for slot in &mut values[range.clone()] {
                    *slot = Err(id);
                }
            }
        }
    }
    params
        .iter()
        .zip(&plan.slots)
        .map(|(p, slot)| {
            let mut s = blank_sample(p);
            let Some(positions) = slot else {
                s.note = "no SSM address".into();
                return s;
            };
            let bytes: Result<Vec<u8>, usize> = positions.iter().map(|&i| values[i]).collect();
            match (bytes, &p.addressing) {
                (Ok(data), Addressing::Memory { length, bit, .. }) => {
                    decode_memory(&mut s, p, &data, *length, *bit)
                }
                (Err(e), _) => s.note = errors.get(e).cloned().unwrap_or_default(),
                _ => s.note = "no SSM address".into(),
            }
            s
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Setting a log up over a driver (shared by the CLI and the desktop)
// ---------------------------------------------------------------------------

impl std::str::FromStr for SsmTarget {
    type Err = String;

    /// `ecu`/`engine` or `tcu`/`transmission`, any case.
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ecu" | "engine" => Ok(SsmTarget::Ecu),
            "tcu" | "transmission" => Ok(SsmTarget::Tcu),
            other => Err(format!("SSM target '{other}': expected ecu or tcu")),
        }
    }
}

/// What carries SSM to the module.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SsmWire {
    /// SSM2 packets on the driver's raw K-line
    /// ([`Interface::kline`](opentune_core::Interface::kline)), 4800 baud,
    /// no wake-up.
    #[default]
    KLine,
    /// SSM payloads over the driver's ISO-TP transport on `7E0`/`7E8`
    /// (`7E1`/`7E9` for the TCU).
    Can,
}

impl SsmWire {
    /// Bind a driver's [`ConnectConfig`](opentune_core::ConnectConfig) to
    /// `target` over this wire: on K-line the module address in `tx_id`,
    /// 4800 baud and `init=none` (SSM2 has no wake-up); on CAN the module's
    /// ISO-TP ids.
    pub fn configure(self, cfg: &mut opentune_core::ConnectConfig, target: SsmTarget) {
        match self {
            SsmWire::KLine => {
                cfg.bitrate = wire::BAUD;
                cfg.tx_id = u32::from(target.address());
                cfg.options.insert("init".into(), "none".into());
                cfg.options.insert("baud".into(), wire::BAUD.to_string());
            }
            SsmWire::Can => (cfg.tx_id, cfg.rx_id) = target.can_ids(),
        }
    }

    /// Get an opened driver ready: on K-line it must offer a raw K-line,
    /// which is set to 4800 baud with stale input dropped; on CAN it must
    /// expose a diagnostic transport.
    pub fn prepare(self, drv: &mut dyn opentune_core::Interface) -> Result<(), String> {
        let id = drv.info().id;
        match self {
            SsmWire::KLine => {
                let link = drv.kline().ok_or_else(|| {
                    format!(
                        "driver '{id}' offers no raw K-line; SSM over K-line needs kkl or \
                         kline-bridge (or the ssmsim simulator)"
                    )
                })?;
                link.set_baud(wire::BAUD)
                    .and_then(|()| link.clear_input())
                    .map_err(|e| format!("prepare the K-line for SSM: {e}"))
            }
            SsmWire::Can => drv.diag().map(|_| ()).ok_or_else(|| {
                format!("driver '{id}' does not expose a diagnostic transport for SSM over CAN")
            }),
        }
    }

    /// Run `f` over the SSM payload transport of an opened driver: an
    /// [`SsmKLine`](wire::SsmKLine) to `target` on its raw K-line, or its
    /// diagnostic transport on CAN. `Err` when the driver has neither (it was
    /// closed, or [`prepare`](Self::prepare) would have refused it).
    pub fn with_transport<R>(
        self,
        drv: &mut dyn opentune_core::Interface,
        target: SsmTarget,
        f: impl FnOnce(&mut dyn opentune_core::protocol::DiagTransport) -> R,
    ) -> Result<R, String> {
        match self {
            SsmWire::KLine => {
                let link = drv.kline().ok_or("the K-line is closed")?;
                Ok(f(&mut wire::SsmKLine::new(link, target.address())))
            }
            SsmWire::Can => Ok(f(drv.diag().ok_or("the transport is closed")?)),
        }
    }
}

/// An SSM log set up for one module ([`prepare`]).
#[derive(Debug, Clone, PartialEq)]
pub struct SsmPrepared {
    /// The module's `BF` identification.
    pub init: SsmInit,
    /// The definition reduced to what the module supports.
    pub def: LogDef,
    /// The session config, switched to batched `A8` reads.
    pub cfg: crate::FastLogConfig,
    /// What was left out and why ([`SsmSelection::notes`]).
    pub notes: Vec<String>,
    /// Requested parameter ids that are not logged (no supported SSM
    /// parameter by that id).
    pub not_logged: Vec<String>,
}

/// Identify the module (`BF`), keep the definition's parameters it supports
/// ([`select`]) and switch `cfg` to batched `A8` reads under
/// `max_packet_len`. `ids` are the requested parameter ids (empty: all);
/// those that end up not logged are listed in
/// [`SsmPrepared::not_logged`]. `cfg.enabled` (UDS `0x2C`) is ignored: SSM
/// always batches. Errors when the module does not answer `BF`, nothing can
/// be logged or the packet limit is too small.
pub fn prepare(
    diag: &mut dyn RawDiag,
    def: &LogDef,
    mut cfg: crate::FastLogConfig,
    target: SsmTarget,
    max_packet_len: usize,
    ids: &[&str],
) -> Result<SsmPrepared, String> {
    let init =
        identify(diag).map_err(|e| format!("SSM init (BF) to the {target:?} failed: {e}"))?;
    let sel = select(def, &init, target);
    let not_logged = ids
        .iter()
        .filter(|id| !sel.def.params().any(|(_, _, p)| p.id == **id))
        .map(|id| id.to_string())
        .collect();
    if sel.is_empty() {
        return Err(format!(
            "no SSM parameter in the definition is supported by this module (ROM ID {})",
            init.rom_id_hex()
        ));
    }
    cfg.enabled = false;
    cfg.ssm = Some(SsmPollConfig { max_packet_len });
    cfg.validate()?;
    Ok(SsmPrepared {
        init,
        def: sel.def,
        cfg,
        notes: sel.notes,
        not_logged,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_parse_and_wires_configure() {
        assert_eq!("ECU".parse::<SsmTarget>(), Ok(SsmTarget::Ecu));
        assert_eq!(" transmission ".parse::<SsmTarget>(), Ok(SsmTarget::Tcu));
        assert!("abs"
            .parse::<SsmTarget>()
            .unwrap_err()
            .contains("ecu or tcu"));
        let mut k = opentune_core::ConnectConfig::default();
        SsmWire::KLine.configure(&mut k, SsmTarget::Tcu);
        assert_eq!((k.tx_id, k.bitrate), (0x18, 4800));
        assert_eq!(k.options.get("init").map(String::as_str), Some("none"));
        let mut c = opentune_core::ConnectConfig::default();
        SsmWire::Can.configure(&mut c, SsmTarget::Tcu);
        assert_eq!((c.tx_id, c.rx_id), (0x7E1, 0x7E9));
    }

    #[test]
    fn rom_ids_match_case_insensitively_and_in_lists() {
        assert!(rom_matches("5a5a000001", "5A5A000001"));
        assert!(rom_matches("1111111111, 5A5A000001", "5A5A000001"));
        assert!(!rom_matches("5A5A000002", "5A5A000001"));
    }

    #[test]
    fn poll_config_needs_room_for_an_address() {
        assert!(SsmPollConfig::default().validate().is_ok());
        let tiny = SsmPollConfig { max_packet_len: 9 };
        assert!(tiny.validate().is_err());
    }

    #[test]
    fn ids_list_is_capped() {
        let ids: Vec<String> = (0..10).map(|i| format!("P{i}")).collect();
        assert!(id_list(&ids).ends_with("P7, … (+2 more)"));
    }

    #[test]
    fn a8_replies_are_checked() {
        assert!(check_a8(&[0xE8, 1, 2], 2).is_ok());
        assert!(check_a8(&[0xE8, 1], 2).unwrap_err().contains("1 byte"));
        assert!(check_a8(&[0x7F, 0xA8, 0x22], 1)
            .unwrap_err()
            .contains("conditionsNotCorrect"));
        assert!(check_a8(&[0xE0, 1], 1).unwrap_err().contains("unexpected"));
    }
}
