//! Wire codec for the **esp32-isotp-ble-bridge** host protocol.
//!
//! Dependency-free (std only) so the desktop/CLI driver, the tests and — later —
//! the Android app can share it. Written from the firmware's public source and
//! README (Switchleg1/esp32-isotp-ble-bridge, MIT; see
//! `docs/ISOTP-BLE-BRIDGE.md` for the protocol note and attribution). No
//! firmware code is copied; this is an independent implementation of the same
//! byte layout.
//!
//! ## Frame layout
//!
//! Every message, in both directions and on both transports (BLE GATT and the
//! USB serial port), is an 8-byte little-endian header followed by `len` bytes:
//!
//! | offset | size | field   | meaning |
//! |-------:|-----:|---------|---------|
//! | 0      | 1    | `0xF1`  | header id |
//! | 1      | 1    | flags   | command flags (`FLAG_*`), `0` for plain ISO-TP data |
//! | 2      | 2    | `rx_id` | the CAN id on the *other* side of this payload |
//! | 4      | 2    | `tx_id` | the CAN id that carries (or carried) this payload |
//! | 6      | 2    | `len`   | payload length |
//!
//! A tester request to the engine ECU therefore has `tx_id = 0x7E0`,
//! `rx_id = 0x7E8`; the ECU's answer comes back with `tx_id = 0x7E8`,
//! `rx_id = 0x7E0`. Several frames may be concatenated in one BLE write or
//! notification.
//!
//! ## Split packets (BLE only)
//!
//! A frame longer than one ATT payload (`MTU - 3`) is cut into chunks: the
//! first chunk is the first `MTU - 3` bytes of the encoded frame with
//! [`FLAG_SPLIT`] OR-ed into the flags byte; every further chunk is
//! `[0xF2, seq, data…]` with `seq` counting up from 1 (wrapping as a `u8`).
//! The receiver appends chunks until it holds `len` payload bytes. The same
//! scheme is used host→device and device→host. The serial transport is a plain
//! byte stream and never splits.

use std::fmt;

/// First byte of every frame header.
pub const HEADER_ID: u8 = 0xF1;
/// First byte of every continuation chunk of a split frame.
pub const PARTIAL_ID: u8 = 0xF2;
/// Encoded header length.
pub const HEADER_LEN: usize = 8;

/// Persist mode on (with [`FLAG_PERSIST_ADD`] frames queued first).
pub const FLAG_PERSIST_ENABLE: u8 = 0x01;
/// Clear the persist message list.
pub const FLAG_PERSIST_CLEAR: u8 = 0x02;
/// Add this frame's payload to the persist list of its link.
pub const FLAG_PERSIST_ADD: u8 = 0x04;
/// First chunk of a split frame (more chunks follow).
pub const FLAG_SPLIT: u8 = 0x08;
/// Settings read (together with [`FLAG_SETTINGS`]).
pub const FLAG_SETTINGS_GET: u8 = 0x40;
/// Settings frame; the low bits name the [`Setting`].
pub const FLAG_SETTINGS: u8 = 0x80;

/// 16-bit GATT UUID of the bridge's primary service.
pub const SERVICE_UUID16: u16 = 0xABF0;
/// Characteristic the host writes frames to (write without response).
pub const DATA_WRITE_UUID16: u16 = 0xABF1;
/// Characteristic the device notifies frames on.
pub const DATA_NOTIFY_UUID16: u16 = 0xABF2;
/// "Command" characteristic: present but ignored by the firmware.
pub const COMMAND_UUID16: u16 = 0xABF3;
/// "Status" characteristic: present, never notified by the firmware.
pub const STATUS_UUID16: u16 = 0xABF4;

/// Default advertised GAP name (changeable with [`Setting::GapName`]).
pub const DEFAULT_DEVICE_NAME: &str = "BLE_TO_ISOTP20";
/// Longest GAP name the firmware accepts.
pub const MAX_GAP_NAME: usize = 14;
/// Serial (USB-UART) baud rate of the firmware.
pub const SERIAL_BAUD: u32 = 250_000;
/// Default ATT MTU before the central negotiates a larger one.
pub const DEFAULT_ATT_MTU: usize = 23;
/// Largest characteristic value the firmware declares (its write buffer).
pub const MAX_ATT_VALUE: usize = 512;
/// The firmware's fixed CAN bit rate.
pub const CAN_BITRATE: u32 = 500_000;
/// Largest classic ISO-TP payload (12-bit length).
pub const MAX_ISOTP_PAYLOAD: usize = 4095;

