//! Backend for the open-source **OpenHaldex** AWD controller.
//!
//! An OpenHaldex sits between the car's drivetrain CAN and the Haldex
//! controller. In *Stock* mode it passes frames through; in the other modes it
//! rewrites the lock request the Haldex sees. This module models what its
//! public interfaces carry, written fresh from these MIT-licensed documents of
//! **Forbes-Automotive/OpenHaldex-C6** (no code copied):
//!
//! * README "Modes", "CAN Broadcast / Change Mode": the mode set, the state
//!   broadcast on `0x6B0` and the mode-change command on `0x6A0` (only honoured
//!   when *Broadcast over CAN* is enabled on the unit).
//! * `documents/MOBILE_APP_OPENHALDEX.md`: the BLE GATT contract (mode values
//!   `0..=5`, the 17-byte Status record).
//! * `include/OpenHaldexC6_canID.h` (public source): supplier-specific UDS
//!   identifiers the unit answers on the Haldex diagnostic pair `0x70F`/`0x779`
//!   — `0xFDA0` lock status, `0xFDA1` per-wheel slip (read) and `0xFDA2` mode
//!   (write). These reach the unit from the OBD port only where the gateway
//!   routes `0x70F` to the drivetrain CAN (MQB); on PQ cars connect to the
//!   drivetrain CAN directly.
//!
//! Validation state: **synthetic-tested** only (no OpenHaldex unit on hand).
use opentune_core::RawDiag;
use serde::Serialize;

/// CAN id of the OpenHaldex state broadcast.
pub const BROADCAST_ID: u32 = 0x6B0;
/// CAN id the OpenHaldex accepts mode-change commands on.
pub const COMMAND_ID: u32 = 0x6A0;
/// UDS request id the OpenHaldex answers its supplier DIDs on (the Haldex
/// physical diagnostic address).
pub const UDS_REQ_ID: u32 = 0x70F;
/// UDS response id.
pub const UDS_RESP_ID: u32 = 0x779;
/// RDBI: `[commanded lock %, applied engagement raw, mode]`.
pub const DID_LOCK: u16 = 0xFDA0;
/// RDBI: four signed slip bytes `[FL, FR, RL, RR]`, `-128` = no data.
pub const DID_SLIP: u16 = 0xFDA1;
/// WDBI: `[mode]` sets the drive mode.
pub const DID_MODE: u16 = 0xFDA2;

/// OpenHaldex operating mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// OEM behaviour (pass-through).
    Stock,
    /// Zero lock (front-wheel drive).
    Fwd,
    /// "50:50" — 100 % lock.
    #[serde(rename = "5050")]
    Split5050,
    /// "60:40" — 40 % lock.
    #[serde(rename = "6040")]
    Split6040,
    /// "75:25" — 25 % lock.
    #[serde(rename = "7525")]
    Split7525,
    /// User-defined speed/throttle lock map on the unit.
    Expert,
}

impl Mode {
    /// Every mode, in the unit's index order.
    pub const ALL: [Mode; 6] = [
        Mode::Stock,
        Mode::Fwd,
        Mode::Split5050,
        Mode::Split6040,
        Mode::Split7525,
        Mode::Expert,
    ];

    /// Mode index `0..=5` used by the BLE contract, the UDS DIDs and the
    /// broadcast's mode byte.
    pub fn index(self) -> u8 {
        Mode::ALL.iter().position(|&m| m == self).unwrap_or(0) as u8
    }

    /// Mode for an index `0..=5`.
    pub fn from_index(i: u8) -> Option<Mode> {
        Mode::ALL.get(i as usize).copied()
    }

    /// Value of byte 0 in a `0x6A0` command frame. Note the README's table:
    /// Stock `1`, FWD `2`, 50:50 `3`, 60:40 `4`, 75:25 `5`, Expert `6`
    /// (one higher than [`index`](Self::index)).
    pub fn can_command(self) -> u8 {
        self.index() + 1
    }

    /// Mode for a `0x6A0` command byte.
    pub fn from_can_command(v: u8) -> Option<Mode> {
        v.checked_sub(1).and_then(Mode::from_index)
    }

    /// The fixed lock this mode requests, in percent; `None` for Stock (the
    /// car decides) and Expert (the unit's own map decides).
    pub fn nominal_lock_pct(self) -> Option<u8> {
        match self {
            Mode::Fwd => Some(0),
            Mode::Split7525 => Some(25),
            Mode::Split6040 => Some(40),
            Mode::Split5050 => Some(100),
            Mode::Stock | Mode::Expert => None,
        }
    }

