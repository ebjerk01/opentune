//! ISO 13400-2 DoIP **vehicle discovery and entity status over UDP**, and the
//! one-call hop from a discovered entity to an activated [`DoIpTransport`].
//!
//! - [`DoIpUdpClient::identify`] sends a vehicle identification request — all
//!   entities (`0x0001`), by EID (`0x0002`) or by VIN (`0x0003`), see
//!   [`VehicleIdRequest`] — to a unicast, broadcast (`255.255.255.255`) or IPv6
//!   multicast (`ff02::1`) address on UDP port 13400, and collects the vehicle
//!   identification responses (`0x0004`) that arrive before the timeout, one
//!   per entity (repeats are counted, not listed).
//! - [`DoIpUdpClient::listen`] collects unsolicited vehicle announcements (an
//!   entity sends three after power-up / link-up) on the discovery port.
//! - [`DoIpUdpClient::entity_status`] (`0x4001` → `0x4002`) and
//!   [`DoIpUdpClient::power_mode`] (`0x4003` → `0x4004`).
//! - [`connect_entity`] dials a discovered entity's TCP data port and runs
//!   routing activation (central-security activation when the entity asked for
//!   it with further-action `0x10`).
//!
//! Every datagram is checked like an entity checks it: one DoIP message per
//! datagram, version / inverse version, a supported version, a sane declared
//! length that matches the datagram, and the fixed length of its payload type
//! ([`parse_datagram`]). Malformed datagrams are counted and dropped; a generic
//! negative acknowledge (`0x0000`) is reported by name.
//!
//! Implemented from the ISO 13400-2 message catalogue; no code copied. Offline
//! tests run against loopback sockets and the `opentune-udssim` UDP entity.
//!
//! 🔒 Broadcast / multicast discovery on a real vehicle network is untested.
use crate::protocol::doip::{
    check_header, generic_nack, payload_len_valid, payload_type, remaining_ms, routing_activation,
    DoIpMessage, DoIpTransport, VehicleInfo, DEFAULT_PORT, HEADER_LEN, PROTOCOL_VERSION,
};
use crate::{Error, Result};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

/// Receive buffer for one discovery datagram.
const UDP_BUF: usize = 4096;

/// Largest payload accepted in one UDP DoIP datagram (either side).
pub const MAX_UDP_PAYLOAD: usize = UDP_BUF - HEADER_LEN;

/// ISO 13400-2 `A_DoIP_Ctrl`: how long a tester waits for UDP answers (2 s).
pub const A_DOIP_CTRL_MS: u64 = 2000;

/// The IPv6 link-local all-nodes group, used in place of IPv4 broadcast.
pub const IPV6_ALL_NODES: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1);

/// The IPv4 limited-broadcast discovery destination.
pub const IPV4_BROADCAST: Ipv4Addr = Ipv4Addr::BROADCAST;

/// Further-action-required values in a vehicle announcement (ISO 13400-2 Table 6).
pub mod further_action {
    /// No further action required.
    pub const NONE: u8 = 0x00;
    /// Routing activation required to initiate central security.
    pub const CENTRAL_SECURITY: u8 = 0x10;

    /// A readable name for a further-action value.
    pub fn name(code: u8) -> &'static str {
        match code {
            NONE => "noFurtherActionRequired",
            CENTRAL_SECURITY => "routingActivationRequiredForCentralSecurity",
            0x11..=0xFF => "vmSpecific",
            _ => "reserved",
        }
    }
}

/// VIN/GID sync status values in a vehicle announcement (ISO 13400-2 Table 7).
pub mod sync_status {
    /// VIN and/or GID are synchronized.
    pub const SYNCHRONIZED: u8 = 0x00;
    /// Incomplete: VIN and GID are not synchronized.
    pub const INCOMPLETE: u8 = 0x10;

    /// A readable name for a sync-status value.
    pub fn name(code: u8) -> &'static str {
        match code {
            SYNCHRONIZED => "synchronized",
            INCOMPLETE => "incomplete",
            _ => "reserved",
        }
    }
}

/// Node types in an entity status response (ISO 13400-2 Table 11).
pub mod node_type {
    /// A DoIP gateway (routes to ECUs behind it).
    pub const GATEWAY: u8 = 0x00;
    /// A DoIP node (a single ECU on Ethernet).
    pub const NODE: u8 = 0x01;

