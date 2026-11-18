//! Host driver for the open-source **esp32-isotp-ble-bridge** firmware
//! (Switchleg1/esp32-isotp-ble-bridge, MIT; the BLE/ISO-TP bridge used by
//! SimosTools and VW_Flash on a Macchina A0 or a DIY clone).
//!
//! The device runs ISO-TP (ISO 15765-2) itself on a fixed table of CAN id
//! pairs and exchanges whole ISO-TP payloads with the host in a small framed
//! protocol, over BLE GATT or over its USB serial port. This driver speaks that
//! protocol and exposes the bridge as an ISO-TP-level [`DiagTransport`], so
//! opentune-core's `UdsClient` and every service engine run over it.
//!
//! * [`codec`] — the wire format, dependency-free (shared with the Android app
//!   later).
//! * [`link`] — the byte transport trait plus the serial link.
//! * [`bridge`] — a session: splitting, reassembly, answer matching, settings.
//! * [`mock`] — a simulated bridge firmware + ECU for tests.
//! * `ble` — the BLE link over `btleplug`, behind the optional `btleplug`
//!   cargo feature (off by default, so default builds pull no BLE/D-Bus deps).
//!
//! **Validation state: unverified.** Written from the firmware's public source
//! and tested against the in-process mock only; never run against a real
//! bridge or car. See `docs/ISOTP-BLE-BRIDGE.md`.

#[cfg(feature = "btleplug")]
pub mod ble;
pub mod bridge;
pub mod codec;
pub mod link;
pub mod mock;

pub use bridge::Bridge;
pub use link::{Framing, Link, SerialLink};

use opentune_core::protocol::diag::DiagTransport;
use opentune_core::protocol::seedkey::SeedKey;
use opentune_core::protocol::uds::UdsClient;
use opentune_core::{
    Capabilities, ConnectConfig, DiagLayer, EcuId, Error, Interface, InterfaceInfo, MemoryRegion,
    Phase, Progress, Protocol, Result,
};
use std::time::Duration;

/// Registry id of this driver.
pub const DRIVER_ID: &str = "isotp-ble";
/// Registry display name.
pub const DRIVER_NAME: &str = "ESP32 ISO-TP BLE bridge (Switchleg1 fw, unverified)";

/// Driver for the esp32-isotp-ble-bridge firmware.
#[derive(Default)]
pub struct IsoTpBleDriver {
    bridge: Option<Bridge>,
    tx_id: u16,
    rx_id: u16,
    cfg: Option<ConnectConfig>,
    seed_key: Option<Box<dyn SeedKey>>,
}

/// How `open` reaches the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportChoice {
    /// USB serial port at `baud`.
    Serial { port: String, baud: u32 },
    /// BLE: by advertised `name`, or by `address` when given.
    Ble {
        name: String,
        address: Option<String>,
        scan_ms: u64,
        att_mtu: Option<usize>,
    },
}

fn opt<'a>(cfg: &'a ConnectConfig, key: &str) -> Option<&'a str> {
    cfg.options
        .get(key)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
}

fn parse_num<T: std::str::FromStr>(cfg: &ConnectConfig, key: &str) -> Result<Option<T>> {
    match opt(cfg, key) {
        None => Ok(None),
        Some(v) => v
            .parse()
            .map(Some)
            .map_err(|_| Error::Config(format!("{DRIVER_ID}: option {key}='{v}' is not a number"))),
    }
}

fn truthy(v: Option<&str>) -> bool {
    matches!(v, Some("1" | "true" | "yes" | "on"))
}