    /// Short stable name (`stock`, `fwd`, `5050`, `6040`, `7525`, `expert`).
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Stock => "stock",
            Mode::Fwd => "fwd",
            Mode::Split5050 => "5050",
            Mode::Split6040 => "6040",
            Mode::Split7525 => "7525",
            Mode::Expert => "expert",
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Mode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let k: String = s
            .trim()
            .to_ascii_lowercase()
            .chars()
            .filter(|c| !matches!(c, ':' | '/' | '-' | '_' | ' '))
            .collect();
        Ok(match k.as_str() {
            "stock" | "oem" => Mode::Stock,
            "fwd" => Mode::Fwd,
            "5050" => Mode::Split5050,
            "6040" => Mode::Split6040,
            "7525" => Mode::Split7525,
            "expert" | "custom" => Mode::Expert,
            _ => {
                return Err(format!(
                    "unknown OpenHaldex mode '{s}' (stock, fwd, 5050, 6040, 7525, expert)"
                ))
            }
        })
    }
}

/// The `0x6A0` frame that requests `mode` (8 bytes, unused bytes zero).
pub fn command_frame(mode: Mode) -> (u32, [u8; 8]) {
    let mut d = [0u8; 8];
    d[0] = mode.can_command();
    (COMMAND_ID, d)
}

/// The OpenHaldex state broadcast (`0x6B0`), as the README documents it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Broadcast {
    /// Byte 1: standalone flags (bitmask for Gen1/2/4/5).
    pub standalone_flags: u8,
    /// Byte 2: engagement requested by the firmware.
    pub requested: u8,
    /// Byte 3: lock percent returned by the differential.
    pub actual_pct: u8,
    /// Byte 4: vehicle speed, km/h.
    pub speed_kmh: u8,
    /// Byte 5: legacy mode-override flag.
    pub mode_override: u8,
    /// Byte 6: mode index `0..=5` (raw, for unknown values).
    pub mode_index: u8,
    /// Byte 6 as a [`Mode`].
    pub mode: Option<Mode>,
    /// Byte 7: driver's pedal, percent.
    pub pedal_pct: u8,
}

impl Broadcast {
    /// Decode an 8-byte broadcast payload (`None` if shorter).
    pub fn decode(d: &[u8]) -> Option<Self> {
        if d.len() < 8 {
            return None;
        }
        Some(Self {
            standalone_flags: d[1],
            requested: d[2],
            actual_pct: d[3],
            speed_kmh: d[4],
            mode_override: d[5],
            mode_index: d[6],
            mode: Mode::from_index(d[6]),
            pedal_pct: d[7],
        })
    }

    /// Encode (used by the simulator and tests).
    pub fn encode(&self) -> [u8; 8] {
        [
            0,
            self.standalone_flags,
            self.requested,
            self.actual_pct,
            self.speed_kmh,
            self.mode_override,
            self.mode_index,
            self.pedal_pct,
        ]
    }
}

/// Status flag names of the BLE Status record, bit order (bits 14-15 reserved).
pub const BLE_STATUS_FLAGS: [&str; 14] = [
    "chassis-can-healthy",
    "haldex-can-healthy",
    "can-bus-failure",
    "tc-off",
    "asr-off",
    "hazards-on",
    "external-button-force",
    "brake-input",
    "handbrake-input",
    "haldex-temp-protection",
    "haldex-coupling-open",
    "haldex-speed-limit",
    "standalone",
    "external-diag-tool",
];

/// The 17-byte BLE Status record (layout version 1).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BleStatus {
    pub sequence: u8,
    pub mode: Option<Mode>,
    pub controller_enabled: bool,
    pub lock_target_pct: u8,
    /// `None` when unknown (Haldex CAN not healthy).
    pub lock_actual_pct: Option<u8>,
    pub speed_kmh: Option<u16>,
    pub throttle_pct: Option<u8>,
    pub rpm: Option<u16>,
    pub boost_raw: Option<u16>,
    /// Steering-wheel angle in degrees (sign direction unverified upstream).
    pub steering_deg: Option<f32>,
    pub flags: u16,
}