/// 128-bit form of a Bluetooth SIG 16-bit UUID
/// (`0000xxxx-0000-1000-8000-00805F9B34FB`).
pub const fn uuid16_to_u128(short: u16) -> u128 {
    0x0000_0000_0000_1000_8000_0080_5F9B_34FB | ((short as u128) << 96)
}

/// One ISO-TP link the firmware has compiled in. Requests for any other
/// tx/rx pair are dropped silently by the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkDef {
    /// Short name used in messages.
    pub name: &'static str,
    /// Tester → ECU CAN id.
    pub tx_id: u16,
    /// ECU → tester CAN id.
    pub rx_id: u16,
    /// Largest payload the device buffers for this link, either direction.
    pub max_payload: usize,
}

/// The link table of firmware v1.03 (`isotp_link_containers.c`).
pub const LINKS: [LinkDef; 4] = [
    LinkDef {
        name: "ecu",
        tx_id: 0x7E0,
        rx_id: 0x7E8,
        max_payload: MAX_ISOTP_PAYLOAD,
    },
    LinkDef {
        name: "tcu",
        tx_id: 0x7E1,
        rx_id: 0x7E9,
        max_payload: MAX_ISOTP_PAYLOAD,
    },
    LinkDef {
        name: "haldex",
        tx_id: 0x7E5,
        rx_id: 0x7ED,
        max_payload: MAX_ISOTP_PAYLOAD,
    },
    LinkDef {
        name: "dtc",
        tx_id: 0x700,
        rx_id: 0x7E8,
        max_payload: 511,
    },
];

/// The compiled-in link for a tester `tx_id` / ECU `rx_id` pair, if any.
pub fn find_link(tx_id: u16, rx_id: u16) -> Option<&'static LinkDef> {
    LINKS.iter().find(|l| l.tx_id == tx_id && l.rx_id == rx_id)
}

/// A device setting, numbered as the firmware's `BRG_SETTING_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Setting {
    /// Per-link override of the separation time between the consecutive
    /// frames the device sends, in **microseconds** (`u16`; 0 = use the ECU's
    /// flow-control STmin). Addressed by the link's tx/rx ids.
    IsoTpStminUs = 1,
    /// Status LED colour, `0x00RRGGBB` (`u32`).
    LedColor = 2,
    /// Persist mode: delay between replayed messages, ms (`u16`).
    PersistDelayMs = 3,
    /// Persist mode: extra delay per queued BLE message, ms (`u16`).
    PersistQueueDelayMs = 4,
    /// Delay after each BLE notification burst, FreeRTOS ticks (`u16`).
    BleSendDelay = 5,
    /// How long to wait for more replies to pack into one notification,
    /// FreeRTOS ticks (`u16`).
    BleMultiDelay = 6,
    /// Password (string, ≤ 64 bytes). Only acted on by firmware built with
    /// `PASSWORD_CHECK`, which the published build is not.
    Password = 7,
    /// Advertised BLE name (ASCII, ≤ 14 bytes), stored in NVS.
    GapName = 8,
}

impl Setting {
    /// Every setting, in firmware order.
    pub const ALL: [Setting; 8] = [
        Setting::IsoTpStminUs,
        Setting::LedColor,
        Setting::PersistDelayMs,
        Setting::PersistQueueDelayMs,
        Setting::BleSendDelay,
        Setting::BleMultiDelay,
        Setting::Password,
        Setting::GapName,
    ];

    /// The setting with this number.
    pub fn from_u8(v: u8) -> Option<Setting> {
        Setting::ALL.iter().copied().find(|s| *s as u8 == v)
    }

    /// Exact value length the firmware accepts on a set, or `None` for the
    /// variable-length string settings.
    pub fn value_len(self) -> Option<usize> {
        match self {
            Setting::LedColor => Some(4),
            Setting::Password | Setting::GapName => None,
            _ => Some(2),
        }
    }

