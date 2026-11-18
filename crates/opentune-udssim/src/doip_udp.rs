//! The DoIP entity's **UDP side** (ISO 13400-2 §6.4-6.6): vehicle
//! identification (`0x0001` / by EID `0x0002` / by VIN `0x0003` → `0x0004`),
//! unsolicited vehicle announcements, entity status (`0x4001` → `0x4002`) and
//! diagnostic power mode (`0x4003` → `0x4004`), plus loopback socket servers
//! for both the UDP side and the TCP data side ([`DoIpSimStream`]).
//!
//! [`DoIpUdpEntity::handle`] is the pure datagram-in / datagram-out entity;
//! [`UdpEntityServer`] puts it on a socket (loopback by default) and
//! [`TcpEntityServer`] serves [`DoIpSimStream`] connections, so OpenTune's
//! `DoIpUdpClient` + `connect_entity` (and python-doipclient) run end to end.
//!
//! Entity rules: the strict generic-header check (incorrect pattern /
//! unsupported version `0x00`, too large `0x02`, bad length `0x04`), TCP-only
//! or unknown payload types `0x01`; an EID / VIN request that does not match
//! gets no answer; answers and NACKs from other entities on the discovery port
//! are ignored (no NACK ping-pong). Fault injection: no reply, a reply with a
//! bad protocol version, or a generic NACK instead of the answer.
//!
//! Offline conformance only.

use crate::doip::DoIpSimStream;
use opentune_core::protocol::doip::{generic_nack, payload_type, DoIpMessage, VehicleInfo};
use opentune_core::protocol::doip_discovery::{
    node_type, parse_datagram, EntityStatus, PowerMode, VehicleIdRequest,
};
use opentune_core::protocol::DoIpStream;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

/// The placeholder VIN the simulator announces (also its `0xF190` DID in the
/// `doip_tcp_entity` example). Not a real vehicle.
pub const SIM_VIN: &[u8; 17] = b"WVWZZZ00000000000";

/// The simulator's default EID: a locally administered MAC-style placeholder.
pub const SIM_EID: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x10];

/// What the entity says about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityIdentity {
    /// Vehicle announcement / identification response (VIN, logical address,
    /// EID, GID, further action, sync status).
    pub info: VehicleInfo,
    /// Entity status response.
    pub status: EntityStatus,
    /// Diagnostic power mode.
    pub power_mode: PowerMode,
}

impl EntityIdentity {
    /// A DoIP node at `logical_address` with the placeholder VIN and EID.
    pub fn new(logical_address: u16) -> Self {
        Self {
            info: VehicleInfo {
                vin: *SIM_VIN,
                logical_address,
                eid: SIM_EID,
                gid: SIM_EID,
                further_action: 0x00,
                sync_status: Some(0x00),
            },
            status: EntityStatus {
                node_type: node_type::NODE,
                max_open_sockets: 1,
                open_sockets: 0,
                max_data_size: Some(0x0000_0FFF),
            },
            power_mode: PowerMode::Ready,
        }
    }
}

impl Default for EntityIdentity {
    fn default() -> Self {
        Self::new(0x0010)
    }
}

/// A misbehaviour for the next answer (or every answer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UdpFault {
    /// Drop the request: send nothing.
    NoReply,
    /// Answer with a header whose protocol version / inverse do not match.
    BadVersion,
    /// Answer with a generic negative ack carrying this code.
    Nack(u8),
}

impl std::str::FromStr for UdpFault {
    type Err = String;

    /// `no-reply`, `bad-version` or `nack:CODE` (hex or decimal).
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "no-reply" => Ok(Self::NoReply),
            "bad-version" => Ok(Self::BadVersion),
            _ => {
                let code = s.strip_prefix("nack:").ok_or_else(|| {
                    format!("unknown UDP fault {s:?} (no-reply|bad-version|nack:CODE)")
                })?;
                let v = match code.strip_prefix("0x").or_else(|| code.strip_prefix("0X")) {
                    Some(h) => u8::from_str_radix(h, 16),
                    None => code.parse(),
                };
                v.map(Self::Nack)
                    .map_err(|_| format!("bad NACK code {code:?}"))
            }
        }
    }
}

/// The UDP side of a simulated DoIP entity.
#[derive(Debug, Clone)]
pub struct DoIpUdpEntity {
    identity: EntityIdentity,
    once: VecDeque<UdpFault>,
    always: Option<UdpFault>,
    requests: usize,
}

