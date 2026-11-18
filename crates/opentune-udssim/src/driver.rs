//! The `udssim` registry driver: an in-process simulated ECU behind the
//! [`Interface`] trait, so `opentune read / flash / info --driver udssim` (and
//! any embedder's commands) run with no hardware.
//!
//! The ECU is a [`UdsServer`] with the flash model enabled
//! ([`default_server`]): one 64 KiB flash region at `0x0000_0000` whose initial
//! contents are the historical upload ramp (byte `i` = `i & 0xFF`), identity
//! DIDs (F187 / F188 / F189 / F18A / F190 / F191 / F197), programming at
//! SecurityAccess level 0x11.
//!
//! - [`Interface::diag`] hands out the simulator transport (every engine and
//!   the UDS module flasher run over it).
//! - `read_memory` is `23` ReadMemoryByAddress in 0x80-byte chunks.
//! - `write_memory` programs over UDS: extended → programming session,
//!   SecurityAccess 0x11/0x12 with the attached [`SeedKey`], eraseMemory,
//!   RequestDownload / TransferData / RequestTransferExit, the checksum
//!   routine, ECUReset. It needs a seed-key handler: attach
//!   [`TestSeedKey`] (id [`TEST_SEED_KEY_ID`], `--seedkey udssim-test` on the
//!   CLI). It is the simulator's **test-only** transform (`key = seed XOR
//!   0xA5`), not any manufacturer's algorithm.
//!
//! ## Shared state
//!
//! Like the virtual car, every driver opened on the same instance name talks to
//! the same ECU until the process ends, so a write survives a re-open (the CLI
//! and the desktop open a driver per operation).
//!
//! ## Options ([`ConnectConfig::options`])
//!
//! - `ecu` (or `port`): instance name, default `default`.
//! - `reset`: `1` / `true` — rebuild the ECU from [`default_server`] on open.
//! - `readable`: `0` / `false` — refuse ReadMemoryByAddress of flash (to test
//!   the no-backup paths).
//!
//! Offline only: nothing here touches a bus.
use crate::flash::crc16_ccitt_false;
use crate::{FlashConfig, FlashRegion, SimTransport, UdsServer};
use opentune_core::protocol::seedkey::SeedKey;
use opentune_core::protocol::uds::{encode_address_and_length, seed_is_unlocked};
use opentune_core::protocol::{DiagTransport, UdsClient};
use opentune_core::{
    Capabilities, ConnectConfig, EcuId, Error, Interface, InterfaceInfo, MemoryRegion, Phase,
    Progress, Result,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// The registry id.
pub const DRIVER_ID: &str = "udssim";
/// The registry description.
pub const DRIVER_NAME: &str = "UDS simulator (offline)";
/// Id of the simulator's test seed-key handler.
pub const TEST_SEED_KEY_ID: &str = "udssim-test";
/// The test transform's XOR mask (the server's default checker).
pub const TEST_XOR_MASK: u8 = 0xA5;
/// Base address of the default flash region.
pub const FLASH_BASE: u32 = 0x0000_0000;
/// Size of the default flash region.
pub const FLASH_SIZE: usize = 0x1_0000;
/// The programming security level (request-seed, odd).
pub const SECURITY_LEVEL: u8 = 0x11;

const READ_CHUNK: u32 = 0x80;
const ALFID: u8 = 0x44;

/// The simulator's **test-only** seed→key handler (`key = seed XOR 0xA5`).
/// Not, and not derived from, any manufacturer's algorithm.
#[derive(Debug, Clone, Copy, Default)]
pub struct TestSeedKey;

impl SeedKey for TestSeedKey {
    fn id(&self) -> &str {
        TEST_SEED_KEY_ID
    }
    fn compute(&self, seed: &[u8], _level: u8) -> Result<Vec<u8>> {
        Ok(seed.iter().map(|b| b ^ TEST_XOR_MASK).collect())
    }
}

/// A seed-key resolver for front ends: [`TestSeedKey`] for
/// [`TEST_SEED_KEY_ID`], `None` otherwise.
pub fn test_seed_key(id: &str) -> Option<Box<dyn SeedKey>> {
    (id == TEST_SEED_KEY_ID).then(|| Box::new(TestSeedKey) as Box<dyn SeedKey>)
}

/// The default simulated ECU (see the module docs).
pub fn default_server() -> UdsServer {
    let mut s = UdsServer::new();
    s.set_did(0xF187, "OTSIM-0001");
    s.set_did(0xF188, "SIMSW-0001");
    s.set_did(0xF189, "0001");
    s.set_did(0xF18A, "OpenTune");
    // A fictional 17-character VIN.
    s.set_did(0xF190, "SIMULATED0VIN0001");
    s.set_did(0xF191, "OTSIM-HW-01");
    s.set_did(0xF197, "UDSSIM");
    s.enable_flash(FlashConfig {
        regions: vec![FlashRegion::new(
            FLASH_BASE,
            (0..FLASH_SIZE)
                .map(|i| (i & 0xFF) as u8)
                .collect::<Vec<u8>>(),
        )],
        security_level: SECURITY_LEVEL,
        max_block_len: 0x0402,
        ..FlashConfig::default()
    });
    s
}

/// An ECU shared between driver instances.
pub type SharedEcu = Arc<Mutex<SimTransport>>;

fn instances() -> &'static Mutex<HashMap<String, SharedEcu>> {
    static I: OnceLock<Mutex<HashMap<String, SharedEcu>>> = OnceLock::new();
    I.get_or_init(|| Mutex::new(HashMap::new()))
}