    /// A readable name for a node type.
    pub fn name(code: u8) -> &'static str {
        match code {
            GATEWAY => "gateway",
            NODE => "node",
            _ => "reserved",
        }
    }
}

/// A vehicle identification request: every entity, or only the one with a
/// given EID or VIN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VehicleIdRequest {
    /// `0x0001`: every entity answers.
    All,
    /// `0x0002`: only the entity with this EID answers.
    ByEid([u8; 6]),
    /// `0x0003`: only entities of the vehicle with this VIN answer.
    ByVin([u8; 17]),
}

impl VehicleIdRequest {
    /// Select by VIN: exactly 17 ASCII characters.
    pub fn by_vin(vin: &str) -> Result<Self> {
        let b = vin.as_bytes();
        if b.len() != 17 || !vin.is_ascii() {
            return Err(Error::Config(format!(
                "a DoIP VIN selector is 17 ASCII characters, got {:?}",
                vin
            )));
        }
        let mut v = [0u8; 17];
        v.copy_from_slice(b);
        Ok(Self::ByVin(v))
    }

    /// Select by EID: 6 bytes of hex, optionally separated by `:` or `-`
    /// (`02:00:00:00:00:10` or `020000000010`).
    pub fn by_eid(hex: &str) -> Result<Self> {
        let clean: String = hex.chars().filter(|c| !matches!(c, ':' | '-')).collect();
        let bad = || Error::Config(format!("a DoIP EID is 6 hex bytes, got {hex:?}"));
        if clean.len() != 12 || !clean.is_ascii() {
            return Err(bad());
        }
        let mut eid = [0u8; 6];
        for (i, b) in eid.iter_mut().enumerate() {
            *b = u8::from_str_radix(&clean[2 * i..2 * i + 2], 16).map_err(|_| bad())?;
        }
        Ok(Self::ByEid(eid))
    }

    /// The request's payload type.
    pub fn payload_type(&self) -> u16 {
        match self {
            Self::All => payload_type::VEHICLE_ID_REQUEST,
            Self::ByEid(_) => payload_type::VEHICLE_ID_REQUEST_EID,
            Self::ByVin(_) => payload_type::VEHICLE_ID_REQUEST_VIN,
        }
    }

    /// The request as a DoIP message.
    pub fn to_message(&self) -> DoIpMessage {
        let payload = match self {
            Self::All => Vec::new(),
            Self::ByEid(e) => e.to_vec(),
            Self::ByVin(v) => v.to_vec(),
        };
        DoIpMessage::new(self.payload_type(), payload)
    }

    /// Decode a vehicle identification request (the entity side).
    pub fn from_message(msg: &DoIpMessage) -> Result<Self> {
        let p = &msg.payload;
        match (msg.payload_type, p.len()) {
            (payload_type::VEHICLE_ID_REQUEST, 0) => Ok(Self::All),
            (payload_type::VEHICLE_ID_REQUEST_EID, 6) => {
                let mut e = [0u8; 6];
                e.copy_from_slice(p);
                Ok(Self::ByEid(e))
            }
            (payload_type::VEHICLE_ID_REQUEST_VIN, 17) => {
                let mut v = [0u8; 17];
                v.copy_from_slice(p);
                Ok(Self::ByVin(v))
            }
            (t, n) => Err(Error::Protocol(format!(
                "not a vehicle identification request: type 0x{t:04X}, {n} bytes"
            ))),
        }
    }

    /// Whether an entity with `info` should answer this request.
    pub fn matches(&self, info: &VehicleInfo) -> bool {
        match self {
            Self::All => true,
            Self::ByEid(e) => &info.eid == e,
            Self::ByVin(v) => &info.vin == v,
        }
    }
}

/// A DoIP entity status response (`0x4002`, ISO 13400-2 Table 11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntityStatus {
    /// [`node_type::GATEWAY`] or [`node_type::NODE`].
    pub node_type: u8,
    /// Max. concurrent TCP_DATA sockets (MCTS).
    pub max_open_sockets: u8,
    /// Currently open TCP_DATA sockets (NCTS).
    pub open_sockets: u8,
    /// Max. size of one logical request (MDS); optional in the response.
    pub max_data_size: Option<u32>,
}