impl DoIpUdpEntity {
    /// An entity with `identity`.
    pub fn new(identity: EntityIdentity) -> Self {
        Self {
            identity,
            once: VecDeque::new(),
            always: None,
            requests: 0,
        }
    }

    /// What the entity announces.
    pub fn identity(&self) -> &EntityIdentity {
        &self.identity
    }

    /// What the entity announces, mutably.
    pub fn identity_mut(&mut self) -> &mut EntityIdentity {
        &mut self.identity
    }

    /// Apply `fault` to the next answer only (queued in order).
    pub fn inject_once(&mut self, fault: UdpFault) {
        self.once.push_back(fault);
    }

    /// Apply `fault` to every answer (after any one-shot faults), or clear it.
    pub fn set_fault(&mut self, fault: Option<UdpFault>) {
        self.always = fault;
    }

    /// Datagrams handled so far.
    pub fn request_count(&self) -> usize {
        self.requests
    }

    /// The vehicle announcement datagram (also the identification response).
    pub fn announcement(&self) -> Vec<u8> {
        DoIpMessage::new(
            payload_type::VEHICLE_ANNOUNCEMENT,
            self.identity.info.to_payload(),
        )
        .encode()
    }

    fn nack(code: u8) -> Vec<u8> {
        DoIpMessage::new(payload_type::GENERIC_NEGATIVE_ACK, vec![code]).encode()
    }

    /// Handle one received datagram; `Some` is the datagram to send back.
    pub fn handle(&mut self, datagram: &[u8]) -> Option<Vec<u8>> {
        self.requests += 1;
        let msg = match parse_datagram(datagram) {
            Ok((_, msg)) => msg,
            Err(code) => return Some(Self::nack(code)),
        };
        let reply = match msg.payload_type {
            payload_type::VEHICLE_ID_REQUEST
            | payload_type::VEHICLE_ID_REQUEST_EID
            | payload_type::VEHICLE_ID_REQUEST_VIN => {
                let req = VehicleIdRequest::from_message(&msg).ok()?;
                if !req.matches(&self.identity.info) {
                    return None;
                }
                self.announcement()
            }
            payload_type::ENTITY_STATUS_REQUEST => DoIpMessage::new(
                payload_type::ENTITY_STATUS_RESPONSE,
                self.identity.status.to_payload(),
            )
            .encode(),
            payload_type::POWER_MODE_REQUEST => DoIpMessage::new(
                payload_type::POWER_MODE_RESPONSE,
                vec![self.identity.power_mode.to_byte()],
            )
            .encode(),
            // Answers and NACKs from other entities / testers on the shared
            // discovery port: never answered (no NACK ping-pong).
            payload_type::GENERIC_NEGATIVE_ACK
            | payload_type::VEHICLE_ANNOUNCEMENT
            | payload_type::ENTITY_STATUS_RESPONSE
            | payload_type::POWER_MODE_RESPONSE => return None,
            // TCP-only (routing, alive check, diagnostics) or unknown types.
            _ => return Some(Self::nack(generic_nack::UNKNOWN_PAYLOAD_TYPE)),
        };
        match self.once.pop_front().or(self.always) {
            None => Some(reply),
            Some(UdpFault::NoReply) => None,
            Some(UdpFault::Nack(code)) => Some(Self::nack(code)),
            Some(UdpFault::BadVersion) => {
                let mut bad = reply;
                bad[0] = 0x02;
                bad[1] = 0x02; // not the inverse of 0x02
                Some(bad)
            }
        }
    }
}

const POLL: Duration = Duration::from_millis(20);