impl BleStatus {
    /// Decode a Status notification. Errors on a short payload or a layout
    /// version other than 1 (the contract's "update the app" rule); extra
    /// trailing bytes are ignored.
    pub fn decode(d: &[u8]) -> Result<Self, String> {
        if d.len() < 17 {
            return Err(format!("BLE status too short: {} bytes (need 17)", d.len()));
        }
        if d[0] != 1 {
            return Err(format!("unsupported BLE status layout version {}", d[0]));
        }
        let u16le = |i: usize| u16::from_le_bytes([d[i], d[i + 1]]);
        let opt8 = |v: u8| (v != 0xFF).then_some(v);
        let opt16 = |v: u16| (v != 0xFFFF).then_some(v);
        let steer = i16::from_le_bytes([d[13], d[14]]);
        Ok(Self {
            sequence: d[1],
            mode: Mode::from_index(d[2]),
            controller_enabled: d[3] != 0,
            lock_target_pct: d[4],
            lock_actual_pct: opt8(d[5]),
            speed_kmh: opt16(u16le(6)),
            throttle_pct: opt8(d[8]),
            rpm: opt16(u16le(9)),
            boost_raw: opt16(u16le(11)),
            steering_deg: (steer != i16::MIN).then(|| steer as f32 / 10.0),
            flags: u16le(15),
        })
    }

    /// Names of the set flags.
    pub fn flag_names(&self) -> Vec<&'static str> {
        BLE_STATUS_FLAGS
            .iter()
            .enumerate()
            .filter(|(i, _)| self.flags & (1 << i) != 0)
            .map(|(_, n)| *n)
            .collect()
    }
}

/// Lock status read from DID `0xFDA0`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct LockStatus {
    /// Lock the unit is commanding, percent.
    pub commanded_pct: u8,
    /// Engagement the Haldex reports, raw (generation-specific scale).
    pub engagement_raw: u8,
    /// Live mode index.
    pub mode_index: u8,
    pub mode: Option<Mode>,
}

/// Per-wheel slip read from DID `0xFDA1` (`None` = no data).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Slip {
    pub fl: Option<i8>,
    pub fr: Option<i8>,
    pub rl: Option<i8>,
    pub rr: Option<i8>,
}

/// Errors from the OpenHaldex UDS client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OhError {
    /// The request did not complete (no answer, timeout, I/O).
    Transport(String),
    /// The unit answered `7F <sid> <nrc>`.
    Negative { service: u8, nrc: u8 },
    /// A malformed or mis-correlated reply.
    Protocol(String),
}

impl std::fmt::Display for OhError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OhError::Transport(e) => write!(f, "transport: {e}"),
            OhError::Negative { service, nrc } => {
                write!(f, "negative response to 0x{service:02X}: NRC 0x{nrc:02X}")
            }
            OhError::Protocol(e) => write!(f, "protocol: {e}"),
        }
    }
}

impl std::error::Error for OhError {}

/// UDS client for an OpenHaldex's supplier DIDs, over any [`RawDiag`] bound
/// to [`UDS_REQ_ID`]/[`UDS_RESP_ID`].
pub struct OpenHaldexUds<'a> {
    diag: &'a mut dyn RawDiag,
}

impl<'a> OpenHaldexUds<'a> {
    pub fn new(diag: &'a mut dyn RawDiag) -> Self {
        Self { diag }
    }

    fn exchange(&mut self, req: &[u8]) -> Result<Vec<u8>, OhError> {
        let resp = self.diag.request(req).map_err(OhError::Transport)?;
        match resp.first() {
            Some(0x7F) if resp.len() >= 3 && resp[1] == req[0] => Err(OhError::Negative {
                service: req[0],
                nrc: resp[2],
            }),
            Some(&b) if b == req[0] + 0x40 => Ok(resp),
            _ => Err(OhError::Protocol(format!(
                "unexpected reply {resp:02X?} to {req:02X?}"
            ))),
        }
    }

    fn read_did(&mut self, did: u16) -> Result<Vec<u8>, OhError> {
        let [h, l] = did.to_be_bytes();
        let resp = self.exchange(&[0x22, h, l])?;
        if resp.len() < 3 || resp[1] != h || resp[2] != l {
            return Err(OhError::Protocol(format!(
                "RDBI reply does not echo DID 0x{did:04X}"
            )));
        }
        Ok(resp[3..].to_vec())
    }

    /// Read DID `0xFDA0`. Read-only.
    pub fn read_lock(&mut self) -> Result<LockStatus, OhError> {
        let d = self.read_did(DID_LOCK)?;
        if d.len() < 3 {
            return Err(OhError::Protocol(format!(
                "0xFDA0 carries {} byte(s), need 3",
                d.len()
            )));
        }
        Ok(LockStatus {
            commanded_pct: d[0],
            engagement_raw: d[1],
            mode_index: d[2],
            mode: Mode::from_index(d[2]),
        })
    }