    /// Largest value for the string settings.
    pub fn max_len(self) -> usize {
        match self {
            Setting::Password => 64,
            Setting::GapName => MAX_GAP_NAME,
            other => other.value_len().unwrap_or(0),
        }
    }

    /// True when the setting is addressed by a link's tx/rx ids (and its read
    /// reply carries those ids); the others use ids 0/0.
    pub fn per_link(self) -> bool {
        self == Setting::IsoTpStminUs
    }

    /// Whether the firmware answers a read of this setting.
    pub fn readable(self) -> bool {
        self != Setting::Password
    }
}

/// A codec error. Each names what was wrong with the bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    /// Fewer than [`HEADER_LEN`] bytes where a header was expected.
    ShortHeader(usize),
    /// The first byte was not [`HEADER_ID`].
    BadHeaderId(u8),
    /// The header announced more payload than the packet holds and was not
    /// marked [`FLAG_SPLIT`].
    Truncated { announced: usize, available: usize },
    /// A continuation chunk arrived out of order.
    OutOfOrder { expected: u8, got: u8 },
    /// A continuation chunk arrived with no split frame in progress.
    UnexpectedContinuation,
    /// A split frame grew past its announced length.
    Overrun { announced: usize, got: usize },
    /// A payload too long for the 16-bit length field.
    TooLong(usize),
    /// An ATT payload too small to carry a header plus data.
    MtuTooSmall(usize),
    /// A setting value of the wrong length.
    BadSettingValue { setting: Setting, len: usize },
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodecError::ShortHeader(n) => write!(f, "short header ({n} of {HEADER_LEN} bytes)"),
            CodecError::BadHeaderId(b) => {
                write!(f, "bad header id 0x{b:02X} (expected 0x{HEADER_ID:02X})")
            }
            CodecError::Truncated {
                announced,
                available,
            } => write!(
                f,
                "frame announces {announced} payload bytes but only {available} arrived"
            ),
            CodecError::OutOfOrder { expected, got } => {
                write!(f, "split chunk {got} out of order (expected {expected})")
            }
            CodecError::UnexpectedContinuation => {
                write!(f, "continuation chunk without a split frame in progress")
            }
            CodecError::Overrun { announced, got } => {
                write!(
                    f,
                    "split frame overran: {got} bytes for {announced} announced"
                )
            }
            CodecError::TooLong(n) => write!(f, "payload of {n} bytes is too long"),
            CodecError::MtuTooSmall(n) => write!(f, "ATT payload of {n} bytes is too small"),
            CodecError::BadSettingValue { setting, len } => {
                write!(
                    f,
                    "setting {setting:?}: value of {len} bytes is not accepted"
                )
            }
        }
    }
}

impl std::error::Error for CodecError {}

/// The 8-byte frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Header {
    pub flags: u8,
    pub rx_id: u16,
    pub tx_id: u16,
    pub len: u16,
}

impl Header {
    /// Encode to wire bytes.
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let r = self.rx_id.to_le_bytes();
        let t = self.tx_id.to_le_bytes();
        let l = self.len.to_le_bytes();
        [HEADER_ID, self.flags, r[0], r[1], t[0], t[1], l[0], l[1]]
    }

    /// Decode the first [`HEADER_LEN`] bytes of `b`.
    pub fn decode(b: &[u8]) -> Result<Header, CodecError> {
        if b.len() < HEADER_LEN {
            return Err(CodecError::ShortHeader(b.len()));
        }
        if b[0] != HEADER_ID {
            return Err(CodecError::BadHeaderId(b[0]));
        }
        Ok(Header {
            flags: b[1],
            rx_id: u16::from_le_bytes([b[2], b[3]]),
            tx_id: u16::from_le_bytes([b[4], b[5]]),
            len: u16::from_le_bytes([b[6], b[7]]),
        })
    }

    /// The setting a settings frame addresses (flags with the settings/get
    /// bits masked off), if this is one. Setting 8 shares its bit with
    /// [`FLAG_SPLIT`], which is why settings frames are never split.
    pub fn setting(&self) -> Option<Setting> {
        if self.flags & FLAG_SETTINGS == 0 {
            return None;
        }
        Setting::from_u8(self.flags & !(FLAG_SETTINGS | FLAG_SETTINGS_GET))
    }
}

