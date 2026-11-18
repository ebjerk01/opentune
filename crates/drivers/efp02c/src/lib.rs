//! EFI Technology **EFP02C** USB-CAN adapter driver for OpenTune.
//!
//! The EFP02C is a small USB 1.1 CAN box (USB `2323:0001`, one vendor-specific
//! interface, bulk endpoints `0x02` OUT / `0x82` IN, 16-byte packets). This
//! driver talks to it directly over USB through [`nusb`] (pure Rust: no libusb,
//! no vendor DLL) and layers the fixed opentune-core ISO-TP + UDS stack on top
//! of an [`Efp02cLink`](link::Efp02cLink) [`CanLink`], plus a raw-bus monitor
//! ([`monitor::Efp02cMonitor`]). It is a *generic CAN adapter* driver only:
//! no ECU protocol of the box's vendor is implemented, and the vendor's
//! licensing exchange is never sent.
//!
//! **Validation state: UNVERIFIED (Bench tier).** The record layout
//! ([`proto`]) was implemented from captures of the project owner's own unit
//! on a 1 Mbit/s bench bus, driven from Linux without vendor software. It is to
//! be re-confirmed from a black-box capture of the vendor software. Until then:
//!
//! * only **1 Mbit/s** is accepted (the one confirmed configuration) — any
//!   other bitrate is refused rather than guessed;
//! * only **11-bit** identifiers are accepted;
//! * ECU **writes are disabled** (`write_memory` refuses; the capability is
//!   not declared). Reads, identification and the diagnostic transport work.
//!
//! Config ([`ConnectConfig::options`]):
//! - `bitrate`: must be `1000000` if given (overrides [`ConnectConfig::bitrate`];
//!   an unchanged default selects 1 Mbit/s, any other explicit rate is refused).
//! - `termination`: `on` (default) / `off` — the box's bus terminator.
//! - `identify`: `on` reads the box's identification block during open and
//!   shows its model text as the firmware string (diagnostic only).
//! - `usb_index`: which attached EFP02C to use (default 0).
//!
//! Platform notes: on Windows the device needs the WinUSB driver bound (e.g.
//! with Zadig); on Linux a udev rule grants access (see docs/DRIVERS.md).

pub mod link;
pub mod monitor;
pub mod proto;
pub mod transport;

use link::{Efp02cLink, LinkOptions, Timing};
use opentune_core::protocol::diag::DiagTransport;
use opentune_core::protocol::isotp::IsoTp;
use opentune_core::protocol::seedkey::SeedKey;
use opentune_core::protocol::tp20channel::Tp20Options;
use opentune_core::protocol::uds::{next_block_seq, seed_is_unlocked, UdsClient};
use opentune_core::{
    Capabilities, ConnectConfig, EcuId, Error, Interface, InterfaceInfo, MemoryRegion, Phase,
    Progress, Protocol, Result,
};
use proto::CONFIRMED_BITRATE;
use transport::Efp02cTransport;

/// Driver id in the registry.
pub const DRIVER_ID: &str = "efp02c";
/// Display name in the registry (states the validation level).
pub const DRIVER_NAME: &str = "EFI EFP02C USB-CAN (unverified)";

/// Refuse any bitrate but the confirmed 1 Mbit/s.
pub fn check_bitrate(bitrate: u32) -> Result<()> {
    if bitrate == CONFIRMED_BITRATE {
        Ok(())
    } else {
        Err(Error::Unsupported(format!(
            "efp02c: only 1 Mbit/s (bitrate=1000000) is confirmed for the EFP02C; \
             {bitrate} bit/s is refused rather than guessed"
        )))
    }
}

/// The bitrate a connection asks for: option `bitrate`, else `cfg.bitrate`.
/// An untouched `cfg.bitrate` (the generic 500 kbit/s default) means "not
/// specified" and selects the box's one confirmed rate, so the desktop and CLI
/// work without extra options; an explicit other rate is still refused.
fn requested_bitrate(cfg: &ConnectConfig) -> Result<u32> {
    match cfg.options.get("bitrate") {
        Some(b) => b
            .trim()
            .parse()
            .map_err(|_| Error::Config(format!("efp02c: bad bitrate '{b}'"))),
        None if cfg.bitrate == ConnectConfig::default().bitrate => Ok(CONFIRMED_BITRATE),
        None => Ok(cfg.bitrate),
    }
}

/// Everything [`Interface::open`] refuses before touching the USB device.
pub fn check_config(cfg: &ConnectConfig) -> Result<()> {
    if Tp20Options::from_config(cfg)?.is_some() {
        return Err(Error::Unsupported(
            "efp02c: VW TP2.0 runs on 500 kbit/s buses; only 1 Mbit/s is confirmed for this \
             adapter"
                .into(),
        ));
    }
    if !matches!(cfg.protocol, Protocol::IsoTp | Protocol::RawCan) {
        return Err(Error::Unsupported(
            "efp02c: the EFP02C driver supports CAN (ISO-TP / raw CAN) only".into(),
        ));
    }
    check_bitrate(requested_bitrate(cfg)?)?;
    if cfg.tx_id > 0x7FF || cfg.rx_id > 0x7FF {
        return Err(Error::Unsupported(format!(
            "efp02c: only 11-bit CAN ids are confirmed for the EFP02C (got {:X}/{:X})",
            cfg.tx_id, cfg.rx_id
        )));
    }
    LinkOptions::from_options(&cfg.options)?;
    Ok(())
}