    /// Read DID `0xFDA1`. Read-only.
    pub fn read_slip(&mut self) -> Result<Slip, OhError> {
        let d = self.read_did(DID_SLIP)?;
        if d.len() < 4 {
            return Err(OhError::Protocol(format!(
                "0xFDA1 carries {} byte(s), need 4",
                d.len()
            )));
        }
        let s = |b: u8| (b != 0x80).then_some(b as i8);
        Ok(Slip {
            fl: s(d[0]),
            fr: s(d[1]),
            rl: s(d[2]),
            rr: s(d[3]),
        })
    }

    /// **Write**: set the mode with WDBI `0xFDA2`, then read `0xFDA0` back and
    /// return it. Errors if the unit does not report the requested mode
    /// afterwards (it rejects a mode change while its controller is disabled).
    /// Callers must gate this behind an explicit user confirmation.
    pub fn set_mode(&mut self, mode: Mode) -> Result<LockStatus, OhError> {
        let [h, l] = DID_MODE.to_be_bytes();
        let resp = self.exchange(&[0x2E, h, l, mode.index()])?;
        if resp.len() < 3 || resp[1] != h || resp[2] != l {
            return Err(OhError::Protocol("WDBI reply does not echo 0xFDA2".into()));
        }
        let st = self.read_lock()?;
        if st.mode != Some(mode) {
            return Err(OhError::Protocol(format!(
                "unit reports mode index {} after setting {mode}",
                st.mode_index
            )));
        }
        Ok(st)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_numbering_matches_docs() {
        assert_eq!(Mode::Stock.index(), 0);
        assert_eq!(Mode::Expert.index(), 5);
        assert_eq!(Mode::Stock.can_command(), 1);
        assert_eq!(Mode::Split5050.can_command(), 3);
        assert_eq!(Mode::Expert.can_command(), 6);
        assert_eq!(Mode::from_can_command(4), Some(Mode::Split6040));
        assert_eq!(Mode::from_can_command(0), None);
        assert_eq!(Mode::from_can_command(7), None);
        for m in Mode::ALL {
            assert_eq!(Mode::from_index(m.index()), Some(m));
            assert_eq!(m.as_str().parse::<Mode>(), Ok(m));
        }
        assert_eq!("50:50".parse::<Mode>(), Ok(Mode::Split5050));
        assert_eq!("Custom".parse::<Mode>(), Ok(Mode::Expert));
        assert!("awd".parse::<Mode>().is_err());
        assert_eq!(Mode::Split6040.nominal_lock_pct(), Some(40));
        assert_eq!(Mode::Stock.nominal_lock_pct(), None);
        assert_eq!(command_frame(Mode::Fwd), (0x6A0, [2, 0, 0, 0, 0, 0, 0, 0]));
    }

    #[test]
    fn broadcast_round_trip() {
        let b = Broadcast::decode(&[0, 0x10, 0xC0, 40, 87, 0, 3, 55]).unwrap();
        assert_eq!(b.mode, Some(Mode::Split6040));
        assert_eq!((b.actual_pct, b.speed_kmh, b.pedal_pct), (40, 87, 55));
        assert_eq!(Broadcast::decode(&b.encode()), Some(b));
        assert!(Broadcast::decode(&[0; 7]).is_none());
    }

    #[test]
    fn ble_status_decodes_and_rejects() {
        let mut d = vec![1, 9, 2, 1, 100, 0xFF];
        d.extend_from_slice(&120u16.to_le_bytes());
        d.push(0xFF);
        d.extend_from_slice(&3000u16.to_le_bytes());
        d.extend_from_slice(&0xFFFFu16.to_le_bytes());
        d.extend_from_slice(&(-125i16).to_le_bytes());
        d.extend_from_slice(&0b1_0000_0000_0011u16.to_le_bytes());
        d.push(0xAB); // appended future field is ignored
        let s = BleStatus::decode(&d).unwrap();
        assert_eq!(s.mode, Some(Mode::Split5050));
        assert!(s.controller_enabled);
        assert_eq!(s.lock_actual_pct, None);
        assert_eq!(s.speed_kmh, Some(120));
        assert_eq!(s.throttle_pct, None);
        assert_eq!(s.rpm, Some(3000));
        assert_eq!(s.boost_raw, None);
        assert_eq!(s.steering_deg, Some(-12.5));
        assert_eq!(
            s.flag_names(),
            vec!["chassis-can-healthy", "haldex-can-healthy", "standalone"]
        );
        assert!(BleStatus::decode(&d[..16]).is_err());
        d[0] = 2;
        assert!(BleStatus::decode(&d).is_err());
    }
}