fn fresh() -> SharedEcu {
    Arc::new(Mutex::new(SimTransport::new(default_server())))
}

/// The shared ECU named `name` (blank = `default`), created on first use.
pub fn shared_ecu(name: &str) -> SharedEcu {
    let name = if name.trim().is_empty() {
        "default"
    } else {
        name.trim()
    };
    let mut map = instances().lock().unwrap_or_else(|e| e.into_inner());
    map.entry(name.to_string()).or_insert_with(fresh).clone()
}

/// Rebuild the shared ECU named `name` from [`default_server`].
pub fn reset_ecu(name: &str) -> SharedEcu {
    let ecu = shared_ecu(name);
    *ecu.lock().unwrap_or_else(|e| e.into_inner()) = SimTransport::new(default_server());
    ecu
}

/// A [`DiagTransport`] over a shared ECU (locks it per request).
pub struct SharedTransport(SharedEcu);

impl SharedTransport {
    /// The ECU behind this transport.
    pub fn ecu(&self) -> &SharedEcu {
        &self.0
    }
}

impl DiagTransport for SharedTransport {
    fn request(&mut self, req: &[u8], timeout_ms: u64) -> Result<Vec<u8>> {
        self.0
            .lock()
            .map_err(|_| Error::Protocol("udssim ECU lock poisoned".into()))?
            .request(req, timeout_ms)
    }
}