impl EntityStatus {
    /// Parse a 3- or 7-byte entity status payload.
    pub fn parse(p: &[u8]) -> Result<Self> {
        let max_data_size = match p.len() {
            3 => None,
            7 => Some(u32::from_be_bytes([p[3], p[4], p[5], p[6]])),
            n => {
                return Err(Error::Protocol(format!(
                    "DoIP entity status response must be 3 or 7 bytes, got {n}"
                )))
            }
        };
        Ok(Self {
            node_type: p[0],
            max_open_sockets: p[1],
            open_sockets: p[2],
            max_data_size,
        })
    }

    /// Encode as an entity status payload.
    pub fn to_payload(&self) -> Vec<u8> {
        let mut p = vec![self.node_type, self.max_open_sockets, self.open_sockets];
        if let Some(m) = self.max_data_size {
            p.extend_from_slice(&m.to_be_bytes());
        }
        p
    }
}

/// Diagnostic power mode (`0x4004`, ISO 13400-2 Table 9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerMode {
    /// `0x00`: not ready for reliable diagnostics.
    NotReady,
    /// `0x01`: ready.
    Ready,
    /// `0x02`: the entity does not report a power mode.
    NotSupported,
    /// Any other (reserved) value.
    Reserved(u8),
}

impl PowerMode {
    /// From the response byte.
    pub fn from_byte(b: u8) -> Self {
        match b {
            0x00 => Self::NotReady,
            0x01 => Self::Ready,
            0x02 => Self::NotSupported,
            other => Self::Reserved(other),
        }
    }

    /// The response byte.
    pub fn to_byte(self) -> u8 {
        match self {
            Self::NotReady => 0x00,
            Self::Ready => 0x01,
            Self::NotSupported => 0x02,
            Self::Reserved(b) => b,
        }
    }

    /// A readable name.
    pub fn name(self) -> &'static str {
        match self {
            Self::NotReady => "notReady",
            Self::Ready => "ready",
            Self::NotSupported => "notSupported",
            Self::Reserved(_) => "reserved",
        }
    }
}

/// Validate one UDP datagram as exactly one DoIP message (the entity-side
/// rules, used by testers too): the generic header ([`check_header`], with
/// [`MAX_UDP_PAYLOAD`]), a declared length equal to the rest of the datagram,
/// and a payload length valid for a known payload type. Returns the header
/// version and the message, or the generic NACK code an entity answers with.
/// Unknown payload types pass; the caller decides (`0x01`).
pub fn parse_datagram(data: &[u8]) -> std::result::Result<(u8, DoIpMessage), u8> {
    let (version, ptype, plen) = check_header(data, MAX_UDP_PAYLOAD)?;
    if data.len() != HEADER_LEN + plen || payload_len_valid(ptype, plen) == Some(false) {
        return Err(generic_nack::INVALID_PAYLOAD_LENGTH);
    }
    Ok((version, DoIpMessage::new(ptype, data[HEADER_LEN..].to_vec())))
}

/// One entity found by discovery: where its answer came from, and what it said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredEntity {
    /// Source address of the answer (the entity's IP; the port is its UDP port).
    pub addr: SocketAddr,
    /// The vehicle identification response / announcement.
    pub info: VehicleInfo,
}

/// What a discovery round collected.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Discovery {
    /// Distinct entities, in order of first answer.
    pub entities: Vec<DiscoveredEntity>,
    /// Generic negative acks received: `(from, code)`.
    pub nacks: Vec<(SocketAddr, u8)>,
    /// Repeated answers from an entity already listed (same IP, logical
    /// address and EID).
    pub duplicates: usize,
    /// Datagrams dropped as malformed (bad pattern / version, bad length, a
    /// vehicle announcement that does not parse).
    pub rejected: usize,
    /// Well-formed DoIP messages of other payload types.
    pub ignored: usize,
}

impl Discovery {
    /// Add an answer, folding a repeat from the same entity into `duplicates`.
    pub fn add(&mut self, addr: SocketAddr, info: VehicleInfo) {
        let same = |e: &DiscoveredEntity| {
            e.addr.ip() == addr.ip()
                && e.info.logical_address == info.logical_address
                && e.info.eid == info.eid
        };
        if self.entities.iter().any(same) {
            self.duplicates += 1;
        } else {
            self.entities.push(DiscoveredEntity { addr, info });
        }
    }
}

/// What one receive produced.
enum Rx {
    Msg(SocketAddr, DoIpMessage),
    Rejected(SocketAddr),
    /// A transient socket error (e.g. a Windows ICMP port-unreachable reset).
    Noise,
    Timeout,
}

