//! ISO 13400 Diagnostics-over-IP (DoIP): a [`DiagTransport`] that runs UDS over
//! a TCP connection to a DoIP gateway, so the existing [`UdsClient`] works over
//! Ethernet with no CAN hardware.
//!
//! [`UdsClient`]: crate::protocol::uds::UdsClient
//!
//! The transport is generic over a [`DoIpStream`] — a minimal byte-stream
//! abstraction (`write_all` + `read_exact` with a timeout) — so it is fully
//! testable offline: against an in-memory mock, or against a `std::net`
//! loopback server in a test thread. A blanket [`DoIpStream`] implementation
//! for [`std::net::TcpStream`] is provided, and [`DoIpTransport::connect`]
//! dials `ip:port` over TCP for real gateways.
//!
//! Implemented from the ISO 13400-2 message catalogue (header, routing
//! activation, diagnostic message + ack); reimplemented, not derived from any
//! GPL source.
use crate::protocol::diag::DiagTransport;
use crate::{Error, Result};

/// DoIP protocol version. 0x02 = ISO 13400-2:2012; the header carries its
/// bitwise inverse for validation.
pub const PROTOCOL_VERSION: u8 = 0x02;

/// The fixed DoIP header length: version, inverse version, payload type (2),
/// payload length (4).
pub const HEADER_LEN: usize = 8;

/// The IANA-registered DoIP port for both the TCP data socket and the UDP
/// discovery socket (ISO 13400-2 §5.1).
pub const DEFAULT_PORT: u16 = 13400;

/// DoIP functional-group logical address (ISO 13400-2, Table 13). The CAN
/// functional broadcast id 0x7DF maps to this over DoIP.
pub const FUNCTIONAL_GROUP_ADDRESS: u16 = 0xE400;

/// DoIP payload types (ISO 13400-2, Table 17).
pub mod payload_type {
    pub const GENERIC_NEGATIVE_ACK: u16 = 0x0000;
    pub const VEHICLE_ID_REQUEST: u16 = 0x0001;
    /// Vehicle identification request with an EID (6-byte) selector.
    pub const VEHICLE_ID_REQUEST_EID: u16 = 0x0002;
    /// Vehicle identification request with a VIN (17-byte) selector.
    pub const VEHICLE_ID_REQUEST_VIN: u16 = 0x0003;
    /// Vehicle announcement / vehicle identification response — same payload
    /// format whether solicited (response to 0x0001/0x0002/0x0003) or an
    /// unsolicited power-on announcement.
    pub const VEHICLE_ANNOUNCEMENT: u16 = 0x0004;
    pub const ROUTING_ACTIVATION_REQUEST: u16 = 0x0005;
    pub const ROUTING_ACTIVATION_RESPONSE: u16 = 0x0006;
    pub const ALIVE_CHECK_REQUEST: u16 = 0x0007;
    pub const ALIVE_CHECK_RESPONSE: u16 = 0x0008;
    pub const DIAGNOSTIC_MESSAGE: u16 = 0x8001;
    pub const DIAGNOSTIC_MESSAGE_POS_ACK: u16 = 0x8002;
    pub const DIAGNOSTIC_MESSAGE_NEG_ACK: u16 = 0x8003;
}

/// Generic-header negative-ack codes (ISO 13400-2, Table 18).
pub mod generic_nack {
    pub const INCORRECT_PATTERN: u8 = 0x00;
    pub const UNKNOWN_PAYLOAD_TYPE: u8 = 0x01;
    pub const MESSAGE_TOO_LARGE: u8 = 0x02;
    pub const OUT_OF_MEMORY: u8 = 0x03;
    pub const INVALID_PAYLOAD_LENGTH: u8 = 0x04;

    pub fn name(code: u8) -> &'static str {
        match code {
            INCORRECT_PATTERN => "incorrectPatternFormat",
            UNKNOWN_PAYLOAD_TYPE => "unknownPayloadType",
            MESSAGE_TOO_LARGE => "messageTooLarge",
            OUT_OF_MEMORY => "outOfMemory",
            INVALID_PAYLOAD_LENGTH => "invalidPayloadLength",
            _ => "unknown",
        }
    }
}

/// Diagnostic-message negative-ack codes (ISO 13400-2, Table 24).
pub mod diag_nack {
    pub const INVALID_SOURCE_ADDRESS: u8 = 0x02;
    pub const UNKNOWN_TARGET_ADDRESS: u8 = 0x03;
    pub const MESSAGE_TOO_LARGE: u8 = 0x04;
    pub const OUT_OF_MEMORY: u8 = 0x05;
    pub const TARGET_UNREACHABLE: u8 = 0x06;
    pub const UNKNOWN_NETWORK: u8 = 0x07;
    pub const TRANSPORT_PROTOCOL_ERROR: u8 = 0x08;