/// EFI Technology EFP02C interface driver.
#[derive(Default)]
pub struct Efp02cDriver {
    isotp: Option<IsoTp>,
    cfg: Option<ConnectConfig>,
    /// Seed/key handler for UDS security access. OpenTune ships none.
    seed_key: Option<Box<dyn SeedKey>>,
    /// Raw-bus capture (independent of `open`).
    monitor: monitor::Efp02cMonitor,
    /// Model text from the identification block (`identify=on`).
    ident: Option<String>,
    describe: Option<String>,
}

impl Efp02cDriver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install the seed/key handler for service 0x27 before a read of a
    /// protected ECU. OpenTune ships no OEM algorithms.
    pub fn set_seed_key(&mut self, seed_key: Box<dyn SeedKey>) {
        self.seed_key = Some(seed_key);
    }

    /// Open over an already-opened transport (a [`transport::Replay`] in
    /// tests) with the same checks and init sequence as [`Interface::open`].
    pub fn open_with_transport(
        &mut self,
        t: Box<dyn Efp02cTransport>,
        cfg: &ConnectConfig,
        timing: Timing,
    ) -> Result<()> {
        check_config(cfg)?;
        self.close()?;
        let mut lo = LinkOptions::from_options(&cfg.options)?;
        lo.timing = timing;
        let link = Efp02cLink::open(t, lo)?;
        self.ident = link.ident().map(|b| {
            format!(
                "{} (identification block, checksum ok)",
                b.text().unwrap_or_else(|| "no model text".into())
            )
        });
        self.describe = Some(link.describe());
        self.isotp = Some(IsoTp::new(Box::new(link), cfg.tx_id, cfg.rx_id));
        self.cfg = Some(cfg.clone());
        Ok(())
    }

    fn isotp(&mut self) -> Result<&mut IsoTp> {
        self.isotp.as_mut().ok_or(Error::NotConnected)
    }

    /// UDS security access if a handler is set or the config requires it.
    fn security_access(&mut self) -> Result<()> {
        let opts = self.cfg.as_ref().map(|c| &c.options);
        let required = opts.is_some_and(|o| {
            o.contains_key("seedkey")
                || matches!(
                    o.get("security").map(String::as_str),
                    Some("required" | "true" | "1" | "yes")
                )
        });
        let level = opts
            .and_then(|o| o.get("seedkey_level"))
            .and_then(|s| {
                let t = s.trim();
                match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                    Some(h) => u8::from_str_radix(h, 16).ok(),
                    None => t.parse().ok(),
                }
            })
            .unwrap_or(0x11);
        let sk = self.seed_key.as_deref();
        let tp = self.isotp.as_mut().ok_or(Error::NotConnected)?;
        match sk {
            None if required => Err(Error::Config(
                "ECU requires security access but no seed/key handler is set".into(),
            )),
            None => Ok(()),
            Some(sk) => {
                let mut uds = UdsClient::new(tp);
                let seed = uds.request_seed(level)?;
                if seed_is_unlocked(&seed) {
                    return Ok(());
                }
                let key = sk.compute(&seed, level)?;
                uds.send_key(level.wrapping_add(1), &key)
            }
        }
    }
}

impl Interface for Efp02cDriver {
    fn set_seed_key(&mut self, seed_key: Box<dyn SeedKey>) -> bool {
        Efp02cDriver::set_seed_key(self, seed_key);
        true
    }

    fn info(&self) -> InterfaceInfo {
        let mut display_name = DRIVER_NAME.to_string();
        if let Some(d) = &self.describe {
            display_name = format!("{display_name}, {d}");
        }
        InterfaceInfo {
            id: DRIVER_ID.into(),
            display_name,
            firmware: self.ident.clone(),
            // Serial numbers are deliberately not decoded.
            serial: None,
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            can: true,
            read_memory: true,
            // Disabled until the protocol is confirmed (see crate docs).
            write_memory: false,
            ..Default::default()
        }
    }

    fn diag(&mut self) -> Option<&mut dyn DiagTransport> {
        self.isotp.as_mut().map(|t| t as &mut dyn DiagTransport)
    }

    fn can_monitor(&mut self) -> Option<&mut dyn opentune_core::CanMonitor> {
        Some(&mut self.monitor)
    }

    fn open(&mut self, cfg: &ConnectConfig) -> Result<()> {
        check_config(cfg)?;
        let lo = LinkOptions::from_options(&cfg.options)?;
        // Release a previous session first: it holds the USB interface.
        self.close()?;
        let t = transport::open_usb(lo.usb_index)?;
        self.open_with_transport(t, cfg, Timing::CAPTURED)
    }