/// Pick the transport from the connect options: `transport=serial|ble`
/// (default: serial when `port` is given, else BLE).
pub fn transport_choice(cfg: &ConnectConfig) -> Result<TransportChoice> {
    let kind = opt(cfg, "transport").map(str::to_ascii_lowercase);
    let port = opt(cfg, "port");
    match (kind.as_deref(), port) {
        (Some("serial"), None) => Err(Error::Config(format!(
            "{DRIVER_ID}: transport=serial needs options[\"port\"] (e.g. /dev/ttyUSB0)"
        ))),
        (Some("serial"), Some(p)) | (None, Some(p)) => Ok(TransportChoice::Serial {
            port: p.to_string(),
            baud: parse_num(cfg, "baud")?.unwrap_or(codec::SERIAL_BAUD),
        }),
        (Some("ble"), _) | (None, None) => Ok(TransportChoice::Ble {
            name: opt(cfg, "name")
                .unwrap_or(codec::DEFAULT_DEVICE_NAME)
                .to_string(),
            address: opt(cfg, "address").map(str::to_string),
            scan_ms: parse_num(cfg, "scan_ms")?.unwrap_or(5000),
            att_mtu: parse_num(cfg, "att_mtu")?,
        }),
        (Some(other), _) => Err(Error::Config(format!(
            "{DRIVER_ID}: unknown transport '{other}' (serial or ble)"
        ))),
    }
}

/// Check a connect config against what the firmware can do, returning the
/// 11-bit tester/ECU ids.
pub fn validate_config(cfg: &ConnectConfig) -> Result<(u16, u16)> {
    match cfg.protocol {
        Protocol::IsoTp => {}
        other => {
            return Err(Error::Unsupported(format!(
                "{DRIVER_ID}: the bridge firmware only does ISO-TP on CAN (got {other:?})"
            )))
        }
    }
    if cfg.bitrate != codec::CAN_BITRATE {
        return Err(Error::Config(format!(
            "{DRIVER_ID}: the bridge firmware is fixed at 500 kbit/s (asked for {})",
            cfg.bitrate
        )));
    }
    if cfg.tx_id > 0x7FF || cfg.rx_id > 0x7FF {
        return Err(Error::Config(format!(
            "{DRIVER_ID}: the bridge firmware uses 11-bit CAN ids only (got {:X}/{:X})",
            cfg.tx_id, cfg.rx_id
        )));
    }
    let (tx, rx) = (cfg.tx_id as u16, cfg.rx_id as u16);
    if codec::find_link(tx, rx).is_none() && !truthy(opt(cfg, "any_ids")) {
        let known: Vec<String> = codec::LINKS
            .iter()
            .map(|l| format!("{:X}/{:X} ({})", l.tx_id, l.rx_id, l.name))
            .collect();
        return Err(Error::Config(format!(
            "{DRIVER_ID}: the bridge firmware only serves the CAN id pairs {}; {tx:X}/{rx:X} \
             would be dropped silently (set option any_ids=1 for a firmware with a different table)",
            known.join(", ")
        )));
    }
    Ok((tx, rx))
}

impl IsoTpBleDriver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open over an already-connected [`Link`] (the mock in tests; the
    /// Android app's own BLE link later). Applies the same config checks and
    /// the `stmin_us` option as [`Interface::open`].
    pub fn open_with_link(&mut self, link: Box<dyn Link>, cfg: &ConnectConfig) -> Result<()> {
        let (tx, rx) = validate_config(cfg)?;
        let mut bridge = Bridge::new(link);
        bridge.drain()?;
        if let Some(us) = parse_num::<u16>(cfg, "stmin_us")? {
            bridge.set_setting(codec::Setting::IsoTpStminUs, tx, rx, &us.to_le_bytes())?;
        }
        self.bridge = Some(bridge);
        self.tx_id = tx;
        self.rx_id = rx;
        self.cfg = Some(cfg.clone());
        Ok(())
    }

    /// The open session, for settings and diagnostics.
    pub fn bridge(&mut self) -> Option<&mut Bridge> {
        self.bridge.as_mut()
    }

    fn transport(&mut self) -> Result<&mut dyn DiagTransport> {
        if self.bridge.is_none() {
            return Err(Error::NotConnected);
        }
        Ok(self as &mut dyn DiagTransport)
    }

    fn security_access(&mut self) -> Result<()> {
        let required = security_required(self.cfg.as_ref());
        let level = seedkey_level(self.cfg.as_ref(), 0x11);
        let sk = self.seed_key.take();
        let r = match self.transport() {
            Ok(t) => uds_security_access(t, sk.as_deref(), level, required),
            Err(e) => Err(e),
        };
        self.seed_key = sk;
        r
    }
}