    pub fn name(code: u8) -> &'static str {
        match code {
            INVALID_SOURCE_ADDRESS => "invalidSourceAddress",
            UNKNOWN_TARGET_ADDRESS => "unknownTargetAddress",
            MESSAGE_TOO_LARGE => "diagnosticMessageTooLarge",
            OUT_OF_MEMORY => "outOfMemory",
            TARGET_UNREACHABLE => "targetUnreachable",
            UNKNOWN_NETWORK => "unknownNetwork",
            TRANSPORT_PROTOCOL_ERROR => "transportProtocolError",
            _ => "unknown",
        }
    }
}

/// Routing-activation types and response codes (ISO 13400-2, Tables 23 & 25).
pub mod routing_activation {
    /// Default activation type.
    pub const TYPE_DEFAULT: u8 = 0x00;
    /// WWH-OBD activation type.
    pub const TYPE_WWH_OBD: u8 = 0x01;
    /// Central-security activation type.
    pub const TYPE_CENTRAL_SECURITY: u8 = 0xE0;

    /// "Routing successfully activated."
    pub const RESPONSE_SUCCESS: u8 = 0x10;
    pub const RESPONSE_UNKNOWN_SOURCE: u8 = 0x00;
    pub const RESPONSE_NO_SOCKET: u8 = 0x01;
    pub const RESPONSE_SOURCE_IN_USE: u8 = 0x02;
    pub const RESPONSE_SOURCE_ALREADY_ACTIVE: u8 = 0x03;
    pub const RESPONSE_MISSING_AUTHENTICATION: u8 = 0x04;
    pub const RESPONSE_REJECTED_CONFIRMATION: u8 = 0x05;
    pub const RESPONSE_UNSUPPORTED_TYPE: u8 = 0x06;
    /// "Routing activation requires confirmation; vehicle manufacturer specific."
    pub const RESPONSE_SUCCESS_CONFIRMATION_REQUIRED: u8 = 0x11;

    pub fn name(code: u8) -> &'static str {
        match code {
            RESPONSE_UNKNOWN_SOURCE => "unknownSourceAddress",
            RESPONSE_NO_SOCKET => "noFreeSocket",
            RESPONSE_SOURCE_IN_USE => "sourceAddressInUseByAnotherSocket",
            RESPONSE_SOURCE_ALREADY_ACTIVE => "sourceAddressAlreadyActivated",
            RESPONSE_MISSING_AUTHENTICATION => "missingAuthentication",
            RESPONSE_REJECTED_CONFIRMATION => "rejectedConfirmation",
            RESPONSE_UNSUPPORTED_TYPE => "unsupportedActivationType",
            RESPONSE_SUCCESS => "routingSuccessfullyActivated",
            RESPONSE_SUCCESS_CONFIRMATION_REQUIRED => "activatedConfirmationRequired",
            _ => "unknown",
        }
    }
}

/// A DoIP vehicle identification response / announcement (ISO 13400-2, Table
/// 10): the entity's VIN, its logical address, the EID and GID group
/// identifiers, and the "further action required" / VIN-GID sync fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VehicleInfo {
    /// 17-byte VIN (ASCII; 0x00-padded when the entity has no VIN yet).
    pub vin: [u8; 17],
    /// The DoIP entity's logical address.
    pub logical_address: u16,
    /// Entity ID (typically the MAC address of the responding entity).
    pub eid: [u8; 6],
    /// Group ID (0 or equal to the EID when the entity is not grouped).
    pub gid: [u8; 6],
    /// Further-action-required byte (0x00 = none).
    pub further_action: u8,
    /// Optional VIN/GID sync status byte (only present in 2012+ responses).
    pub sync_status: Option<u8>,
}

impl VehicleInfo {
    /// Minimum payload length: VIN(17) + address(2) + EID(6) + GID(6) + further(1).
    pub const MIN_LEN: usize = 17 + 2 + 6 + 6 + 1;

    /// Parse a vehicle announcement / identification response payload.
    pub fn parse(p: &[u8]) -> Result<Self> {
        if p.len() < Self::MIN_LEN {
            return Err(Error::Protocol(format!(
                "short DoIP vehicle announcement: {} bytes, need {}",
                p.len(),
                Self::MIN_LEN
            )));
        }
        let mut vin = [0u8; 17];
        vin.copy_from_slice(&p[0..17]);
        let logical_address = u16::from_be_bytes([p[17], p[18]]);
        let mut eid = [0u8; 6];
        eid.copy_from_slice(&p[19..25]);
        let mut gid = [0u8; 6];
        gid.copy_from_slice(&p[25..31]);
        let further_action = p[31];
        let sync_status = p.get(32).copied();
        Ok(Self {
            vin,
            logical_address,
            eid,
            gid,
            further_action,
            sync_status,
        })
    }

    /// The VIN as a trimmed ASCII string (trailing NULs/spaces removed).
    pub fn vin_string(&self) -> String {
        String::from_utf8_lossy(&self.vin)
            .trim_end_matches(['\0', ' '])
            .to_string()
    }
}

/// Build an (unselective) vehicle identification request (payload type 0x0001,
/// empty payload). Broadcast over UDP for discovery, or sent over the TCP data
/// socket to identify a known entity.
pub fn vehicle_identification_request() -> DoIpMessage {
    DoIpMessage::new(payload_type::VEHICLE_ID_REQUEST, Vec::new())
}