fn nack_error(code: u8) -> Error {
    Error::Protocol(format!(
        "DoIP generic negative ack 0x{code:02X} ({})",
        generic_nack::name(code)
    ))
}

/// Whether `ip` addresses many hosts (so any source may answer).
fn is_group(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_broadcast() || v4.is_multicast() || v4.is_unspecified(),
        IpAddr::V6(v6) => v6.is_multicast() || v6.is_unspecified(),
    }
}

/// A UDP socket speaking the DoIP discovery / status messages.
pub struct DoIpUdpClient {
    sock: UdpSocket,
    version: u8,
}

impl DoIpUdpClient {
    /// Bind `addr` (port 0 for an ephemeral tester port).
    pub fn bind(addr: SocketAddr) -> Result<Self> {
        Ok(Self {
            sock: UdpSocket::bind(addr)?,
            version: PROTOCOL_VERSION,
        })
    }

    /// Bind an ephemeral port of `dest`'s address family, with broadcast
    /// enabled for IPv4 (needed for `255.255.255.255` and subnet broadcasts,
    /// harmless for unicast).
    pub fn for_destination(dest: SocketAddr) -> Result<Self> {
        let any: SocketAddr = match dest {
            SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
            SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
        };
        let c = Self::bind(any)?;
        if dest.is_ipv4() {
            c.set_broadcast(true)?;
        }
        Ok(c)
    }

    /// Bind the discovery port to hear unsolicited vehicle announcements
    /// (`0.0.0.0:13400`, or `[::]:13400` which also joins `ff02::1`).
    pub fn listener(bind: SocketAddr) -> Result<Self> {
        let c = Self::bind(bind)?;
        if let IpAddr::V6(v6) = bind.ip() {
            if v6.is_unspecified() {
                // Best effort: a host without IPv6 multicast still hears
                // unicast announcements.
                let _ = c.sock.join_multicast_v6(&IPV6_ALL_NODES, 0);
            }
        }
        Ok(c)
    }

    /// Allow sending to broadcast addresses.
    pub fn set_broadcast(&self, on: bool) -> Result<()> {
        self.sock.set_broadcast(on)?;
        Ok(())
    }

    /// The protocol version written into request headers (default 0x02; 0xFF
    /// is accepted by entities for vehicle identification requests only).
    pub fn set_protocol_version(&mut self, version: u8) {
        self.version = version;
    }

