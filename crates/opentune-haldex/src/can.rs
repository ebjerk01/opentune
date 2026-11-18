//! Passive decoder for Haldex-related CAN frames.
//!
//! Decodes the frames a Haldex controller sends and receives on the
//! drivetrain CAN, plus the OpenHaldex controller's own frames. Nothing here
//! transmits: it turns `(id, data)` into typed values for logs and live
//! monitors.
//!
//! | Id | Name | Platform | Direction | Source |
//! |---|---|---|---|---|
//! | `0x2A0` | `Bremse_4` | PQ | ABS -> Haldex: **lock request** | opendbc `vw_pq.dbc` (MIT) |
//! | `0x2C0` | `Allrad_1` | PQ | Haldex -> car: **lock actual**, status flags | opendbc `vw_pq.dbc`; Gen4 refinements OpenHaldex-C6 (MIT) |
//! | `0x4A0` | `Bremse_3` | PQ | wheel speeds | opendbc `vw_pq.dbc` |
//! | `0x280` | `Motor_1` | PQ | engine speed, pedal | opendbc `vw_pq.dbc` |
//! | `0x118` | `Allrad_03` | MQB | Haldex Gen5 -> car: lock actual, status | OpenHaldex-C6 / -S3 (MIT) |
//! | `0x0B2` | `ESP_19` | MQB | wheel speeds | opendbc `vw_mqb.dbc` (MIT) |
//! | `0x6B0` | OpenHaldex broadcast | any | OpenHaldex state | OpenHaldex-C6 README |
//! | `0x6A0` | OpenHaldex command | any | mode-change request | OpenHaldex-C6 README |
//!
//! PQ and MQB use disjoint ids for these frames, so [`Bus::Auto`] decodes both;
//! pass [`Bus::Pq`] or [`Bus::Mqb`] to ignore the other platform's ids when a
//! bus reuses one of them for something else.
use crate::openhaldex::{self, Broadcast, Mode};
use serde::Serialize;

/// PQ `Bremse_4` — ABS lock request to the Haldex.
pub const ID_BREMSE_4: u32 = 0x2A0;
/// PQ `Allrad_1` — Haldex status (Gen1/2/4).
pub const ID_ALLRAD_1: u32 = 0x2C0;
/// PQ `Bremse_3` — wheel speeds.
pub const ID_BREMSE_3: u32 = 0x4A0;
/// PQ `Motor_1` — engine speed and pedal.
pub const ID_MOTOR_1: u32 = 0x280;
/// MQB `Allrad_03` — Haldex Gen5 status.
pub const ID_ALLRAD_03: u32 = 0x118;
/// MQB `ESP_19` — wheel speeds.
pub const ID_ESP_19: u32 = 0x0B2;

/// Which platform's frame set to decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Bus {
    /// Both PQ and MQB ids (they do not overlap for these frames).
    #[default]
    Auto,
    /// PQ35/PQ46 drivetrain CAN (Haldex Gen1/2/4).
    Pq,
    /// MQB drivetrain CAN (Haldex Gen5).
    Mqb,
}

impl std::str::FromStr for Bus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Bus::Auto),
            "pq" | "pq35" | "pq46" | "gen4" => Ok(Bus::Pq),
            "mqb" | "gen5" => Ok(Bus::Mqb),
            other => Err(format!("unknown bus '{other}' (auto, pq, mqb)")),
        }
    }
}

/// The four wheel speeds in km/h (front-left, front-right, rear-left,
/// rear-right).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct WheelSpeeds {
    pub fl: f32,
    pub fr: f32,
    pub rl: f32,
    pub rr: f32,
}

impl WheelSpeeds {
    /// Mean front minus mean rear speed (km/h): positive = fronts turning
    /// faster, i.e. front slip the coupling would act on.
    pub fn front_rear_delta(&self) -> f32 {
        (self.fl + self.fr) / 2.0 - (self.rl + self.rr) / 2.0
    }

    /// Fastest wheel.
    pub fn max(&self) -> f32 {
        self.fl.max(self.fr).max(self.rl).max(self.rr)
    }
}