/// A parsed DoIP message: a payload type and its (already length-checked) body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoIpMessage {
    pub payload_type: u16,
    pub payload: Vec<u8>,
}

impl DoIpMessage {
    pub fn new(payload_type: u16, payload: Vec<u8>) -> Self {
        Self {
            payload_type,
            payload,
        }
    }

    /// Encode the message with its 8-byte DoIP header.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        out.push(PROTOCOL_VERSION);
        out.push(!PROTOCOL_VERSION);
        out.extend_from_slice(&self.payload_type.to_be_bytes());
        out.extend_from_slice(&(self.payload.len() as u32).to_be_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    /// Parse and validate the 8-byte header. Returns the payload type and the
    /// declared payload length. Rejects a version whose inverse byte does not
    /// match (ISO 13400-2 §5.2).
    pub fn parse_header(hdr: &[u8]) -> Result<(u16, usize)> {
        if hdr.len() < HEADER_LEN {
            return Err(Error::Protocol("short DoIP header".into()));
        }
        let version = hdr[0];
        let inverse = hdr[1];
        if inverse != !version {
            return Err(Error::Protocol(format!(
                "DoIP header version/inverse mismatch: 0x{version:02X}/0x{inverse:02X}"
            )));
        }
        let ptype = u16::from_be_bytes([hdr[2], hdr[3]]);
        let plen = u32::from_be_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as usize;
        Ok((ptype, plen))
    }
}

/// A byte stream the DoIP transport runs over. Abstracts [`std::net::TcpStream`]
/// so the transport can be tested against an in-memory or loopback mock.
pub trait DoIpStream {
    /// Write the whole buffer.
    fn write_all(&mut self, data: &[u8]) -> Result<()>;
    /// Fill `buf` completely, waiting at most `timeout_ms` for the data.
    fn read_exact(&mut self, buf: &mut [u8], timeout_ms: u64) -> Result<()>;
}

/// `std::net::TcpStream` as a DoIP byte stream: per-call read timeout + a full
/// `read_exact`.
impl DoIpStream for std::net::TcpStream {
    fn write_all(&mut self, data: &[u8]) -> Result<()> {
        use std::io::Write;
        Write::write_all(self, data)?;
        Ok(())
    }

    fn read_exact(&mut self, buf: &mut [u8], timeout_ms: u64) -> Result<()> {
        use std::io::Read;
        self.set_read_timeout(Some(std::time::Duration::from_millis(timeout_ms.max(1))))?;
        Read::read_exact(self, buf).map_err(|e| match e.kind() {
            std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::UnexpectedEof => Error::Timeout(timeout_ms),
            _ => Error::Io(e),
        })
    }
}

/// ISO 13400 DoIP transport. Opens a session (routing activation) and then
/// carries each UDS request as a diagnostic message, returning the UDS response.
pub struct DoIpTransport<S: DoIpStream> {
    stream: S,
    /// The tester's logical (source) address.
    pub source_address: u16,
    /// The target ECU's logical address.
    pub target_address: u16,
    /// Whether routing activation has completed successfully.
    activated: bool,
    /// The DoIP entity's logical address, learned from the routing-activation
    /// response (ISO 13400-2 §7.1). May differ from `target_address` when a
    /// gateway routes to several ECUs.
    entity_address: Option<u16>,
}

impl DoIpTransport<std::net::TcpStream> {
    /// Dial `ip:port` over TCP and build a transport. The caller then calls
    /// [`activate`](DoIpTransport::activate) before diagnostics.
    pub fn connect(
        ip: std::net::IpAddr,
        port: u16,
        source_address: u16,
        target_address: u16,
    ) -> Result<Self> {
        let stream = std::net::TcpStream::connect((ip, port))?;
        Ok(Self::with_stream(stream, source_address, target_address))
    }
}

impl<S: DoIpStream> DoIpTransport<S> {
    /// Build a transport over an existing stream (the testable constructor).
    pub fn with_stream(stream: S, source_address: u16, target_address: u16) -> Self {
        Self {
            stream,
            source_address,
            target_address,
            activated: false,
            entity_address: None,
        }
    }

    /// Read one DoIP message (header + payload), enforcing the declared length.
    fn read_message(&mut self, timeout_ms: u64) -> Result<DoIpMessage> {
        let mut hdr = [0u8; HEADER_LEN];
        self.stream.read_exact(&mut hdr, timeout_ms)?;
        let (ptype, plen) = DoIpMessage::parse_header(&hdr)?;
        // Guard against absurd declared lengths from a hostile/garbled peer.
        if plen > 0x0100_0000 {
            return Err(Error::Protocol("DoIP payload length too large".into()));
        }
        let mut payload = vec![0u8; plen];
        if plen > 0 {
            self.stream.read_exact(&mut payload, timeout_ms)?;
        }
        Ok(DoIpMessage::new(ptype, payload))
    }