/// A [`DoIpUdpEntity`] on a UDP socket, served from a background thread until
/// dropped.
pub struct UdpEntityServer {
    sock: UdpSocket,
    entity: Arc<Mutex<DoIpUdpEntity>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl UdpEntityServer {
    /// Bind `bind` (e.g. `127.0.0.1:0`, or `0.0.0.0:13400` for a LAN demo)
    /// and start answering.
    pub fn spawn(bind: SocketAddr, entity: DoIpUdpEntity) -> std::io::Result<Self> {
        let sock = UdpSocket::bind(bind)?;
        sock.set_read_timeout(Some(POLL))?;
        let entity = Arc::new(Mutex::new(entity));
        let stop = Arc::new(AtomicBool::new(false));
        let (rx, ent, flag) = (sock.try_clone()?, entity.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while !flag.load(Ordering::Relaxed) {
                let Ok((n, from)) = rx.recv_from(&mut buf) else {
                    continue;
                };
                let reply = ent.lock().map(|mut e| e.handle(&buf[..n])).ok().flatten();
                if let Some(reply) = reply {
                    let _ = rx.send_to(&reply, from);
                }
            }
        });
        Ok(Self {
            sock,
            entity,
            stop,
            thread: Some(thread),
        })
    }

    /// The bound address.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.sock.local_addr()
    }

    /// The entity, to change its identity or inject faults while it runs.
    pub fn entity(&self) -> MutexGuard<'_, DoIpUdpEntity> {
        self.entity.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Send `count` unsolicited vehicle announcements to `to`, `interval`
    /// apart (ISO: 3 at 500 ms after start-up), from the entity's own port.
    pub fn announce(
        &self,
        to: SocketAddr,
        count: usize,
        interval: Duration,
    ) -> std::io::Result<()> {
        if to.ip().is_ipv4() && to.ip() != std::net::Ipv4Addr::LOCALHOST {
            self.sock.set_broadcast(true)?;
        }
        for i in 0..count {
            if i > 0 {
                std::thread::sleep(interval);
            }
            let ann = self.entity().announcement();
            self.sock.send_to(&ann, to)?;
        }
        Ok(())
    }
}