/// PQ `Allrad_1` (0x2C0) status flags, byte 0 (names from `vw_pq.dbc`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct Allrad1Flags {
    /// bit 0 `Fehler_Allrad_Kupplung` — coupling fault.
    pub clutch_fault: bool,
    /// bit 1 `Ubertemperaturschutz` — over-temperature protection active.
    pub overtemp_protection: bool,
    /// bit 2 `Fehlerstatus_Kupplungssteifigkeit` — clutch torque signal fault.
    pub torque_signal_fault: bool,
    /// bit 3 `Kupplung_komplett_offen` — coupling fully open.
    pub coupling_open: bool,
    /// bit 4 `Notlauf` — limp / fault reaction.
    pub limp: bool,
    /// bit 5 `Allrad_Warnlampe` — AWD warning lamp request.
    pub warning_lamp: bool,
    /// bit 6 `Geschwindigkeitsbegrenzung` — speed limitation.
    pub speed_limit: bool,
}

impl Allrad1Flags {
    fn from_byte(b: u8) -> Self {
        Self {
            clutch_fault: b & 0x01 != 0,
            overtemp_protection: b & 0x02 != 0,
            torque_signal_fault: b & 0x04 != 0,
            coupling_open: b & 0x08 != 0,
            limp: b & 0x10 != 0,
            warning_lamp: b & 0x20 != 0,
            speed_limit: b & 0x40 != 0,
        }
    }

    /// Names of the flags that are set (for summaries).
    pub fn names(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        for (on, n) in [
            (self.clutch_fault, "clutch-fault"),
            (self.overtemp_protection, "overtemp-protection"),
            (self.torque_signal_fault, "torque-signal-fault"),
            (self.coupling_open, "coupling-open"),
            (self.limp, "limp"),
            (self.warning_lamp, "warning-lamp"),
            (self.speed_limit, "speed-limit"),
        ] {
            if on {
                v.push(n);
            }
        }
        v
    }
}

/// One decoded Haldex-related frame.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum HaldexFrame {
    /// PQ `Bremse_4` (0x2A0): the lock request the ABS (or an OpenHaldex in
    /// its place) sends to the Haldex.
    LockRequestPq {
        /// Byte 0 raw (`ABS_Vorgabewert_mitte_Kupplung`).
        raw: u8,
        /// Byte 0 in the DBC scale, `3 * raw - 381` Nm/min.
        center_nm_per_min: i32,
        /// Byte 0 as a lock percentage on the lever scale OpenHaldex-C6
        /// documents for Gen4 (`<= 0x80` = open, `0x80 + n` linear,
        /// `>= 0xFE` = full). Community confidence.
        lock_pct: f32,
        /// Byte 1 in the DBC scale (`ABS_Vorgabewert_hinten_Kupplung`, 0.7874 %).
        rear_pct: Option<f32>,
    },
    /// PQ `Allrad_1` (0x2C0): the Haldex's own status.
    StatusPq {
        flags: Allrad1Flags,
        /// Byte 1 raw (`Kupplungssteifigkeit_Mitte_Ist`).
        actual_raw: u8,
        /// Byte 1 in the DBC scale, `3 * raw - 381` Nm/min.
        actual_nm_per_min: i32,
        /// Byte 1 as engagement percent on the Gen4 scale (same lever scale as
        /// the request: `0x81..=0xFD` linear, `>= 0xFE` = 100 %). Community
        /// confidence (OpenHaldex-C6).
        engagement_pct: f32,
        /// Byte 3 bit 7 `Fehlerspeichereintrag` — a fault is stored.
        fault_stored: Option<bool>,
        /// Byte 4 raw. `vw_pq.dbc` names it `Kupplungssteifigkeit_Hinten_Ist`
        /// (0.7874 %); OpenHaldex-C6 reads it on Gen4 as the unit temperature.
        byte4_raw: Option<u8>,
        /// Byte 4 read as Gen4 temperature, `raw - 60` °C (`0xFF` = invalid).
        /// Community confidence (OpenHaldex-C6); not meaningful on Gen1/2.
        gen4_temp_c: Option<i16>,
        /// Byte 7 high nibble: alive counter.
        counter: Option<u8>,
    },
    /// MQB `Allrad_03` (0x118): Haldex Gen5 status (OpenHaldex-C6 / -S3).
    StatusMqb {
        counter: u8,
        /// Byte 1 bits 4-5: 0 = normal, 2 = fault reaction (clutch released).
        status: u8,
        /// Byte 1 bit 6: the unit accepts the lock request it receives.
        request_accepted: bool,
        /// Byte 2 raw: 0..=250 = 0..100 % (0.4 %/bit); `> 250` = no value yet.
        actual_raw: u8,
        /// Byte 2 as percent, `None` when the unit reports "no value".
        engagement_pct: Option<f32>,
    },
    /// Wheel speeds (`Bremse_3` on PQ, `ESP_19` on MQB).
    WheelSpeeds(WheelSpeeds),
    /// PQ `Motor_1` (0x280): engine speed and pedal.
    EnginePq { rpm: f32, pedal_pct: f32 },
    /// OpenHaldex state broadcast (0x6B0).
    OpenHaldexBroadcast(Broadcast),
    /// OpenHaldex mode-change command (0x6A0). `None` for an unknown value.
    OpenHaldexCommand { value: u8, mode: Option<Mode> },
}