    fn write_message(&mut self, msg: &DoIpMessage) -> Result<()> {
        self.stream.write_all(&msg.encode())
    }

    /// Perform the routing-activation handshake with the default activation
    /// type (ISO 13400-2 §7.1). Must succeed before diagnostic messages are
    /// accepted by the gateway.
    pub fn activate(&mut self, timeout_ms: u64) -> Result<()> {
        self.activate_with(routing_activation::TYPE_DEFAULT, timeout_ms)
    }

    /// Perform the routing-activation handshake with an explicit activation
    /// type (e.g. [`routing_activation::TYPE_CENTRAL_SECURITY`]).
    pub fn activate_with(&mut self, activation_type: u8, timeout_ms: u64) -> Result<()> {
        // Request payload: source address (2) + activation type (1) + reserved (4).
        let mut payload = Vec::with_capacity(7);
        payload.extend_from_slice(&self.source_address.to_be_bytes());
        payload.push(activation_type);
        payload.extend_from_slice(&[0u8; 4]); // ISO reserved
        self.write_message(&DoIpMessage::new(
            payload_type::ROUTING_ACTIVATION_REQUEST,
            payload,
        ))?;

        // The entity may interleave an alive-check request before answering.
        let resp = loop {
            let msg = self.read_message(timeout_ms)?;
            match msg.payload_type {
                payload_type::ROUTING_ACTIVATION_RESPONSE => break msg,
                payload_type::ALIVE_CHECK_REQUEST => self.answer_alive_check()?,
                payload_type::GENERIC_NEGATIVE_ACK => {
                    let nack = msg.payload.first().copied().unwrap_or(0);
                    return Err(Error::Protocol(format!(
                        "DoIP generic negative ack 0x{nack:02X} ({})",
                        generic_nack::name(nack)
                    )));
                }
                other => {
                    return Err(Error::Protocol(format!(
                        "expected routing activation response, got payload type 0x{other:04X}"
                    )));
                }
            }
        };
        // Response: tester addr (2) + entity addr (2) + response code (1) + reserved (4).
        if resp.payload.len() < 5 {
            return Err(Error::Protocol("short routing activation response".into()));
        }
        self.entity_address = Some(u16::from_be_bytes([resp.payload[2], resp.payload[3]]));
        let code = resp.payload[4];
        if code != routing_activation::RESPONSE_SUCCESS
            && code != routing_activation::RESPONSE_SUCCESS_CONFIRMATION_REQUIRED
        {
            return Err(Error::Protocol(format!(
                "routing activation denied: response code 0x{code:02X} ({})",
                routing_activation::name(code)
            )));
        }
        self.activated = true;
        Ok(())
    }

    /// Send a vehicle identification request over the (already connected) TCP
    /// data socket and parse the entity's announcement response. Useful to read
    /// the VIN and the entity's logical address before (or without) routing
    /// activation.
    pub fn identify(&mut self, timeout_ms: u64) -> Result<VehicleInfo> {
        self.write_message(&vehicle_identification_request())?;
        loop {
            let msg = self.read_message(timeout_ms)?;
            match msg.payload_type {
                payload_type::VEHICLE_ANNOUNCEMENT => {
                    let info = VehicleInfo::parse(&msg.payload)?;
                    self.entity_address = Some(info.logical_address);
                    return Ok(info);
                }
                payload_type::ALIVE_CHECK_REQUEST => self.answer_alive_check()?,
                payload_type::GENERIC_NEGATIVE_ACK => {
                    let nack = msg.payload.first().copied().unwrap_or(0);
                    return Err(Error::Protocol(format!(
                        "DoIP generic negative ack 0x{nack:02X} ({})",
                        generic_nack::name(nack)
                    )));
                }
                other => {
                    return Err(Error::Protocol(format!(
                        "expected vehicle announcement, got payload type 0x{other:04X}"
                    )));
                }
            }
        }
    }

    /// Reply to an entity-initiated alive-check request (ISO 13400-2 §7.2) with
    /// our source address, keeping the socket from being torn down.
    fn answer_alive_check(&mut self) -> Result<()> {
        let payload = self.source_address.to_be_bytes().to_vec();
        self.write_message(&DoIpMessage::new(
            payload_type::ALIVE_CHECK_RESPONSE,
            payload,
        ))
    }

    /// True once routing activation has succeeded.
    pub fn is_activated(&self) -> bool {
        self.activated
    }

    /// The DoIP entity's logical address, once learned from routing activation
    /// or a vehicle identification response.
    pub fn entity_address(&self) -> Option<u16> {
        self.entity_address
    }

    /// Write one UDS payload as a DoIP diagnostic message.
    fn send_diagnostic(&mut self, uds: &[u8]) -> Result<()> {
        // Diagnostic message payload: source (2) + target (2) + UDS data.
        let mut payload = Vec::with_capacity(4 + uds.len());
        payload.extend_from_slice(&self.source_address.to_be_bytes());
        payload.extend_from_slice(&self.target_address.to_be_bytes());
        payload.extend_from_slice(uds);
        self.write_message(&DoIpMessage::new(payload_type::DIAGNOSTIC_MESSAGE, payload))
    }

