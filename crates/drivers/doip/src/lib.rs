//! The `doip` interface driver: UDS over Ethernet to a vehicle's ISO 13400
//! DoIP entity, so every OpenTune engine (coding, adaptation, auto-scan,
//! service functions, live logging, OBD inspection) and the UDS module flasher
//! run over `--driver doip` with no CAN adapter.
//!
//! `open` finds the entity — dialled directly by address, or first found by a
//! UDP vehicle identification request (all, by VIN or by EID) — then connects
//! its TCP data port and runs routing activation. [`Interface::diag`] hands out
//! the activated [`DoIpTransport`]; requests go to the target logical address
//! (the entity's own unless `target` says otherwise). Options: see
//! [`options`] and `docs/DRIVERS.md`.
//!
//! - `read_memory`: ReadMemoryByAddress (`0x23`, format `0x44`) in
//!   `read_chunk` pieces, after an optional session change and SecurityAccess
//!   with a caller-supplied [`SeedKey`]. OpenTune ships no OEM algorithm.
//! - `write_memory`: not offered. Programming a module goes through the UDS
//!   module flasher, which runs on [`Interface::diag`].
//! - No raw CAN, no battery voltage (DoIP carries neither).
//!
//! Validation state: tested against the `opentune-udssim` DoIP entity on
//! loopback only. 🔒 Real vehicles (gateway routing, activation lines,
//! central-security activation) are untested; the desktop keeps the driver in
//! the Bench tier.
pub mod options;

use opentune_core::protocol::doip::routing_activation;
use opentune_core::protocol::doip_discovery::{
    connect_entity, ConnectOptions, DiscoveredEntity, Discovery, DoIpUdpClient,
};
use opentune_core::protocol::seedkey::SeedKey;
use opentune_core::protocol::uds::{encode_address_and_length, seed_is_unlocked};
use opentune_core::protocol::{DiagTransport, DoIpTransport, UdsClient};
use opentune_core::{
    Capabilities, ConnectConfig, EcuId, Error, Interface, InterfaceInfo, MemoryRegion, Phase,
    Progress, Result,
};
use options::{DoipOptions, Locate};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// The registry id.
pub const DRIVER_ID: &str = "doip";
/// The registry description.
pub const DRIVER_NAME: &str = "DoIP (ISO 13400, UDS over Ethernet)";

/// ReadMemoryByAddress format: 4-byte address, 4-byte size.
const ALFID: u8 = 0x44;

/// The open connection.
struct Session {
    transport: DoIpTransport<TcpStream>,
    peer: SocketAddr,
    entity: Option<DiscoveredEntity>,
    opts: DoipOptions,
}

/// The `doip` interface driver. See the crate docs.
#[derive(Default)]
pub struct DoipDriver {
    session: Option<Session>,
    seed_key: Option<Box<dyn SeedKey>>,
}

