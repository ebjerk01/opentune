//! Offline file-backed driver. Pretends a `.bin` on disk is an ECU's flash, so
//! the whole read/verify/write pipeline is testable with no hardware.
//!
//! Config options (in [`ConnectConfig::options`]):
//! - `file`: path to the flash image to load (required unless `coding_store`).
//! - `out`:  path to write back to (defaults to `file`).
//! - `base`: hex base address mapped to file offset 0 (default `0`).
//! - `ecu`:  ecu name reported by `read_ecu_id` (default `mock-ecu`).
//! - `coding_store`: path to a JSON coding store (`{ "<did>": "<hex>" }`) that
//!   backs an offline UDS [`DiagTransport`] for module-coding reads/writes,
//!   exposed through [`Interface::diag`]. Writes (0x2E) persist back to the file.
use opentune_core::protocol::DiagTransport;
use opentune_core::{
    Capabilities, ConnectConfig, EcuId, Error, Interface, InterfaceInfo, MemoryRegion, Phase,
    Progress, Result,
};
use std::collections::HashMap;
use std::path::PathBuf;

pub mod monitor;

#[derive(Default)]
pub struct MockDriver {
    path: Option<PathBuf>,
    out: Option<PathBuf>,
    base: u32,
    ecu: String,
    data: Vec<u8>,
    open: bool,
    diag: Option<MockDiag>,
    monitor: monitor::MockMonitor,
}

/// An offline UDS [`DiagTransport`] backed by a file of DID → bytes, so module
/// coding (ReadDataByIdentifier 0x22 / WriteDataByIdentifier 0x2E) can be driven
/// end-to-end with no hardware. A write updates the in-memory store and persists
/// the whole store back to its JSON file, so a later `read` sees the new value.
pub struct MockDiag {
    store: HashMap<u16, Vec<u8>>,
    path: PathBuf,
}

impl MockDiag {
    /// Load a coding store from a JSON object mapping each DID (decimal, or
    /// `0x`-prefixed hex) to a hex byte string (whitespace allowed), e.g.
    /// `{ "0x0401": "05 00", "1026": "0000 2A" }`.
    fn load(path: PathBuf) -> Result<Self> {
        let text = std::fs::read_to_string(&path)
            .map_err(|e| Error::Config(format!("read coding store {}: {e}", path.display())))?;
        let obj: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| Error::Config(format!("coding store is not valid JSON: {e}")))?;
        let map = obj
            .as_object()
            .ok_or_else(|| Error::Config("coding store must be a JSON object".into()))?;
        let mut store = HashMap::new();
        for (k, v) in map {
            let did = parse_did(k)?;
            let hex = v.as_str().ok_or_else(|| {
                Error::Config(format!("coding store DID {k} value is not a string"))
            })?;
            store.insert(did, parse_hex(hex)?);
        }
        Ok(MockDiag { store, path })
    }

    /// Persist the current store back to its JSON file, DIDs as `0x`-prefixed
    /// 4-digit hex keys and values as contiguous hex byte strings.
    fn persist(&self) -> Result<()> {
        let mut map = serde_json::Map::new();
        // Sort by DID for a stable, diff-friendly file.
        let mut dids: Vec<u16> = self.store.keys().copied().collect();
        dids.sort_unstable();
        for did in dids {
            let bytes = &self.store[&did];
            let hex: String = bytes.iter().map(|b| format!("{b:02X}")).collect();
            map.insert(format!("0x{did:04X}"), serde_json::Value::String(hex));
        }
        let text = serde_json::to_string_pretty(&serde_json::Value::Object(map))
            .map_err(|e| Error::Config(format!("serialize coding store: {e}")))?;
        std::fs::write(&self.path, text)
            .map_err(|e| Error::Config(format!("write coding store {}: {e}", self.path.display())))
    }
}

/// Parse a DID key: decimal, or `0x`/`0X`-prefixed hex.
fn parse_did(s: &str) -> Result<u16> {
    let s = s.trim();
    let parsed = if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u16::from_str_radix(hex, 16)
    } else {
        s.parse::<u16>()
    };
    parsed.map_err(|e| Error::Config(format!("bad DID `{s}` in coding store: {e}")))
}

/// Parse a hex byte string (whitespace between bytes allowed) into bytes.
fn parse_hex(s: &str) -> Result<Vec<u8>> {
    let compact: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if !compact.len().is_multiple_of(2) {
        return Err(Error::Config(format!(
            "coding store hex `{s}` has an odd number of nibbles"
        )));
    }
    (0..compact.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&compact[i..i + 2], 16)
                .map_err(|e| Error::Config(format!("bad hex in coding store `{s}`: {e}")))
        })
        .collect()
}

impl DiagTransport for MockDiag {
    fn request(&mut self, req: &[u8], _timeout_ms: u64) -> Result<Vec<u8>> {
        if req.is_empty() {
            return Err(Error::Protocol("empty request".into()));
        }
        match req[0] {
            0x22 => {
                if req.len() < 3 {
                    return Err(Error::Protocol("short ReadDataByIdentifier request".into()));
                }
                let did = ((req[1] as u16) << 8) | req[2] as u16;
                match self.store.get(&did) {
                    Some(data) => {
                        let mut r = vec![0x62, req[1], req[2]];
                        r.extend_from_slice(data);
                        Ok(r)
                    }
                    // requestOutOfRange: this module has no such DID.
                    None => Ok(vec![0x7F, 0x22, 0x31]),
                }
            }
            0x2E => {
                if req.len() < 3 {
                    return Err(Error::Protocol(
                        "short WriteDataByIdentifier request".into(),
                    ));
                }
                let did = ((req[1] as u16) << 8) | req[2] as u16;
                self.store.insert(did, req[3..].to_vec());
                self.persist()?;
                Ok(vec![0x6E, req[1], req[2]])
            }
            // serviceNotSupported for anything the coding engine does not use.
            other => Ok(vec![0x7F, other, 0x11]),
        }
    }
}