/// The driver is the session's [`DiagTransport`] (what [`Interface::diag`]
/// hands out).
impl DiagTransport for IsoTpBleDriver {
    fn request(&mut self, req: &[u8], timeout_ms: u64) -> Result<Vec<u8>> {
        let (tx, rx) = (self.tx_id, self.rx_id);
        let bridge = self.bridge.as_mut().ok_or(Error::NotConnected)?;
        bridge.request(tx, rx, req, Duration::from_millis(timeout_ms.max(1)))
    }
}

impl Interface for IsoTpBleDriver {
    fn info(&self) -> InterfaceInfo {
        InterfaceInfo {
            id: DRIVER_ID.into(),
            display_name: DRIVER_NAME.into(),
            firmware: None,
            serial: self.bridge.as_ref().map(Bridge::describe),
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            can: true,
            read_memory: true,
            write_memory: true,
            ..Default::default()
        }
    }

    fn open(&mut self, cfg: &ConnectConfig) -> Result<()> {
        validate_config(cfg)?;
        let link: Box<dyn Link> = match transport_choice(cfg)? {
            TransportChoice::Serial { port, baud } => Box::new(SerialLink::open(&port, baud)?),
            TransportChoice::Ble {
                name,
                address,
                scan_ms,
                att_mtu,
            } => open_ble(&name, address.as_deref(), scan_ms, att_mtu)?,
        };
        self.open_with_link(link, cfg)
    }

    fn close(&mut self) -> Result<()> {
        self.bridge = None;
        self.cfg = None;
        Ok(())
    }

    fn read_ecu_id(&mut self) -> Result<EcuId> {
        let t = self.transport()?;
        read_uds_ecu_id(t)
    }