/// One complete frame: header + payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub header: Header,
    pub payload: Vec<u8>,
}

impl Packet {
    fn new(flags: u8, tx_id: u16, rx_id: u16, payload: Vec<u8>) -> Result<Packet, CodecError> {
        let len = u16::try_from(payload.len()).map_err(|_| CodecError::TooLong(payload.len()))?;
        Ok(Packet {
            header: Header {
                flags,
                rx_id,
                tx_id,
                len,
            },
            payload,
        })
    }

    /// ISO-TP payload to send on `tx_id`, with the answer expected on `rx_id`.
    pub fn data(tx_id: u16, rx_id: u16, payload: &[u8]) -> Result<Packet, CodecError> {
        Packet::new(0, tx_id, rx_id, payload.to_vec())
    }

    /// Set a device setting. `tx_id`/`rx_id` select the link for
    /// [`Setting::IsoTpStminUs`] and are ignored (send 0) for the others.
    pub fn set_setting(
        setting: Setting,
        tx_id: u16,
        rx_id: u16,
        value: &[u8],
    ) -> Result<Packet, CodecError> {
        let ok = match setting.value_len() {
            Some(n) => value.len() == n,
            None => value.len() <= setting.max_len(),
        };
        if !ok {
            return Err(CodecError::BadSettingValue {
                setting,
                len: value.len(),
            });
        }
        Packet::new(FLAG_SETTINGS | setting as u8, tx_id, rx_id, value.to_vec())
    }

    /// Read a device setting (empty payload; see [`Setting::per_link`]).
    pub fn get_setting(setting: Setting, tx_id: u16, rx_id: u16) -> Packet {
        Packet {
            header: Header {
                flags: FLAG_SETTINGS | FLAG_SETTINGS_GET | setting as u8,
                rx_id,
                tx_id,
                len: 0,
            },
            payload: Vec::new(),
        }
    }

    /// Per-link STmin override for the frames the device sends, µs.
    pub fn set_stmin_us(tx_id: u16, rx_id: u16, us: u16) -> Packet {
        Packet::set_setting(Setting::IsoTpStminUs, tx_id, rx_id, &us.to_le_bytes())
            .expect("fixed-size value")
    }

    /// A `u16` global setting.
    pub fn set_u16(setting: Setting, value: u16) -> Result<Packet, CodecError> {
        Packet::set_setting(setting, 0, 0, &value.to_le_bytes())
    }

    /// Status LED colour `0x00RRGGBB`.
    pub fn set_led_color(rgb: u32) -> Packet {
        Packet::set_setting(Setting::LedColor, 0, 0, &rgb.to_le_bytes()).expect("fixed-size value")
    }

    /// New advertised BLE name (stored in the device's NVS).
    pub fn set_gap_name(name: &str) -> Result<Packet, CodecError> {
        Packet::set_setting(Setting::GapName, 0, 0, name.as_bytes())
    }

    /// Queue `payload` for replay on the `tx_id`/`rx_id` link in persist mode.
    pub fn persist_add(tx_id: u16, rx_id: u16, payload: &[u8]) -> Result<Packet, CodecError> {
        Packet::new(FLAG_PERSIST_ADD, tx_id, rx_id, payload.to_vec())
    }

    /// Start persist mode (replays the queued messages until stopped).
    pub fn persist_start() -> Packet {
        Packet {
            header: Header {
                flags: FLAG_PERSIST_ENABLE,
                ..Header::default()
            },
            payload: Vec::new(),
        }
    }

    /// Stop persist mode, optionally clearing the queued messages.
    pub fn persist_stop(clear: bool) -> Packet {
        Packet {
            header: Header {
                flags: if clear { FLAG_PERSIST_CLEAR } else { 0 },
                ..Header::default()
            },
            payload: Vec::new(),
        }
    }

    /// Encode header + payload.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        let mut h = self.header;
        h.len = self.payload.len() as u16;
        out.extend_from_slice(&h.encode());
        out.extend_from_slice(&self.payload);
        out
    }
}