impl MockDriver {
    pub fn new() -> Self {
        Self {
            ecu: "mock-ecu".into(),
            ..Default::default()
        }
    }

    fn offset(&self, address: u32) -> Result<usize> {
        let off = address.checked_sub(self.base).ok_or_else(|| {
            Error::Config(format!(
                "address 0x{address:08X} below base 0x{:08X}",
                self.base
            ))
        })?;
        Ok(off as usize)
    }
}

impl Interface for MockDriver {
    fn info(&self) -> InterfaceInfo {
        InterfaceInfo {
            id: "mock".into(),
            display_name: "Mock / file driver".into(),
            firmware: Some("offline".into()),
            serial: None,
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            read_memory: true,
            write_memory: true,
            battery_voltage: true,
            ..Default::default()
        }
    }

    fn open(&mut self, cfg: &ConnectConfig) -> Result<()> {
        // A coding store backs the UDS DiagTransport; a flash `file` backs the
        // memory read/write pipeline. At least one must be given.
        if let Some(store) = cfg.options.get("coding_store") {
            self.diag = Some(MockDiag::load(PathBuf::from(store))?);
        }
        match cfg.options.get("file") {
            Some(file) => {
                self.path = Some(PathBuf::from(file));
                self.data = std::fs::read(self.path.as_ref().unwrap())?;
            }
            None if self.diag.is_some() => {}
            None => {
                return Err(Error::Config(
                    "mock driver needs options[\"file\"] or options[\"coding_store\"]".into(),
                ))
            }
        }
        self.out = cfg.options.get("out").map(PathBuf::from);
        self.base = match cfg.options.get("base") {
            Some(s) => u32::from_str_radix(s.trim_start_matches("0x"), 16)
                .map_err(|e| Error::Config(format!("bad base: {e}")))?,
            None => 0,
        };
        if let Some(e) = cfg.options.get("ecu") {
            self.ecu = e.clone();
        }
        self.open = true;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.open = false;
        Ok(())
    }

    fn battery_millivolts(&mut self) -> Result<Option<u32>> {
        Ok(Some(12_600))
    }

    fn read_ecu_id(&mut self) -> Result<EcuId> {
        if !self.open {
            return Err(Error::NotConnected);
        }
        Ok(EcuId {
            ecu_name: Some(self.ecu.clone()),
            sw_number: Some(format!("{} bytes", self.data.len())),
            ..Default::default()
        })
    }

    fn read_memory(
        &mut self,
        region: MemoryRegion,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<Vec<u8>> {
        if !self.open {
            return Err(Error::NotConnected);
        }
        let start = self.offset(region.address)?;
        let end = start
            .checked_add(region.length as usize)
            .filter(|&e| e <= self.data.len())
            .ok_or_else(|| Error::Config("read past end of image".into()))?;
        progress(Progress {
            phase: Phase::Read,
            done: 0,
            total: region.length as u64,
        });
        let out = self.data[start..end].to_vec();
        progress(Progress {
            phase: Phase::Read,
            done: region.length as u64,
            total: region.length as u64,
        });
        Ok(out)
    }

    /// The mock sends nothing on any bus; its plan is the one file slice a
    /// read would copy, checked exactly like [`read_memory`](Self::read_memory).
    fn plan_read_memory(&mut self, region: MemoryRegion) -> Option<Result<Vec<String>>> {
        let plan = (|| {
            if !self.open {
                return Err(Error::NotConnected);
            }
            let start = self.offset(region.address)?;
            let end = start
                .checked_add(region.length as usize)
                .filter(|&e| e <= self.data.len())
                .ok_or_else(|| Error::Config("read past end of image".into()))?;
            let file = self
                .path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<no file>".into());
            Ok(vec![format!(
                "copy {file} [0x{start:X}..0x{end:X}] ({} bytes)",
                region.length
            )])
        })();
        Some(plan)
    }

    fn write_memory(
        &mut self,
        region: MemoryRegion,
        data: &[u8],
        progress: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        if !self.open {
            return Err(Error::NotConnected);
        }
        if data.len() != region.length as usize {
            return Err(Error::Config("data length != region length".into()));
        }
        let start = self.offset(region.address)?;
        let end = start
            .checked_add(data.len())
            .filter(|&e| e <= self.data.len())
            .ok_or_else(|| Error::Config("write past end of image".into()))?;
        progress(Progress {
            phase: Phase::Write,
            done: 0,
            total: data.len() as u64,
        });
        self.data[start..end].copy_from_slice(data);
        let out = self.out.clone().or_else(|| self.path.clone()).unwrap();
        std::fs::write(out, &self.data)?;
        progress(Progress {
            phase: Phase::Write,
            done: data.len() as u64,
            total: data.len() as u64,
        });
        Ok(())
    }

    fn diag(&mut self) -> Option<&mut dyn DiagTransport> {
        self.diag.as_mut().map(|d| d as &mut dyn DiagTransport)
    }

    /// Synthetic bus traffic for offline CAN-dump work (see [`monitor`]).
    fn can_monitor(&mut self) -> Option<&mut dyn opentune_core::CanMonitor> {
        Some(&mut self.monitor)
    }
}