fn hex_line(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Pick the entity to connect from a discovery round: the only one, or the
/// only one at the requested target address.
pub fn pick_entity(found: &Discovery, target: Option<u16>) -> Result<DiscoveredEntity> {
    let list = &found.entities;
    if let [one] = list.as_slice() {
        return Ok(one.clone());
    }
    if list.is_empty() {
        let mut why = String::new();
        if !found.nacks.is_empty() {
            let codes: Vec<String> = found
                .nacks
                .iter()
                .map(|(a, c)| format!("{a}: 0x{c:02X}"))
                .collect();
            why.push_str(&format!("; negative acks: {}", codes.join(", ")));
        }
        if found.rejected > 0 {
            why.push_str(&format!("; {} malformed answer(s) dropped", found.rejected));
        }
        return Err(Error::Protocol(format!(
            "no DoIP entity answered the vehicle identification request{why}"
        )));
    }
    if let Some(t) = target {
        let at: Vec<&DiscoveredEntity> = list
            .iter()
            .filter(|e| e.info.logical_address == t)
            .collect();
        if let [one] = at.as_slice() {
            return Ok((*one).clone());
        }
    }
    let names: Vec<String> = list
        .iter()
        .map(|e| {
            format!(
                "{} (address 0x{:04X}, EID {}, VIN {})",
                e.addr.ip(),
                e.info.logical_address,
                e.info.eid_string(),
                e.info.vin_string()
            )
        })
        .collect();
    Err(Error::Config(format!(
        "doip: {} entities answered: {}; choose one with ip=… or eid=…",
        list.len(),
        names.join("; ")
    )))
}

/// Run the discovery `locate` describes (a no-op for a direct address).
pub fn discover(opts: &DoipOptions) -> Result<Option<Discovery>> {
    let Locate::Discover { dest, request } = &opts.locate else {
        return Ok(None);
    };
    let client = DoIpUdpClient::for_destination(*dest)?;
    let found = if opts.locate.is_group() {
        client.identify(*dest, request, opts.discover_timeout_ms)?
    } else {
        client.identify_first(*dest, request, opts.discover_timeout_ms)?
    };
    Ok(Some(found))
}

impl DoipDriver {
    /// A closed driver.
    pub fn new() -> Self {
        Self::default()
    }

    fn session(&mut self) -> Result<&mut Session> {
        self.session.as_mut().ok_or(Error::NotConnected)
    }

    /// The entity found by discovery (`None` for a direct address).
    pub fn entity(&self) -> Option<&DiscoveredEntity> {
        self.session.as_ref().and_then(|s| s.entity.as_ref())
    }

    /// The TCP peer of the open connection.
    pub fn peer(&self) -> Option<SocketAddr> {
        self.session.as_ref().map(|s| s.peer)
    }

    /// The logical address requests go to.
    pub fn target_address(&self) -> Option<u16> {
        self.session.as_ref().map(|s| s.transport.target_address)
    }

    /// The entity's logical address from routing activation.
    pub fn entity_address(&self) -> Option<u16> {
        self.session
            .as_ref()
            .and_then(|s| s.transport.entity_address())
    }

    /// Whether routing activation answered `0x11` (activated, confirmation
    /// required) instead of a plain `0x10`. The entity may still gate
    /// diagnostics behind an out-of-band confirmation.
    pub fn activation_confirmation_required(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|s| s.transport.activation_confirmation_required())
    }

    fn connect(opts: DoipOptions) -> Result<Session> {
        match discover(&opts)? {
            Some(found) => {
                let entity = pick_entity(&found, opts.target)?;
                let copts = ConnectOptions {
                    tcp_port: opts.tcp_port,
                    tester_address: opts.tester,
                    target_address: opts.target,
                    activation_type: opts.activation,
                    timeout_ms: opts.timeout_ms,
                };
                let transport = connect_entity(&entity, &copts)
                    .map_err(|e| connect_error(entity.addr.ip(), opts.tcp_port, e))?;
                let mut peer = entity.addr;
                peer.set_port(opts.tcp_port);
                Ok(Session {
                    transport,
                    peer,
                    entity: Some(entity),
                    opts,
                })
            }
            None => {
                let Locate::Direct(addr) = opts.locate else {
                    unreachable!("discover() handled every other kind")
                };
                let transport = Self::dial(addr, &opts)
                    .map_err(|e| connect_error(addr.ip(), addr.port(), e))?;
                Ok(Session {
                    transport,
                    peer: addr,
                    entity: None,
                    opts,
                })
            }
        }
    }

    /// TCP connect + routing activation at a known address.
    fn dial(addr: SocketAddr, opts: &DoipOptions) -> Result<DoIpTransport<TcpStream>> {
        let stream =
            TcpStream::connect_timeout(&addr, Duration::from_millis(opts.timeout_ms.max(1)))?;
        stream.set_nodelay(true)?;
        let mut t = DoIpTransport::with_stream(stream, opts.tester, opts.target.unwrap_or(0));
        t.activate_with(
            opts.activation.unwrap_or(routing_activation::TYPE_DEFAULT),
            opts.timeout_ms,
        )?;
        if opts.target.is_none() {
            t.target_address = t.entity_address().ok_or_else(|| {
                Error::Protocol("routing activation did not name the entity address".into())
            })?;
        }
        Ok(t)
    }

    /// Session change + SecurityAccess before a block read, as configured.
    fn prepare_read(&mut self, progress: &mut dyn FnMut(Progress)) -> Result<()> {
        let Self { session, seed_key } = self;
        let s = session.as_mut().ok_or(Error::NotConnected)?;
        let opts = s.opts.clone();
        let mut uds = UdsClient::new(&mut s.transport);
        progress(Progress {
            phase: Phase::Connect,
            done: 0,
            total: 0,
        });
        if let Some(sess) = opts.session {
            uds.session(sess).map_err(|e| {
                Error::Protocol(format!(
                    "session 0x{sess:02X} before the read: {e} (check ignition, or set session=none / another session)"
                ))
            })?;
        }
        match seed_key.as_deref() {
            Some(sk) => {
                progress(Progress {
                    phase: Phase::SecurityAccess,
                    done: 0,
                    total: 0,
                });
                let level = opts.seedkey_level;
                let seed = uds.request_seed(level)?;
                if !seed_is_unlocked(&seed) {
                    let key = sk.compute(&seed, level)?;
                    uds.send_key_detailed(level + 1, &key)?;
                }
                progress(Progress {
                    phase: Phase::SecurityAccess,
                    done: 1,
                    total: 1,
                });
            }
            None if opts.security_required => {
                return Err(Error::Safety(
                    "this read needs SecurityAccess: supply a seed/key handler (nothing was sent)"
                        .into(),
                ));
            }
            None => {}
        }
        Ok(())
    }
}