impl Drop for UdpEntityServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// [`DoIpSimStream`] connections on a TCP listener (the DoIP data port),
/// one at a time, each against a fresh entity from `make`. Served from a
/// background thread until dropped.
pub struct TcpEntityServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl TcpEntityServer {
    /// Bind `bind` and serve; `make` builds the entity for each connection.
    pub fn spawn<F>(bind: SocketAddr, make: F) -> std::io::Result<Self>
    where
        F: Fn() -> DoIpSimStream + Send + 'static,
    {
        let listener = TcpListener::bind(bind)?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((sock, _)) => {
                        let _ = serve_tcp(sock, make(), &flag);
                    }
                    Err(_) => std::thread::sleep(POLL),
                }
            }
        });
        Ok(Self {
            addr,
            stop,
            thread: Some(thread),
        })
    }

    /// The bound address.
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for TcpEntityServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Bridge one TCP connection to `entity` until the peer closes or `stop`.
fn serve_tcp(
    mut sock: TcpStream,
    mut entity: DoIpSimStream,
    stop: &AtomicBool,
) -> std::io::Result<()> {
    sock.set_nonblocking(false)?;
    sock.set_nodelay(true)?;
    sock.set_read_timeout(Some(POLL))?;
    let mut buf = [0u8; 4096];
    while !stop.load(Ordering::Relaxed) {
        match sock.read(&mut buf) {
            Ok(0) => return Ok(()),
            // The entity never fails a write; framing errors become NACKs.
            Ok(n) => {
                let _ = entity.write_all(&buf[..n]);
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => return Err(e),
        }
        let out = entity.take_outbound();
        if !out.is_empty() {
            Write::write_all(&mut sock, &out)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity() -> DoIpUdpEntity {
        DoIpUdpEntity::new(EntityIdentity::default())
    }

    fn decode(d: &[u8]) -> DoIpMessage {
        parse_datagram(d).expect("valid answer").1
    }

    #[test]
    fn identification_by_all_eid_and_vin() {
        let mut e = entity();
        let all = e
            .handle(&VehicleIdRequest::All.to_message().encode())
            .unwrap();
        let info = VehicleInfo::parse(&decode(&all).payload).unwrap();
        assert_eq!(info, e.identity().info);
        assert!(e
            .handle(&VehicleIdRequest::ByEid(SIM_EID).to_message().encode())
            .is_some());
        assert!(e
            .handle(&VehicleIdRequest::ByEid([9; 6]).to_message().encode())
            .is_none());
        assert!(e
            .handle(&VehicleIdRequest::ByVin(*SIM_VIN).to_message().encode())
            .is_some());
        assert!(e
            .handle(&VehicleIdRequest::ByVin([b'X'; 17]).to_message().encode())
            .is_none());
        // The default version 0xFF is accepted for identification requests.
        assert!(e
            .handle(&VehicleIdRequest::All.to_message().encode_with_version(0xFF))
            .is_some());
    }

    #[test]
    fn status_and_power_mode() {
        let mut e = entity();
        let s = e
            .handle(&DoIpMessage::new(payload_type::ENTITY_STATUS_REQUEST, vec![]).encode())
            .unwrap();
        let m = decode(&s);
        assert_eq!(m.payload_type, payload_type::ENTITY_STATUS_RESPONSE);
        assert_eq!(
            EntityStatus::parse(&m.payload).unwrap(),
            e.identity().status
        );
        e.identity_mut().power_mode = PowerMode::NotReady;
        let p = e
            .handle(&DoIpMessage::new(payload_type::POWER_MODE_REQUEST, vec![]).encode())
            .unwrap();
        assert_eq!(decode(&p).payload, vec![0x00]);
    }

    #[test]
    fn header_and_type_errors_are_generic_nacks() {
        let mut e = entity();
        let nack_code = |d: Vec<u8>| {
            let m = decode(&d);
            assert_eq!(m.payload_type, payload_type::GENERIC_NEGATIVE_ACK);
            m.payload[0]
        };
        let mut bad = VehicleIdRequest::All.to_message().encode();
        bad[1] = 0;
        assert_eq!(
            nack_code(e.handle(&bad).unwrap()),
            generic_nack::INCORRECT_PATTERN
        );
        let v9 = VehicleIdRequest::All.to_message().encode_with_version(9);
        assert_eq!(
            nack_code(e.handle(&v9).unwrap()),
            generic_nack::INCORRECT_PATTERN
        );
        let mut big = DoIpMessage::new(0x4001, vec![]).encode();
        big[4..8].copy_from_slice(&0x10_0000u32.to_be_bytes());
        assert_eq!(
            nack_code(e.handle(&big).unwrap()),
            generic_nack::MESSAGE_TOO_LARGE
        );
        let badlen = DoIpMessage::new(payload_type::VEHICLE_ID_REQUEST_VIN, vec![1; 3]).encode();
        assert_eq!(
            nack_code(e.handle(&badlen).unwrap()),
            generic_nack::INVALID_PAYLOAD_LENGTH
        );
        let tcp_only = DoIpMessage::new(payload_type::ALIVE_CHECK_REQUEST, vec![]).encode();
        assert_eq!(
            nack_code(e.handle(&tcp_only).unwrap()),
            generic_nack::UNKNOWN_PAYLOAD_TYPE
        );
        let unknown = DoIpMessage::new(0x7777, vec![]).encode();
        assert_eq!(
            nack_code(e.handle(&unknown).unwrap()),
            generic_nack::UNKNOWN_PAYLOAD_TYPE
        );
        // Another entity's announcement / a NACK is ignored.
        assert!(e.handle(&e.announcement()).is_none());
        assert!(e
            .handle(&DoIpMessage::new(payload_type::GENERIC_NEGATIVE_ACK, vec![0]).encode())
            .is_none());
    }

    #[test]
    fn faults_once_then_always() {
        let mut e = entity();
        let req = DoIpMessage::new(payload_type::POWER_MODE_REQUEST, vec![]).encode();
        e.inject_once(UdpFault::NoReply);
        e.inject_once(UdpFault::Nack(0x03));
        e.set_fault(Some(UdpFault::BadVersion));
        assert!(e.handle(&req).is_none());
        assert_eq!(decode(&e.handle(&req).unwrap()).payload, vec![0x03]);
        assert!(parse_datagram(&e.handle(&req).unwrap()).is_err());
        e.set_fault(None);
        assert!(parse_datagram(&e.handle(&req).unwrap()).is_ok());
        assert_eq!(e.request_count(), 4);
    }

    #[test]
    fn fault_names_parse() {
        assert_eq!("no-reply".parse(), Ok(UdpFault::NoReply));
        assert_eq!("bad-version".parse(), Ok(UdpFault::BadVersion));
        assert_eq!("nack:0x04".parse(), Ok(UdpFault::Nack(4)));
        assert_eq!("nack:1".parse(), Ok(UdpFault::Nack(1)));
        assert!("nack:zz".parse::<UdpFault>().is_err());
        assert!("weird".parse::<UdpFault>().is_err());
    }
}