    /// The bound local address.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.sock.local_addr()?)
    }

    /// Send one message to `dest`.
    pub fn send_to(&self, msg: &DoIpMessage, dest: SocketAddr) -> Result<()> {
        self.sock
            .send_to(&msg.encode_with_version(self.version), dest)?;
        Ok(())
    }

    fn recv(&self, deadline: Instant) -> Result<Rx> {
        let Some(left) = remaining_ms(deadline) else {
            return Ok(Rx::Timeout);
        };
        self.sock
            .set_read_timeout(Some(Duration::from_millis(left)))?;
        let mut buf = [0u8; UDP_BUF];
        match self.sock.recv_from(&mut buf) {
            Ok((n, from)) => Ok(match parse_datagram(&buf[..n]) {
                Ok((_, msg)) => Rx::Msg(from, msg),
                Err(_) => Rx::Rejected(from),
            }),
            Err(e) => match e.kind() {
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => Ok(Rx::Timeout),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionRefused => {
                    Ok(Rx::Noise)
                }
                _ => Err(Error::Io(e)),
            },
        }
    }

    /// Fold received messages into `out` until `deadline` (or the first entity
    /// when `first_only`).
    fn collect(&self, out: &mut Discovery, deadline: Instant, first_only: bool) -> Result<()> {
        loop {
            match self.recv(deadline)? {
                Rx::Timeout => return Ok(()),
                Rx::Noise => {}
                Rx::Rejected(_) => out.rejected += 1,
                Rx::Msg(from, msg) => match msg.payload_type {
                    payload_type::VEHICLE_ANNOUNCEMENT => match VehicleInfo::parse(&msg.payload) {
                        Ok(info) => {
                            out.add(from, info);
                            if first_only {
                                return Ok(());
                            }
                        }
                        Err(_) => out.rejected += 1,
                    },
                    payload_type::GENERIC_NEGATIVE_ACK => out.nacks.push((from, msg.payload[0])),
                    _ => out.ignored += 1,
                },
            }
        }
    }

    /// Send a vehicle identification request to `dest` and collect every
    /// answer for `timeout_ms` (use [`A_DOIP_CTRL_MS`] for a broadcast).
    pub fn identify(
        &self,
        dest: SocketAddr,
        req: &VehicleIdRequest,
        timeout_ms: u64,
    ) -> Result<Discovery> {
        self.send_to(&req.to_message(), dest)?;
        let mut out = Discovery::default();
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
        self.collect(&mut out, deadline, false)?;
        Ok(out)
    }

    /// Like [`identify`](Self::identify) but returns as soon as one entity
    /// answered — for a unicast destination there is nothing more to wait for.
    pub fn identify_first(
        &self,
        dest: SocketAddr,
        req: &VehicleIdRequest,
        timeout_ms: u64,
    ) -> Result<Discovery> {
        self.send_to(&req.to_message(), dest)?;
        let mut out = Discovery::default();
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
        self.collect(&mut out, deadline, true)?;
        Ok(out)
    }

    /// Collect unsolicited vehicle announcements for `timeout_ms` (bind with
    /// [`listener`](Self::listener) on the discovery port).
    pub fn listen(&self, timeout_ms: u64) -> Result<Discovery> {
        let mut out = Discovery::default();
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
        self.collect(&mut out, deadline, false)?;
        Ok(out)
    }

    /// Send `req` and wait for a message of type `expect` from `dest` (any
    /// source when `dest` is a group address). A generic NACK from it is an
    /// error naming the code; a timeout says whether only malformed answers
    /// came back.
    fn request_one(
        &self,
        dest: SocketAddr,
        req: DoIpMessage,
        expect: u16,
        timeout_ms: u64,
    ) -> Result<(SocketAddr, DoIpMessage)> {
        self.send_to(&req, dest)?;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
        let ours = |from: &SocketAddr| is_group(dest.ip()) || from.ip() == dest.ip();
        let mut rejected = 0usize;
        loop {
            match self.recv(deadline)? {
                Rx::Timeout if rejected > 0 => {
                    return Err(Error::Protocol(format!(
                        "no valid DoIP answer from {dest} within {timeout_ms} ms \
                         ({rejected} malformed datagram(s) dropped: bad header, \
                         version or length)"
                    )))
                }
                Rx::Timeout => return Err(Error::Timeout(timeout_ms)),
                Rx::Noise => {}
                Rx::Rejected(from) => {
                    if ours(&from) {
                        rejected += 1;
                    }
                }
                Rx::Msg(from, msg) if ours(&from) => {
                    if msg.payload_type == expect {
                        return Ok((from, msg));
                    }
                    if msg.payload_type == payload_type::GENERIC_NEGATIVE_ACK {
                        return Err(nack_error(msg.payload[0]));
                    }
                }
                Rx::Msg(..) => {}
            }
        }
    }

    /// Ask `dest` for its entity status (`0x4001` → `0x4002`).
    pub fn entity_status(&self, dest: SocketAddr, timeout_ms: u64) -> Result<EntityStatus> {
        let (_, msg) = self.request_one(
            dest,
            DoIpMessage::new(payload_type::ENTITY_STATUS_REQUEST, Vec::new()),
            payload_type::ENTITY_STATUS_RESPONSE,
            timeout_ms,
        )?;
        EntityStatus::parse(&msg.payload)
    }

    /// Ask `dest` for its diagnostic power mode (`0x4003` → `0x4004`).
    pub fn power_mode(&self, dest: SocketAddr, timeout_ms: u64) -> Result<PowerMode> {
        let (_, msg) = self.request_one(
            dest,
            DoIpMessage::new(payload_type::POWER_MODE_REQUEST, Vec::new()),
            payload_type::POWER_MODE_RESPONSE,
            timeout_ms,
        )?;
        Ok(PowerMode::from_byte(msg.payload[0]))
    }
}

/// How [`connect_entity`] opens the diagnostic connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectOptions {
    /// The entity's TCP data port (13400).
    pub tcp_port: u16,
    /// The tester's logical address (0x0E00 is the usual external tester).
    pub tester_address: u16,
    /// The ECU to address; default: the entity's own logical address.
    pub target_address: Option<u16>,
    /// Routing activation type; default: central security (`0xE0`) when the
    /// entity announced further action `0x10`, else default (`0x00`).
    pub activation_type: Option<u8>,
    /// TCP connect and routing activation timeout, each.
    pub timeout_ms: u64,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            tcp_port: DEFAULT_PORT,
            tester_address: 0x0E00,
            target_address: None,
            activation_type: None,
            timeout_ms: A_DOIP_CTRL_MS,
        }
    }
}