fn connect_error(ip: std::net::IpAddr, port: u16, e: Error) -> Error {
    match e {
        Error::Io(io) => Error::Io(std::io::Error::new(
            io.kind(),
            format!("DoIP entity {ip} TCP {port}: {io}"),
        )),
        other => other,
    }
}

impl Interface for DoipDriver {
    fn info(&self) -> InterfaceInfo {
        let (firmware, serial) = match &self.session {
            Some(s) => (
                Some(format!("{} → 0x{:04X}", s.peer, s.transport.target_address)),
                s.entity.as_ref().map(|e| e.info.eid_string()),
            ),
            None => (None, None),
        };
        InterfaceInfo {
            id: DRIVER_ID.into(),
            display_name: DRIVER_NAME.into(),
            firmware,
            serial,
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            doip: true,
            read_memory: true,
            ..Capabilities::default()
        }
    }

    fn open(&mut self, cfg: &ConnectConfig) -> Result<()> {
        self.session = None;
        let opts = DoipOptions::from_config(cfg)?;
        self.session = Some(Self::connect(opts)?);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.session = None;
        Ok(())
    }

    fn read_ecu_id(&mut self) -> Result<EcuId> {
        let s = self.session()?;
        let announced_vin = s
            .entity
            .as_ref()
            .map(|e| e.info.vin_string())
            .filter(|v| !v.is_empty());
        let mut raw = std::collections::BTreeMap::new();
        raw.insert(
            "doip_target".to_string(),
            format!("0x{:04X}", s.transport.target_address),
        );
        if let Some(e) = &s.entity {
            raw.insert("doip_eid".to_string(), e.info.eid_string());
        }
        let mut uds = UdsClient::new(&mut s.transport);
        let mut did = |d: u16| {
            uds.read_data_by_id(d)
                .ok()
                .map(|v| String::from_utf8_lossy(&v).trim().to_string())
                .filter(|v| !v.is_empty())
        };
        Ok(EcuId {
            hw_number: did(0xF187).or_else(|| did(0xF191)),
            sw_number: did(0xF188),
            vin: did(0xF190).or(announced_vin),
            supplier: did(0xF18A),
            ecu_name: did(0xF197),
            raw,
        })
    }