    /// Read DoIP messages until a diagnostic message (the UDS response) arrives,
    /// validating and skipping any interleaved diagnostic-message acks.
    fn read_uds_response(&mut self, timeout_ms: u64) -> Result<Vec<u8>> {
        loop {
            let msg = self.read_message(timeout_ms)?;
            match msg.payload_type {
                payload_type::DIAGNOSTIC_MESSAGE => {
                    // source(2) + target(2) + UDS data.
                    if msg.payload.len() < 4 {
                        return Err(Error::Protocol("short DoIP diagnostic message".into()));
                    }
                    return Ok(msg.payload[4..].to_vec());
                }
                payload_type::DIAGNOSTIC_MESSAGE_POS_ACK => {
                    // source(2)+target(2)+ack code(1); code 0 = ok. Keep reading.
                    if msg.payload.len() >= 5 && msg.payload[4] != 0x00 {
                        return Err(Error::Protocol(format!(
                            "diagnostic message ack code 0x{:02X}",
                            msg.payload[4]
                        )));
                    }
                }
                payload_type::DIAGNOSTIC_MESSAGE_NEG_ACK => {
                    let code = msg.payload.get(4).copied().unwrap_or(0);
                    return Err(Error::Protocol(format!(
                        "diagnostic message negative ack 0x{code:02X} ({})",
                        diag_nack::name(code)
                    )));
                }
                payload_type::ALIVE_CHECK_REQUEST => {
                    // The entity is probing the socket mid-exchange; answer and
                    // keep waiting for the diagnostic response.
                    self.answer_alive_check()?;
                }
                payload_type::GENERIC_NEGATIVE_ACK => {
                    let nack = msg.payload.first().copied().unwrap_or(0);
                    return Err(Error::Protocol(format!(
                        "DoIP generic negative ack 0x{nack:02X} ({})",
                        generic_nack::name(nack)
                    )));
                }
                other => {
                    return Err(Error::Protocol(format!(
                        "unexpected DoIP payload type 0x{other:04X}"
                    )));
                }
            }
        }
    }
}

impl<S: DoIpStream> DiagTransport for DoIpTransport<S> {
    fn request(&mut self, req: &[u8], timeout_ms: u64) -> Result<Vec<u8>> {
        if !self.activated {
            return Err(Error::Protocol(
                "DoIP routing not activated; call activate() first".into(),
            ));
        }
        self.send_diagnostic(req)?;
        // Honor UDS responsePending (0x78): the ECU sends more diagnostic
        // messages (each its own DoIP message) until the real response.
        for _ in 0..32 {
            let resp = self.read_uds_response(timeout_ms)?;
            if resp.len() >= 3 && resp[0] == 0x7F && resp[2] == 0x78 {
                continue;
            }
            return Ok(resp);
        }
        Err(Error::Timeout(timeout_ms))
    }
}

/// Discover DoIP entities by UDP vehicle identification (ISO 13400-2 §6.2).
///
/// Binds `bind_addr`, sends one vehicle identification request to `dest_addr`,
/// and collects every vehicle announcement that arrives within `timeout_ms`
/// (each entity answers once; a bus may hold several). Set `broadcast` to send
/// to a broadcast/multicast address.
///
/// 🔒 The real-network broadcast path (`dest_addr` = `255.255.255.255:13400`,
/// `broadcast = true`) is untested against physical silicon; the offline
/// loopback/unicast path is exercised in the tests.
pub fn discover(
    bind_addr: std::net::SocketAddr,
    dest_addr: std::net::SocketAddr,
    timeout_ms: u64,
    broadcast: bool,
) -> Result<Vec<(std::net::SocketAddr, VehicleInfo)>> {
    let sock = std::net::UdpSocket::bind(bind_addr)?;
    sock.set_broadcast(broadcast)?;
    sock.set_read_timeout(Some(std::time::Duration::from_millis(timeout_ms.max(1))))?;
    sock.send_to(&vehicle_identification_request().encode(), dest_addr)?;

    let mut found = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        match sock.recv_from(&mut buf) {
            Ok((n, from)) => {
                if n < HEADER_LEN {
                    continue;
                }
                let Ok((ptype, plen)) = DoIpMessage::parse_header(&buf[..HEADER_LEN]) else {
                    continue;
                };
                if ptype == payload_type::VEHICLE_ANNOUNCEMENT && n >= HEADER_LEN + plen {
                    if let Ok(info) = VehicleInfo::parse(&buf[HEADER_LEN..HEADER_LEN + plen]) {
                        found.push((from, info));
                    }
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break
            }
            Err(e) => return Err(Error::Io(e)),
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::uds::UdsClient;
    use std::collections::VecDeque;

    /// An in-memory DoIP peer: scripted responses the transport reads, and a
    /// log of what the transport wrote.
    struct MockStream {
        /// Bytes the transport will read, enqueued as whole DoIP messages.
        inbox: VecDeque<u8>,
        /// Everything the transport wrote.
        written: Vec<u8>,
    }

    impl MockStream {
        fn new(messages: Vec<DoIpMessage>) -> Self {
            let mut inbox = VecDeque::new();
            for m in messages {
                inbox.extend(m.encode());
            }
            Self {
                inbox,
                written: Vec::new(),
            }
        }
    }

    impl DoIpStream for MockStream {
        fn write_all(&mut self, data: &[u8]) -> Result<()> {
            self.written.extend_from_slice(data);
            Ok(())
        }
        fn read_exact(&mut self, buf: &mut [u8], timeout_ms: u64) -> Result<()> {
            if self.inbox.len() < buf.len() {
                return Err(Error::Timeout(timeout_ms));
            }
            for b in buf.iter_mut() {
                *b = self.inbox.pop_front().unwrap();
            }
            Ok(())
        }
    }

    fn routing_ok(tester: u16, entity: u16) -> DoIpMessage {
        let mut p = Vec::new();
        p.extend_from_slice(&tester.to_be_bytes());
        p.extend_from_slice(&entity.to_be_bytes());
        p.push(routing_activation::RESPONSE_SUCCESS);
        p.extend_from_slice(&[0u8; 4]);
        DoIpMessage::new(payload_type::ROUTING_ACTIVATION_RESPONSE, p)
    }

    fn diag_pos_ack(src: u16, tgt: u16) -> DoIpMessage {
        let mut p = Vec::new();
        p.extend_from_slice(&src.to_be_bytes());
        p.extend_from_slice(&tgt.to_be_bytes());
        p.push(0x00); // ack
        DoIpMessage::new(payload_type::DIAGNOSTIC_MESSAGE_POS_ACK, p)
    }

    fn diag_msg(src: u16, tgt: u16, uds: &[u8]) -> DoIpMessage {
        let mut p = Vec::new();
        p.extend_from_slice(&src.to_be_bytes());
        p.extend_from_slice(&tgt.to_be_bytes());
        p.extend_from_slice(uds);
        DoIpMessage::new(payload_type::DIAGNOSTIC_MESSAGE, p)
    }

    fn announcement(vin: &[u8; 17], addr: u16, eid: [u8; 6]) -> DoIpMessage {
        let mut p = Vec::new();
        p.extend_from_slice(vin);
        p.extend_from_slice(&addr.to_be_bytes());
        p.extend_from_slice(&eid);
        p.extend_from_slice(&eid); // GID = EID (not grouped)
        p.push(0x00); // no further action
        DoIpMessage::new(payload_type::VEHICLE_ANNOUNCEMENT, p)
    }

    fn alive_check_request() -> DoIpMessage {
        DoIpMessage::new(payload_type::ALIVE_CHECK_REQUEST, Vec::new())
    }

    #[test]
    fn header_encode_decode_round_trips() {
        let msg = DoIpMessage::new(payload_type::DIAGNOSTIC_MESSAGE, vec![1, 2, 3, 4, 5]);
        let bytes = msg.encode();
        assert_eq!(bytes[0], PROTOCOL_VERSION);
        assert_eq!(bytes[1], !PROTOCOL_VERSION);
        let (ptype, plen) = DoIpMessage::parse_header(&bytes).unwrap();
        assert_eq!(ptype, payload_type::DIAGNOSTIC_MESSAGE);
        assert_eq!(plen, 5);
        assert_eq!(&bytes[HEADER_LEN..], &msg.payload[..]);
    }

    #[test]
    fn header_rejects_bad_inverse_version() {
        let mut bytes = DoIpMessage::new(payload_type::VEHICLE_ID_REQUEST, vec![]).encode();
        bytes[1] = 0x00; // wrong inverse
        assert!(matches!(
            DoIpMessage::parse_header(&bytes),
            Err(Error::Protocol(_))
        ));
    }

    #[test]
    fn routing_activation_handshake() {
        let tester = 0x0E00;
        let entity = 0x1000;
        let stream = MockStream::new(vec![routing_ok(tester, entity)]);
        let mut tp = DoIpTransport::with_stream(stream, tester, entity);
        tp.activate(1000).unwrap();
        assert!(tp.is_activated());

        // The transport sent exactly one routing-activation request with our
        // source address and the default activation type.
        let (ptype, plen) = DoIpMessage::parse_header(&tp.stream.written).unwrap();
        assert_eq!(ptype, payload_type::ROUTING_ACTIVATION_REQUEST);
        assert_eq!(plen, 7);
        let body = &tp.stream.written[HEADER_LEN..];
        assert_eq!(u16::from_be_bytes([body[0], body[1]]), tester);
        assert_eq!(body[2], routing_activation::TYPE_DEFAULT);
    }

    #[test]
    fn routing_activation_denied_errors() {
        let mut p = Vec::new();
        p.extend_from_slice(&0x0E00u16.to_be_bytes());
        p.extend_from_slice(&0x1000u16.to_be_bytes());
        p.push(0x06); // "routing activation denied, SA unknown"
        p.extend_from_slice(&[0u8; 4]);
        let denied = DoIpMessage::new(payload_type::ROUTING_ACTIVATION_RESPONSE, p);
        let mut tp = DoIpTransport::with_stream(MockStream::new(vec![denied]), 0x0E00, 0x1000);
        assert!(tp.activate(1000).is_err());
        assert!(!tp.is_activated());
    }

    #[test]
    fn uds_round_trip_over_doip() {
        let tester = 0x0E00;
        let ecu = 0x1000;
        // Scripted: routing ok, then (for the RDBI request) pos-ack + response.
        let resp_uds = vec![0x62, 0xF1, 0x90, b'V', b'I', b'N'];
        let msgs = vec![
            routing_ok(tester, ecu),
            diag_pos_ack(ecu, tester),
            diag_msg(ecu, tester, &resp_uds),
        ];
        let mut tp = DoIpTransport::with_stream(MockStream::new(msgs), tester, ecu);
        tp.activate(1000).unwrap();

        // Drive the real UdsClient over the DoIP transport.
        let mut uds = UdsClient::new(&mut tp);
        let data = uds.read_data_by_id(0xF190).unwrap();
        assert_eq!(&data, b"VIN");
    }

    #[test]
    fn uds_response_pending_is_followed() {
        let tester = 0x0E00;
        let ecu = 0x1000;
        // pos-ack, then responsePending (0x7F 22 78), then the real response.
        let pending = vec![0x7F, 0x22, 0x78];
        let real = vec![0x62, 0xF1, 0x90, 0x42];
        let msgs = vec![
            routing_ok(tester, ecu),
            diag_pos_ack(ecu, tester),
            diag_msg(ecu, tester, &pending),
            diag_msg(ecu, tester, &real),
        ];
        let mut tp = DoIpTransport::with_stream(MockStream::new(msgs), tester, ecu);
        tp.activate(1000).unwrap();
        let mut uds = UdsClient::new(&mut tp);
        let data = uds.read_data_by_id(0xF190).unwrap();
        assert_eq!(data, vec![0x42]);
    }

    #[test]
    fn request_before_activation_errors() {
        let mut tp = DoIpTransport::with_stream(MockStream::new(vec![]), 0x0E00, 0x1000);
        assert!(tp.request(&[0x22, 0xF1, 0x90], 1000).is_err());
    }

    #[test]
    fn vehicle_info_parses_and_trims_vin() {
        let mut vin = *b"WVWZZZ1KZAW000000";
        let info =
            VehicleInfo::parse(&announcement(&vin, 0x1000, [1, 2, 3, 4, 5, 6]).payload).unwrap();
        assert_eq!(info.logical_address, 0x1000);
        assert_eq!(info.eid, [1, 2, 3, 4, 5, 6]);
        assert_eq!(info.gid, [1, 2, 3, 4, 5, 6]);
        assert_eq!(info.further_action, 0x00);
        assert_eq!(info.sync_status, None);
        assert_eq!(info.vin_string(), "WVWZZZ1KZAW000000");

        // A padded VIN trims trailing NULs.
        vin[14] = 0;
        vin[15] = 0;
        vin[16] = 0;
        let padded = VehicleInfo::parse(&announcement(&vin, 0x1000, [0; 6]).payload).unwrap();
        assert_eq!(padded.vin_string(), "WVWZZZ1KZAW000");
    }

    #[test]
    fn vehicle_info_rejects_short_payload() {
        assert!(matches!(
            VehicleInfo::parse(&[0u8; 10]),
            Err(Error::Protocol(_))
        ));
    }

    #[test]
    fn identify_over_tcp_stream() {
        let vin = *b"ABCDEFGH123456789";
        let msgs = vec![announcement(&vin, 0x1234, [0xAA; 6])];
        let mut tp = DoIpTransport::with_stream(MockStream::new(msgs), 0x0E00, 0x1000);
        let info = tp.identify(1000).unwrap();
        assert_eq!(info.logical_address, 0x1234);
        assert_eq!(tp.entity_address(), Some(0x1234));
        // It sent exactly one vehicle identification request (0x0001, no body).
        let (ptype, plen) = DoIpMessage::parse_header(&tp.stream.written).unwrap();
        assert_eq!(ptype, payload_type::VEHICLE_ID_REQUEST);
        assert_eq!(plen, 0);
    }

    #[test]
    fn activation_learns_entity_address() {
        let stream = MockStream::new(vec![routing_ok(0x0E00, 0x1000)]);
        let mut tp = DoIpTransport::with_stream(stream, 0x0E00, 0x1000);
        tp.activate(1000).unwrap();
        assert_eq!(tp.entity_address(), Some(0x1000));
    }

    #[test]
    fn alive_check_mid_exchange_is_answered() {
        let tester = 0x0E00;
        let ecu = 0x1000;
        // routing ok, then an alive-check request before the real response.
        let resp_uds = vec![0x62, 0xF1, 0x90, 0x7B];
        let msgs = vec![
            routing_ok(tester, ecu),
            alive_check_request(),
            diag_pos_ack(ecu, tester),
            diag_msg(ecu, tester, &resp_uds),
        ];
        let mut tp = DoIpTransport::with_stream(MockStream::new(msgs), tester, ecu);
        tp.activate(1000).unwrap();
        let before = tp.stream.written.len();
        let mut uds = UdsClient::new(&mut tp);
        assert_eq!(uds.read_data_by_id(0xF190).unwrap(), vec![0x7B]);
        // After the diagnostic request, an alive-check response (type 0x0008)
        // was written in answer to the entity's probe. Walk every message
        // written during the exchange and confirm one is the alive response.
        let mut rest = &tp.stream.written[before..];
        let mut saw_alive = false;
        while rest.len() >= HEADER_LEN {
            let (ptype, plen) = DoIpMessage::parse_header(rest).unwrap();
            if ptype == payload_type::ALIVE_CHECK_RESPONSE {
                saw_alive = true;
            }
            rest = &rest[HEADER_LEN + plen..];
        }
        assert!(saw_alive, "expected an alive-check response to be sent");
    }

    #[test]
    fn routing_activation_confirmation_required_is_accepted() {
        let mut p = Vec::new();
        p.extend_from_slice(&0x0E00u16.to_be_bytes());
        p.extend_from_slice(&0x1000u16.to_be_bytes());
        p.push(routing_activation::RESPONSE_SUCCESS_CONFIRMATION_REQUIRED);
        p.extend_from_slice(&[0u8; 4]);
        let msg = DoIpMessage::new(payload_type::ROUTING_ACTIVATION_RESPONSE, p);
        let mut tp = DoIpTransport::with_stream(MockStream::new(vec![msg]), 0x0E00, 0x1000);
        tp.activate(1000).unwrap();
        assert!(tp.is_activated());
    }

    /// Offline UDP discovery: a loopback "entity" answers one vehicle
    /// identification request with an announcement. Unicast to 127.0.0.1 — no
    /// broadcast, no external network.
    #[test]
    fn udp_discover_loopback() {
        use std::net::UdpSocket;

        let entity = UdpSocket::bind("127.0.0.1:0").unwrap();
        let entity_addr = entity.local_addr().unwrap();

        let server = std::thread::spawn(move || {
            let mut buf = [0u8; 256];
            let (n, from) = entity.recv_from(&mut buf).unwrap();
            let (ptype, _) = DoIpMessage::parse_header(&buf[..HEADER_LEN]).unwrap();
            assert_eq!(ptype, payload_type::VEHICLE_ID_REQUEST);
            assert_eq!(n, HEADER_LEN); // empty request
            let vin = *b"DISCOVERYVIN00001";
            entity
                .send_to(&announcement(&vin, 0x2001, [9; 6]).encode(), from)
                .unwrap();
        });

        let found = discover("127.0.0.1:0".parse().unwrap(), entity_addr, 500, false).unwrap();
        server.join().unwrap();

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1.logical_address, 0x2001);
        assert_eq!(found[0].1.vin_string(), "DISCOVERYVIN00001");
    }

    /// Proves the `std::net::TcpStream` `DoIpStream` impl works end to end: a
    /// loopback DoIP "gateway" in a thread answers a routing activation and one
    /// UDS request. Offline — no external network.
    #[test]
    fn loopback_tcp_round_trip() {
        use std::io::{Read, Write};
        use std::net::{TcpListener, TcpStream};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();

            // Read one full DoIP message helper (std::io, not the DoIp trait).
            let read_msg = |sock: &mut TcpStream| -> DoIpMessage {
                let mut hdr = [0u8; HEADER_LEN];
                Read::read_exact(sock, &mut hdr).unwrap();
                let (ptype, plen) = DoIpMessage::parse_header(&hdr).unwrap();
                let mut payload = vec![0u8; plen];
                Read::read_exact(sock, &mut payload).unwrap();
                DoIpMessage::new(ptype, payload)
            };

            // Routing activation.
            let req = read_msg(&mut sock);
            assert_eq!(req.payload_type, payload_type::ROUTING_ACTIVATION_REQUEST);
            let tester = u16::from_be_bytes([req.payload[0], req.payload[1]]);
            Write::write_all(&mut sock, &routing_ok(tester, 0x1000).encode()).unwrap();

            // One diagnostic request -> pos-ack + response.
            let diag = read_msg(&mut sock);
            assert_eq!(diag.payload_type, payload_type::DIAGNOSTIC_MESSAGE);
            Write::write_all(&mut sock, &diag_pos_ack(0x1000, tester).encode()).unwrap();
            Write::write_all(
                &mut sock,
                &diag_msg(0x1000, tester, &[0x62, 0xF1, 0x90, 0x99]).encode(),
            )
            .unwrap();
        });

        let stream = TcpStream::connect(addr).unwrap();
        let mut tp = DoIpTransport::with_stream(stream, 0x0E00, 0x1000);
        tp.activate(2000).unwrap();
        let mut uds = UdsClient::new(&mut tp);
        let data = uds.read_data_by_id(0xF190).unwrap();
        assert_eq!(data, vec![0x99]);

        server.join().unwrap();
    }
}