/// Connect a discovered entity's TCP data socket and run routing activation:
/// the returned transport is ready for a `UdsClient`.
pub fn connect_entity(
    entity: &DiscoveredEntity,
    opts: &ConnectOptions,
) -> Result<DoIpTransport<TcpStream>> {
    // Keep the source address (and an IPv6 scope id); only the port changes.
    let mut addr = entity.addr;
    addr.set_port(opts.tcp_port);
    let timeout = Duration::from_millis(opts.timeout_ms.max(1));
    let stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_nodelay(true)?;
    let target = opts
        .target_address
        .unwrap_or(entity.info.logical_address);
    let kind = opts.activation_type.unwrap_or(
        if entity.info.further_action == further_action::CENTRAL_SECURITY {
            routing_activation::TYPE_CENTRAL_SECURITY
        } else {
            routing_activation::TYPE_DEFAULT
        },
    );
    let mut tp = DoIpTransport::with_stream(stream, opts.tester_address, target);
    tp.activate_with(kind, opts.timeout_ms)?;
    Ok(tp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::doip::{DEFAULT_PROTOCOL_VERSION, VehicleInfo};

    fn info(addr: u16, eid: [u8; 6]) -> VehicleInfo {
        VehicleInfo {
            vin: *b"SIMVIN00000000001",
            logical_address: addr,
            eid,
            gid: eid,
            further_action: 0,
            sync_status: Some(0),
        }
    }

    #[test]
    fn vehicle_id_request_codecs_round_trip() {
        for req in [
            VehicleIdRequest::All,
            VehicleIdRequest::by_eid("02:00:00:00:00:10").unwrap(),
            VehicleIdRequest::by_vin("SIMVIN00000000001").unwrap(),
        ] {
            let msg = req.to_message();
            assert_eq!(payload_len_valid(msg.payload_type, msg.payload.len()), Some(true));
            assert_eq!(VehicleIdRequest::from_message(&msg).unwrap(), req);
        }
        assert_eq!(
            VehicleIdRequest::by_eid("020000000010").unwrap(),
            VehicleIdRequest::ByEid([2, 0, 0, 0, 0, 0x10])
        );
        assert!(VehicleIdRequest::by_eid("0200").is_err());
        assert!(VehicleIdRequest::by_eid("zz0000000010").is_err());
        assert!(VehicleIdRequest::by_vin("SHORT").is_err());
        // Wrong length for the type is not a request.
        let bad = DoIpMessage::new(payload_type::VEHICLE_ID_REQUEST_EID, vec![1, 2]);
        assert!(VehicleIdRequest::from_message(&bad).is_err());
    }

    #[test]
    fn vehicle_id_request_selectors_match() {
        let i = info(0x10, [2, 0, 0, 0, 0, 0x10]);
        assert!(VehicleIdRequest::All.matches(&i));
        assert!(VehicleIdRequest::ByEid([2, 0, 0, 0, 0, 0x10]).matches(&i));
        assert!(!VehicleIdRequest::ByEid([9; 6]).matches(&i));
        assert!(VehicleIdRequest::by_vin("SIMVIN00000000001").unwrap().matches(&i));
        assert!(!VehicleIdRequest::by_vin("SIMVIN00000000002").unwrap().matches(&i));
    }

    #[test]
    fn vehicle_info_payload_round_trips_with_and_without_sync() {
        let mut i = info(0x1234, [1, 2, 3, 4, 5, 6]);
        let p = i.to_payload();
        assert_eq!(p.len(), 33);
        assert_eq!(VehicleInfo::parse(&p).unwrap(), i);
        i.sync_status = None;
        let p = i.to_payload();
        assert_eq!(p.len(), 32);
        assert_eq!(VehicleInfo::parse(&p).unwrap(), i);
        assert_eq!(i.eid_string(), "01:02:03:04:05:06");
    }

    #[test]
    fn entity_status_and_power_mode_codecs() {
        let s = EntityStatus {
            node_type: node_type::NODE,
            max_open_sockets: 4,
            open_sockets: 1,
            max_data_size: Some(0x0FFF),
        };
        assert_eq!(s.to_payload(), vec![1, 4, 1, 0, 0, 0x0F, 0xFF]);
        assert_eq!(EntityStatus::parse(&s.to_payload()).unwrap(), s);
        let short = EntityStatus::parse(&[0, 1, 0]).unwrap();
        assert_eq!(short.max_data_size, None);
        assert_eq!(node_type::name(short.node_type), "gateway");
        assert!(EntityStatus::parse(&[0, 1]).is_err());
        assert!(EntityStatus::parse(&[0; 5]).is_err());

        for b in 0..=4u8 {
            assert_eq!(PowerMode::from_byte(b).to_byte(), b);
        }
        assert_eq!(PowerMode::from_byte(1), PowerMode::Ready);
        assert_eq!(PowerMode::from_byte(9).name(), "reserved");
    }

    #[test]
    fn header_checks_follow_the_entity_order() {
        let ok = DoIpMessage::new(payload_type::ENTITY_STATUS_REQUEST, vec![]).encode();
        assert_eq!(
            check_header(&ok, MAX_UDP_PAYLOAD),
            Ok((PROTOCOL_VERSION, payload_type::ENTITY_STATUS_REQUEST, 0))
        );
        // Inverse mismatch, unsupported version, short header: pattern (0x00).
        let mut bad = ok.clone();
        bad[1] = 0x00;
        assert_eq!(check_header(&bad, MAX_UDP_PAYLOAD), Err(generic_nack::INCORRECT_PATTERN));
        let v9 = DoIpMessage::new(payload_type::ENTITY_STATUS_REQUEST, vec![]).encode_with_version(9);
        assert_eq!(check_header(&v9, MAX_UDP_PAYLOAD), Err(generic_nack::INCORRECT_PATTERN));
        assert_eq!(check_header(&ok[..5], MAX_UDP_PAYLOAD), Err(generic_nack::INCORRECT_PATTERN));
        // 0xFF is the default version for vehicle identification requests only.
        let vid = VehicleIdRequest::All
            .to_message()
            .encode_with_version(DEFAULT_PROTOCOL_VERSION);
        assert!(check_header(&vid, MAX_UDP_PAYLOAD).is_ok());
        let st = DoIpMessage::new(payload_type::ENTITY_STATUS_REQUEST, vec![])
            .encode_with_version(DEFAULT_PROTOCOL_VERSION);
        assert_eq!(check_header(&st, MAX_UDP_PAYLOAD), Err(generic_nack::INCORRECT_PATTERN));
        // Too large: 0x02.
        let mut big = ok.clone();
        big[4..8].copy_from_slice(&(MAX_UDP_PAYLOAD as u32 + 1).to_be_bytes());
        assert_eq!(check_header(&big, MAX_UDP_PAYLOAD), Err(generic_nack::MESSAGE_TOO_LARGE));
    }

    #[test]
    fn datagrams_must_hold_exactly_one_valid_message() {
        let req = VehicleIdRequest::ByEid([1; 6]).to_message().encode();
        assert!(parse_datagram(&req).is_ok());
        // Trailing junk / truncated payload: invalid length (0x04).
        let mut long = req.clone();
        long.push(0);
        assert_eq!(parse_datagram(&long), Err(generic_nack::INVALID_PAYLOAD_LENGTH));
        assert_eq!(
            parse_datagram(&req[..req.len() - 1]),
            Err(generic_nack::INVALID_PAYLOAD_LENGTH)
        );
        // A consistent header with the wrong fixed length for its type.
        let wrong = DoIpMessage::new(payload_type::POWER_MODE_RESPONSE, vec![1, 2]).encode();
        assert_eq!(parse_datagram(&wrong), Err(generic_nack::INVALID_PAYLOAD_LENGTH));
        // Unknown types pass; the caller decides.
        let unknown = DoIpMessage::new(0x7777, vec![1]).encode();
        assert_eq!(parse_datagram(&unknown).unwrap().1.payload_type, 0x7777);
    }

    #[test]
    fn discovery_folds_repeats_per_entity() {
        let a: SocketAddr = "127.0.0.1:13400".parse().unwrap();
        let b: SocketAddr = "127.0.0.2:13400".parse().unwrap();
        let mut d = Discovery::default();
        d.add(a, info(0x10, [1; 6]));
        d.add(a, info(0x10, [1; 6])); // repeat
        d.add(a, info(0x11, [2; 6])); // second entity behind one IP
        d.add(b, info(0x10, [1; 6])); // same ids on another host
        assert_eq!(d.entities.len(), 3);
        assert_eq!(d.duplicates, 1);
    }

    /// A tiny scripted UDP peer: answers each request with `reply(request)`.
    fn peer<F>(reply: F) -> (SocketAddr, std::thread::JoinHandle<()>)
    where
        F: Fn(&[u8]) -> Vec<Vec<u8>> + Send + 'static,
    {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = sock.local_addr().unwrap();
        sock.set_read_timeout(Some(Duration::from_millis(1500)))
            .unwrap();
        let h = std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            if let Ok((n, from)) = sock.recv_from(&mut buf) {
                for d in reply(&buf[..n]) {
                    sock.send_to(&d, from).unwrap();
                }
            }
        });
        (addr, h)
    }

    #[test]
    fn identify_collects_dedups_and_counts_junk() {
        let (addr, h) = peer(|req| {
            let (_, msg) = parse_datagram(req).unwrap();
            assert_eq!(msg.payload_type, payload_type::VEHICLE_ID_REQUEST);
            let ann = DoIpMessage::new(
                payload_type::VEHICLE_ANNOUNCEMENT,
                info(0x10, [1; 6]).to_payload(),
            )
            .encode();
            let mut bad = ann.clone();
            bad[0] = 0x07; // inverse no longer matches
            let nack = DoIpMessage::new(payload_type::GENERIC_NEGATIVE_ACK, vec![0x04]).encode();
            let other = DoIpMessage::new(payload_type::POWER_MODE_RESPONSE, vec![1]).encode();
            vec![ann.clone(), ann, bad, nack, other]
        });
        let c = DoIpUdpClient::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let d = c.identify(addr, &VehicleIdRequest::All, 300).unwrap();
        h.join().unwrap();
        assert_eq!(d.entities.len(), 1);
        assert_eq!(d.entities[0].info.logical_address, 0x10);
        assert_eq!(d.duplicates, 1);
        assert_eq!(d.rejected, 1);
        assert_eq!(d.nacks, vec![(addr, 0x04)]);
        assert_eq!(d.ignored, 1);
    }

    #[test]
    fn status_and_power_over_udp() {
        let (addr, h) = peer(|req| {
            let (_, msg) = parse_datagram(req).unwrap();
            assert_eq!(msg.payload_type, payload_type::ENTITY_STATUS_REQUEST);
            vec![DoIpMessage::new(payload_type::ENTITY_STATUS_RESPONSE, vec![0, 2, 1]).encode()]
        });
        let c = DoIpUdpClient::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let s = c.entity_status(addr, 1000).unwrap();
        h.join().unwrap();
        assert_eq!((s.node_type, s.max_open_sockets, s.open_sockets), (0, 2, 1));

        let (addr, h) = peer(|_| {
            vec![DoIpMessage::new(payload_type::POWER_MODE_RESPONSE, vec![0x00]).encode()]
        });
        assert_eq!(c.power_mode(addr, 1000).unwrap(), PowerMode::NotReady);
        h.join().unwrap();
    }

    #[test]
    fn nack_and_malformed_answers_are_named() {
        let (addr, h) = peer(|_| {
            vec![DoIpMessage::new(payload_type::GENERIC_NEGATIVE_ACK, vec![0x01]).encode()]
        });
        let c = DoIpUdpClient::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        match c.entity_status(addr, 1000) {
            Err(Error::Protocol(m)) => assert!(m.contains("unknownPayloadType"), "{m}"),
            other => panic!("expected a NACK error, got {other:?}"),
        }
        h.join().unwrap();

        let (addr, h) = peer(|_| {
            let mut d = DoIpMessage::new(payload_type::POWER_MODE_RESPONSE, vec![1]).encode();
            d[0] = 0x02;
            d[1] = 0x02; // bad inverse
            vec![d]
        });
        match c.power_mode(addr, 200) {
            Err(Error::Protocol(m)) => assert!(m.contains("malformed"), "{m}"),
            other => panic!("expected a malformed-answer error, got {other:?}"),
        }
        h.join().unwrap();

        let (addr, h) = peer(|_| Vec::new());
        assert!(matches!(c.power_mode(addr, 100), Err(Error::Timeout(100))));
        h.join().unwrap();
    }
}