    fn read_memory(
        &mut self,
        region: MemoryRegion,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<Vec<u8>> {
        self.prepare_read(progress)?;
        let s = self.session()?;
        let chunk = s.opts.read_chunk;
        let mut uds = UdsClient::new(&mut s.transport);
        let total = u64::from(region.length);
        let mut out = Vec::with_capacity(region.length as usize);
        progress(Progress {
            phase: Phase::Read,
            done: 0,
            total,
        });
        while (out.len() as u64) < total {
            let n = chunk.min(region.length - out.len() as u32);
            let addr = region.address + out.len() as u32;
            let d = uds.read_memory_by_address(ALFID, addr, n).map_err(|e| {
                Error::Protocol(format!(
                    "ReadMemoryByAddress at 0x{addr:08X} ({} of {total} bytes read): {e}",
                    out.len()
                ))
            })?;
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
        _region: MemoryRegion,
        _data: &[u8],
        _progress: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        Err(Error::Unsupported(
            "the doip driver does not program memory blocks; flash a module over UDS with the \
             module flasher (`opentune flash --container …`), which runs on the driver's \
             diagnostic transport"
                .into(),
        ))
    }

    fn plan_read_memory(&mut self, region: MemoryRegion) -> Option<Result<Vec<String>>> {
        let s = match self.session.as_ref() {
            Some(s) => s,
            None => return Some(Err(Error::NotConnected)),
        };
        let o = &s.opts;
        let mut lines = Vec::new();
        if let Some(sess) = o.session {
            lines.push(hex_line(&[0x10, sess]));
        }
        if self.seed_key.is_some() {
            lines.push(hex_line(&[0x27, o.seedkey_level]));
            lines.push(format!("{} <key>", hex_line(&[0x27, o.seedkey_level + 1])));
        } else if o.security_required {
            return Some(Err(Error::Safety(
                "this read needs SecurityAccess: supply a seed/key handler".into(),
            )));
        }
        let mut off = 0u32;
        while off < region.length {
            let n = o.read_chunk.min(region.length - off);
            let mut req = vec![0x23, ALFID];
            match encode_address_and_length(ALFID, region.address + off, n) {
                Ok(b) => req.extend(b),
                Err(e) => return Some(Err(e)),
            }
            lines.push(hex_line(&req));
            off += n;
        }
        Some(Ok(lines))
    }

    fn diag(&mut self) -> Option<&mut dyn DiagTransport> {
        self.session
            .as_mut()
            .map(|s| &mut s.transport as &mut dyn DiagTransport)
    }

    fn set_seed_key(&mut self, seed_key: Box<dyn SeedKey>) -> bool {
        self.seed_key = Some(seed_key);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentune_core::protocol::doip::VehicleInfo;

    fn ent(ip: [u8; 4], addr: u16, eid: u8) -> DiscoveredEntity {
        DiscoveredEntity {
            addr: (ip, 13400).into(),
            info: VehicleInfo {
                vin: *b"WVWZZZ00000000000",
                logical_address: addr,
                eid: [2, 0, 0, 0, 0, eid],
                gid: [0; 6],
                further_action: 0,
                sync_status: None,
            },
        }
    }

    #[test]
    fn picking_an_entity() {
        let mut d = Discovery::default();
        let e = pick_entity(&d, None).unwrap_err().to_string();
        assert!(e.contains("no DoIP entity answered"), "{e}");
        d.nacks
            .push(((std::net::Ipv4Addr::LOCALHOST, 13400).into(), 0x04));
        d.rejected = 2;
        let e = pick_entity(&d, None).unwrap_err().to_string();
        assert!(e.contains("0x04") && e.contains("2 malformed"), "{e}");

        let a = ent([192, 0, 2, 10], 0x0010, 0x10);
        let b = ent([192, 0, 2, 11], 0x0020, 0x20);
        d.entities = vec![a.clone()];
        assert_eq!(pick_entity(&d, Some(0x0099)).unwrap(), a);
        d.entities = vec![a.clone(), b.clone()];
        let e = pick_entity(&d, None).unwrap_err().to_string();
        assert!(
            e.contains("2 entities answered") && e.contains("192.0.2.11"),
            "{e}"
        );
        assert_eq!(pick_entity(&d, Some(0x0020)).unwrap(), b);
    }

    #[test]
    fn closed_driver_is_honest() {
        let mut d = DoipDriver::new();
        let caps = d.capabilities();
        assert!(caps.doip && caps.read_memory);
        assert!(!caps.can && !caps.write_memory && !caps.battery_voltage && !caps.kline);
        assert!(d.diag().is_none());
        assert!(matches!(d.read_ecu_id(), Err(Error::NotConnected)));
        assert!(matches!(
            d.plan_read_memory(MemoryRegion::new(0, 16)),
            Some(Err(Error::NotConnected))
        ));
        let e = d
            .write_memory(MemoryRegion::new(0, 1), &[0], &mut |_| {})
            .unwrap_err();
        assert!(matches!(e, Error::Unsupported(_)), "{e}");
        assert_eq!(d.info().id, DRIVER_ID);
        assert!(d.info().firmware.is_none());
    }

    #[test]
    fn bad_options_fail_before_any_network_io() {
        let mut d = DoipDriver::new();
        let mut cfg = ConnectConfig::default();
        cfg.options
            .insert("port".into(), "192.0.2.10?bogus=1".into());
        assert!(matches!(d.open(&cfg), Err(Error::Config(_))));
    }
}