fn truthy(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// The `udssim` interface driver. See the module docs.
#[derive(Default)]
pub struct UdssimDriver {
    attached: Option<SharedEcu>,
    transport: Option<SharedTransport>,
    seed_key: Option<Box<dyn SeedKey>>,
}

impl UdssimDriver {
    /// A driver that resolves its ECU from the `ecu` / `port` option.
    pub fn new() -> Self {
        Self::default()
    }

    /// A driver bound to one specific ECU (ignores `ecu` / `port` / `reset`).
    pub fn with_ecu(ecu: SharedEcu) -> Self {
        Self {
            attached: Some(ecu),
            ..Self::default()
        }
    }

    /// The ECU of the open session.
    pub fn ecu(&self) -> Option<&SharedEcu> {
        self.transport.as_ref().map(SharedTransport::ecu)
    }

    fn transport(&mut self) -> Result<&mut SharedTransport> {
        self.transport.as_mut().ok_or(Error::NotConnected)
    }

    fn unlock(&mut self) -> Result<()> {
        let sk = self.seed_key.take();
        let res = (|| {
            let t = self.transport()?;
            let mut uds = UdsClient::new(t);
            uds.session(0x03)?;
            uds.session(0x02)?;
            let seed = uds.request_seed(SECURITY_LEVEL)?;
            if seed_is_unlocked(&seed) {
                return Ok(());
            }
            let Some(sk) = sk.as_deref() else {
                return Err(Error::Safety(format!(
                    "the simulated ECU needs SecurityAccess 0x{SECURITY_LEVEL:02X} to program: \
                     attach a seed-key handler (CLI: --seedkey {TEST_SEED_KEY_ID})"
                )));
            };
            let key = sk.compute(&seed, SECURITY_LEVEL)?;
            uds.send_key_detailed(SECURITY_LEVEL + 1, &key)
        })();
        self.seed_key = sk;
        res
    }
}

impl Interface for UdssimDriver {
    fn info(&self) -> InterfaceInfo {
        InterfaceInfo {
            id: DRIVER_ID.into(),
            display_name: DRIVER_NAME.into(),
            firmware: None,
            serial: None,
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            can: true,
            read_memory: true,
            write_memory: true,
            battery_voltage: true,
            ..Capabilities::default()
        }
    }

    fn open(&mut self, cfg: &ConnectConfig) -> Result<()> {
        let opt = |k: &str| cfg.options.get(k).map(String::as_str);
        let ecu = match &self.attached {
            Some(e) => e.clone(),
            None => {
                let name = opt("ecu").or_else(|| opt("port")).unwrap_or("default");
                if opt("reset").and_then(truthy) == Some(true) {
                    reset_ecu(name)
                } else {
                    shared_ecu(name)
                }
            }
        };
        if let Some(r) = opt("readable").and_then(truthy) {
            let mut t = ecu.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(f) = t.server_mut().flash_mut() {
                f.config.readable = r;
            }
        }
        self.transport = Some(SharedTransport(ecu));
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.transport = None;
        Ok(())
    }

    fn battery_millivolts(&mut self) -> Result<Option<u32>> {
        Ok(Some(12_600))
    }

    fn read_ecu_id(&mut self) -> Result<EcuId> {
        let t = self.transport()?;
        let mut uds = UdsClient::new(t);
        let mut did = |d: u16| {
            uds.read_data_by_id(d)
                .ok()
                .map(|v| String::from_utf8_lossy(&v).trim().to_string())
        };
        Ok(EcuId {
            hw_number: did(0xF187).or_else(|| did(0xF191)),
            sw_number: did(0xF188),
            vin: did(0xF190),
            supplier: did(0xF18A),
            ecu_name: did(0xF197),
            ..EcuId::default()
        })
    }

    fn read_memory(
        &mut self,
        region: MemoryRegion,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<Vec<u8>> {
        let t = self.transport()?;
        let mut uds = UdsClient::new(t);
        let mut out = Vec::with_capacity(region.length as usize);
        let total = u64::from(region.length);
        while (out.len() as u64) < total {
            let n = READ_CHUNK.min(region.length - out.len() as u32);
            let addr = region.address + out.len() as u32;
            let d = uds.read_memory_by_address(ALFID, addr, n)?;
            if d.len() != n as usize {
                return Err(Error::Protocol(format!(
                    "short read at 0x{addr:08X}: {} of {n} bytes",
                    d.len()
                )));
            }
            out.extend(d);
            progress(Progress {
                phase: Phase::Read,
                done: out.len() as u64,
                total,
            });
        }
        Ok(out)
    }

    fn write_memory(
        &mut self,
        region: MemoryRegion,
        data: &[u8],
        progress: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        if data.len() != region.length as usize {
            return Err(Error::Config(format!(
                "data length {} != region length {}",
                data.len(),
                region.length
            )));
        }
        progress(Progress {
            phase: Phase::SecurityAccess,
            done: 0,
            total: 0,
        });
        self.unlock()?;
        let t = self.transport()?;
        let mut uds = UdsClient::new(t);
        let mut range = vec![ALFID];
        range.extend(encode_address_and_length(
            ALFID,
            region.address,
            region.length,
        )?);
        progress(Progress {
            phase: Phase::Erase,
            done: 0,
            total: u64::from(region.length),
        });
        let st = uds.routine_control(0x01, 0xFF00, &range)?;
        if st.first().is_some_and(|s| *s != 0) {
            return Err(Error::Verify(format!("erase reported status {st:02X?}")));
        }
        let mbl = uds.request_download_with_format(0x00, ALFID, region.address, region.length)?;
        let n = mbl.saturating_sub(2).max(1);
        let mut seq: u8 = 1;
        let mut done = 0u64;
        for chunk in data.chunks(n) {
            uds.transfer_data(seq, chunk)?;
            seq = seq.wrapping_add(1);
            done += chunk.len() as u64;
            progress(Progress {
                phase: Phase::Write,
                done,
                total: u64::from(region.length),
            });
        }
        uds.transfer_exit()?;
        let mut check = range;
        check.extend(crc16_ccitt_false(data).to_be_bytes());
        let st = uds.routine_control(0x01, 0x0202, &check)?;
        if st.first() != Some(&0x00) {
            return Err(Error::Verify(format!(
                "the simulated ECU's checksum routine reported {st:02X?}"
            )));
        }
        uds.ecu_reset(0x01)?;
        Ok(())
    }

    fn plan_read_memory(&mut self, region: MemoryRegion) -> Option<Result<Vec<String>>> {
        let mut lines = Vec::new();
        let mut off = 0u32;
        while off < region.length {
            let n = READ_CHUNK.min(region.length - off);
            let mut req = vec![0x23, ALFID];
            match encode_address_and_length(ALFID, region.address + off, n) {
                Ok(b) => req.extend(b),
                Err(e) => return Some(Err(e)),
            }
            lines.push(
                req.iter()
                    .map(|b| format!("{b:02X}"))
                    .collect::<Vec<_>>()
                    .join(" "),
            );
            off += n;
        }
        Some(Ok(lines))
    }

    fn diag(&mut self) -> Option<&mut dyn DiagTransport> {
        self.transport.as_mut().map(|t| t as &mut dyn DiagTransport)
    }

    fn set_seed_key(&mut self, seed_key: Box<dyn SeedKey>) -> bool {
        self.seed_key = Some(seed_key);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(name: &str) -> UdssimDriver {
        let mut d = UdssimDriver::new();
        let mut cfg = ConnectConfig::default();
        cfg.options.insert("ecu".into(), name.into());
        cfg.options.insert("reset".into(), "1".into());
        d.open(&cfg).unwrap();
        d
    }

    #[test]
    fn identity_and_default_contents() {
        let mut d = open("t-id");
        let id = d.read_ecu_id().unwrap();
        assert_eq!(id.hw_number.as_deref(), Some("OTSIM-0001"));
        assert_eq!(id.vin.as_deref(), Some("SIMULATED0VIN0001"));
        let r = d
            .read_memory(MemoryRegion::new(0x100, 0x90), &mut |_| {})
            .unwrap();
        assert_eq!(
            r,
            (0x100..0x190).map(|i| (i & 0xFF) as u8).collect::<Vec<_>>()
        );
    }

    #[test]
    fn write_needs_a_seed_key_then_round_trips_across_reopen() {
        let mut d = open("t-rw");
        let region = MemoryRegion::new(0x2000, 0x500);
        let data: Vec<u8> = (0..0x500).map(|i| (i * 3 % 256) as u8).collect();
        let e = d.write_memory(region, &data, &mut |_| {}).unwrap_err();
        assert!(e.to_string().contains(TEST_SEED_KEY_ID), "{e}");
        assert!(d.set_seed_key(test_seed_key(TEST_SEED_KEY_ID).unwrap()));
        d.write_memory(region, &data, &mut |_| {}).unwrap();
        d.close().unwrap();
        // A new driver on the same instance sees the write.
        let mut d2 = UdssimDriver::new();
        let mut cfg = ConnectConfig::default();
        cfg.options.insert("ecu".into(), "t-rw".into());
        d2.open(&cfg).unwrap();
        assert_eq!(d2.read_memory(region, &mut |_| {}).unwrap(), data);
        assert!(d2
            .ecu()
            .unwrap()
            .lock()
            .unwrap()
            .server()
            .flash()
            .unwrap()
            .is_valid());
    }

    #[test]
    fn plan_and_diag() {
        let mut d = open("t-plan");
        let lines = d
            .plan_read_memory(MemoryRegion::new(0x10, 0x100))
            .unwrap()
            .unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "23 44 00 00 00 10 00 00 00 80");
        let t = d.diag().unwrap();
        assert_eq!(t.request(&[0x3E, 0x00], 100).unwrap(), vec![0x7E, 0x00]);
        assert!(test_seed_key("other").is_none());
    }
}