    fn read_memory(
        &mut self,
        region: MemoryRegion,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<Vec<u8>> {
        self.require_uds()?;
        self.security_access()?;
        uds_read_memory(self.transport()?, region, progress)
    }

    fn write_memory(
        &mut self,
        region: MemoryRegion,
        data: &[u8],
        progress: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        if data.len() != region.length as usize {
            return Err(Error::Config("data length != region length".into()));
        }
        self.require_uds()?;
        self.security_access()?;
        uds_write_memory(self.transport()?, region, data, progress)
    }

    fn diag(&mut self) -> Option<&mut dyn DiagTransport> {
        if self.bridge.is_some() {
            Some(self as &mut dyn DiagTransport)
        } else {
            None
        }
    }

    fn set_seed_key(&mut self, seed_key: Box<dyn SeedKey>) -> bool {
        self.seed_key = Some(seed_key);
        true
    }
}

impl IsoTpBleDriver {
    fn require_uds(&self) -> Result<()> {
        match self.cfg.as_ref().map(|c| c.diag_layer) {
            Some(DiagLayer::Uds) => Ok(()),
            Some(DiagLayer::Kwp2000) => Err(Error::Unsupported(format!(
                "{DRIVER_ID}: block read/write is implemented for UDS only; use diag() for KWP2000 on CAN"
            ))),
            None => Err(Error::NotConnected),
        }
    }
}

#[cfg(feature = "btleplug")]
fn open_ble(
    name: &str,
    address: Option<&str>,
    scan_ms: u64,
    att_mtu: Option<usize>,
) -> Result<Box<dyn Link>> {
    let opts = ble::BleOptions {
        name: name.to_string(),
        address: address.map(str::to_string),
        scan_timeout: Duration::from_millis(scan_ms),
        att_mtu,
    };
    Ok(Box::new(ble::BleLink::connect(&opts)?))
}

#[cfg(not(feature = "btleplug"))]
fn open_ble(
    _name: &str,
    _address: Option<&str>,
    _scan_ms: u64,
    _att_mtu: Option<usize>,
) -> Result<Box<dyn Link>> {
    Err(Error::Unsupported(format!(
        "{DRIVER_ID}: this build has no BLE support (rebuild with the `btleplug` cargo feature), \
         or connect over USB with options[\"port\"]"
    )))
}

// --- UDS helpers (each OpenTune driver carries its own copy) -----------------

fn decode_text(bytes: &[u8]) -> String {
    let printable = !bytes.is_empty()
        && bytes
            .iter()
            .all(|&b| (0x20..=0x7E).contains(&b) || b == 0x00);
    if printable {
        String::from_utf8_lossy(bytes)
            .trim_matches(|c| c == '\0' || c == ' ')
            .to_string()
    } else {
        bytes.iter().map(|b| format!("{b:02X}")).collect()
    }
}

fn read_uds_ecu_id(transport: &mut dyn DiagTransport) -> Result<EcuId> {
    let mut id = EcuId::default();
    let mut uds = UdsClient::new(transport);
    let dids: &[(u16, &str)] = &[
        (0xF190, "vin"),
        (0xF194, "sw_number"),
        (0xF191, "hw_number"),
        (0xF18C, "ecu_serial"),
        (0xF18A, "supplier"),
        (0xF197, "ecu_name"),
    ];
    let mut got_any = false;
    for (did, name) in dids {
        match uds.read_data_by_id(*did) {
            Ok(bytes) => {
                got_any = true;
                let value = decode_text(&bytes);
                match *name {
                    "vin" => id.vin = Some(value.clone()),
                    "sw_number" => id.sw_number = Some(value.clone()),
                    "hw_number" => id.hw_number = Some(value.clone()),
                    "supplier" => id.supplier = Some(value.clone()),
                    "ecu_name" => id.ecu_name = Some(value.clone()),
                    _ => {}
                }
                id.raw.insert(name.to_string(), value);
            }
            Err(Error::NegativeResponse { .. }) => continue,
            Err(e) => {
                if got_any {
                    break;
                }
                return Err(e);
            }
        }
    }
    Ok(id)
}

fn uds_read_memory(
    transport: &mut dyn DiagTransport,
    region: MemoryRegion,
    progress: &mut dyn FnMut(Progress),
) -> Result<Vec<u8>> {
    let total = region.length as u64;
    let mut uds = UdsClient::new(transport);
    uds.request_upload(0x00, region.address, region.length)?;
    progress(Progress {
        phase: Phase::Read,
        done: 0,
        total,
    });
    let mut out = Vec::with_capacity(region.length as usize);
    let mut seq: u8 = 1;
    while (out.len() as u32) < region.length {
        let resp = uds.transfer_data(seq, &[])?;
        let chunk = resp.get(1..).unwrap_or(&[]);
        if chunk.is_empty() {
            return Err(Error::Protocol("empty transferData block".into()));
        }
        out.extend_from_slice(chunk);
        seq = seq.wrapping_add(1);
        progress(Progress {
            phase: Phase::Read,
            done: out.len().min(region.length as usize) as u64,
            total,
        });
    }
    out.truncate(region.length as usize);
    let _ = uds.transfer_exit();
    Ok(out)
}

fn uds_write_memory(
    transport: &mut dyn DiagTransport,
    region: MemoryRegion,
    data: &[u8],
    progress: &mut dyn FnMut(Progress),
) -> Result<()> {
    let total = data.len() as u64;
    let mut uds = UdsClient::new(transport);
    let max_block = uds.request_download(0x00, region.address, region.length)?;
    // maxNumberOfBlockLength counts the SID and sequence byte; the device's
    // ISO-TP buffer caps one message at 4095 bytes.
    let chunk_size = max_block
        .min(codec::MAX_ISOTP_PAYLOAD)
        .saturating_sub(2)
        .max(1);
    progress(Progress {
        phase: Phase::Write,
        done: 0,
        total,
    });
    let mut seq: u8 = 1;
    let mut done = 0u64;
    for chunk in data.chunks(chunk_size) {
        uds.transfer_data(seq, chunk)?;
        seq = seq.wrapping_add(1);
        done += chunk.len() as u64;
        progress(Progress {
            phase: Phase::Write,
            done,
            total,
        });
    }
    uds.transfer_exit()?;
    Ok(())
}

fn security_required(cfg: Option<&ConnectConfig>) -> bool {
    cfg.map(|c| {
        c.options.contains_key("seedkey")
            || matches!(
                c.options.get("security").map(String::as_str),
                Some("required" | "true" | "1" | "yes")
            )
    })
    .unwrap_or(false)
}

fn seedkey_level(cfg: Option<&ConnectConfig>, default: u8) -> u8 {
    cfg.and_then(|c| c.options.get("seedkey_level"))
        .and_then(|s| parse_access_level(s))
        .unwrap_or(default)
}

fn parse_access_level(s: &str) -> Option<u8> {
    let t = s.trim();
    match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(h) => u8::from_str_radix(h, 16).ok(),
        None => t.parse().ok(),
    }
}