/// Lock percent on the Gen2/Gen4 lever scale shared by `Bremse_4` byte 0 and
/// `Allrad_1` byte 1: `<= 0x80` = 0 %, `0x81..=0xFD` linear over 125 steps,
/// `>= 0xFE` = 100 %.
pub fn lever_pct(raw: u8) -> f32 {
    match raw {
        0..=0x80 => 0.0,
        0xFE | 0xFF => 100.0,
        r => ((r - 0x80) as f32 * 100.0 / 125.0).min(100.0),
    }
}

/// Inverse of [`lever_pct`]: the lever byte for a lock percentage (0 -> 0x80,
/// 100 -> 0xFE).
pub fn lever_raw(pct: f32) -> u8 {
    let p = pct.clamp(0.0, 100.0);
    if p >= 100.0 {
        0xFE
    } else {
        0x80 + (p * 125.0 / 100.0).round() as u8
    }
}

/// Little-endian (Intel, `@1` in a DBC) unsigned signal: `len` bits from bit
/// `start`. `None` if the frame is too short.
fn bits_le(data: &[u8], start: usize, len: usize) -> Option<u32> {
    if (start + len).div_ceil(8) > data.len() {
        return None;
    }
    let mut v: u64 = 0;
    for (i, b) in data.iter().enumerate().take(8) {
        v |= (*b as u64) << (8 * i);
    }
    Some(((v >> start) & ((1u64 << len) - 1)) as u32)
}

fn pq_ids(bus: Bus) -> bool {
    matches!(bus, Bus::Auto | Bus::Pq)
}

fn mqb_ids(bus: Bus) -> bool {
    matches!(bus, Bus::Auto | Bus::Mqb)
}