/// Cut one encoded frame into BLE writes/notifications of at most
/// `att_payload` bytes (ATT MTU − 3), using the split-packet scheme. A frame
/// that fits is returned as one chunk, unchanged.
pub fn split(encoded: &[u8], att_payload: usize) -> Result<Vec<Vec<u8>>, CodecError> {
    if att_payload < HEADER_LEN + 1 {
        return Err(CodecError::MtuTooSmall(att_payload));
    }
    if encoded.len() <= att_payload {
        return Ok(vec![encoded.to_vec()]);
    }
    let mut first = encoded[..att_payload].to_vec();
    first[1] |= FLAG_SPLIT;
    let mut out = vec![first];
    let mut seq: u8 = 1;
    for chunk in encoded[att_payload..].chunks(att_payload - 2) {
        let mut c = Vec::with_capacity(chunk.len() + 2);
        c.push(PARTIAL_ID);
        c.push(seq);
        c.extend_from_slice(chunk);
        out.push(c);
        seq = seq.wrapping_add(1);
    }
    Ok(out)
}

/// Encode a frame and [`split`] it for one ATT payload size.
pub fn encode_for_ble(packet: &Packet, att_payload: usize) -> Result<Vec<Vec<u8>>, CodecError> {
    split(&packet.encode(), att_payload)
}