    fn close(&mut self) -> Result<()> {
        // Dropping the link resets the box.
        self.isotp = None;
        self.cfg = None;
        self.ident = None;
        self.describe = None;
        Ok(())
    }

    fn read_ecu_id(&mut self) -> Result<EcuId> {
        let tp = self.isotp()?;
        let mut uds = UdsClient::new(tp);
        let mut id = EcuId::default();
        let mut got_any = false;
        for (did, name) in [(0xF190u16, "vin"), (0xF189, "sw"), (0xF191, "hw")] {
            match uds.read_data_by_id(did) {
                Ok(v) => {
                    got_any = true;
                    let s = String::from_utf8_lossy(&v)
                        .trim_matches(|c: char| c == '\0' || c == ' ')
                        .to_string();
                    match name {
                        "vin" => id.vin = Some(s),
                        "sw" => id.sw_number = Some(s),
                        _ => id.hw_number = Some(s),
                    }
                }
                Err(Error::NegativeResponse { .. }) => {}
                Err(e) if !got_any => return Err(e),
                Err(_) => break,
            }
        }
        Ok(id)
    }

    fn read_memory(
        &mut self,
        region: MemoryRegion,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<Vec<u8>> {
        self.security_access()?;
        let tp = self.isotp()?;
        let mut uds = UdsClient::new(tp);
        uds.request_upload(0x00, region.address, region.length)?;
        let total = region.length as u64;
        let mut out = Vec::with_capacity(region.length as usize);
        let mut seq = 1u8;
        while (out.len() as u32) < region.length {
            let resp = uds.transfer_data(seq, &[])?;
            if resp.len() <= 1 {
                return Err(Error::Protocol("efp02c: empty transferData block".into()));
            }
            out.extend_from_slice(&resp[1..]);
            seq = next_block_seq(seq);
            progress(Progress {
                phase: Phase::Read,
                done: (out.len() as u64).min(total),
                total,
            });
        }
        let _ = uds.transfer_exit();
        out.truncate(region.length as usize);
        Ok(out)
    }

    fn write_memory(
        &mut self,
        _region: MemoryRegion,
        _data: &[u8],
        _progress: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        Err(Error::Unsupported(
            "efp02c: ECU writes are disabled — this driver is UNVERIFIED until its USB protocol \
             is re-confirmed from a capture of the vendor software"
                .into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_1m() -> ConnectConfig {
        let mut c = ConnectConfig::default();
        c.options.insert("bitrate".into(), "1000000".into());
        c
    }

    #[test]
    fn refuses_unconfirmed_configs_before_usb() {
        // The untouched default selects the confirmed 1 Mbit/s.
        check_config(&ConnectConfig::default()).unwrap();
        // An explicit other rate is refused.
        let mut c = ConnectConfig::default();
        c.options.insert("bitrate".into(), "500000".into());
        let e = check_config(&c).unwrap_err();
        assert!(
            matches!(e, Error::Unsupported(ref m) if m.contains("1 Mbit/s")),
            "{e}"
        );
        let c = ConnectConfig {
            bitrate: 250_000,
            ..ConnectConfig::default()
        };
        assert!(matches!(check_config(&c), Err(Error::Unsupported(_))));
        // 29-bit ids.
        let mut c = cfg_1m();
        c.tx_id = 0x18DA_10F1;
        c.rx_id = 0x18DA_F110;
        assert!(matches!(check_config(&c), Err(Error::Unsupported(_))));
        // TP2.0.
        let mut c = cfg_1m();
        c.options.insert("transport".into(), "tp20".into());
        c.options.insert("tp20_addr".into(), "0x01".into());
        assert!(check_config(&c).is_err());
        // K-line.
        let mut c = cfg_1m();
        c.protocol = Protocol::Iso14230Kline;
        assert!(matches!(check_config(&c), Err(Error::Unsupported(_))));
        // Bad option values.
        let mut c = cfg_1m();
        c.options.insert("bitrate".into(), "fast".into());
        assert!(matches!(check_config(&c), Err(Error::Config(_))));
        // The confirmed setup, via the option or the field.
        check_config(&cfg_1m()).unwrap();
        let c = ConnectConfig {
            bitrate: 1_000_000,
            ..ConnectConfig::default()
        };
        check_config(&c).unwrap();
    }

    #[test]
    fn capabilities_are_honest() {
        let d = Efp02cDriver::new();
        let c = d.capabilities();
        assert!(c.can && c.read_memory);
        assert!(!c.write_memory && !c.vwtp20 && !c.can_fd && !c.kline);
        assert_eq!(d.info().id, DRIVER_ID);
        assert!(d.info().display_name.contains("unverified"));
    }

    #[test]
    fn writes_are_refused() {
        let mut d = Efp02cDriver::new();
        let e = d
            .write_memory(
                MemoryRegion {
                    address: 0,
                    length: 1,
                },
                &[0],
                &mut |_| {},
            )
            .unwrap_err();
        assert!(matches!(e, Error::Unsupported(_)));
    }
}