fn uds_security_access(
    transport: &mut dyn DiagTransport,
    seed_key: Option<&dyn SeedKey>,
    request_level: u8,
    required: bool,
) -> Result<()> {
    match seed_key {
        None if required => Err(Error::Config(
            "ECU requires security access but no seed/key handler is set \
             (call set_seed_key with an algorithm for this ECU)"
                .into(),
        )),
        None => Ok(()),
        Some(sk) => {
            let mut uds = UdsClient::new(transport);
            let seed = uds.request_seed(request_level)?;
            if !seed.is_empty() && seed.iter().all(|&b| b == 0) {
                return Ok(());
            }
            let key = sk.compute(&seed, request_level)?;
            uds.send_key(request_level.wrapping_add(1), &key)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(opts: &[(&str, &str)]) -> ConnectConfig {
        let mut c = ConnectConfig::default();
        for (k, v) in opts {
            c.options.insert(k.to_string(), v.to_string());
        }
        c
    }

    #[test]
    fn transport_selection() {
        assert_eq!(
            transport_choice(&cfg(&[("port", "/dev/ttyUSB0")])).unwrap(),
            TransportChoice::Serial {
                port: "/dev/ttyUSB0".into(),
                baud: 250_000
            }
        );
        assert!(matches!(
            transport_choice(&cfg(&[])).unwrap(),
            TransportChoice::Ble { ref name, address: None, scan_ms: 5000, att_mtu: None }
                if name == "BLE_TO_ISOTP20"
        ));
        assert!(matches!(
            transport_choice(&cfg(&[
                ("transport", "ble"),
                ("port", "x"),
                ("att_mtu", "247")
            ]))
            .unwrap(),
            TransportChoice::Ble {
                att_mtu: Some(247),
                ..
            }
        ));
        assert!(transport_choice(&cfg(&[("transport", "serial")])).is_err());
        assert!(transport_choice(&cfg(&[("transport", "wifi")])).is_err());
        assert!(transport_choice(&cfg(&[("scan_ms", "soon")])).is_err());
    }

    #[test]
    fn config_checks_follow_the_firmware_limits() {
        assert_eq!(validate_config(&cfg(&[])).unwrap(), (0x7E0, 0x7E8));
        let mut c = cfg(&[]);
        c.bitrate = 250_000;
        assert!(validate_config(&c).is_err());
        let mut c = cfg(&[]);
        c.tx_id = 0x18DA10F1;
        assert!(validate_config(&c).is_err());
        let mut c = cfg(&[]);
        c.tx_id = 0x710;
        c.rx_id = 0x77A;
        let e = validate_config(&c).unwrap_err().to_string();
        assert!(e.contains("7E5/7ED (haldex)"), "{e}");
        c.options.insert("any_ids".into(), "1".into());
        assert_eq!(validate_config(&c).unwrap(), (0x710, 0x77A));
        let mut c = cfg(&[]);
        c.protocol = Protocol::Iso14230Kline;
        assert!(matches!(validate_config(&c), Err(Error::Unsupported(_))));
    }

    #[cfg(not(feature = "btleplug"))]
    #[test]
    fn ble_without_the_feature_says_how_to_get_it() {
        let e = IsoTpBleDriver::new()
            .open(&cfg(&[]))
            .unwrap_err()
            .to_string();
        assert!(e.contains("btleplug"), "{e}");
    }
}