/// Pack several encoded frames into as few BLE values as possible, like the
/// firmware's "multisend": frames are concatenated while they fit in one
/// `att_payload`; a frame that does not fit on its own is [`split`].
pub fn pack_for_ble(packets: &[Packet], att_payload: usize) -> Result<Vec<Vec<u8>>, CodecError> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    for p in packets {
        let enc = p.encode();
        if enc.len() > att_payload {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            out.extend(split(&enc, att_payload)?);
        } else if cur.len() + enc.len() <= att_payload {
            cur.extend_from_slice(&enc);
        } else {
            out.push(std::mem::replace(&mut cur, enc));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

/// Reassembles frames from BLE values (writes or notifications): handles
/// several frames per value and split frames across values. A reassembled
/// split frame keeps [`FLAG_SPLIT`] in its flags, exactly as the firmware's
/// own reassembly does (which matters for settings, see [`Header::setting`]).
#[derive(Debug, Default)]
pub struct Reassembler {
    pending: Option<Pending>,
}

#[derive(Debug)]
struct Pending {
    header: Header,
    data: Vec<u8>,
    next_seq: u8,
}

impl Reassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// True while a split frame is waiting for more chunks.
    pub fn in_progress(&self) -> bool {
        self.pending.is_some()
    }

    /// Drop any partial split frame.
    pub fn reset(&mut self) {
        self.pending = None;
    }

    /// Feed one BLE value. Complete frames are appended to `out` (also those
    /// parsed before an error). On error the rest of the value and any split
    /// frame in progress are dropped, as the firmware does.
    pub fn push(&mut self, value: &[u8], out: &mut Vec<Packet>) -> Result<(), CodecError> {
        if value.is_empty() {
            return Ok(());
        }
        if value[0] == PARTIAL_ID {
            let Some(p) = self.pending.as_mut() else {
                return Err(CodecError::UnexpectedContinuation);
            };
            let got = value.get(1).copied().unwrap_or(0);
            if got != p.next_seq {
                let expected = p.next_seq;
                self.pending = None;
                return Err(CodecError::OutOfOrder { expected, got });
            }
            p.data.extend_from_slice(&value[2.min(value.len())..]);
            p.next_seq = p.next_seq.wrapping_add(1);
            let announced = p.header.len as usize;
            if p.data.len() == announced {
                let p = self.pending.take().expect("pending");
                out.push(Packet {
                    header: p.header,
                    payload: p.data,
                });
            } else if p.data.len() > announced {
                let got = p.data.len();
                self.pending = None;
                return Err(CodecError::Overrun { announced, got });
            }
            return Ok(());
        }
        // Any non-continuation value ends a split frame in progress.
        self.pending = None;
        let mut rest = value;
        while !rest.is_empty() {
            let header = Header::decode(rest)?;
            rest = &rest[HEADER_LEN..];
            let announced = header.len as usize;
            if announced > rest.len() {
                if header.flags & FLAG_SPLIT == 0 {
                    return Err(CodecError::Truncated {
                        announced,
                        available: rest.len(),
                    });
                }
                self.pending = Some(Pending {
                    header,
                    data: rest.to_vec(),
                    next_seq: 1,
                });
                return Ok(());
            }
            out.push(Packet {
                header,
                payload: rest[..announced].to_vec(),
            });
            rest = &rest[announced..];
        }
        Ok(())
    }
}

/// Decodes frames from a byte stream (the serial transport): bytes arrive in
/// arbitrary pieces; frames are delimited by their header length only.
#[derive(Debug, Default)]
pub struct StreamDecoder {
    buf: Vec<u8>,
}

/// Largest frame the firmware's UART receive buffer holds.
pub const MAX_STREAM_FRAME: usize = 8192;

impl StreamDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bytes held for an incomplete frame.
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// Feed bytes; complete frames are appended to `out`. Garbage before a
    /// header is skipped (resynchronising on the next `0xF1`) and reported
    /// once per call as an error after the good frames were delivered.
    pub fn push(&mut self, bytes: &[u8], out: &mut Vec<Packet>) -> Result<(), CodecError> {
        self.buf.extend_from_slice(bytes);
        let mut err = None;
        loop {
            if self.buf.is_empty() {
                break;
            }
            if self.buf[0] != HEADER_ID {
                let skip = self
                    .buf
                    .iter()
                    .position(|&b| b == HEADER_ID)
                    .unwrap_or(self.buf.len());
                err.get_or_insert(CodecError::BadHeaderId(self.buf[0]));
                self.buf.drain(..skip);
                continue;
            }
            if self.buf.len() < HEADER_LEN {
                break;
            }
            let header = Header::decode(&self.buf)?;
            let total = HEADER_LEN + header.len as usize;
            if total > MAX_STREAM_FRAME {
                err.get_or_insert(CodecError::TooLong(header.len as usize));
                self.buf.drain(..1);
                continue;
            }
            if self.buf.len() < total {
                break;
            }
            let payload = self.buf[HEADER_LEN..total].to_vec();
            self.buf.drain(..total);
            out.push(Packet { header, payload });
        }
        match err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_layout_matches_the_firmware_struct() {
        // Request for the engine ECU: UDS 22 F1 90 on 0x7E0, answer on 0x7E8.
        let p = Packet::data(0x7E0, 0x7E8, &[0x22, 0xF1, 0x90]).unwrap();
        assert_eq!(
            p.encode(),
            vec![0xF1, 0x00, 0xE8, 0x07, 0xE0, 0x07, 0x03, 0x00, 0x22, 0xF1, 0x90]
        );
        let h = Header::decode(&p.encode()).unwrap();
        assert_eq!(h.tx_id, 0x7E0);
        assert_eq!(h.rx_id, 0x7E8);
        assert_eq!(h.len, 3);
    }

    #[test]
    fn settings_frames() {
        // STmin override 350 µs on the ECU link.
        let p = Packet::set_stmin_us(0x7E0, 0x7E8, 350);
        assert_eq!(
            p.encode(),
            vec![0xF1, 0x81, 0xE8, 0x07, 0xE0, 0x07, 0x02, 0x00, 0x5E, 0x01]
        );
        let g = Packet::get_setting(Setting::GapName, 0, 0);
        assert_eq!(g.encode(), vec![0xF1, 0xC8, 0, 0, 0, 0, 0, 0]);
        assert_eq!(g.header.setting(), Some(Setting::GapName));
        let led = Packet::set_led_color(0x00FF8000);
        assert_eq!(led.encode()[1], 0x82);
        assert_eq!(&led.encode()[8..], &[0x00, 0x80, 0xFF, 0x00]);
        assert!(Packet::set_gap_name("A_NAME_THAT_IS_TOO_LONG").is_err());
        assert!(Packet::set_setting(Setting::BleSendDelay, 0, 0, &[1]).is_err());
        assert_eq!(Packet::persist_start().encode()[1], FLAG_PERSIST_ENABLE);
        assert_eq!(Packet::persist_stop(true).encode()[1], FLAG_PERSIST_CLEAR);
        assert_eq!(Packet::persist_stop(false).encode()[1], 0);
        let add = Packet::persist_add(0x7E0, 0x7E8, &[0x22, 0xF4, 0x0C]).unwrap();
        assert_eq!(add.encode()[1], FLAG_PERSIST_ADD);
    }

    #[test]
    fn uuid_expansion() {
        assert_eq!(
            uuid16_to_u128(DATA_WRITE_UUID16),
            0x0000ABF1_0000_1000_8000_00805F9B34FB
        );
    }

    #[test]
    fn split_matches_the_documented_scheme_at_the_default_mtu() {
        // 31-byte payload -> 39 encoded bytes; ATT payload 20.
        let payload: Vec<u8> = (0..31).collect();
        let p = Packet::data(0x7E0, 0x7E8, &payload).unwrap();
        let chunks = encode_for_ble(&p, DEFAULT_ATT_MTU - 3).unwrap();
        assert_eq!(chunks.len(), 3);
        // First chunk: the header with the split flag, then 12 payload bytes.
        assert_eq!(
            &chunks[0][..8],
            &[0xF1, 0x08, 0xE8, 0x07, 0xE0, 0x07, 31, 0]
        );
        assert_eq!(&chunks[0][8..], &payload[..12]);
        // Then [F2, seq, up to 18 bytes].
        assert_eq!(chunks[1][..2], [0xF2, 1]);
        assert_eq!(&chunks[1][2..], &payload[12..30]);
        assert_eq!(chunks[2], vec![0xF2, 2, 30]);
    }

    #[test]
    fn split_does_not_emit_an_empty_tail() {
        let payload: Vec<u8> = (0..30).collect();
        let enc = Packet::data(0x7E0, 0x7E8, &payload).unwrap().encode();
        let chunks = split(&enc, 20).unwrap();
        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().all(|c| c.len() <= 20));
    }

    #[test]
    fn split_round_trips_through_the_reassembler_for_many_sizes() {
        for att in [9usize, 20, 23, 100, 182, 244, 509] {
            for n in [0usize, 1, 7, 12, 13, 50, 255, 256, 1000, 4095] {
                let payload: Vec<u8> = (0..n).map(|i| (i * 7 + n) as u8).collect();
                let p = Packet::data(0x7E1, 0x7E9, &payload).unwrap();
                let chunks = encode_for_ble(&p, att).unwrap();
                assert!(chunks.iter().all(|c| c.len() <= att), "att {att} n {n}");
                let mut r = Reassembler::new();
                let mut out = Vec::new();
                for c in &chunks {
                    r.push(c, &mut out).unwrap();
                }
                assert_eq!(out.len(), 1, "att {att} n {n}");
                let split_used = chunks.len() > 1;
                assert_eq!(out[0].header.flags, if split_used { FLAG_SPLIT } else { 0 });
                assert_eq!(out[0].payload, p.payload, "att {att} n {n}");
                assert_eq!((out[0].header.tx_id, out[0].header.rx_id), (0x7E1, 0x7E9));
                assert!(!r.in_progress());
            }
        }
    }

    #[test]
    fn sequence_numbers_wrap_like_a_u8() {
        let payload = vec![0x55u8; 4095];
        let chunks = split(&Packet::data(0x7E0, 0x7E8, &payload).unwrap().encode(), 9).unwrap();
        assert!(chunks.len() > 300);
        assert_eq!(chunks[255][1], 255);
        assert_eq!(chunks[256][1], 0);
        let mut r = Reassembler::new();
        let mut out = Vec::new();
        for c in &chunks {
            r.push(c, &mut out).unwrap();
        }
        assert_eq!(out[0].payload, payload);
    }

    #[test]
    fn several_frames_in_one_notification() {
        let a = Packet::data(0x7E8, 0x7E0, &[0x62, 0xF1, 0x90]).unwrap();
        let b = Packet::data(0x7E9, 0x7E1, &[0x7E, 0x00]).unwrap();
        // 11 + 10 bytes: two frames in one value once the MTU allows it.
        assert_eq!(pack_for_ble(&[a.clone(), b.clone()], 20).unwrap().len(), 2);
        let packed = pack_for_ble(&[a.clone(), b.clone()], 21).unwrap();
        assert_eq!(packed.len(), 1);
        let mut out = Vec::new();
        Reassembler::new().push(&packed[0], &mut out).unwrap();
        assert_eq!(out, vec![a, b]);
    }

    #[test]
    fn pack_splits_oversized_and_flushes_in_order() {
        let small = Packet::data(0x7E8, 0x7E0, &[1, 2]).unwrap();
        let big = Packet::data(0x7E8, 0x7E0, &[9u8; 40]).unwrap();
        let packed = pack_for_ble(&[small.clone(), big.clone(), small.clone()], 20).unwrap();
        let mut r = Reassembler::new();
        let mut out = Vec::new();
        for v in &packed {
            r.push(v, &mut out).unwrap();
        }
        let payloads: Vec<&[u8]> = out.iter().map(|p| &p.payload[..]).collect();
        assert_eq!(
            payloads,
            vec![&small.payload[..], &big.payload[..], &small.payload[..]]
        );
        assert_eq!(out[1].header.flags, FLAG_SPLIT);
    }

    #[test]
    fn reassembler_errors() {
        let mut r = Reassembler::new();
        let mut out = Vec::new();
        assert_eq!(
            r.push(&[0xF2, 1, 0], &mut out),
            Err(CodecError::UnexpectedContinuation)
        );
        assert_eq!(
            r.push(&[0xAA, 0, 0, 0, 0, 0, 0, 0], &mut out),
            Err(CodecError::BadHeaderId(0xAA))
        );
        // Announces 5, carries 2, no split flag.
        assert_eq!(
            r.push(&[0xF1, 0, 0xE8, 7, 0xE0, 7, 5, 0, 1, 2], &mut out),
            Err(CodecError::Truncated {
                announced: 5,
                available: 2
            })
        );
        // Split start, then a chunk out of order.
        r.push(&[0xF1, 0x08, 0xE8, 7, 0xE0, 7, 5, 0, 1, 2], &mut out)
            .unwrap();
        assert!(r.in_progress());
        assert_eq!(
            r.push(&[0xF2, 2, 3, 4, 5], &mut out),
            Err(CodecError::OutOfOrder {
                expected: 1,
                got: 2
            })
        );
        assert!(!r.in_progress());
        // Overrun.
        r.push(&[0xF1, 0x08, 0xE8, 7, 0xE0, 7, 3, 0, 1, 2], &mut out)
            .unwrap();
        assert_eq!(
            r.push(&[0xF2, 1, 3, 4], &mut out),
            Err(CodecError::Overrun {
                announced: 3,
                got: 4
            })
        );
        assert!(out.is_empty());
        // A good frame before a bad one is still delivered.
        let good = Packet::data(0x7E8, 0x7E0, &[0x50, 0x03]).unwrap().encode();
        let mut v = good.clone();
        v.extend_from_slice(&[0x00; 8]);
        assert!(r.push(&v, &mut out).is_err());
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn stream_decoder_handles_arbitrary_chunking_and_garbage() {
        let a = Packet::data(0x7E8, 0x7E0, &(0..200).map(|i| i as u8).collect::<Vec<_>>()).unwrap();
        let b = Packet::set_stmin_us(0x7E0, 0x7E8, 0);
        let mut bytes = vec![0x00, 0x13];
        bytes.extend(a.encode());
        bytes.extend(b.encode());
        for step in [1usize, 3, 7, 64, 1000] {
            let mut d = StreamDecoder::new();
            let mut out = Vec::new();
            let mut errors = 0;
            for c in bytes.chunks(step) {
                if d.push(c, &mut out).is_err() {
                    errors += 1;
                }
            }
            assert_eq!(out, vec![a.clone(), b.clone()], "step {step}");
            assert!(errors >= 1, "garbage reported");
            assert_eq!(d.buffered(), 0);
        }
    }

    #[test]
    fn links_table() {
        assert_eq!(find_link(0x7E0, 0x7E8).unwrap().name, "ecu");
        assert_eq!(find_link(0x7E5, 0x7ED).unwrap().name, "haldex");
        assert!(find_link(0x7E0, 0x7E9).is_none());
        assert_eq!(Setting::from_u8(8), Some(Setting::GapName));
        assert_eq!(Setting::from_u8(9), None);
    }
}