/// Decode one frame. `extended` frames never match (all ids here are 11-bit).
/// Returns `None` for an id this module does not know or a frame too short to
/// carry the decoded fields.
pub fn decode_frame(id: u32, extended: bool, data: &[u8], bus: Bus) -> Option<HaldexFrame> {
    if extended {
        return None;
    }
    match id {
        ID_BREMSE_4 if pq_ids(bus) => {
            let raw = *data.first()?;
            Some(HaldexFrame::LockRequestPq {
                raw,
                center_nm_per_min: 3 * raw as i32 - 381,
                lock_pct: lever_pct(raw),
                rear_pct: data.get(1).map(|&b| b as f32 * 0.7874),
            })
        }
        ID_ALLRAD_1 if pq_ids(bus) => {
            if data.len() < 2 {
                return None;
            }
            let actual_raw = data[1];
            let byte4 = data.get(4).copied();
            Some(HaldexFrame::StatusPq {
                flags: Allrad1Flags::from_byte(data[0]),
                actual_raw,
                actual_nm_per_min: 3 * actual_raw as i32 - 381,
                engagement_pct: lever_pct(actual_raw),
                fault_stored: data.get(3).map(|b| b & 0x80 != 0),
                byte4_raw: byte4,
                gen4_temp_c: byte4.filter(|&b| b != 0xFF).map(|b| b as i16 - 60),
                counter: data.get(7).map(|b| b >> 4),
            })
        }
        ID_BREMSE_3 if pq_ids(bus) => {
            // vw_pq.dbc: Radgeschw VL @1, VR @17, HL @33, HR @49, 15 bit, 0.01 km/h.
            let w = |start| bits_le(data, start, 15).map(|v| v as f32 * 0.01);
            Some(HaldexFrame::WheelSpeeds(WheelSpeeds {
                fl: w(1)?,
                fr: w(17)?,
                rl: w(33)?,
                rr: w(49)?,
            }))
        }
        ID_MOTOR_1 if pq_ids(bus) => {
            // vw_pq.dbc: Motordrehzahl @16 16 bit 0.25 rpm; Fahrpedalwert @40 0.4 %.
            Some(HaldexFrame::EnginePq {
                rpm: bits_le(data, 16, 16)? as f32 * 0.25,
                pedal_pct: bits_le(data, 40, 8)? as f32 * 0.4,
            })
        }
        ID_ALLRAD_03 if mqb_ids(bus) => {
            if data.len() < 3 {
                return None;
            }
            let actual_raw = data[2];
            Some(HaldexFrame::StatusMqb {
                counter: data[1] & 0x0F,
                status: (data[1] >> 4) & 0x03,
                request_accepted: data[1] & 0x40 != 0,
                actual_raw,
                engagement_pct: (actual_raw <= 250).then_some(actual_raw as f32 * 0.4),
            })
        }
        ID_ESP_19 if mqb_ids(bus) => {
            // vw_mqb.dbc: HL @0, HR @16, VL @32, VR @48, 16 bit, 0.0075 km/h.
            let w = |start| bits_le(data, start, 16).map(|v| v as f32 * 0.0075);
            Some(HaldexFrame::WheelSpeeds(WheelSpeeds {
                rl: w(0)?,
                rr: w(16)?,
                fl: w(32)?,
                fr: w(48)?,
            }))
        }
        openhaldex::BROADCAST_ID => Broadcast::decode(data).map(HaldexFrame::OpenHaldexBroadcast),
        openhaldex::COMMAND_ID => {
            let value = *data.first()?;
            Some(HaldexFrame::OpenHaldexCommand {
                value,
                mode: Mode::from_can_command(value),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lever_scale_round_trips() {
        assert_eq!(lever_pct(0x7F), 0.0);
        assert_eq!(lever_pct(0x80), 0.0);
        assert_eq!(lever_pct(0xFE), 100.0);
        assert!((lever_pct(0xBF) - 50.4).abs() < 0.01);
        for pct in [0.0, 25.0, 40.0, 60.0, 99.0, 100.0] {
            assert!((lever_pct(lever_raw(pct)) - pct).abs() <= 0.8, "{pct}");
        }
        assert_eq!(lever_raw(150.0), 0xFE);
        assert_eq!(lever_raw(-3.0), 0x80);
    }

    #[test]
    fn allrad_1_decodes_flags_lock_and_temp() {
        // Coupling open (bit 3) + fault stored, idle lock 0x80, temperature 84 °C.
        let f = decode_frame(
            ID_ALLRAD_1,
            false,
            &[0x18, 0x80, 0x00, 0x80, 0x90, 0xC0, 0xFE, 0x73],
            Bus::Pq,
        )
        .unwrap();
        let HaldexFrame::StatusPq {
            flags,
            actual_nm_per_min,
            engagement_pct,
            fault_stored,
            gen4_temp_c,
            counter,
            ..
        } = f
        else {
            panic!("{f:?}")
        };
        assert!(flags.coupling_open && flags.limp && !flags.clutch_fault);
        assert_eq!(flags.names(), vec!["coupling-open", "limp"]);
        assert_eq!(actual_nm_per_min, 3);
        assert_eq!(engagement_pct, 0.0);
        assert_eq!(fault_stored, Some(true));
        assert_eq!(gen4_temp_c, Some(84));
        assert_eq!(counter, Some(7));
        // A 2-byte (Gen1) frame still decodes, without the optional fields.
        let short = decode_frame(ID_ALLRAD_1, false, &[0x00, 0xC6], Bus::Auto).unwrap();
        assert!(matches!(
            short,
            HaldexFrame::StatusPq {
                gen4_temp_c: None,
                counter: None,
                ..
            }
        ));
    }

    #[test]
    fn bremse_4_lock_request() {
        let f = decode_frame(ID_BREMSE_4, false, &[0xFE, 0x00, 0x00], Bus::Auto).unwrap();
        assert!(matches!(f, HaldexFrame::LockRequestPq { lock_pct, .. } if lock_pct == 100.0));
        let f = decode_frame(ID_BREMSE_4, false, &[0x7F], Bus::Pq).unwrap();
        assert!(matches!(
            f,
            HaldexFrame::LockRequestPq { lock_pct, center_nm_per_min: 0, rear_pct: None, .. } if lock_pct == 0.0
        ));
        // MQB filter ignores the PQ id.
        assert!(decode_frame(ID_BREMSE_4, false, &[0xFE], Bus::Mqb).is_none());
    }

    #[test]
    fn pq_wheel_speeds_and_engine() {
        // VL 50.00 km/h = 5000 << 1, the others 0.
        let v = 5000u64 << 1;
        let data = v.to_le_bytes();
        let HaldexFrame::WheelSpeeds(w) = decode_frame(ID_BREMSE_3, false, &data, Bus::Pq).unwrap()
        else {
            panic!()
        };
        assert!((w.fl - 50.0).abs() < 0.01 && w.rr == 0.0);
        assert!((w.front_rear_delta() - 25.0).abs() < 0.01);
        // 3000 rpm = 12000 raw at bytes 2-3, pedal 50 % = 125 at byte 5.
        let mut m = [0u8; 8];
        m[2..4].copy_from_slice(&12000u16.to_le_bytes());
        m[5] = 125;
        assert_eq!(
            decode_frame(ID_MOTOR_1, false, &m, Bus::Auto).unwrap(),
            HaldexFrame::EnginePq {
                rpm: 3000.0,
                pedal_pct: 50.0
            }
        );
        assert!(decode_frame(ID_BREMSE_3, false, &data[..4], Bus::Pq).is_none());
    }

    #[test]
    fn mqb_status_and_wheels() {
        let f = decode_frame(ID_ALLRAD_03, false, &[0x00, 0x45, 125, 0x12], Bus::Mqb).unwrap();
        assert_eq!(
            f,
            HaldexFrame::StatusMqb {
                counter: 5,
                status: 0,
                request_accepted: true,
                actual_raw: 125,
                engagement_pct: Some(50.0)
            }
        );
        let f = decode_frame(ID_ALLRAD_03, false, &[0x00, 0x20, 0xFE], Bus::Auto).unwrap();
        assert!(matches!(
            f,
            HaldexFrame::StatusMqb {
                status: 2,
                engagement_pct: None,
                ..
            }
        ));
        let mut d = [0u8; 8];
        d[4..6].copy_from_slice(&4000u16.to_le_bytes()); // VL 30 km/h
        let HaldexFrame::WheelSpeeds(w) = decode_frame(ID_ESP_19, false, &d, Bus::Mqb).unwrap()
        else {
            panic!()
        };
        assert!((w.fl - 30.0).abs() < 0.01 && w.rl == 0.0);
        assert!(decode_frame(ID_ESP_19, false, &d, Bus::Pq).is_none());
    }

    #[test]
    fn unknown_and_extended_ignored() {
        assert!(decode_frame(0x7E8, false, &[1, 2, 3], Bus::Auto).is_none());
        assert!(decode_frame(ID_ALLRAD_1, true, &[0, 0x80], Bus::Auto).is_none());
        assert!("pq".parse::<Bus>().unwrap() == Bus::Pq && "x".parse::<Bus>().is_err());
    }
}
